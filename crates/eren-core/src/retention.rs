//! How long a finished run's transcript is kept.
//!
//! Every event an engine emits is persisted to `events` — tool inputs, file
//! contents, the model's text — and until now nothing ever removed one: a
//! reconnecting page replays from the log, so the log was simply kept. That
//! is the right default for a tool whose history is its review trail, and
//! the wrong one forever: the table is the largest thing in the database,
//! every backup carries all of it, and "everything an agent ever read" is a
//! lot to hold for a run nobody will open again.
//!
//! `EREN_EVENT_RETENTION_DAYS` is the operator's standing decision. Unset (or
//! `0`), nothing is pruned. Set, the events of runs that *finished* more than
//! that many days ago are deleted, hourly, by the scheduler. The run itself —
//! its status, cost, tokens, error, the card's comments and summaries — stays;
//! only the transcript goes, and a transcript view of a pruned run is empty.
//! An unfinished run is never touched, however old its row.
//!
//! An environment variable rather than a dashboard setting on purpose: it is
//! a machine-level knob like `EREN_MAX_CONCURRENT`, decided by whoever runs
//! the database, and `docs_tests` holds every such variable to `.env.example`
//! and the README.

use crate::db::Db;

/// The retention window, or `None` to keep every event forever.
pub fn days() -> Option<u32> {
    parse_days(eren_shared::brand::var("EVENT_RETENTION_DAYS").as_deref())
}

/// `None` for unset, empty, `0`, or anything that is not a count of days.
fn parse_days(raw: Option<&str>) -> Option<u32> {
    raw?.trim().parse::<u32>().ok().filter(|d| *d > 0)
}

/// Delete the events of runs that finished more than `days` days ago.
/// Returns how many rows went.
pub async fn prune_events(db: &Db, days: u32) -> anyhow::Result<u64> {
    Ok(sqlx::query(
        "DELETE FROM events e
          USING runs r
          WHERE e.run_id = r.id
            AND r.finished_at IS NOT NULL
            AND r.finished_at < now() - make_interval(days => $1)",
    )
    .bind(i32::try_from(days).unwrap_or(i32::MAX))
    .execute(&db.pool)
    .await?
    .rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_zero_or_nonsense_means_keep_forever() {
        for raw in [None, Some(""), Some(" "), Some("0"), Some("abc"), Some("-3"), Some("1.5")] {
            assert_eq!(parse_days(raw), None, "{raw:?}");
        }
        assert_eq!(parse_days(Some("30")), Some(30));
        assert_eq!(parse_days(Some(" 90 ")), Some(90));
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb;
    use uuid::Uuid;

    async fn run_with_events(db: &Db, task: Uuid, finished: Option<&str>) -> Uuid {
        let run: Uuid = sqlx::query_scalar(
            "INSERT INTO runs (task_id, status, finished_at, cost_usd)
             VALUES ($1, 'completed', CASE WHEN $2::text IS NULL THEN NULL
                                           ELSE now() - $2::interval END, 1.25)
             RETURNING id",
        )
        .bind(task)
        .bind(finished)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        for seq in 0..3 {
            sqlx::query(
                "INSERT INTO events (run_id, seq, type, payload) VALUES ($1, $2, 'x', '{}')",
            )
            .bind(run)
            .bind(seq)
            .execute(&db.pool)
            .await
            .unwrap();
        }
        run
    }

    async fn events_of(db: &Db, run: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM events WHERE run_id = $1")
            .bind(run)
            .fetch_one(&db.pool)
            .await
            .unwrap()
    }

    /// Only the transcript of a run that finished outside the window goes;
    /// the run, its cost, a recent run and an unfinished one are untouched.
    #[tokio::test]
    async fn prunes_old_finished_transcripts_and_nothing_else() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let db = &t.db;
        let dir = tempfile::tempdir().unwrap();
        let (_, project) = t.project(dir.path(), true).await;
        let task = t.card(project, "old work").await;

        let old = run_with_events(db, task, Some("40 days")).await;
        let recent = run_with_events(db, task, Some("1 day")).await;
        let unfinished = run_with_events(db, task, None).await;

        assert_eq!(prune_events(db, 30).await.unwrap(), 3);
        assert_eq!(events_of(db, old).await, 0);
        assert_eq!(events_of(db, recent).await, 3);
        assert_eq!(events_of(db, unfinished).await, 3);

        let (status, cost): (String, Option<f64>) =
            sqlx::query_as("SELECT status, cost_usd FROM runs WHERE id = $1")
                .bind(old)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(status, "completed");
        assert_eq!(cost, Some(1.25));

        // Idempotent: nothing left to prune.
        assert_eq!(prune_events(db, 30).await.unwrap(), 0);
        t.finish().await;
    }
}
