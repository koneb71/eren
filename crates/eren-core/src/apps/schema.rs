//! Turning declared models into tables, and deciding what that costs.
//!
//! Pure. Everything here takes what the manifest says and what Postgres
//! actually has, and returns the statements that would reconcile them — it
//! never runs one. That split is what makes the destructive half reviewable:
//! the SQL a person approves is the SQL that executes, because there is nothing
//! between the two.
//!
//! ## `required` is not `NOT NULL`
//!
//! A declared field is never given a `NOT NULL` constraint, and that is a
//! decision rather than an oversight. Adding a required field to a table that
//! already has rows cannot be done without inventing a value for every existing
//! row, and inventing someone's data is worse than checking at the door. So
//! `required` is enforced on write, by Eren, where it can say which field was
//! missing. The three columns Eren owns — `id`, `created_at`, `updated_at` —
//! are `NOT NULL`, because it is Eren that fills them.

use super::manifest::{FieldType, Model, RESERVED_FIELDS};
use sha2::{Digest, Sha256};

/// A table as Postgres currently has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTable {
    pub name: String,
    pub columns: Vec<LiveColumn>,
    pub indexes: Vec<LiveIndex>,
    pub foreign_keys: Vec<LiveForeignKey>,
}

/// An index that is not a constraint's own (the primary key's is not one a
/// manifest asks for).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveIndex {
    pub name: String,
    /// The column, for an index on exactly one; `None` for anything wider.
    pub column: Option<String>,
}

/// A single-column foreign key, and the table it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveForeignKey {
    pub name: String,
    pub column: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveColumn {
    pub name: String,
    /// `information_schema.columns.data_type`, verbatim.
    pub data_type: String,
}

/// One statement, and what it means in words.
///
/// Serialised into a pending plan, so the SQL a person approved is byte for
/// byte the SQL that later runs. Recomputing it at apply time would mean
/// executing statements derived from a manifest that may have changed since.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stmt {
    pub sql: String,
    /// Whether running this can lose something that is already there.
    pub destructive: bool,
    /// A sentence for the person being asked. Written as an effect, not as a
    /// restatement of the SQL, which they can already see.
    pub why: String,
}

impl Stmt {
    fn safe(sql: impl Into<String>, why: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            destructive: false,
            why: why.into(),
        }
    }
    fn destroys(sql: impl Into<String>, why: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            destructive: true,
            why: why.into(),
        }
    }
}

/// The `information_schema` spelling of a field's type.
///
/// Kept beside `FieldType::sql` deliberately: one is what we write, the other
/// is what Postgres says when asked back, and they are not the same string.
/// Comparing the wrong one would report every `datetime` column as drifted
/// forever, and a permanent false alarm is how a gate stops being read.
pub fn live_type_of(ty: &FieldType) -> &'static str {
    match ty {
        FieldType::Text => "text",
        FieldType::Int => "bigint",
        FieldType::Decimal => "numeric",
        FieldType::Bool => "boolean",
        FieldType::Date => "date",
        FieldType::Datetime => "timestamp with time zone",
        FieldType::Json => "jsonb",
        FieldType::Ref(_) => "uuid",
    }
}

/// The longest name Postgres keeps. A longer one is cut to this many bytes
/// without a word, so `CREATE INDEX` reports success under a name nobody
/// asked for — which no longer ends in `_idx`, and two long names can come out
/// the same, the second `IF NOT EXISTS` then quietly doing nothing.
const MAX_IDENT: usize = 63;

/// The name Eren gives an index or key on one field.
///
/// Unchanged wherever it fits, so an app whose names always fitted sees no
/// difference. A longer one keeps its suffix and gives up the middle to a
/// short hash of the whole name, which is what keeps two of them apart.
fn object_name(model: &str, field: &str, suffix: &str) -> String {
    let full = format!("{model}_{field}_{suffix}");
    if full.len() <= MAX_IDENT {
        return full;
    }
    let digest = Sha256::digest(full.as_bytes());
    let hash: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    // Every manifest identifier is ASCII (see `manifest::ident`), so cutting
    // at a byte count cannot split a character.
    let keep = MAX_IDENT - hash.len() - suffix.len() - 2;
    format!("{}_{hash}_{suffix}", &full[..keep])
}

/// What Postgres named it before [`object_name`] shortened long names
/// itself: the first 63 bytes. `None` where the name always fitted.
fn truncated_name(model: &str, field: &str, suffix: &str) -> Option<String> {
    let full = format!("{model}_{field}_{suffix}");
    (full.len() > MAX_IDENT).then(|| full[..MAX_IDENT].to_string())
}

fn index_name(model: &str, field: &str) -> String {
    object_name(model, field, "idx")
}

fn fk_name(model: &str, field: &str) -> String {
    object_name(model, field, "fk")
}

/// Whether an index is one the manifest's `indexes` speak for, and so one the
/// planner may drop: Eren's spelling, or what Postgres made of a long one.
fn is_managed_index(model: &str, name: &str) -> bool {
    name.ends_with("_idx") || (name.len() == MAX_IDENT && name.starts_with(&format!("{model}_")))
}

/// Reconcile a schema with what a manifest declares.
///
/// Ordered so it can be run top to bottom in one transaction: the schema, then
/// the foreign keys the manifest no longer wants, then every table, then the
/// keys it does. Unwanted keys go first because Postgres will not change a
/// column's type while a key holds it; wanted ones come last because a `ref:`
/// may point at a model declared after it, and a manifest's legality should
/// not depend on the order someone happened to write it in.
pub fn plan(schema: &str, models: &[Model], live: &[LiveTable]) -> Vec<Stmt> {
    let mut out = vec![Stmt::safe(
        format!("CREATE SCHEMA IF NOT EXISTS {}", q(schema)),
        format!("Make room for this app's tables in a schema of its own ({schema})."),
    )];

    // A key on a field that is no longer a `ref:`, or now points elsewhere.
    // Dropping one changes no value — it only stops checking them. A table
    // about to be dropped is left to its own CASCADE.
    for table in live {
        let Some(model) = models.iter().find(|m| m.name == table.name) else {
            continue;
        };
        for key in &table.foreign_keys {
            let wanted = model.fields.iter().any(|f| {
                f.name == key.column && matches!(&f.ty, FieldType::Ref(t) if *t == key.target)
            });
            if !wanted {
                out.push(Stmt::safe(
                    format!(
                        "ALTER TABLE {}.{} DROP CONSTRAINT IF EXISTS {}",
                        q(schema),
                        q(&table.name),
                        q(&key.name)
                    ),
                    format!(
                        "Stop requiring \"{}\" to point at a row in \"{}\". No values change.",
                        key.column, key.target
                    ),
                ));
            }
        }
    }

    for model in models {
        let table = live.iter().find(|t| t.name == model.name);
        match table {
            None => out.push(create_table(schema, model)),
            Some(table) => alter_table(schema, model, table, &mut out),
        }
        // Outside the match, because a table being new is not a reason to skip
        // its indexes — which is exactly the bug this shape replaces: indexes
        // lived in the alter path only, so a declared index first appeared on
        // the *second* reconcile and a freshly installed app never had one.
        reconcile_indexes(schema, model, table, &mut out);
    }

    // A table Postgres has that the manifest no longer declares. Dropping it is
    // the only way "make it match" can mean anything, but it is also the single
    // most expensive mistake available here, so it is always a question.
    for table in live {
        if !models.iter().any(|m| m.name == table.name) {
            out.push(Stmt::destroys(
                format!("DROP TABLE {}.{} CASCADE", q(schema), q(&table.name)),
                format!(
                    "Delete the \"{}\" table and every row in it. The manifest no \
                     longer declares this model.",
                    table.name
                ),
            ));
        }
    }

    for model in models {
        let table = live.iter().find(|t| t.name == model.name);
        for field in &model.fields {
            let FieldType::Ref(target) = &field.ty else {
                continue;
            };
            // Asked of Postgres, not inferred from the column being there: a
            // field that became a `ref:`, or changed what it points at, has
            // the column already and still needs its key.
            let keyed = table.is_some_and(|t| {
                t.foreign_keys
                    .iter()
                    .any(|k| k.column == field.name && k.target == *target)
            });
            if keyed {
                continue;
            }
            // A column that already holds values may hold some that are not
            // ids in the target — every one of them, for a key that moved to
            // another model — and the key cannot be added over them. They are
            // cleared first, which loses them, so that is a question. A
            // column created in this same plan holds nothing yet.
            let has_values = table.is_some_and(|t| t.columns.iter().any(|c| c.name == field.name));
            if has_values {
                out.push(Stmt::destroys(
                    format!(
                        "UPDATE {}.{} AS t SET {} = NULL WHERE t.{} IS NOT NULL \
                         AND NOT EXISTS (SELECT 1 FROM {}.{} AS r WHERE r.id = t.{})",
                        q(schema),
                        q(&model.name),
                        q(&field.name),
                        q(&field.name),
                        q(schema),
                        q(target),
                        q(&field.name)
                    ),
                    format!(
                        "Clear every \"{}\" in \"{}\" that does not point at a row in \
                         \"{target}\". Those values are lost.",
                        field.name, model.name
                    ),
                ));
            }
            out.push(Stmt::safe(
                format!(
                    "ALTER TABLE {}.{} ADD CONSTRAINT {} FOREIGN KEY ({}) \
                     REFERENCES {}.{}(id) ON DELETE SET NULL",
                    q(schema),
                    q(&model.name),
                    q(&fk_name(&model.name, &field.name)),
                    q(&field.name),
                    q(schema),
                    q(target)
                ),
                format!(
                    "Point \"{}\" at a row in \"{target}\", and clear it if that \
                     row is deleted.",
                    field.name
                ),
            ));
        }
    }

    out
}

fn create_table(schema: &str, model: &Model) -> Stmt {
    let mut cols = vec![
        "\"id\" UUID PRIMARY KEY DEFAULT gen_random_uuid()".to_string(),
        "\"created_at\" TIMESTAMPTZ NOT NULL DEFAULT now()".to_string(),
        "\"updated_at\" TIMESTAMPTZ NOT NULL DEFAULT now()".to_string(),
    ];
    for field in &model.fields {
        cols.push(format!("{} {}", q(&field.name), field.ty.sql()));
    }
    Stmt::safe(
        format!(
            "CREATE TABLE IF NOT EXISTS {}.{} (\n  {}\n)",
            q(schema),
            q(&model.name),
            cols.join(",\n  ")
        ),
        format!("Create the \"{}\" table.", model.name),
    )
}

fn alter_table(schema: &str, model: &Model, live: &LiveTable, out: &mut Vec<Stmt>) {
    for field in &model.fields {
        match live.columns.iter().find(|c| c.name == field.name) {
            None => out.push(Stmt::safe(
                format!(
                    "ALTER TABLE {}.{} ADD COLUMN IF NOT EXISTS {} {}",
                    q(schema),
                    q(&model.name),
                    q(&field.name),
                    field.ty.sql()
                ),
                format!("Add \"{}\" to \"{}\".", field.name, model.name),
            )),
            Some(column) if column.data_type != live_type_of(&field.ty) => {
                out.push(Stmt::destroys(
                    format!(
                        "ALTER TABLE {}.{} ALTER COLUMN {} TYPE {} USING {}::{}",
                        q(schema),
                        q(&model.name),
                        q(&field.name),
                        field.ty.sql(),
                        q(&field.name),
                        field.ty.sql()
                    ),
                    format!(
                        "Change \"{}\" from {} to {}. Values that will not convert are \
                         lost, and the change cannot be undone.",
                        field.name,
                        column.data_type,
                        field.ty.as_str()
                    ),
                ));
            }
            Some(_) => {}
        }
    }

    // A column Postgres has that the model no longer declares. The three Eren
    // owns are skipped: they are not the manifest's to remove.
    for column in &live.columns {
        if RESERVED_FIELDS.contains(&column.name.as_str()) {
            continue;
        }
        if !model.fields.iter().any(|f| f.name == column.name) {
            out.push(Stmt::destroys(
                format!(
                    "ALTER TABLE {}.{} DROP COLUMN {}",
                    q(schema),
                    q(&model.name),
                    q(&column.name)
                ),
                format!(
                    "Delete the \"{}\" column of \"{}\" and everything stored in it.",
                    column.name, model.name
                ),
            ));
        }
    }
}

/// Bring a model's indexes in line. Neither direction is ever a question: an
/// index holds no data, so creating or dropping one cannot lose anything.
///
/// `live` is `None` when the table is being created in this same plan, which is
/// why this is not part of `alter_table`.
fn reconcile_indexes(schema: &str, model: &Model, live: Option<&LiveTable>, out: &mut Vec<Stmt>) {
    let existing: &[LiveIndex] = live.map_or(&[], |t| t.indexes.as_slice());
    // Every existing index a declared one accounts for; the rest of the
    // managed ones are dropped below.
    let mut kept: Vec<&str> = Vec::new();

    for field in &model.indexes {
        let name = index_name(&model.name, field);
        if let Some(found) = existing.iter().find(|i| i.name == name) {
            kept.push(&found.name);
            continue;
        }
        // Made before long names were shortened here, so Postgres cut it. It
        // is renamed rather than rebuilt — but only if it really is on this
        // field: two long names could be cut to the same one, and then the
        // index belongs to whichever field claimed the name first.
        let cut = truncated_name(&model.name, field, "idx");
        if let Some(found) = existing
            .iter()
            .find(|i| Some(&i.name) == cut.as_ref() && i.column.as_deref() == Some(field.as_str()))
        {
            kept.push(&found.name);
            out.push(Stmt::safe(
                format!(
                    "ALTER INDEX {}.{} RENAME TO {}",
                    q(schema),
                    q(&found.name),
                    q(&name)
                ),
                format!(
                    "Rename the \"{}\" index to \"{name}\", which Postgres keeps whole. No \
                     rows are affected.",
                    found.name
                ),
            ));
            continue;
        }
        out.push(Stmt::safe(
            format!(
                "CREATE INDEX IF NOT EXISTS {} ON {}.{} ({})",
                q(&name),
                q(schema),
                q(&model.name),
                q(field)
            ),
            format!("Make looking up \"{}\" by \"{field}\" fast.", model.name),
        ));
    }
    for index in existing {
        if kept.contains(&index.name.as_str()) || !is_managed_index(&model.name, &index.name) {
            continue;
        }
        let name = &index.name;
        out.push(Stmt::safe(
            format!("DROP INDEX IF EXISTS {}.{}", q(schema), q(name)),
            format!("Drop the \"{name}\" index. No rows are affected."),
        ));
    }
}

/// Quote an identifier for Postgres.
///
/// Every name reaching this has already been through the manifest's charset
/// check — lower-case letters, digits and underscores — so there is nothing
/// here for the quoting to save us from. It is belt and braces, and the doubled
/// quote handles the case where a future caller forgets the braces.
fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Whether a plan needs a person before it can run.
pub fn needs_approval(plan: &[Stmt]) -> bool {
    plan.iter().any(|s| s.destructive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::manifest;

    fn models(yaml: &str) -> Vec<Model> {
        manifest::parse(yaml)
            .expect("test manifest must parse")
            .models
    }

    const ONE: &str = "name: T\nmodels:\n  expense:\n    fields:\n      \
                       note: { type: text }\n      amount: { type: decimal }\n";

    fn live_expense() -> Vec<LiveTable> {
        vec![LiveTable {
            name: "expense".into(),
            columns: vec![
                LiveColumn {
                    name: "id".into(),
                    data_type: "uuid".into(),
                },
                LiveColumn {
                    name: "created_at".into(),
                    data_type: "timestamp with time zone".into(),
                },
                LiveColumn {
                    name: "updated_at".into(),
                    data_type: "timestamp with time zone".into(),
                },
                LiveColumn {
                    name: "note".into(),
                    data_type: "text".into(),
                },
                LiveColumn {
                    name: "amount".into(),
                    data_type: "numeric".into(),
                },
            ],
            indexes: vec![],
            foreign_keys: vec![],
        }]
    }

    #[test]
    fn a_schema_that_already_matches_plans_nothing() {
        // The property that keeps the gate meaningful: a rebuild that changed
        // no models must not ask. A plan that always has something in it is a
        // plan nobody reads.
        let plan = plan("app_t", &models(ONE), &live_expense());
        let real: Vec<&Stmt> = plan
            .iter()
            .filter(|s| !s.sql.starts_with("CREATE SCHEMA"))
            .collect();
        assert!(real.is_empty(), "expected nothing to do, got {real:#?}");
        assert!(!needs_approval(&plan));
    }

    #[test]
    fn a_new_model_is_created_without_asking() {
        let plan = plan("app_t", &models(ONE), &[]);
        assert!(!needs_approval(&plan));
        let create = plan
            .iter()
            .find(|s| s.sql.contains("CREATE TABLE"))
            .unwrap();
        // Eren's own three columns are there, and they are the only NOT NULLs.
        assert!(create.sql.contains("\"id\" UUID PRIMARY KEY"));
        assert!(create.sql.contains("\"created_at\" TIMESTAMPTZ NOT NULL"));
        assert!(create.sql.contains("\"note\" TEXT"));
        assert!(create.sql.contains("\"amount\" NUMERIC"));
    }

    #[test]
    fn a_declared_field_is_never_not_null() {
        // Adding a required column to a table with rows would mean inventing a
        // value for every one of them. `required` is checked on write instead.
        let m = models(
            "name: T\nmodels:\n  t:\n    fields:\n      a: { type: text, required: true }\n",
        );
        let plan = plan("app_t", &m, &[]);
        let create = plan
            .iter()
            .find(|s| s.sql.contains("CREATE TABLE"))
            .unwrap();
        assert!(create.sql.contains("\"a\" TEXT"));
        assert!(
            !create.sql.contains("\"a\" TEXT NOT NULL"),
            "a declared field must not be NOT NULL: {}",
            create.sql
        );
    }

    #[test]
    fn adding_a_field_applies_itself_but_removing_one_asks() {
        let grown = models(
            "name: T\nmodels:\n  expense:\n    fields:\n      note: { type: text }\n      \
             amount: { type: decimal }\n      paid: { type: bool }\n",
        );
        let added = plan("app_t", &grown, &live_expense());
        assert!(!needs_approval(&added), "adding a column loses nothing");
        assert!(added
            .iter()
            .any(|s| s.sql.contains("ADD COLUMN IF NOT EXISTS \"paid\"")));

        let shrunk =
            models("name: T\nmodels:\n  expense:\n    fields:\n      note: { type: text }\n");
        let removed = plan("app_t", &shrunk, &live_expense());
        assert!(needs_approval(&removed));
        let drop = removed
            .iter()
            .find(|s| s.sql.contains("DROP COLUMN"))
            .unwrap();
        assert!(drop.destructive);
        assert!(drop.sql.contains("\"amount\""));
        assert!(drop.why.contains("everything stored in it"), "{}", drop.why);
    }

    #[test]
    fn changing_a_type_asks_and_says_what_it_costs() {
        let retyped = models(
            "name: T\nmodels:\n  expense:\n    fields:\n      note: { type: text }\n      \
                    amount: { type: int }\n",
        );
        let plan = plan("app_t", &retyped, &live_expense());
        assert!(needs_approval(&plan));
        let alter = plan
            .iter()
            .find(|s| s.sql.contains("ALTER COLUMN"))
            .unwrap();
        assert!(alter.destructive);
        assert!(
            alter.why.contains("numeric"),
            "names what it is now: {}",
            alter.why
        );
        assert!(
            alter.why.contains("int"),
            "names what it becomes: {}",
            alter.why
        );
    }

    #[test]
    fn dropping_a_whole_model_asks() {
        let plan = plan("app_t", &[], &live_expense());
        let drop = plan.iter().find(|s| s.sql.contains("DROP TABLE")).unwrap();
        assert!(drop.destructive);
        assert!(drop.sql.contains("\"app_t\".\"expense\""));
    }

    #[test]
    fn the_columns_eren_owns_are_never_proposed_for_deletion() {
        // They are absent from every manifest by construction, so a naive diff
        // would try to drop all three on the first edit of every app.
        let plan = plan("app_t", &models(ONE), &live_expense());
        for reserved in RESERVED_FIELDS {
            assert!(
                !plan
                    .iter()
                    .any(|s| s.sql.contains(&format!("DROP COLUMN \"{reserved}\""))),
                "proposed dropping {reserved}"
            );
        }
    }

    #[test]
    fn what_postgres_calls_a_type_is_what_gets_compared() {
        // The false-alarm bug this prevents: we write TIMESTAMPTZ, Postgres
        // reports "timestamp with time zone", and comparing our spelling to its
        // spelling marks every datetime column as drifted on every build.
        let m = models("name: T\nmodels:\n  t:\n    fields:\n      at: { type: datetime }\n");
        let live = vec![LiveTable {
            name: "t".into(),
            columns: vec![LiveColumn {
                name: "at".into(),
                data_type: "timestamp with time zone".into(),
            }],
            indexes: vec![],
            foreign_keys: vec![],
        }];
        let plan = plan("app_t", &m, &live);
        assert!(
            !plan.iter().any(|s| s.sql.contains("ALTER COLUMN")),
            "a matching datetime column must not look changed"
        );
        assert_eq!(
            live_type_of(&FieldType::Datetime),
            "timestamp with time zone"
        );
        assert_eq!(FieldType::Datetime.sql(), "TIMESTAMPTZ");
    }

    #[test]
    fn a_brand_new_table_gets_its_declared_indexes_too() {
        // The bug this pins: index reconciliation used to live only in the
        // path for tables that already exist, so a declared index first
        // appeared on the *second* reconcile and a freshly installed app never
        // had one at all. Caught against a real Postgres, not by a test.
        let m = models(
            "name: T\nmodels:\n  entry:\n    fields:\n      at: { type: datetime }\n    \
             indexes: [at]\n",
        );
        let fresh = plan("app_t", &m, &[]);
        let create = fresh
            .iter()
            .position(|s| s.sql.contains("CREATE TABLE"))
            .unwrap();
        let index = fresh
            .iter()
            .position(|s| s.sql.contains("CREATE INDEX"))
            .expect("a new table must get its indexes in the same plan");
        assert!(index > create, "the index was created before its table");
        assert!(fresh[index].sql.contains("\"entry_at_idx\""));
    }

    #[test]
    fn indexes_move_in_both_directions_and_neither_is_a_question() {
        let indexed = models(
            "name: T\nmodels:\n  expense:\n    fields:\n      note: { type: text }\n      \
             amount: { type: decimal }\n    indexes: [note]\n",
        );
        let added = plan("app_t", &indexed, &live_expense());
        assert!(!needs_approval(&added));
        assert!(added.iter().any(|s| s.sql.contains("CREATE INDEX")));

        let mut live = live_expense();
        live[0].indexes = vec![LiveIndex {
            name: "expense_note_idx".into(),
            column: Some("note".into()),
        }];
        let removed = plan("app_t", &models(ONE), &live);
        assert!(!needs_approval(&removed), "an index holds no data");
        assert!(removed.iter().any(|s| s.sql.contains("DROP INDEX")));
    }

    #[test]
    fn foreign_keys_come_after_every_table_exists() {
        // A ref may point at a model declared later, and a manifest's legality
        // should not depend on the order someone wrote it in.
        let m = models(
            "name: T\nmodels:\n  line:\n    fields:\n      order_id: { type: \"ref:order\" }\n  \
             order:\n    fields:\n      total: { type: decimal }\n",
        );
        let plan = plan("app_t", &m, &[]);
        let last_create = plan
            .iter()
            .rposition(|s| s.sql.contains("CREATE TABLE"))
            .unwrap();
        let fk = plan
            .iter()
            .position(|s| s.sql.contains("FOREIGN KEY"))
            .unwrap();
        assert!(
            fk > last_create,
            "the key was added before its target existed"
        );
        assert!(plan[fk].sql.contains("REFERENCES \"app_t\".\"order\"(id)"));
        assert!(!needs_approval(&plan));
    }

    fn col(name: &str, data_type: &str) -> LiveColumn {
        LiveColumn {
            name: name.into(),
            data_type: data_type.into(),
        }
    }

    /// `line.order_id` as Postgres has it, keyed to `target` (or not at all),
    /// beside two tables it could point at.
    fn live_line(target: Option<&str>, order_id_type: &str) -> Vec<LiveTable> {
        let table = |name: &str, extra: Vec<LiveColumn>, keys: Vec<LiveForeignKey>| LiveTable {
            name: name.into(),
            columns: [
                vec![
                    col("id", "uuid"),
                    col("created_at", "timestamp with time zone"),
                    col("updated_at", "timestamp with time zone"),
                ],
                extra,
            ]
            .concat(),
            indexes: vec![],
            foreign_keys: keys,
        };
        vec![
            table(
                "line",
                vec![col("order_id", order_id_type)],
                target
                    .map(|t| LiveForeignKey {
                        name: "line_order_id_fk".into(),
                        column: "order_id".into(),
                        target: t.into(),
                    })
                    .into_iter()
                    .collect(),
            ),
            table("order", vec![col("total", "numeric")], vec![]),
            table("customer", vec![col("total", "numeric")], vec![]),
        ]
    }

    fn line_manifest(order_id: &str) -> Vec<Model> {
        models(&format!(
            "name: T\nmodels:\n  line:\n    fields:\n      order_id: {{ type: \"{order_id}\" }}\n  \
             order:\n    fields:\n      total: {{ type: decimal }}\n  \
             customer:\n    fields:\n      total: {{ type: decimal }}\n"
        ))
    }

    #[test]
    fn a_key_that_is_already_there_is_left_alone() {
        let plan = plan(
            "app_t",
            &line_manifest("ref:order"),
            &live_line(Some("order"), "uuid"),
        );
        assert!(
            !plan
                .iter()
                .any(|s| s.sql.contains("CONSTRAINT") || s.sql.contains("UPDATE")),
            "{plan:#?}"
        );
    }

    #[test]
    fn a_field_that_becomes_a_ref_gets_its_key_after_a_question() {
        // The column is there already, so the old "add the key with the
        // column" rule never added one.
        let plan = plan(
            "app_t",
            &line_manifest("ref:order"),
            &live_line(None, "text"),
        );
        assert!(needs_approval(&plan));
        let retype = plan
            .iter()
            .position(|s| s.sql.contains("ALTER COLUMN"))
            .unwrap();
        let clear = plan
            .iter()
            .position(|s| s.sql.starts_with("UPDATE"))
            .unwrap();
        let key = plan
            .iter()
            .position(|s| s.sql.contains("FOREIGN KEY"))
            .unwrap();
        assert!(retype < clear && clear < key, "{plan:#?}");
        assert!(plan[clear].destructive);
        assert!(plan[clear].sql.contains("FROM \"app_t\".\"order\" AS r"));
        assert!(plan[key].sql.contains("REFERENCES \"app_t\".\"order\"(id)"));
    }

    #[test]
    fn a_ref_that_points_elsewhere_moves_its_key() {
        let plan = plan(
            "app_t",
            &line_manifest("ref:customer"),
            &live_line(Some("order"), "uuid"),
        );
        assert!(needs_approval(&plan), "the old ids are not customers");
        let drop = plan
            .iter()
            .position(|s| {
                s.sql
                    .contains("DROP CONSTRAINT IF EXISTS \"line_order_id_fk\"")
            })
            .unwrap();
        let key = plan
            .iter()
            .position(|s| s.sql.contains("FOREIGN KEY"))
            .unwrap();
        assert!(drop < key);
        assert!(!plan[drop].destructive, "dropping a key changes no value");
        assert!(plan[key]
            .sql
            .contains("REFERENCES \"app_t\".\"customer\"(id)"));
    }

    #[test]
    fn a_ref_that_stops_being_one_loses_its_key_before_its_type_changes() {
        // Postgres refuses to retype a column a key still holds.
        let plan = plan(
            "app_t",
            &line_manifest("text"),
            &live_line(Some("order"), "uuid"),
        );
        let drop = plan
            .iter()
            .position(|s| s.sql.contains("DROP CONSTRAINT"))
            .unwrap();
        let retype = plan
            .iter()
            .position(|s| s.sql.contains("ALTER COLUMN"))
            .unwrap();
        assert!(drop < retype, "{plan:#?}");
        assert!(!plan.iter().any(|s| s.sql.contains("FOREIGN KEY")));
    }

    const LONG_MODEL: &str = "a_model_name_that_uses_most_of_the_forty_eight_c";

    #[test]
    fn a_long_name_fits_postgres_and_two_of_them_stay_apart() {
        assert_eq!(LONG_MODEL.len(), 48);
        let a = index_name(LONG_MODEL, "description_long_first");
        let b = index_name(LONG_MODEL, "description_long_second");
        for name in [&a, &b] {
            assert!(name.len() <= MAX_IDENT, "{name} is {} bytes", name.len());
            assert!(name.ends_with("_idx"), "{name}");
        }
        // Cut to 63 bytes, these two were the same name.
        assert_eq!(
            truncated_name(LONG_MODEL, "description_long_first", "idx"),
            truncated_name(LONG_MODEL, "description_long_second", "idx")
        );
        assert_ne!(a, b);
        // And a name that always fitted is the name it always had.
        assert_eq!(index_name("expense", "note"), "expense_note_idx");
        assert_eq!(fk_name("line", "order_id"), "line_order_id_fk");
    }

    #[test]
    fn an_index_postgres_cut_short_is_renamed_not_rebuilt() {
        let m = models(&format!(
            "name: T\nmodels:\n  {LONG_MODEL}:\n    fields:\n      description_long_first: {{ type: text }}\n      \
             description_long_second: {{ type: text }}\n    indexes: [description_long_first, description_long_second]\n"
        ));
        let cut = truncated_name(LONG_MODEL, "description_long_first", "idx").unwrap();
        let live = vec![LiveTable {
            name: LONG_MODEL.into(),
            columns: vec![
                col("id", "uuid"),
                col("created_at", "timestamp with time zone"),
                col("updated_at", "timestamp with time zone"),
                col("description_long_first", "text"),
                col("description_long_second", "text"),
            ],
            // The one index the two cut names made, on the field that got
            // there first; the second's `IF NOT EXISTS` did nothing.
            indexes: vec![LiveIndex {
                name: cut.clone(),
                column: Some("description_long_first".into()),
            }],
            foreign_keys: vec![],
        }];
        let plan = plan("app_t", &m, &live);
        assert!(!needs_approval(&plan));
        let renames: Vec<_> = plan
            .iter()
            .filter(|s| s.sql.starts_with("ALTER INDEX"))
            .collect();
        assert_eq!(renames.len(), 1, "{plan:#?}");
        assert!(renames[0].sql.ends_with(&format!(
            "RENAME TO \"{}\"",
            index_name(LONG_MODEL, "description_long_first")
        )));
        let creates: Vec<_> = plan
            .iter()
            .filter(|s| s.sql.starts_with("CREATE INDEX"))
            .collect();
        assert_eq!(creates.len(), 1, "{plan:#?}");
        assert!(creates[0]
            .sql
            .contains(&index_name(LONG_MODEL, "description_long_second")));
        assert!(!plan.iter().any(|s| s.sql.starts_with("DROP INDEX")));
    }

    #[test]
    fn every_statement_can_explain_itself_to_the_person_approving_it() {
        let shrunk =
            models("name: T\nmodels:\n  expense:\n    fields:\n      note: { type: text }\n");
        for stmt in plan("app_t", &shrunk, &live_expense()) {
            assert!(!stmt.why.is_empty(), "no explanation for: {}", stmt.sql);
            assert!(stmt.why.ends_with('.'), "not a sentence: {}", stmt.why);
        }
    }
}
