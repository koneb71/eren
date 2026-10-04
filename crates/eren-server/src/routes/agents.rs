use super::{internal, ApiError};
use crate::auth::Caller;
use crate::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use eren_core::runs::utility::{extract_json, utility_run};
use eren_core::scope::Owned;
use eren_shared::{ModelTier, ReasoningEffort};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/agents", get(list).post(create))
        .route("/agents/generate", post(generate))
        .route("/agents/{id}", patch(patch_agent).delete(remove))
        .route("/agents/{id}/pause", post(pause))
        .route("/agents/{id}/resume", post(resume))
        .route("/agents/{id}/retire", post(retire))
        .route("/agents/{id}/memories", get(memories))
        .route("/agent-memories/{id}", axum::routing::delete(forget))
        .route("/workspaces/{id}/org-chart", get(org_chart))
        .route("/agents/{id}/heartbeats", get(heartbeats))
        .route("/workspaces/{id}/heartbeats", get(workspace_heartbeats))
}

/// An agent's recent heartbeats: what each one did.
async fn heartbeats(
    State(state): State<AppState>,
    caller: Caller,
    Path(agent): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Agent(agent)).await?;
    let beats = eren_core::heartbeat::recent(&state.db, agent, 50)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "beats": beats })))
}

/// Beats across the workspace that started or fired something.
async fn workspace_heartbeats(
    State(state): State<AppState>,
    caller: Caller,
    Path(workspace): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Workspace(workspace)).await?;
    let beats = eren_core::heartbeat::recent_in(&state.db, workspace, 20)
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        "beats": beats.into_iter().map(|(agent, b)| json!({ "agent": agent, "beat": b })).collect::<Vec<_>>()
    })))
}

/// The workspace's agents as a reporting tree, with what each is doing.
async fn org_chart(
    State(state): State<AppState>,
    caller: Caller,
    Path(workspace): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Workspace(workspace)).await?;
    let nodes = eren_core::org_chart::chart(&state.db, workspace)
        .await
        .map_err(internal)?;
    Ok(Json(
        json!({ "nodes": nodes, "maxDepth": eren_core::org_chart::MAX_DEPTH }),
    ))
}

/// What this agent remembers, newest first — shown in the agent drawer so the
/// user can see (and prune) what will be fed into its next runs.
async fn memories(
    State(state): State<AppState>,
    caller: Caller,
    Path(agent_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Agent(agent_id)).await?;
    let rows = sqlx::query(
        "SELECT m.id, m.kind, m.content, m.created_at, p.name AS project_name
         FROM agent_memories m LEFT JOIN projects p ON p.id = m.project_id
         WHERE m.agent_id=$1 ORDER BY m.created_at DESC LIMIT 50",
    )
    .bind(agent_id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;
    Ok(Json(json!({
        "memories": rows.iter().map(|r| json!({
            "id": r.get::<Uuid, _>("id"),
            "kind": r.get::<String, _>("kind"),
            "content": r.get::<String, _>("content"),
            "projectName": r.get::<Option<String>, _>("project_name"),
            "ts": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        })).collect::<Vec<_>>()
    })))
}

/// Forget one memory. The user owns the agent's memory, not the agent.
async fn forget(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::AgentMemory(id)).await?;
    sqlx::query("DELETE FROM agent_memories WHERE id=$1")
        .bind(id)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "deleted": true })))
}

fn agent_json(r: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": r.get::<Uuid, _>("id"),
        "name": r.get::<String, _>("name"),
        "icon": r.get::<String, _>("icon"),
        "color": r.get::<String, _>("color"),
        "description": r.get::<String, _>("description"),
        "systemPrompt": r.get::<String, _>("system_prompt"),
        "modelTier": r.get::<String, _>("model_tier"),
        "allowedTools": r.get::<Vec<String>, _>("allowed_tools"),
        "permissionPreset": r.get::<Option<String>, _>("permission_preset"),
        // Null means "inherit" — see the AgentBody doc comment.
        "engine": r.get::<Option<String>, _>("engine"),
        "effort": r.get::<Option<String>, _>("effort"),
        "builtin": r.get::<bool, _>("builtin"),
        // active | paused | retired | pending_approval — see `eren_core::agents`.
        "status": r.get::<String, _>("status"),
        "pauseReason": r.get::<Option<String>, _>("pause_reason"),
        "pausedAt": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("paused_at"),
        "maxConcurrent": r.get::<Option<i32>, _>("max_concurrent"),
        "maxDailyRuns": r.get::<Option<i32>, _>("max_daily_runs"),
        "cooldownSecs": r.get::<Option<i32>, _>("cooldown_secs"),
        // The org chart: who this agent reports to, and what it is called there.
        "reportsTo": r.get::<Option<Uuid>, _>("reports_to"),
        "title": r.get::<Option<String>, _>("title"),
        "heartbeatSecs": r.get::<Option<i32>, _>("heartbeat_secs"),
        "lastHeartbeatAt": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_heartbeat_at"),
    })
}

#[derive(Deserialize)]
struct WsFilter {
    workspace_id: Option<Uuid>,
}

async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Query(filter): Query<WsFilter>,
) -> Result<Json<Value>, ApiError> {
    let workspaces = caller.workspace_filter(&state, filter.workspace_id).await?;
    let rows = sqlx::query(
        "SELECT * FROM agents WHERE $1::uuid[] IS NULL OR workspace_id = ANY($1)
          ORDER BY created_at ASC",
    )
    .bind(workspaces)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;
    Ok(Json(
        json!({ "agents": rows.iter().map(agent_json).collect::<Vec<_>>() }),
    ))
}

#[derive(Deserialize)]
struct AgentBody {
    workspace_id: Uuid,
    name: String,
    #[serde(default = "default_icon")]
    icon: String,
    #[serde(default = "default_color")]
    color: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    system_prompt: String,
    #[serde(default)]
    model_tier: ModelTier,
    #[serde(default)]
    allowed_tools: Vec<String>,
    /// `None` means "inherit the workspace default" — the right starting
    /// point, because an agent describes what someone is good at, not how
    /// much you trust the machine today.
    #[serde(default)]
    permission_preset: Option<String>,
    /// None leaves the CLI's own default alone.
    #[serde(default)]
    effort: Option<String>,
    /// Which CLI this agent prefers. `None` inherits, same reasoning as
    /// `permission_preset`: an agent describes a skill, not a toolchain.
    #[serde(default)]
    engine: Option<String>,
    /// How hard the agent may be worked; `None` is no limit (0077).
    #[serde(default)]
    max_concurrent: Option<i32>,
    #[serde(default)]
    max_daily_runs: Option<i32>,
    #[serde(default)]
    cooldown_secs: Option<i32>,
}

/// A limit is a positive number or nothing — zero would mean "never", which
/// is what pausing the agent is for.
fn check_limits(limits: [Option<i32>; 3]) -> Result<(), ApiError> {
    if limits.iter().flatten().any(|v| *v <= 0) {
        return Err((
            StatusCode::BAD_REQUEST,
            "limits must be more than zero — leave one empty for no limit, or pause the agent"
                .into(),
        ));
    }
    Ok(())
}

fn default_icon() -> String {
    "bot".into()
}
fn default_color() -> String {
    "#4f46e5".into()
}

async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<AgentBody>,
) -> Result<Json<Value>, ApiError> {
    caller
        .require(&state, Owned::Workspace(body.workspace_id))
        .await?;
    if body.name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name is required".into()));
    }
    check_limits([body.max_concurrent, body.max_daily_runs, body.cooldown_secs])?;

    // One `@` namespace: a skill and an agent are both things you write after
    // an `@`, so one name can only mean one of them. Checked from both sides —
    // the other half lives in `skills::check_name_free`.
    if let Err(why) = eren_core::skills::agent_name_free(&state.db, body.workspace_id, &body.name)
        .await
        .map_err(internal)?
    {
        return Err((StatusCode::CONFLICT, why));
    }
    let tier = serde_json::to_value(body.model_tier).unwrap();
    let row = sqlx::query(
        "INSERT INTO agents (workspace_id, name, icon, color, description, system_prompt,
                             model_tier, allowed_tools, permission_preset, effort, engine,
                             max_concurrent, max_daily_runs, cooldown_secs)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) RETURNING *",
    )
    .bind(body.workspace_id)
    .bind(body.name.trim())
    .bind(&body.icon)
    .bind(&body.color)
    .bind(&body.description)
    .bind(&body.system_prompt)
    .bind(tier.as_str().unwrap())
    .bind(&body.allowed_tools)
    .bind(&body.permission_preset)
    .bind(body.effort.as_deref().filter(|e| !e.is_empty()))
    .bind(body.engine.as_deref().filter(|e| !e.is_empty()))
    .bind(body.max_concurrent)
    .bind(body.max_daily_runs)
    .bind(body.cooldown_secs)
    .fetch_one(&state.db.pool)
    .await
    .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(agent_json(&row)))
}

#[derive(Deserialize)]
pub(crate) struct AgentPatch {
    name: Option<String>,
    icon: Option<String>,
    color: Option<String>,
    description: Option<String>,
    system_prompt: Option<String>,
    model_tier: Option<ModelTier>,
    allowed_tools: Option<Vec<String>>,
    /// Present-but-null clears it back to inheriting the workspace default.
    #[serde(default, deserialize_with = "double_option")]
    permission_preset: Option<Option<String>>,
    /// Present-but-null clears it back to the CLI default.
    #[serde(default, deserialize_with = "double_option")]
    effort: Option<Option<String>>,
    /// Present-but-null clears it back to inheriting.
    #[serde(default, deserialize_with = "double_option")]
    engine: Option<Option<String>>,
    /// Present-but-null removes the limit.
    #[serde(default, deserialize_with = "double_option")]
    max_concurrent: Option<Option<i32>>,
    #[serde(default, deserialize_with = "double_option")]
    max_daily_runs: Option<Option<i32>>,
    #[serde(default, deserialize_with = "double_option")]
    cooldown_secs: Option<Option<i32>>,
    /// Present-but-null puts the agent at the top of the chart.
    #[serde(default, deserialize_with = "double_option")]
    reports_to: Option<Option<Uuid>>,
    /// Present-but-null (or blank) clears it.
    #[serde(default, deserialize_with = "double_option")]
    title: Option<Option<String>>,
    /// Seconds between heartbeats; present-but-null turns them off.
    #[serde(default, deserialize_with = "double_option")]
    heartbeat_secs: Option<Option<i32>>,
}

/// Heartbeat intervals a person can pick. Matches the column's CHECK.
pub const HEARTBEATS: [i32; 4] = [300, 900, 3600, 14_400];

/// Distinguish "field absent" from "field set to null" so clearing works.
pub(crate) fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}

/// The route's door to [`update`], which a revision restore also calls once it
/// has checked the agent itself. The manager is checked here as well as by the
/// chart: the chart asks "same workspace?", this asks "yours?" — and answers
/// someone else's agent with the same 404 as one that does not exist.
async fn patch_agent(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<AgentPatch>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Agent(id)).await?;
    if let Some(Some(manager)) = body.reports_to {
        caller.require(&state, Owned::Agent(manager)).await?;
    }
    update(State(state), Path(id), Json(body)).await
}

pub(crate) async fn update(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<AgentPatch>,
) -> Result<Json<Value>, ApiError> {
    check_limits([
        body.max_concurrent.flatten(),
        body.max_daily_runs.flatten(),
        body.cooldown_secs.flatten(),
    ])?;
    if let Some(Some(secs)) = body.heartbeat_secs {
        if !HEARTBEATS.contains(&secs) {
            return Err((
                StatusCode::BAD_REQUEST,
                "a heartbeat is every 5 minutes, 15 minutes, an hour or 4 hours".into(),
            ));
        }
    }
    let title = body.title.clone().map(|t| {
        t.map(|t| t.trim().chars().take(80).collect::<String>())
            .filter(|t| !t.is_empty())
    });
    let tier = body.model_tier.map(|t| {
        serde_json::to_value(t)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    });
    eren_core::revisions::keep(
        &state.db,
        eren_core::revisions::EntityKind::Agent,
        &id.to_string(),
    )
    .await;
    // One transaction for the whole edit, so a refused manager or a failed
    // row update leaves the agent exactly as it was. The manager is the one
    // field whose rules span other rows (no loops, same workspace), so it is
    // checked and written under the chart's lock, held to the commit.
    let mut tx = state.db.pool.begin().await.map_err(internal)?;
    if let Some(manager) = body.reports_to {
        eren_core::org_chart::set_manager_in(&mut tx, &state.db, id, manager)
            .await
            .map_err(internal)?
            .map_err(|why| (StatusCode::CONFLICT, why.to_string()))?;
    }
    let row = sqlx::query(
        "UPDATE agents SET
            name = COALESCE($1, name), icon = COALESCE($2, icon),
            color = COALESCE($3, color), description = COALESCE($4, description),
            system_prompt = COALESCE($5, system_prompt), model_tier = COALESCE($6, model_tier),
            allowed_tools = COALESCE($7, allowed_tools),
            permission_preset = CASE WHEN $12 THEN $8 ELSE permission_preset END,
            effort = CASE WHEN $10 THEN $9 ELSE effort END,
            engine = CASE WHEN $14 THEN $13 ELSE engine END,
            max_concurrent = CASE WHEN $15 THEN $16 ELSE max_concurrent END,
            max_daily_runs = CASE WHEN $17 THEN $18 ELSE max_daily_runs END,
            cooldown_secs = CASE WHEN $19 THEN $20 ELSE cooldown_secs END,
            title = CASE WHEN $21 THEN $22 ELSE title END,
            heartbeat_secs = CASE WHEN $23 THEN $24 ELSE heartbeat_secs END
         WHERE id = $11 RETURNING *",
    )
    .bind(body.name)
    .bind(body.icon)
    .bind(body.color)
    .bind(body.description)
    .bind(body.system_prompt)
    .bind(tier)
    .bind(body.allowed_tools)
    .bind(body.permission_preset.clone().flatten())
    .bind(body.effort.clone().flatten().filter(|e| !e.is_empty()))
    .bind(body.effort.is_some())
    .bind(id)
    .bind(body.permission_preset.is_some())
    .bind(body.engine.clone().flatten().filter(|e| !e.is_empty()))
    .bind(body.engine.is_some())
    .bind(body.max_concurrent.is_some())
    .bind(body.max_concurrent.flatten())
    .bind(body.max_daily_runs.is_some())
    .bind(body.max_daily_runs.flatten())
    .bind(body.cooldown_secs.is_some())
    .bind(body.cooldown_secs.flatten())
    .bind(title.is_some())
    .bind(title.flatten())
    .bind(body.heartbeat_secs.is_some())
    .bind(body.heartbeat_secs.flatten())
    .fetch_one(&mut *tx)
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    // A raised limit is a new answer for runs already waiting on the old one.
    sqlx::query(
        "UPDATE queue SET not_before = NULL, hold_reason = NULL
          WHERE held_by IS NULL AND hold_reason IS NOT NULL
            AND run_id IN (SELECT r.id FROM runs r LEFT JOIN tasks t ON t.id = r.task_id
                            WHERE COALESCE(r.agent_id, t.agent_id) = $1)",
    )
    .bind(id)
    .execute(&state.db.pool)
    .await
    .map_err(internal)?;
    Ok(Json(agent_json(&row)))
}

/// Delete an agent nothing refers to; retire one that something does.
///
/// An agent named by a card, a run or a routine could never be deleted —
/// those rows keep who did the work, and the delete failed on their foreign
/// keys with a 500. Retiring is what deleting such an agent can honestly mean:
/// no new work, gone from the pickers, and the history still says who did it.
/// Asked of the database rather than by listing the tables that point here,
/// so a table added later cannot bring the 500 back.
async fn remove(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Agent(id)).await?;
    eren_core::revisions::keep(
        &state.db,
        eren_core::revisions::EntityKind::Agent,
        &id.to_string(),
    )
    .await;
    // Its reports move up to its own manager either way — deleted or retired.
    eren_core::org_chart::lift(&state.db, id)
        .await
        .map_err(internal)?;
    let deleted = sqlx::query("DELETE FROM agents WHERE id=$1")
        .bind(id)
        .execute(&state.db.pool)
        .await;
    match deleted {
        Ok(_) => Ok(Json(json!({ "deleted": true }))),
        Err(sqlx::Error::Database(e)) if e.is_foreign_key_violation() => {
            let stopped = state
                .orchestrator
                .retire_agent(id)
                .await
                .map_err(internal)?;
            Ok(Json(
                json!({ "deleted": false, "retired": true, "stopped": stopped }),
            ))
        }
        Err(e) => Err(internal(e)),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PauseBody {
    /// Shown wherever the pause refuses something, so the refusal explains
    /// itself: "Ada is paused (over budget)".
    reason: Option<String>,
    /// Also stop what the agent is doing now, not only what it would start.
    stop_now: bool,
}

async fn pause(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    body: Option<Json<PauseBody>>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Agent(id)).await?;
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let stopped = state
        .orchestrator
        .pause_agent(id, body.reason.as_deref(), body.stop_now)
        .await
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(json!({ "paused": true, "stopped": stopped })))
}

async fn resume(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Agent(id)).await?;
    state
        .orchestrator
        .resume_agent(id)
        .await
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(json!({ "resumed": true })))
}

async fn retire(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Agent(id)).await?;
    let stopped = state
        .orchestrator
        .retire_agent(id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "retired": true, "stopped": stopped })))
}

#[derive(Deserialize)]
struct GenerateBody {
    description: String,
    /// Engine id from `/api/engines`. Omitted means the machine default.
    engine: Option<String>,
    /// Which tier designs the agents. Omitted keeps the previous behaviour.
    ///
    /// Worth choosing rather than fixed, because the tier resolves through the
    /// person's own settings: somebody who mapped Complex to Fable was paying
    /// the most expensive model for every generation without ever being asked.
    /// The note below about effort is the argument — this is one-shot judgement,
    /// which thinking time serves better than model size.
    model_tier: Option<ModelTier>,
}

const GENERATE_PROMPT: &str = r##"You are designing coding agents for a multi-agent workflow platform.
Based on the user's need below, output ONLY a JSON array of 1 to 4 agent definitions — no prose,
no markdown fences. Each element:
{"name": "short name", "icon": "one of: bot|wrench|shield|book|flask|scale",
 "color": "#hex", "description": "one line",
 "system_prompt": "2-6 sentences defining role, approach, and output standards",
 "model_tier": "easy"|"medium"|"complex",
 "permission_preset": "reviewed"|"auto_edit",
 "allowed_tools": []}
Tier guide: easy=mechanical work (Sonnet), medium=typical coding (Opus), complex=review/judging/architecture (Fable).

User's need: "##;

/// Touches no workspace: it only drafts definitions for a person to save
/// through `create`, which checks the workspace then.
async fn generate(
    State(state): State<AppState>,
    _caller: Caller,
    Json(body): Json<GenerateBody>,
) -> Result<Json<Value>, ApiError> {
    if body.description.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "description is required".into()));
    }
    let default_engine = state.orchestrator.default_engine();
    let engine_id = body.engine.as_deref().unwrap_or(&default_engine);
    let engine = state.orchestrator.engine(engine_id).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("unknown engine {engine_id}"),
        )
    })?;
    let tier = body.model_tier.unwrap_or(ModelTier::Complex);
    let model_id = state.orchestrator.model_for(engine_id, tier);
    let prompt = format!("{GENERATE_PROMPT}{}\"", body.description.trim());

    // Designing a team is the kind of one-shot judgement that repays
    // thinking time far more than it repays a bigger model.
    let output = utility_run(
        &state.db,
        engine,
        model_id,
        prompt,
        Some(ReasoningEffort::High),
        Duration::from_secs(180),
    )
    .await
    .map_err(super::run_refused)?;
    match extract_json(&output) {
        Ok(Value::Array(drafts)) => Ok(Json(json!({ "drafts": drafts }))),
        Ok(single @ Value::Object(_)) => Ok(Json(json!({ "drafts": [single] }))),
        _ => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("model output was not valid JSON:\n{output}"),
        )),
    }
}
