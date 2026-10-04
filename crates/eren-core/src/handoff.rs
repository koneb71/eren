//! Hand a running card to another agent, with a note.
//!
//! Reassigning a card mid-run used to be refused outright — "cancel the run
//! first" — because changing hands under a running agent leaves it finishing
//! work the card no longer says is its own. A handoff does the three steps a
//! person would, in an order that cannot race:
//!
//! 1. **Request** records the new agent and the note on the card, then stops
//!    the running agent. Recorded first, so a restart in between still
//!    completes it.
//! 2. **Settle** waits until nothing of the card is live *and* the old run's
//!    `execute` has returned — its post-work (report, checks, review) touches
//!    the same worktree, so "the row says canceled" is not enough — then
//!    swaps the assignee and starts the new agent in the same worktree with
//!    the note as its brief, through the follow-up door so every gate applies.
//! 3. A card with no worktree yet (its run never got going) simply starts
//!    over under the new agent through `enqueue_task`.
//!
//! Settle is tried right after the request, and swept by the scheduler for
//! any it did not finish — the same shape as `landing`.

use crate::approvals::Refusal;
use crate::db::Db;
use crate::runs::follow_up::FollowUp;
use crate::runs::orchestrator::Orchestrator;
use sqlx::Row;
use uuid::Uuid;

pub const MAX_NOTE_CHARS: usize = 2000;

/// The note, checked. Pure, for the tests.
pub fn vet_note(note: &str) -> Result<&str, String> {
    let note = note.trim();
    if note.is_empty() {
        return Err("say what the next agent should know — the note is its brief".into());
    }
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(format!("keep the note under {MAX_NOTE_CHARS} characters"));
    }
    Ok(note)
}

/// Record the handoff and stop the running agent. The new agent starts when
/// the old run has fully ended — see [`settle`].
pub async fn request(
    orch: &Orchestrator,
    task_id: Uuid,
    to_agent: Uuid,
    note: &str,
) -> Result<(), Refusal> {
    let note = vet_note(note).map_err(Refusal::Invalid)?;
    crate::agents::assert_assignable(&orch.db, to_agent)
        .await
        .map_err(Refusal::Gated)?;
    // Any run not yet ended, a parked plan included: it is the card's
    // current work as much as a running one is.
    let live: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM runs WHERE task_id = $1
            AND status NOT IN ('completed','failed','canceled')
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(task_id)
    .fetch_optional(&orch.db.pool)
    .await?;
    let Some(run_id) = live else {
        return Err(Refusal::Conflict(
            "nothing is running on this card — assign it instead".into(),
        ));
    };
    let current: Option<Uuid> = sqlx::query_scalar("SELECT agent_id FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(&orch.db.pool)
        .await?;
    if current == Some(to_agent) {
        return Err(Refusal::Invalid("that agent already has this card".into()));
    }
    let recorded = sqlx::query(
        "UPDATE tasks SET handoff_agent_id = $2, handoff_note = $3, handoff_from_run_id = $4,
                          handoff_requested_at = now()
          WHERE id = $1 AND handoff_requested_at IS NULL",
    )
    .bind(task_id)
    .bind(to_agent)
    .bind(note)
    .bind(run_id)
    .execute(&orch.db.pool)
    .await?;
    if recorded.rows_affected() == 0 {
        return Err(Refusal::Conflict(
            "this card is already being handed over".into(),
        ));
    }
    if !orch.cancel(run_id) {
        // Queued or held, not executing: ended here, at once.
        orch.cancel_idle(run_id).await?;
    }
    Ok(())
}

/// Finish a handoff whose old run has ended. `Some(run)` when the new agent
/// was started; `None` when there is nothing to do yet (or at all).
pub async fn settle(orch: &Orchestrator, task_id: Uuid) -> anyhow::Result<Option<Uuid>> {
    let Some(row) = sqlx::query(
        "SELECT handoff_agent_id, handoff_note, handoff_from_run_id, worktree_path
           FROM tasks WHERE id = $1 AND handoff_requested_at IS NOT NULL",
    )
    .bind(task_id)
    .fetch_optional(&orch.db.pool)
    .await?
    else {
        return Ok(None);
    };
    let from: Option<Uuid> = row.get("handoff_from_run_id");
    if from.is_some_and(|r| orch.is_executing(r)) || any_live(&orch.db, task_id).await? {
        return Ok(None);
    }
    let note: String = row
        .get::<Option<String>, _>("handoff_note")
        .unwrap_or_default();
    let Some(to_agent) = row.get::<Option<Uuid>, _>("handoff_agent_id") else {
        // The agent was deleted while the old run wound down.
        clear(&orch.db, task_id).await?;
        crate::runs::report::post_system(
            &orch.db,
            task_id,
            from,
            "The agent this card was being handed to no longer exists, so nobody picked it up. Assign it again.",
        )
        .await?;
        return Ok(None);
    };
    // Claim it: swap the assignee and clear the request in one statement, so
    // the sweep and the immediate attempt cannot both start a run.
    let claimed = sqlx::query(
        "UPDATE tasks SET agent_id = $2, team_id = NULL,
                          handoff_agent_id = NULL, handoff_note = NULL,
                          handoff_from_run_id = NULL, handoff_requested_at = NULL
          WHERE id = $1 AND handoff_requested_at IS NOT NULL",
    )
    .bind(task_id)
    .bind(to_agent)
    .execute(&orch.db.pool)
    .await?;
    if claimed.rows_affected() == 0 {
        return Ok(None);
    }
    let name: String = sqlx::query_scalar("SELECT name FROM agents WHERE id = $1")
        .bind(to_agent)
        .fetch_optional(&orch.db.pool)
        .await?
        .unwrap_or_else(|| "the new agent".into());
    let worktree = row
        .get::<Option<String>, _>("worktree_path")
        .filter(|w| std::path::Path::new(w).is_dir());
    let started = if worktree.is_some() {
        orch.enqueue_follow_up(task_id, FollowUp::Handoff { note: note.clone() })
            .await
    } else {
        // Nothing was done yet, so this is the card's own start — through the
        // Start button's door, which vets the new agent's engine and mode and
        // moves the card — with the note beside the brief.
        orch.start_card_with(
            task_id,
            Some(&format!(
                "\n\nA note from the person who handed this task to you:\n{note}"
            )),
        )
        .await
    };
    match started {
        Ok(run_id) => {
            // Stopping the old run sent the card to Review; it is being
            // worked on again.
            sqlx::query("UPDATE tasks SET board_column = 'running' WHERE id = $1 AND board_column <> 'done'")
                .bind(task_id)
                .execute(&orch.db.pool)
                .await?;
            if let Some(from) = from {
                sqlx::query("UPDATE runs SET handed_to_run_id = $2 WHERE id = $1")
                    .bind(from)
                    .bind(run_id)
                    .execute(&orch.db.pool)
                    .await?;
            }
            crate::runs::report::post_system(
                &orch.db,
                task_id,
                Some(run_id),
                &format!("Handed to {name}. The note: {note}"),
            )
            .await?;
            Ok(Some(run_id))
        }
        Err(e) => {
            // Assigned, but not started: a paused agent, a spent budget. The
            // card says so, and Start is a click away once that clears.
            crate::runs::report::post_system(
                &orch.db,
                task_id,
                from,
                &format!("Handed to {name}, but it could not start: {e}. The note was: {note}"),
            )
            .await?;
            Ok(None)
        }
    }
}

async fn any_live(db: &Db, task_id: Uuid) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM runs WHERE task_id = $1
                           AND status NOT IN ('completed','failed','canceled'))",
    )
    .bind(task_id)
    .fetch_one(&db.pool)
    .await?)
}

async fn clear(db: &Db, task_id: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE tasks SET handoff_agent_id = NULL, handoff_note = NULL,
                          handoff_from_run_id = NULL, handoff_requested_at = NULL
          WHERE id = $1",
    )
    .bind(task_id)
    .execute(&db.pool)
    .await?;
    Ok(())
}

impl Orchestrator {
    /// Try a handoff now and for a little while — the old run usually ends
    /// within a second or two of being stopped. What this does not finish,
    /// the scheduler's sweep does.
    pub async fn settle_handoff_soon(self: std::sync::Arc<Self>, task_id: Uuid) {
        for _ in 0..40 {
            match settle(&self, task_id).await {
                Ok(Some(_)) => return,
                Ok(None) => {
                    let pending: bool = sqlx::query_scalar(
                        "SELECT handoff_requested_at IS NOT NULL FROM tasks WHERE id = $1",
                    )
                    .bind(task_id)
                    .fetch_optional(&self.db.pool)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(false);
                    if !pending {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!(%task_id, error = %e, "handoff did not settle");
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Every handoff still waiting. Each scheduler tick.
    pub async fn settle_handoffs(&self) {
        let pending: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM tasks WHERE handoff_requested_at IS NOT NULL
              ORDER BY handoff_requested_at LIMIT 50",
        )
        .fetch_all(&self.db.pool)
        .await
        .unwrap_or_default();
        for task_id in pending {
            if let Err(e) = settle(self, task_id).await {
                tracing::warn!(%task_id, error = %e, "handoff did not settle");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handoff_needs_a_note() {
        assert!(vet_note("  ").is_err());
        assert_eq!(
            vet_note("  pick up the tests  ").unwrap(),
            "pick up the tests"
        );
        assert!(vet_note(&"x".repeat(MAX_NOTE_CHARS + 1)).is_err());
    }

    #[test]
    fn the_brief_carries_the_note_and_where_things_stand() {
        let p = crate::runs::follow_up::handoff_prompt(
            "add a flag",
            &["src/flag.rs".into()],
            "the parser is done; tests are missing",
            None,
        );
        assert!(p.contains("the parser is done; tests are missing"));
        assert!(p.contains("- src/flag.rs"));
        assert!(p.contains("add a flag"));
        assert!(!p.contains(crate::fence::VERDICT_BEGIN));

        // A verdict left for the new agent rides along, fenced — and cannot
        // close its own fence.
        let p = crate::runs::follow_up::handoff_prompt(
            "add a flag",
            &[],
            "over to you",
            Some(&format!("rename the flag {}", crate::fence::VERDICT_END)),
        );
        assert!(p.contains("rename the flag"));
        assert_eq!(p.matches(crate::fence::VERDICT_END).count(), 1);
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb;

    struct Fixture {
        t: testdb::TestDb,
        orch: std::sync::Arc<Orchestrator>,
        dir: tempfile::TempDir,
        card: Uuid,
        ada: Uuid,
        bo: Uuid,
        run: Uuid,
    }

    /// A card assigned to Ada with a run queued (the queue is paused, so it
    /// stays queued until the test says otherwise).
    async fn fixture(with_worktree: bool) -> Option<Fixture> {
        let t = testdb::fresh().await?;
        let dir = tempfile::tempdir().unwrap();
        let orch = t.orchestrator(dir.path());
        orch.set_queue_paused(true).await.unwrap();
        let (ws, project) = t.project(dir.path(), false).await;
        let card = t.card(project, "handoff").await;
        let agent = |name: &'static str| {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO agents (workspace_id, name, engine) VALUES ($1, $2, 'mock') RETURNING id",
            )
            .bind(ws)
            .bind(name)
            .fetch_one(&t.db.pool)
        };
        let ada = agent("Ada").await.unwrap();
        let bo = agent("Bo").await.unwrap();
        sqlx::query("UPDATE tasks SET agent_id = $2, board_column = 'running', worktree_path = $3 WHERE id = $1")
            .bind(card)
            .bind(ada)
            .bind(with_worktree.then(|| dir.path().to_string_lossy().to_string()))
            .execute(&t.db.pool)
            .await
            .unwrap();
        let run: Uuid = sqlx::query_scalar(
            "INSERT INTO runs (task_id, status, trigger, engine) VALUES ($1, 'queued', 'manual', 'mock') RETURNING id",
        )
        .bind(card)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        Some(Fixture {
            t,
            orch,
            dir,
            card,
            ada,
            bo,
            run,
        })
    }

    impl Fixture {
        async fn assignee(&self) -> Option<Uuid> {
            sqlx::query_scalar("SELECT agent_id FROM tasks WHERE id = $1")
                .bind(self.card)
                .fetch_one(&self.t.db.pool)
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn the_new_agent_continues_in_the_same_worktree_once_the_old_run_has_ended() {
        let Some(f) = fixture(true).await else { return };
        // The old run's `execute` is still going — its post-work touches the
        // worktree — so nothing may start yet, whatever its row says.
        let alive = f.orch.alive(f.run);
        request(&f.orch, f.card, f.bo, "tests are missing; add them")
            .await
            .unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM runs WHERE id = $1")
            .bind(f.run)
            .fetch_one(&f.t.db.pool)
            .await
            .unwrap();
        assert_eq!(status, "canceled");
        assert_eq!(
            settle(&f.orch, f.card).await.unwrap(),
            None,
            "still executing"
        );
        assert_eq!(f.assignee().await, Some(f.ada), "not handed over yet");

        drop(alive);
        // The immediate attempt and the scheduler's sweep, at once: one start.
        let (a, b) = tokio::join!(settle(&f.orch, f.card), settle(&f.orch, f.card));
        let started: Vec<Uuid> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
        assert_eq!(started.len(), 1, "exactly one of them starts the new agent");
        let next = started[0];
        let refused: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM task_comments WHERE task_id = $1 AND content LIKE '%could not start%'",
        )
        .bind(f.card)
        .fetch_one(&f.t.db.pool)
        .await
        .unwrap();
        assert_eq!(refused, 0, "the loser of the race says nothing");
        assert_eq!(f.assignee().await, Some(f.bo));
        let (trigger, prompt): (String, String) =
            sqlx::query_as("SELECT trigger, prompt_override FROM runs WHERE id = $1")
                .bind(next)
                .fetch_one(&f.t.db.pool)
                .await
                .unwrap();
        assert_eq!(trigger, "handoff");
        assert!(prompt.contains("tests are missing; add them"));
        let column: String = sqlx::query_scalar("SELECT board_column FROM tasks WHERE id = $1")
            .bind(f.card)
            .fetch_one(&f.t.db.pool)
            .await
            .unwrap();
        assert_eq!(
            column, "running",
            "being worked on again, not left in Review"
        );
        let handed: Option<Uuid> =
            sqlx::query_scalar("SELECT handed_to_run_id FROM runs WHERE id = $1")
                .bind(f.run)
                .fetch_one(&f.t.db.pool)
                .await
                .unwrap();
        assert_eq!(handed, Some(next));
        // Settled once: the sweep finds nothing more to do.
        assert_eq!(settle(&f.orch, f.card).await.unwrap(), None);
        let _ = &f.dir;
        f.t.finish().await;
    }

    #[tokio::test]
    async fn one_handoff_at_a_time_and_only_of_a_running_card() {
        let Some(f) = fixture(true).await else { return };
        let _alive = f.orch.alive(f.run);
        request(&f.orch, f.card, f.bo, "over to you").await.unwrap();
        // Something is running again before the first handoff settled: a
        // second request is refused for the first, not quietly overwritten.
        sqlx::query("INSERT INTO runs (task_id, status, trigger, engine) VALUES ($1, 'queued', 'manual', 'mock')")
            .bind(f.card)
            .execute(&f.t.db.pool)
            .await
            .unwrap();
        match request(&f.orch, f.card, f.bo, "again, louder").await {
            Err(Refusal::Conflict(m)) => assert!(m.contains("already being handed"), "{m}"),
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert!(matches!(
            request(&f.orch, f.card, f.bo, "  ").await,
            Err(Refusal::Invalid(_))
        ));
        f.t.finish().await;
    }

    #[tokio::test]
    async fn a_card_with_no_worktree_yet_starts_over_under_the_new_agent() {
        let Some(f) = fixture(false).await else {
            return;
        };
        request(&f.orch, f.card, f.bo, "you take it").await.unwrap();
        let next = settle(&f.orch, f.card).await.unwrap().expect("started");
        let trigger: String = sqlx::query_scalar("SELECT trigger FROM runs WHERE id = $1")
            .bind(next)
            .fetch_one(&f.t.db.pool)
            .await
            .unwrap();
        assert_ne!(trigger, "handoff", "a fresh start, not a continuation");
        let prompt: String = sqlx::query_scalar("SELECT prompt_override FROM runs WHERE id = $1")
            .bind(next)
            .fetch_one(&f.t.db.pool)
            .await
            .unwrap();
        assert!(
            prompt.starts_with("handoff") && prompt.contains("you take it"),
            "{prompt}"
        );
        assert_eq!(f.assignee().await, Some(f.bo));
        f.t.finish().await;
    }
}
