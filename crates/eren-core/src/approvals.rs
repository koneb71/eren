//! Every way a person answers something that is waiting on them.
//!
//! These lived in route handlers, one state machine per screen: a card's plan
//! in `routes/tasks.rs`, a team's in `routes/orgs.rs`, a chat question and a
//! chat plan in `routes/chat.rs`. That was fine while each had exactly one
//! door. The inbox is a second door to all of them, and two doors to one state
//! machine is how they drift — so the transitions live here, once, and the
//! routes and the inbox both call them.
//!
//! Each one keeps the property its route had: the status check is in the
//! `WHERE` of the write, never a read beforehand, so a double click or a race
//! with the executor lands once and the second is refused.

use crate::runs::orchestrator::Orchestrator;
use sqlx::Row;
use uuid::Uuid;

/// Why an answer was not taken. The variants map to HTTP statuses in the
/// server; core says what happened, not how to say it on the wire.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("{0}")]
    NotFound(String),
    /// It is no longer waiting — answered already, or moved on.
    #[error("{0}")]
    Conflict(String),
    /// The answer itself is unusable (an empty note, say).
    #[error("{0}")]
    Invalid(String),
    /// A gate said no: the agent is paused, a budget is spent. Carried whole,
    /// so the server can answer it the way it answers that gate anywhere.
    #[error(transparent)]
    Gated(anyhow::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for Refusal {
    fn from(e: sqlx::Error) -> Self {
        Refusal::Internal(e.into())
    }
}

fn not_waiting() -> Refusal {
    Refusal::Conflict("this run is not waiting for approval".into())
}

/// Whether the run's scope may spend more, asked before an approval queues
/// it: a budget spent while the plan waited refuses here, with the policy's
/// name, rather than holding the approved run in the queue. The planning
/// pass was counted when it was claimed, so it is not held by its own count.
async fn budget_allows(orch: &Orchestrator, run_id: Uuid) -> Result<(), Refusal> {
    orch.budget_allows_more(run_id)
        .await
        .map_err(|over| Refusal::Gated(over.into()))
}

// ── A card's plan ───────────────────────────────────────────────────────────

/// Start the work, from whatever the plan says now.
pub async fn approve_task_plan(orch: &Orchestrator, run_id: Uuid) -> Result<(), Refusal> {
    // A paused agent's plan can be approved later; it is not run now.
    crate::agents::assert_may_dispatch(&orch.db, run_id)
        .await
        .map_err(Refusal::Gated)?;
    budget_allows(orch, run_id).await?;
    let updated = sqlx::query(
        "UPDATE runs SET plan_approved_at = now(), status = 'queued'
         WHERE id = $1 AND status = 'awaiting_approval'",
    )
    .bind(run_id)
    .execute(&orch.db.pool)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(not_waiting());
    }
    // Re-queued rather than resumed in place: the planning dispatch already
    // released its slot, so this takes a fresh one when the queue has room.
    orch.queue(run_id, 10).await?;
    Ok(())
}

/// Send the plan back for another pass, saying what was wrong with it.
pub async fn revise_task_plan(
    orch: &Orchestrator,
    run_id: Uuid,
    note: &str,
) -> Result<(), Refusal> {
    crate::agents::assert_may_dispatch(&orch.db, run_id)
        .await
        .map_err(Refusal::Gated)?;
    budget_allows(orch, run_id).await?;
    let note = note.trim();
    if note.is_empty() {
        return Err(Refusal::Invalid(
            "say what to change — a rejection with no reason just burns another pass".into(),
        ));
    }
    // The rejected plan stays on file: the next pass is shown it alongside the
    // feedback, so it can answer the objection rather than start from nothing.
    let updated = sqlx::query(
        "UPDATE runs SET plan_note = $2, plan_edited = FALSE, status = 'queued'
         WHERE id = $1 AND status = 'awaiting_approval'",
    )
    .bind(run_id)
    .bind(note)
    .execute(&orch.db.pool)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(not_waiting());
    }
    orch.queue(run_id, 10).await?;
    Ok(())
}

// ── A team's plan ───────────────────────────────────────────────────────────

pub async fn approve_org_plan(orch: &Orchestrator, run_id: Uuid) -> Result<(), Refusal> {
    crate::agents::assert_may_dispatch(&orch.db, run_id)
        .await
        .map_err(Refusal::Gated)?;
    budget_allows(orch, run_id).await?;
    let updated = sqlx::query(
        "UPDATE runs SET plan_approved_at = now(), status = 'queued'
         WHERE id = $1 AND status = 'awaiting_approval'",
    )
    .bind(run_id)
    .execute(&orch.db.pool)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(not_waiting());
    }
    orch.queue(run_id, 15).await?;
    orch.post(
        run_id,
        None,
        "system",
        None,
        "status",
        "Plan approved — starting work.",
    )
    .await?;
    Ok(())
}

pub async fn reject_org_plan(
    orch: &Orchestrator,
    run_id: Uuid,
    reason: Option<&str>,
) -> Result<(), Refusal> {
    let reason = reason
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("the plan was rejected");
    let updated = sqlx::query(
        "UPDATE runs SET status='canceled', error_reason=$2, finished_at=now()
         WHERE id=$1 AND status='awaiting_approval'",
    )
    .bind(run_id)
    .bind(reason)
    .execute(&orch.db.pool)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(not_waiting());
    }
    sqlx::query(
        "UPDATE steps SET status='skipped', finished_at=now()
         WHERE run_id=$1 AND status='queued'",
    )
    .bind(run_id)
    .execute(&orch.db.pool)
    .await?;
    orch.post(
        run_id,
        None,
        "system",
        None,
        "status",
        &format!("Run canceled — {reason}"),
    )
    .await?;
    Ok(())
}

// ── A chat's question and plan ──────────────────────────────────────────────

/// A chat's turn still in flight, if any. A new turn cannot start under it.
pub async fn chat_busy(orch: &Orchestrator, chat_id: Uuid) -> Result<bool, Refusal> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM runs WHERE chat_id = $1
                         AND status NOT IN ('completed','failed','canceled'))",
    )
    .bind(chat_id)
    .fetch_one(&orch.db.pool)
    .await?)
}

fn still_working() -> Refusal {
    Refusal::Conflict("the assistant is still working on the previous message".into())
}

/// What a chat answer produced: the message it wrote, and the turn it started.
#[derive(Debug, Clone, Copy)]
pub struct Turn {
    pub message_id: Uuid,
    pub run_id: Uuid,
}

/// Answer a clarifying question, and start the turn that reads the answer.
///
/// One action rather than "mark answered" plus "send a message": an answer
/// recorded without a turn leaves the assistant waiting for something that
/// already happened.
pub async fn answer_chat_question(
    orch: &Orchestrator,
    chat_id: Uuid,
    question_id: Uuid,
    answers: &[Vec<String>],
) -> Result<Turn, Refusal> {
    if chat_busy(orch, chat_id).await? {
        return Err(still_working());
    }
    let engine = gated_engine(orch, chat_id).await?;
    let row = sqlx::query(
        "UPDATE chat_questions SET answered_at = now(), answer = $3
          WHERE id = $1 AND chat_id = $2 AND answered_at IS NULL
        RETURNING questions",
    )
    .bind(question_id)
    .bind(chat_id)
    .bind(serde_json::to_value(answers).map_err(anyhow::Error::from)?)
    .fetch_optional(&orch.db.pool)
    .await?
    .ok_or_else(|| Refusal::Conflict("that question has already been answered".into()))?;

    let questions: Vec<crate::runs::questions::Question> =
        serde_json::from_value(row.get("questions")).map_err(anyhow::Error::from)?;
    let content = crate::runs::questions::answer_message(&questions, answers);
    user_turn(orch, chat_id, &engine, &content).await
}

/// Carry out a chat plan. Approving leaves plan mode: the next turn is the one
/// that acts, so it needs the tools plan mode took away.
pub async fn approve_chat_plan(
    orch: &Orchestrator,
    chat_id: Uuid,
    message_id: Uuid,
    edited: Option<&str>,
) -> Result<Turn, Refusal> {
    if chat_busy(orch, chat_id).await? {
        return Err(still_working());
    }
    let engine = gated_engine(orch, chat_id).await?;
    let claimed = sqlx::query(
        "UPDATE chat_messages SET plan_outcome = 'approved'
          WHERE id = $1 AND chat_id = $2 AND is_plan AND plan_outcome IS NULL",
    )
    .bind(message_id)
    .bind(chat_id)
    .execute(&orch.db.pool)
    .await?;
    if claimed.rows_affected() == 0 {
        return Err(Refusal::Conflict(
            "that plan has already been answered".into(),
        ));
    }
    sqlx::query("UPDATE chats SET plan_mode = false, updated_at = now() WHERE id = $1")
        .bind(chat_id)
        .execute(&orch.db.pool)
        .await?;
    let content = crate::runs::chat_plan::approval(edited.map(str::trim).filter(|p| !p.is_empty()));
    user_turn(orch, chat_id, &engine, &content).await
}

/// The engine the next turn will run on, already vetted. Asked before the
/// answer or approval is recorded: a turn refused after it — no tools on that
/// engine, a spent budget — left the plan approved, the question answered,
/// and nothing running to act on either.
async fn gated_engine(orch: &Orchestrator, chat_id: Uuid) -> Result<String, Refusal> {
    let engine = orch.chat_engine(chat_id).await?;
    orch.vet_chat_turn(chat_id, &engine)
        .await
        .map_err(Refusal::Gated)?;
    Ok(engine)
}

async fn user_turn(
    orch: &Orchestrator,
    chat_id: Uuid,
    engine: &str,
    content: &str,
) -> Result<Turn, Refusal> {
    let message_id: Uuid = sqlx::query_scalar(
        "INSERT INTO chat_messages (chat_id, role, content) VALUES ($1, 'user', $2) RETURNING id",
    )
    .bind(chat_id)
    .bind(content)
    .fetch_one(&orch.db.pool)
    .await?;
    sqlx::query("UPDATE chats SET updated_at = now() WHERE id = $1")
        .bind(chat_id)
        .execute(&orch.db.pool)
        .await?;
    let run_id = orch
        .enqueue_chat_turn(chat_id, engine)
        .await
        .map_err(Refusal::Gated)?;
    Ok(Turn { message_id, run_id })
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb;

    async fn parked(t: &testdb::TestDb, task: Uuid) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO runs (task_id, status, trigger, engine, plan_approval)
             VALUES ($1, 'awaiting_approval', 'manual', 'mock', TRUE) RETURNING id",
        )
        .bind(task)
        .fetch_one(&t.db.pool)
        .await
        .unwrap()
    }

    /// Two doors (the card and the inbox) and a double click: the plan is
    /// approved once, and the second answer is told it is too late.
    #[tokio::test]
    async fn a_plan_is_answered_once() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let orch = t.orchestrator(dir.path());
        orch.set_queue_paused(true).await.unwrap();
        let (_, project) = t.project(dir.path(), true).await;
        let card = t.card(project, "planned").await;
        let run = parked(&t, card).await;

        approve_task_plan(&orch, run).await.unwrap();
        let again = approve_task_plan(&orch, run).await.unwrap_err();
        assert!(matches!(again, Refusal::Conflict(_)), "{again:?}");
        let revise = revise_task_plan(&orch, run, "no").await.unwrap_err();
        assert!(matches!(revise, Refusal::Conflict(_)), "{revise:?}");

        let (status, approved): (String, bool) =
            sqlx::query_as("SELECT status, plan_approved_at IS NOT NULL FROM runs WHERE id = $1")
                .bind(run)
                .fetch_one(&t.db.pool)
                .await
                .unwrap();
        assert_eq!((status.as_str(), approved), ("queued", true));
        t.finish().await;
    }

    #[tokio::test]
    async fn a_revision_must_say_what_to_change() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let orch = t.orchestrator(dir.path());
        let (_, project) = t.project(dir.path(), true).await;
        let card = t.card(project, "planned").await;
        let run = parked(&t, card).await;
        let empty = revise_task_plan(&orch, run, "   ").await.unwrap_err();
        assert!(matches!(empty, Refusal::Invalid(_)), "{empty:?}");
        let still: String = sqlx::query_scalar("SELECT status FROM runs WHERE id = $1")
            .bind(run)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
        assert_eq!(still, "awaiting_approval", "an empty note changes nothing");
        t.finish().await;
    }

    /// Rejecting a team plan cancels the run and drops the work it proposed.
    #[tokio::test]
    async fn a_rejected_team_plan_drops_its_assignments() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let orch = t.orchestrator(dir.path());
        let (_, project) = t.project(dir.path(), true).await;
        let card = t.card(project, "team work").await;
        let run = parked(&t, card).await;
        sqlx::query("INSERT INTO steps (run_id, step_key, status) VALUES ($1, 'a1', 'queued')")
            .bind(run)
            .execute(&t.db.pool)
            .await
            .unwrap();

        reject_org_plan(&orch, run, Some("  ")).await.unwrap();
        let (status, reason): (String, String) =
            sqlx::query_as("SELECT status, error_reason FROM runs WHERE id = $1")
                .bind(run)
                .fetch_one(&t.db.pool)
                .await
                .unwrap();
        assert_eq!(status, "canceled");
        assert_eq!(
            reason, "the plan was rejected",
            "a blank reason reads as none"
        );
        let step: String = sqlx::query_scalar("SELECT status FROM steps WHERE run_id = $1")
            .bind(run)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
        assert_eq!(step, "skipped");
        t.finish().await;
    }

    /// A turn that would be refused is refused before the approval is
    /// recorded: otherwise the plan reads approved, nothing runs, and
    /// approving again says it was already answered.
    #[tokio::test]
    async fn an_approval_whose_turn_is_refused_leaves_the_plan_waiting() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mut orch = Orchestrator::new(
            t.db.clone(),
            crate::bus::EventBus::new(),
            std::sync::Arc::new(crate::worktrees::manager::WorktreeManager::new(
                dir.path().join("wt"),
            )),
            4,
            None,
        );
        orch.register_engine(std::sync::Arc::new(eren_engines::mock::MockEngine::demo()));
        orch.register_engine(std::sync::Arc::new(
            eren_engines::gemini::GeminiEngine::default(),
        ));
        let (_, project) = t.project(dir.path(), true).await;
        let chat: Uuid = sqlx::query_scalar(
            "INSERT INTO chats (project_id, title) VALUES ($1, 'Talk') RETURNING id",
        )
        .bind(project)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        // The conversation so far ran on Gemini, which a project chat refuses.
        sqlx::query(
            "INSERT INTO runs (chat_id, status, trigger, engine) VALUES ($1, 'completed', 'chat', 'gemini')",
        )
        .bind(chat)
        .execute(&t.db.pool)
        .await
        .unwrap();
        let plan: Uuid = sqlx::query_scalar(
            "INSERT INTO chat_messages (chat_id, role, content, is_plan) VALUES ($1, 'assistant', 'the plan', TRUE) RETURNING id",
        )
        .bind(chat)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        let refused = approve_chat_plan(&orch, chat, plan, None)
            .await
            .unwrap_err();
        assert!(matches!(refused, Refusal::Gated(_)), "{refused:?}");
        let outcome: Option<String> =
            sqlx::query_scalar("SELECT plan_outcome FROM chat_messages WHERE id = $1")
                .bind(plan)
                .fetch_one(&t.db.pool)
                .await
                .unwrap();
        assert_eq!(outcome, None, "still waiting for an answer");
        // The chat carries on on the engine it was on, not the machine default.
        assert_eq!(orch.chat_engine(chat).await.unwrap(), "gemini");
    }
}
