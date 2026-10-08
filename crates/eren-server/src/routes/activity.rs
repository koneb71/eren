//! What the machine is doing, and what it has cost.
//!
//! The binding constraint here is a subscription rate limit, not money, and
//! nothing in the app surfaced burn rate — you could only see cost one run
//! at a time, after the fact. This is the operations view: what is running,
//! what is waiting, what is blocked on you, and what the last week cost.

use super::{internal, require_write, ApiError};
use crate::auth::{Admin, Caller};
use crate::AppState;
use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use eren_core::runs::orchestrator::QueueGate;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/activity", get(activity))
        .route("/queue/pause", post(pause))
        .route("/queue/resume", post(resume))
        .route("/queue/budget", post(set_budget))
}

#[derive(Deserialize)]
struct Filter {
    workspace_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct BudgetBody {
    /// Dollars per day. `null` (or absent) removes the cap.
    cap_usd: Option<f64>,
}

/// The queue and its daily cap belong to the machine, not to a workspace, so
/// these three are the admin's.
async fn set_budget(
    State(state): State<AppState>,
    _admin: Admin,
    headers: axum::http::HeaderMap,
    Json(body): Json<BudgetBody>,
) -> Result<Json<Value>, ApiError> {
    // The same gate and the same undo as every other budget write: this is
    // the "Daily budget" policy under an older name.
    super::require_write(&headers, "this changes a budget")?;
    let daily: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM budget_policies WHERE scope_kind = 'machine' AND name = 'Daily budget'",
    )
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?;
    if let Some(id) = daily {
        eren_core::revisions::keep(
            &state.db,
            eren_core::revisions::EntityKind::BudgetPolicy,
            &id.to_string(),
        )
        .await;
    }
    state
        .orchestrator
        .set_daily_budget(body.cap_usd)
        .await
        .map_err(internal)?;
    Ok(Json(
        json!({ "capUsd": state.orchestrator.daily_budget().await }),
    ))
}

async fn pause(
    State(state): State<AppState>,
    _admin: Admin,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_write(&headers, "this stops the queue")?;
    state
        .orchestrator
        .set_queue_paused(true)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "paused": true })))
}

async fn resume(
    State(state): State<AppState>,
    _admin: Admin,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_write(&headers, "this starts the queue")?;
    state
        .orchestrator
        .set_queue_paused(false)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "paused": false })))
}

/// One poll for the whole picture. Deliberately a single endpoint: the view
/// is a dashboard, and five requests to paint one screen would be worse.
async fn activity(
    State(state): State<AppState>,
    caller: Caller,
    Query(filter): Query<Filter>,
) -> Result<Json<Value>, ApiError> {
    // No workspace asked for is every one of the caller's, never the machine's.
    let ws = caller.workspace_filter(&state, filter.workspace_id).await?;

    // Live and waiting runs, with enough context to name what they are.
    // The project join is via whichever of task/workflow/chat/org owns it.
    let runs = sqlx::query(
        "SELECT r.id, r.status, r.trigger, r.cost_usd, r.created_at, r.started_at,
                r.engine, r.model,
                r.goal, t.title AS task_title, w.name AS workflow_name,
                rs.question AS research_question,
                tm.name AS team_name, p.name AS project_name, p.id AS project_id,
                r.team_id, r.task_id, q.hold_reason
         FROM runs r
         LEFT JOIN queue q ON q.run_id = r.id
         LEFT JOIN tasks t ON t.id = r.task_id
         LEFT JOIN workflows w ON w.id = r.workflow_id
         LEFT JOIN teams tm ON tm.id = r.team_id
         LEFT JOIN chats c ON c.id = r.chat_id
         LEFT JOIN researches rs ON rs.id = r.research_id
         LEFT JOIN projects p ON p.id = COALESCE(
             r.project_id, t.project_id, w.project_id, c.project_id, rs.project_id)
         WHERE r.status NOT IN ('completed','failed','canceled')
           -- A general chat or research has no project; its workspace lives
           -- on its own row. Without the COALESCE the filter nulls those runs
           -- out of the page entirely.
           AND ($1::uuid[] IS NULL
                OR COALESCE(p.workspace_id, c.workspace_id, rs.workspace_id) = ANY($1))
         ORDER BY
             CASE r.status WHEN 'awaiting_approval' THEN 0
                           WHEN 'waiting_permission' THEN 1
                           WHEN 'running' THEN 2
                           WHEN 'starting' THEN 3
                           ELSE 4 END,
             r.created_at",
    )
    .bind(&ws)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;

    let live: Vec<Value> = runs
        .iter()
        .map(|r| {
            let label = r
                .get::<Option<String>, _>("task_title")
                .or_else(|| r.get::<Option<String>, _>("workflow_name"))
                .or_else(|| r.get::<Option<String>, _>("goal"))
                // A research run's name is its question. Without the join a
                // research run was not merely untitled here — the workspace
                // filter nulled it out of the page entirely.
                .or_else(|| r.get::<Option<String>, _>("research_question"))
                .unwrap_or_else(|| "Assistant".to_string());
            json!({
                "id": r.get::<Uuid, _>("id"),
                "label": label,
                "status": r.get::<String, _>("status"),
                "trigger": r.get::<String, _>("trigger"),
                "teamName": r.get::<Option<String>, _>("team_name"),
                "projectName": r.get::<Option<String>, _>("project_name"),
                "projectId": r.get::<Option<Uuid>, _>("project_id"),
                "taskId": r.get::<Option<Uuid>, _>("task_id"),
                "isOrg": r.get::<Option<Uuid>, _>("team_id").is_some(),
                "costUsd": r.get::<Option<f64>, _>("cost_usd"),
                "engine": r.get::<String, _>("engine"),
                "model": r.get::<Option<String>, _>("model"),
                "startedAt": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("started_at"),
                "createdAt": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
                // Why a queued run is not starting: the budget holding it.
                "holdReason": r.get::<Option<String>, _>("hold_reason"),
            })
        })
        .collect();

    // Anything actually blocked on a person: a parked plan, or a permission
    // prompt the broker is still holding.
    let mut blocked: Vec<Value> = live
        .iter()
        .filter(|r| r["status"] == "awaiting_approval")
        // Where to send someone who clicks it. A team's plan lives in the team
        // room; a card's lives on the card, and opening an org room for a run
        // that has no team shows an empty one.
        .map(|r| {
            json!({
                "runId": r["id"],
                "kind": "plan",
                "label": r["label"],
                "isOrg": r["isOrg"],
                "projectId": r["projectId"],
            })
        })
        .collect();
    for run in &live {
        let run_id: Uuid = serde_json::from_value(run["id"].clone()).map_err(internal)?;
        for (request_id, tool, input) in state.permissions.pending_for_run(run_id) {
            blocked.push(json!({
                "runId": run_id,
                "kind": "permission",
                "label": run["label"],
                "requestId": request_id,
                "tool": tool,
                // Answering "allow Bash" without seeing the command is not a
                // decision, so the input travels with the prompt.
                "input": input,
            }));
        }
    }

    // Spend by day. Fourteen days is enough to see a trend without turning
    // this into a reporting feature. A run belongs to the day it ran — the
    // same reading a budget's window uses, so "spent today" beside the daily
    // cap can never disagree with whether that cap is spent.
    let daily = sqlx::query(
        "SELECT date_trunc('day', COALESCE(r.finished_at, r.started_at, r.created_at)) AS day,
                SUM(COALESCE(r.cost_usd, 0)) AS cost,
                COUNT(*) AS runs
         FROM runs r
         LEFT JOIN tasks t ON t.id = r.task_id
         LEFT JOIN workflows w ON w.id = r.workflow_id
         LEFT JOIN chats c ON c.id = r.chat_id
         LEFT JOIN researches rs ON rs.id = r.research_id
         LEFT JOIN projects p ON p.id = COALESCE(
             r.project_id, t.project_id, w.project_id, c.project_id, rs.project_id)
         WHERE COALESCE(r.finished_at, r.started_at, r.created_at) > now() - interval '14 days'
           AND ($1::uuid[] IS NULL
                OR COALESCE(p.workspace_id, c.workspace_id, rs.workspace_id) = ANY($1))
         GROUP BY 1 ORDER BY 1",
    )
    .bind(&ws)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;

    // Who is spending it — measured per agent, a team run charged step by
    // step. See `spend::by_agent`.
    let by_agent = super::spend::by(
        &state.db,
        ws.as_deref(),
        14,
        eren_core::spend::Dimension::Agent,
    )
    .await
    .map_err(internal)?;

    let today: f64 = daily
        .last()
        .filter(|r| {
            r.get::<chrono::DateTime<chrono::Utc>, _>("day")
                .date_naive()
                == chrono::Utc::now().date_naive()
        })
        .map(|r| r.get::<Option<f64>, _>("cost").unwrap_or(0.0))
        .unwrap_or(0.0);

    let gate = state.orchestrator.queue_gate().await;
    Ok(Json(json!({
        "paused": gate == QueueGate::Paused,
        "gate": match gate {
            QueueGate::Open => json!({ "state": "open" }),
            QueueGate::Paused => json!({ "state": "paused" }),
            // Named separately from `paused` because there is no resume for
            // it — it clears when its window turns, and a button would lie.
            QueueGate::OverBudget(over) => json!({
                "state": "over_budget",
                "policy": over.policy,
                "policyId": over.policy_id,
                "detail": over.detail,
                "resetsAt": over.resets_at,
                "message": over.to_string(),
            }),
        },
        "budgetUsd": state.orchestrator.daily_budget().await,
        "live": live,
        "blocked": blocked,
        "spend": {
            "today": today,
            "window": daily.iter().map(|r| r.get::<Option<f64>, _>("cost").unwrap_or(0.0)).sum::<f64>(),
            "daily": daily.iter().map(|r| json!({
                "day": r.get::<chrono::DateTime<chrono::Utc>, _>("day"),
                "cost": r.get::<Option<f64>, _>("cost").unwrap_or(0.0),
                "runs": r.get::<i64, _>("runs"),
            })).collect::<Vec<_>>(),
            "byAgent": by_agent.iter().take(8).map(|a| json!({
                "name": a.key,
                "cost": a.cost_usd,
                "steps": a.runs,
            })).collect::<Vec<_>>(),
        },
    })))
}
