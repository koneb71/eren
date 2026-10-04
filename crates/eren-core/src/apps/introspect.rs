//! What an app's schema actually looks like right now.
//!
//! Read from `information_schema` and `pg_indexes` every time, never cached and
//! never mirrored into a table of our own. A registry of "what we think the
//! schema is" drifts the moment anything touches a table outside the code that
//! maintains the registry — and then every diff is computed against a fiction,
//! which is worse than having no diff at all. Postgres already knows.

use super::schema::{LiveColumn, LiveForeignKey, LiveIndex, LiveTable};
use crate::Db;
use sqlx::Row;

/// Every table in an app's schema, with its columns, indexes and foreign keys.
///
/// An absent schema is an empty list, not an error: an app whose tables have
/// never been created is the ordinary state at install, and the plan against an
/// empty list is exactly "create everything".
pub async fn tables(db: &Db, schema: &str) -> anyhow::Result<Vec<LiveTable>> {
    let rows = sqlx::query(
        "SELECT table_name, column_name, data_type
           FROM information_schema.columns
          WHERE table_schema = $1
          ORDER BY table_name, ordinal_position",
    )
    .bind(schema)
    .fetch_all(&db.pool)
    .await?;

    let mut out: Vec<LiveTable> = Vec::new();
    for row in &rows {
        let table: String = row.get("table_name");
        let column = LiveColumn {
            name: row.get("column_name"),
            data_type: row.get("data_type"),
        };
        match out.iter_mut().find(|t| t.name == table) {
            Some(t) => t.columns.push(column),
            None => out.push(LiveTable {
                name: table,
                columns: vec![column],
                indexes: vec![],
                foreign_keys: vec![],
            }),
        }
    }

    // Indexes are a separate catalogue. One a constraint owns — the primary
    // key's — is skipped: it is not something a manifest asks for, so leaving
    // it in would make the planner propose dropping it on every single build.
    // Every other index comes back with the column it covers, by name and
    // not by a suffix: Postgres cut a long `…_idx` short, and the planner can
    // only recognise one of those by the column it is on.
    let idx = sqlx::query(
        "SELECT t.relname::text AS table_name, i.relname::text AS index_name,
                CASE WHEN ix.indnatts = 1 THEN a.attname::text END AS column_name
           FROM pg_index ix
           JOIN pg_class i ON i.oid = ix.indexrelid
           JOIN pg_class t ON t.oid = ix.indrelid
           JOIN pg_namespace n ON n.oid = t.relnamespace
           LEFT JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = ix.indkey[0]
          WHERE n.nspname = $1
            AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = ix.indexrelid)",
    )
    .bind(schema)
    .fetch_all(&db.pool)
    .await?;
    for row in &idx {
        let table: String = row.get("table_name");
        if let Some(t) = out.iter_mut().find(|t| t.name == table) {
            t.indexes.push(LiveIndex {
                name: row.get("index_name"),
                column: row.get("column_name"),
            });
        }
    }

    // Foreign keys, with what they point at — the column existing says
    // nothing about whether it is keyed, or keyed to the model the manifest
    // now names. Only single-column keys within the schema: a `ref:` is one
    // column pointing at another model of the same app.
    let fks = sqlx::query(
        "SELECT cl.relname::text AS table_name, con.conname::text AS key_name,
                att.attname::text AS column_name, ref.relname::text AS target
           FROM pg_constraint con
           JOIN pg_class cl ON cl.oid = con.conrelid
           JOIN pg_namespace n ON n.oid = cl.relnamespace
           JOIN pg_class ref ON ref.oid = con.confrelid
           JOIN pg_attribute att ON att.attrelid = con.conrelid AND att.attnum = con.conkey[1]
          WHERE n.nspname = $1 AND con.contype = 'f' AND cardinality(con.conkey) = 1",
    )
    .bind(schema)
    .fetch_all(&db.pool)
    .await?;
    for row in &fks {
        let table: String = row.get("table_name");
        if let Some(t) = out.iter_mut().find(|t| t.name == table) {
            t.foreign_keys.push(LiveForeignKey {
                name: row.get("key_name"),
                column: row.get("column_name"),
                target: row.get("target"),
            });
        }
    }

    Ok(out)
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::apps::{manifest, run, schema};
    use crate::testdb;

    async fn reconcile(db: &Db, yaml: &str) -> Vec<schema::Stmt> {
        let models = manifest::parse(yaml).unwrap().models;
        let plan = schema::plan(
            "app_probe",
            &models,
            &tables(db, "app_probe").await.unwrap(),
        );
        run(db, &plan).await.unwrap();
        plan
    }

    /// What Postgres has after a plan is what the next plan reads back, so a
    /// schema that matches its manifest plans nothing — for keys that moved
    /// and for names too long for Postgres alike.
    #[tokio::test]
    async fn keys_and_long_names_settle_after_one_plan() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let model = "a_model_name_that_uses_most_of_the_forty_eight_c";
        let settled =
            |plan: &[schema::Stmt]| plan.iter().all(|s| s.sql.starts_with("CREATE SCHEMA"));
        let shape = |line_ref: &str| {
            format!(
                "name: T\nmodels:\n  order:\n    fields:\n      total: {{ type: decimal }}\n  \
                 customer:\n    fields:\n      name: {{ type: text }}\n  \
                 line:\n    fields:\n      order_id: {{ type: \"{line_ref}\" }}\n  \
                 {model}:\n    fields:\n      description_long_first: {{ type: text }}\n      \
                 description_long_second: {{ type: text }}\n    \
                 indexes: [description_long_first, description_long_second]\n"
            )
        };

        reconcile(&t.db, &shape("ref:order")).await;
        let again = reconcile(&t.db, &shape("ref:order")).await;
        assert!(settled(&again), "{again:#?}");
        let live = tables(&t.db, "app_probe").await.unwrap();
        let wide = live.iter().find(|t| t.name == model).unwrap();
        assert_eq!(wide.indexes.len(), 2, "{:#?}", wide.indexes);

        // A line pointing at an order, then the field moved to customers: the
        // old id is not a customer, so it is cleared and the key moves.
        let order: uuid::Uuid =
            sqlx::query_scalar("INSERT INTO app_probe.\"order\" (total) VALUES (1) RETURNING id")
                .fetch_one(&t.db.pool)
                .await
                .unwrap();
        sqlx::query("INSERT INTO app_probe.line (order_id) VALUES ($1)")
            .bind(order)
            .execute(&t.db.pool)
            .await
            .unwrap();
        let moved = reconcile(&t.db, &shape("ref:customer")).await;
        assert!(schema::needs_approval(&moved));
        let live = tables(&t.db, "app_probe").await.unwrap();
        let line = live.iter().find(|t| t.name == "line").unwrap();
        assert_eq!(line.foreign_keys.len(), 1, "{:#?}", line.foreign_keys);
        assert_eq!(line.foreign_keys[0].target, "customer");
        let left: Option<uuid::Uuid> = sqlx::query_scalar("SELECT order_id FROM app_probe.line")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
        assert_eq!(left, None);
        let again = reconcile(&t.db, &shape("ref:customer")).await;
        assert!(settled(&again), "{again:#?}");

        // And back to plain text: the key goes before the type changes.
        reconcile(&t.db, &shape("text")).await;
        let live = tables(&t.db, "app_probe").await.unwrap();
        assert!(live
            .iter()
            .find(|t| t.name == "line")
            .unwrap()
            .foreign_keys
            .is_empty());
        t.finish().await;
    }

    /// An index made before long names were shortened, which Postgres cut to
    /// 63 bytes: found by its column and renamed, never built a second time.
    #[tokio::test]
    async fn an_index_postgres_cut_short_is_adopted() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let model = "a_model_name_that_uses_most_of_the_forty_eight_c";
        let yaml = format!(
            "name: T\nmodels:\n  {model}:\n    fields:\n      description_long_first: {{ type: text }}\n"
        );
        reconcile(&t.db, &yaml).await;
        // What the old naming did: the whole name, which Postgres cuts.
        sqlx::query(&format!(
            "CREATE INDEX \"{model}_description_long_first_idx\" \
             ON app_probe.\"{model}\" (description_long_first)"
        ))
        .execute(&t.db.pool)
        .await
        .unwrap();

        let indexed = format!("{yaml}    indexes: [description_long_first]\n");
        let plan = reconcile(&t.db, &indexed).await;
        assert!(
            plan.iter().any(|s| s.sql.starts_with("ALTER INDEX")),
            "{plan:#?}"
        );
        assert!(
            !plan.iter().any(|s| s.sql.starts_with("CREATE INDEX")),
            "{plan:#?}"
        );
        let live = tables(&t.db, "app_probe").await.unwrap();
        assert_eq!(live[0].indexes.len(), 1, "{:#?}", live[0].indexes);
        let again = reconcile(&t.db, &indexed).await;
        assert!(
            again.iter().all(|s| s.sql.starts_with("CREATE SCHEMA")),
            "{again:#?}"
        );
        t.finish().await;
    }
}
