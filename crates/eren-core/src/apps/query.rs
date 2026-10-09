//! Reading an app's rows without ever accepting SQL.
//!
//! An app says `amount:gt:10`, not `WHERE amount > 10`. That is the whole
//! design: the grammar is closed, so there is no expression an app can write
//! that this module does not already have a variant for, and nothing it sends
//! ever becomes SQL syntax.
//!
//! Three things hold, and all three are tested:
//!
//! * **Identifiers come from the manifest, never from the request.** A field
//!   name in a filter is *looked up* among the model's declared fields and the
//!   declared spelling is what reaches the query. A name that is not declared
//!   is an error, not a column.
//! * **Operators come from an enum.** There is no path from request text to an
//!   operator that is not one of the ten below.
//! * **Values are always bound.** Every one becomes a `$n` placeholder. None is
//!   ever formatted into the statement, whatever it contains.
//!
//! Parameters are bound as **text and cast in SQL** (`$1::numeric`), uniformly.
//! Partly because the workspace's sqlx has no decimal feature and a money
//! column must not round-trip through a float; mostly because one binding path
//! for every type is one path to get right.

use super::manifest::{Field, FieldType, Model, RESERVED_FIELDS};
use std::fmt;

/// The most rows one request may ask for.
pub const MAX_LIMIT: i64 = 1000;
pub const DEFAULT_LIMIT: i64 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError(pub String);

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for QueryError {}

fn err<T>(message: impl Into<String>) -> Result<T, QueryError> {
    Err(QueryError(message.into()))
}

/// The closed set of comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    /// Case-insensitive "contains". Text only.
    Like,
    /// Any of a comma-separated list.
    In,
    IsNull,
    NotNull,
}

impl Op {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "eq" => Self::Eq,
            "ne" => Self::Ne,
            "gt" => Self::Gt,
            "gte" => Self::Gte,
            "lt" => Self::Lt,
            "lte" => Self::Lte,
            "like" => Self::Like,
            "in" => Self::In,
            "isnull" => Self::IsNull,
            "notnull" => Self::NotNull,
            _ => return None,
        })
    }

    fn sql(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Gt => ">",
            Self::Gte => ">=",
            Self::Lt => "<",
            Self::Lte => "<=",
            Self::Like => "ILIKE",
            Self::In => "IN",
            Self::IsNull => "IS NULL",
            Self::NotNull => "IS NOT NULL",
        }
    }

    fn takes_value(self) -> bool {
        !matches!(self, Self::IsNull | Self::NotNull)
    }
}

/// One condition, already checked against the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// The *declared* field name, taken from the model rather than the request.
    pub field: String,
    pub ty: FieldType,
    pub op: Op,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub field: String,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub filters: Vec<Filter>,
    pub order: Option<Order>,
    pub limit: i64,
    pub offset: i64,
}

/// What arrived in the query string, untouched.
#[derive(Debug, Clone, Default)]
pub struct Raw {
    pub filters: Vec<String>,
    pub order: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// The type of a column, whether the manifest declared it or Eren did.
fn type_of(model: &Model, name: &str) -> Option<FieldType> {
    match name {
        "id" => Some(FieldType::Ref(model.name.clone())),
        "created_at" | "updated_at" => Some(FieldType::Datetime),
        _ => model.field(name).map(|f: &Field| f.ty.clone()),
    }
}

/// Resolve a requested name to a declared one.
///
/// The returned string is the *model's* copy, not the caller's. That is the
/// difference between a lookup and a pass-through, and it is the reason no
/// request text ever reaches the statement as an identifier.
fn declared(model: &Model, requested: &str) -> Result<(String, FieldType), QueryError> {
    if let Some(name) = RESERVED_FIELDS.iter().find(|r| **r == requested) {
        let ty = type_of(model, name).expect("reserved columns always have a type");
        return Ok((name.to_string(), ty));
    }
    match model.field(requested) {
        Some(field) => Ok((field.name.clone(), field.ty.clone())),
        None => err(format!(
            "\"{requested}\" is not a field of \"{}\"",
            model.name
        )),
    }
}

/// Check a value is plausible for its column before Postgres has to.
///
/// The cast in the generated SQL would catch these too, but a Postgres error
/// names a type where this can name the field and what was sent.
fn check(ty: &FieldType, value: &str, field: &str) -> Result<(), QueryError> {
    let ok = match ty {
        FieldType::Text | FieldType::Json => true,
        FieldType::Int => value.parse::<i64>().is_ok(),
        FieldType::Decimal => value.parse::<f64>().is_ok(),
        FieldType::Bool => matches!(value, "true" | "false"),
        // Dates and timestamps are left to Postgres, which knows far more
        // about what is a date than a hand-rolled check would.
        FieldType::Date | FieldType::Datetime => !value.trim().is_empty(),
        FieldType::Ref(_) => uuid::Uuid::parse_str(value).is_ok(),
    };
    if ok {
        Ok(())
    } else {
        err(format!(
            "\"{value}\" is not a valid {} for \"{field}\"",
            ty.as_str()
        ))
    }
}

/// `%` and `_` are wildcards in LIKE. A person searching for "50%" means it.
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Read a query string into something already checked against the model.
pub fn parse(model: &Model, raw: &Raw) -> Result<Query, QueryError> {
    let mut filters = Vec::new();
    for text in &raw.filters {
        // Two splits, not `split(':')`: a datetime value has colons in it, and
        // splitting on all of them would make `at:gt:2026-01-01T10:00:00Z` a
        // malformed filter rather than a working one.
        let mut parts = text.splitn(3, ':');
        let (Some(name), Some(op_text)) = (parts.next(), parts.next()) else {
            return err(format!(
                "\"{text}\" is not a filter — expected field:op:value"
            ));
        };
        let value = parts.next();

        let (field, ty) = declared(model, name)?;
        let Some(op) = Op::parse(op_text) else {
            return err(format!(
                "\"{op_text}\" is not a comparison — expected one of eq, ne, gt, \
                 gte, lt, lte, like, in, isnull, notnull"
            ));
        };

        if op == Op::Like && ty != FieldType::Text {
            return err(format!(
                "\"like\" only works on text, and \"{field}\" is {}",
                ty.as_str()
            ));
        }

        let values = match (op.takes_value(), value) {
            (false, _) => Vec::new(),
            (true, None) => return err(format!("\"{op_text}\" on \"{field}\" needs a value")),
            (true, Some(v)) if op == Op::In => {
                let items: Vec<String> = v.split(',').map(str::to_string).collect();
                if items.iter().any(|i| i.is_empty()) {
                    return err(format!("\"in\" on \"{field}\" has an empty entry"));
                }
                for item in &items {
                    check(&ty, item, &field)?;
                }
                items
            }
            (true, Some(v)) if op == Op::Like => vec![format!("%{}%", escape_like(v))],
            (true, Some(v)) => {
                check(&ty, v, &field)?;
                vec![v.to_string()]
            }
        };

        filters.push(Filter {
            field,
            ty,
            op,
            values,
        });
    }

    let order = match &raw.order {
        None => None,
        Some(text) => {
            let (descending, name) = match text.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, text.as_str()),
            };
            let (field, _) = declared(model, name)?;
            Some(Order { field, descending })
        }
    };

    // Clamped rather than refused: a limit of a million is a mistake, not an
    // attack, and failing the request teaches nothing a capped one does not.
    let limit = raw.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = raw.offset.unwrap_or(0).max(0);

    Ok(Query {
        filters,
        order,
        limit,
        offset,
    })
}

/// The SQL cast a value of this type needs on its way in.
pub fn cast(ty: &FieldType) -> &'static str {
    match ty {
        FieldType::Text => "text",
        FieldType::Int => "bigint",
        FieldType::Decimal => "numeric",
        FieldType::Bool => "boolean",
        FieldType::Date => "date",
        FieldType::Datetime => "timestamptz",
        FieldType::Json => "jsonb",
        FieldType::Ref(_) => "uuid",
    }
}

/// A statement fragment and the values it expects, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    pub sql: String,
    pub params: Vec<String>,
}

/// Build the `WHERE`, `ORDER BY`, `LIMIT` and `OFFSET`.
///
/// `next` is the number the first placeholder should take, so this can be
/// appended to a statement that already binds something.
pub fn where_clause(query: &Query, next: usize) -> Fragment {
    let mut params: Vec<String> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    let mut n = next;

    for filter in &query.filters {
        let col = quote(&filter.field);
        let ty = cast(&filter.ty);
        match filter.op {
            Op::IsNull | Op::NotNull => clauses.push(format!("{col} {}", filter.op.sql())),
            Op::In => {
                let holes: Vec<String> = filter
                    .values
                    .iter()
                    .map(|v| {
                        params.push(v.clone());
                        let hole = format!("${n}::{ty}");
                        n += 1;
                        hole
                    })
                    .collect();
                clauses.push(format!("{col} IN ({})", holes.join(", ")));
            }
            Op::Like => {
                params.push(filter.values[0].clone());
                // The escape character has to be named or the backslashes this
                // put in front of the caller's own % would be literal text.
                clauses.push(format!("{col} ILIKE ${n}::text ESCAPE '\\'"));
                n += 1;
            }
            op => {
                params.push(filter.values[0].clone());
                clauses.push(format!("{col} {} ${n}::{ty}", op.sql()));
                n += 1;
            }
        }
    }

    let mut sql = String::new();
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    match &query.order {
        // Direction is a literal, never a parameter: `ORDER BY $1` sorts by a
        // constant in Postgres, silently, which is a bug rather than an
        // injection but no less wrong.
        Some(o) => sql.push_str(&format!(
            " ORDER BY {} {}",
            quote(&o.field),
            if o.descending { "DESC" } else { "ASC" }
        )),
        None => sql.push_str(" ORDER BY \"created_at\" DESC"),
    }
    // Interpolated, and safe to be: both are i64s clamped above, so neither can
    // carry anything but digits.
    sql.push_str(&format!(" LIMIT {} OFFSET {}", query.limit, query.offset));

    Fragment { sql, params }
}

/// Quote an identifier. Everything reaching this came from a manifest that
/// only allows lower-case letters, digits and underscores, so there is nothing
/// to escape — the doubling is for the caller who someday forgets that.
pub fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// The `SELECT` list that turns a row into JSON.
///
/// Built explicitly rather than with `to_jsonb(t)` for one reason worth the
/// extra code: a `numeric` rendered as a JSON number goes through a double on
/// the way to a browser, and an app tracking money must not lose cents to
/// that. Decimals leave as strings.
pub fn projection(model: &Model) -> String {
    let mut parts = vec![
        "'id', t.\"id\"::text".to_string(),
        "'created_at', t.\"created_at\"".to_string(),
        "'updated_at', t.\"updated_at\"".to_string(),
    ];
    for field in &model.fields {
        let col = format!("t.{}", quote(&field.name));
        let value = match field.ty {
            // Text, so no precision is lost between Postgres and JavaScript.
            FieldType::Decimal => format!("{col}::text"),
            FieldType::Ref(_) => format!("{col}::text"),
            _ => col,
        };
        parts.push(format!("'{}', {}", field.name, value));
    }
    // Postgres takes at most 100 arguments to a function, and each column is
    // two of them, so a model past 47 fields could not be read at all. Built
    // in chunks of at most 50 pairs and merged with `||`, which on two
    // objects is their union.
    let chunks: Vec<String> = parts
        .chunks(PAIRS_PER_OBJECT)
        .map(|c| format!("jsonb_build_object({})", c.join(", ")))
        .collect();
    match chunks.len() {
        1 => chunks.into_iter().next().unwrap_or_default(),
        _ => format!("({})", chunks.join(" || ")),
    }
}

/// Key/value pairs in one `jsonb_build_object` — half of Postgres's
/// 100-argument cap on a function call.
const PAIRS_PER_OBJECT: usize = 50;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::manifest;

    fn model() -> Model {
        manifest::parse(
            "name: T\nmodels:\n  expense:\n    fields:\n      \
             note:     { type: text }\n      \
             amount:   { type: decimal }\n      \
             qty:      { type: int }\n      \
             paid:     { type: bool }\n      \
             at:       { type: datetime }\n",
        )
        .unwrap()
        .models
        .remove(0)
    }

    fn raw(filters: &[&str]) -> Raw {
        Raw {
            filters: filters.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn q(filters: &[&str]) -> Result<Query, QueryError> {
        parse(&model(), &raw(filters))
    }

    #[test]
    fn a_field_that_is_not_declared_is_not_a_column() {
        let e = q(&["secret:eq:1"]).unwrap_err();
        assert!(e.0.contains("\"secret\""), "{e}");
        // Including one that exists on another table entirely.
        assert!(q(&["password:eq:x"]).is_err());
    }

    #[test]
    fn nothing_a_caller_sends_becomes_an_identifier() {
        // The property that matters: the name in the output is the *model's*
        // copy. A caller cannot smuggle syntax through the field position even
        // if the lookup somehow matched, because what is emitted is what the
        // manifest declared.
        let query = q(&["note:eq:x"]).unwrap();
        assert_eq!(query.filters[0].field, "note");
        let frag = where_clause(&query, 1);
        assert!(frag.sql.contains("\"note\" = $1::text"), "{}", frag.sql);
    }

    /// `lt` rendered as `<=` for a while, so a less-than filter also matched
    /// equal rows; each comparison renders its own operator.
    #[test]
    fn comparisons_render_their_own_operator() {
        for (op, sql) in [("gt", ">"), ("gte", ">="), ("lt", "<"), ("lte", "<=")] {
            let query = q(&[&format!("qty:{op}:5")]).unwrap();
            let frag = where_clause(&query, 1);
            assert!(
                frag.sql.contains(&format!("\"qty\" {sql} $1::bigint")),
                "{op}: {}",
                frag.sql
            );
        }
    }

    #[test]
    fn an_operator_outside_the_closed_set_is_refused() {
        for bad in ["equals", "EQ", "=", "; drop table", "regex"] {
            let e = q(&[&format!("note:{bad}:x")]).unwrap_err();
            assert!(e.0.contains("not a comparison"), "{bad} was accepted: {e}");
        }
    }

    #[test]
    fn a_value_full_of_sql_is_only_ever_a_value() {
        // The test this module exists for.
        let nasty = "'; DROP TABLE entry; --";
        let query = q(&[&format!("note:eq:{nasty}")]).unwrap();
        let frag = where_clause(&query, 1);
        assert_eq!(frag.params, vec![nasty.to_string()]);
        assert!(
            !frag.sql.contains("DROP"),
            "the value reached the SQL: {}",
            frag.sql
        );
        assert!(frag.sql.contains("$1::text"));
    }

    #[test]
    fn a_value_may_contain_colons() {
        // Splitting on every colon would make a timestamp a malformed filter.
        let query = q(&["at:gt:2026-01-01T10:30:00Z"]).unwrap();
        assert_eq!(query.filters[0].values, vec!["2026-01-01T10:30:00Z"]);
        assert_eq!(query.filters[0].op, Op::Gt);
    }

    #[test]
    fn a_value_has_to_suit_its_column() {
        assert!(q(&["qty:eq:many"]).is_err());
        assert!(q(&["paid:eq:yes"]).is_err());
        assert!(q(&["amount:gt:lots"]).is_err());
        assert!(q(&["qty:eq:12"]).is_ok());
        assert!(q(&["paid:eq:true"]).is_ok());
        assert!(q(&["amount:gt:10.5"]).is_ok());
        // The message says which field and what was sent.
        let e = q(&["qty:eq:many"]).unwrap_err();
        assert!(e.0.contains("many") && e.0.contains("qty"), "{e}");
    }

    #[test]
    fn wildcards_inside_a_search_term_are_literal() {
        // Someone searching for "50%" means fifty percent, not "anything".
        let query = q(&["note:like:50%"]).unwrap();
        assert_eq!(query.filters[0].values, vec!["%50\\%%"]);
        let frag = where_clause(&query, 1);
        assert!(
            frag.sql.contains("ESCAPE"),
            "the escape must be named: {}",
            frag.sql
        );
    }

    #[test]
    fn like_is_refused_on_anything_that_is_not_text() {
        let e = q(&["amount:like:10"]).unwrap_err();
        assert!(e.0.contains("only works on text"), "{e}");
    }

    #[test]
    fn in_binds_every_item_separately() {
        let query = q(&["qty:in:1,2,3"]).unwrap();
        let frag = where_clause(&query, 1);
        assert_eq!(frag.params, vec!["1", "2", "3"]);
        assert!(frag.sql.contains("$1::bigint"));
        assert!(frag.sql.contains("$3::bigint"));
        // Every item is checked, so one bad entry fails the filter.
        assert!(q(&["qty:in:1,many"]).is_err());
        assert!(q(&["qty:in:1,,3"]).is_err());
    }

    #[test]
    fn null_checks_take_no_value_and_bind_nothing() {
        let query = q(&["note:isnull", "at:notnull"]).unwrap();
        let frag = where_clause(&query, 1);
        assert!(frag.params.is_empty());
        assert!(frag.sql.contains("\"note\" IS NULL"));
        assert!(frag.sql.contains("\"at\" IS NOT NULL"));
        // And one that does need a value says so.
        assert!(q(&["note:eq"]).unwrap_err().0.contains("needs a value"));
    }

    #[test]
    fn ordering_is_only_ever_by_a_declared_field() {
        let m = model();
        let ordered = parse(
            &m,
            &Raw {
                order: Some("-at".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            ordered.order,
            Some(Order {
                field: "at".into(),
                descending: true
            })
        );
        assert!(where_clause(&ordered, 1)
            .sql
            .contains("ORDER BY \"at\" DESC"));

        // Not by anything else, and never by request text.
        let e = parse(
            &m,
            &Raw {
                order: Some("(select 1)".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(e.0.contains("not a field"), "{e}");
    }

    #[test]
    fn the_columns_eren_adds_can_be_filtered_and_sorted_on() {
        // They are real columns, and a list sorted by when a row was made is
        // the most ordinary thing an app will want.
        assert!(q(&["created_at:gt:2026-01-01"]).is_ok());
        let m = model();
        assert!(parse(
            &m,
            &Raw {
                order: Some("created_at".into()),
                ..Default::default()
            }
        )
        .is_ok());
        assert!(q(&["id:eq:not-a-uuid"]).is_err());
        assert!(q(&["id:eq:2b1c0e9a-0000-4000-8000-000000000000"]).is_ok());
    }

    #[test]
    fn a_limit_is_clamped_rather_than_refused() {
        let m = model();
        let big = parse(
            &m,
            &Raw {
                limit: Some(1_000_000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(big.limit, MAX_LIMIT);
        let none = parse(
            &m,
            &Raw {
                limit: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(none.limit, 1);
        let back = parse(
            &m,
            &Raw {
                offset: Some(-5),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(back.offset, 0);
        // And it reaches the SQL as digits, because that is all it can be.
        assert!(where_clause(&big, 1).sql.ends_with("LIMIT 1000 OFFSET 0"));
    }

    #[test]
    fn placeholders_carry_on_from_where_the_caller_left_off() {
        let query = q(&["note:eq:a", "qty:eq:1"]).unwrap();
        let frag = where_clause(&query, 3);
        assert!(frag.sql.contains("$3::text"), "{}", frag.sql);
        assert!(frag.sql.contains("$4::bigint"), "{}", frag.sql);
    }

    #[test]
    fn a_decimal_leaves_as_text_so_no_cents_are_lost() {
        // A numeric rendered as a JSON number goes through a double on the way
        // to a browser. For a column holding money that is a real loss.
        let p = projection(&model());
        assert!(p.contains("'amount', t.\"amount\"::text"), "{p}");
        assert!(
            p.contains("'qty', t.\"qty\""),
            "an int is fine as a number: {p}"
        );
        assert!(p.contains("'id', t.\"id\"::text"));
    }

    #[test]
    fn a_wide_model_stays_under_the_function_argument_cap() {
        let fields: String = (0..60)
            .map(|i| format!("      f{i}: {{ type: text }}\n"))
            .collect();
        let wide = manifest::parse(&format!("name: T\nmodels:\n  wide:\n    fields:\n{fields}"))
            .unwrap()
            .models
            .remove(0);
        let p = projection(&wide);
        let objects: Vec<&str> = p.split("jsonb_build_object(").skip(1).collect();
        assert_eq!(objects.len(), 2, "{p}");
        for o in &objects {
            // Each pair is "'name', value" — one comma inside, one between.
            let args = o.matches(", ").count() + 1;
            assert!(args <= 100, "{args} arguments in one call: {p}");
        }
        assert!(p.starts_with("(jsonb_build_object(") && p.contains(") || jsonb_build_object("));
        assert!(p.contains("'f59', t.\"f59\""), "{p}");
        assert!(p.contains("'id', t.\"id\"::text"));
    }

    #[test]
    fn a_malformed_filter_says_what_was_expected() {
        let e = q(&["justafield"]).unwrap_err();
        assert!(e.0.contains("field:op:value"), "{e}");
    }
}
