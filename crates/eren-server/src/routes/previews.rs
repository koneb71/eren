//! Start, read and stop a card's preview.
//!
//! Three verbs on one resource, because a card has at most one preview and
//! "the preview of task X" is the only way anything here is ever addressed.
//!
//! `POST` returns as soon as the row exists rather than when the container is
//! up: a `docker build` of a real project takes minutes, and a button that
//! hangs for them is a button people press twice.

use super::{internal, ApiError};
use crate::auth::{Admin, Caller};
use crate::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use eren_core::scope::Owned;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/docker", get(docker_status))
        .route("/previews/{id}/logs", get(logs))
        .route("/previews/limits", get(get_limits).put(set_limits))
        .route("/previews/disk", get(disk).delete(reclaim))
        .route("/projects/{id}/previews", get(list_for_project))
        .route(
            "/projects/{id}/preview",
            get(current_base).post(start_base).delete(stop_base),
        )
        .route(
            "/projects/{id}/preview-recipe",
            get(get_recipe).post(propose_recipe).put(approve_recipe),
        )
        .route("/tasks/{id}/preview", get(current).post(start).delete(stop))
}

/// Whether previews are possible at all on this machine.
///
/// Probed live, not cached at boot, for the same reason `/api/github` is:
/// Docker Desktop gets started after Eren just as often as before it, and
/// the point of showing this is to say "go and start it".
async fn docker_status(_caller: Caller) -> Json<Value> {
    use eren_core::previews::docker;
    let detected = docker::detect().await;
    // Not installed and installed-but-not-answering are completely different
    // fixes, and so is either one inside a container; `explain` says which.
    let problem = docker::explain(&detected);
    Json(match detected {
        Some(Ok(version)) => json!({
            "installed": true,
            "usable": true,
            "version": version,
        }),
        other => json!({
            "installed": other.is_some(),
            "usable": false,
            "problem": problem,
        }),
    })
}

/// What this preview printed while it was built, and since.
///
/// Two halves because they fail differently: a build log explains an image that
/// never came out, runtime output explains a container that built fine and then
/// refused to serve — which is the case the tail on the card cannot show.
async fn logs(
    State(state): State<AppState>,
    caller: Caller,
    Path(preview_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Preview(preview_id)).await?;
    let (build, runtime) = eren_core::previews::logs(&state.db, preview_id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    Ok(Json(json!({ "build": build, "runtime": runtime })))
}

/// The two numbers that decide whether previews are safe to forget about,
/// alongside what they are currently costing.
async fn get_limits(
    State(state): State<AppState>,
    _caller: Caller,
) -> Result<Json<Value>, ApiError> {
    let limits = eren_core::previews::limits(&state.db).await;
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM previews WHERE status IN ('building','running')")
            .fetch_one(&state.db.pool)
            .await
            .unwrap_or(0);
    Ok(Json(json!({
        "maxLive": limits.max_live,
        "idleMinutes": limits.idle_minutes,
        "live": live,
    })))
}

#[derive(serde::Deserialize)]
struct LimitsBody {
    max_live: i64,
    /// Zero means never idle-stop.
    idle_minutes: i64,
}

/// The admin's: the cap is the machine's, shared by every account's previews.
async fn set_limits(
    State(state): State<AppState>,
    _admin: Admin,
    Json(body): Json<LimitsBody>,
) -> Result<Json<Value>, ApiError> {
    // Clamped in the core rather than rejected here: a slider that refuses is
    // worse than one that stops at its own end.
    let saved = eren_core::previews::set_limits(
        &state.db,
        eren_core::previews::Limits {
            max_live: body.max_live,
            idle_minutes: body.idle_minutes,
            // Not settable from this screen, which is about card previews.
            // Kept as it is rather than defaulted, or saving the preview
            // limits would silently reset the app budget.
            max_live_apps: eren_core::previews::limits(&state.db).await.max_live_apps,
        },
    )
    .await
    .map_err(internal)?;
    Ok(Json(json!({
        "maxLive": saved.max_live,
        "idleMinutes": saved.idle_minutes,
    })))
}

async fn disk(State(state): State<AppState>, _caller: Caller) -> Result<Json<Value>, ApiError> {
    let (bytes, reclaimable) = eren_core::previews::disk(&state.db)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "bytes": bytes, "reclaimable": reclaimable })))
}

/// The admin's: it drops kept images from every workspace's previews, not
/// just the caller's.
async fn reclaim(State(state): State<AppState>, _admin: Admin) -> Result<Json<Value>, ApiError> {
    let freed = eren_core::previews::reclaim_disk(&state.db)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "reclaimed": freed })))
}

/// The project's recipe and whether anyone has approved it.
async fn get_recipe(
    State(state): State<AppState>,
    caller: Caller,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(project_id)).await?;
    let row = sqlx::query(
        "SELECT dockerfile, kind, status, edited FROM preview_recipes WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?;
    Ok(Json(json!({
        "recipe": row.map(|r| json!({
            "dockerfile": r.get::<String, _>("dockerfile"),
            "kind": r.get::<String, _>("kind"),
            "status": r.get::<String, _>("status"),
            "edited": r.get::<bool, _>("edited"),
        })),
    })))
}

/// Ask an agent to write one. Stored as a proposal — never built.
async fn propose_recipe(
    State(state): State<AppState>,
    caller: Caller,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(project_id)).await?;
    let path: String = sqlx::query_scalar("SELECT path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such project".to_string()))?;

    // Gathered here rather than by the agent: it gets no tools and never runs
    // in the project, so what it saw is exactly what the reviewer can see, and
    // a file in the repository cannot talk it into anything on the way past.
    let survey = eren_core::previews::recipe_writer::survey(std::path::Path::new(&path))
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("could not read the project: {e}"),
            )
        })?;

    let engine_id = state.orchestrator.default_engine();
    let engine = state.orchestrator.engine(&engine_id).ok_or((
        StatusCode::PRECONDITION_FAILED,
        "no engine available".to_string(),
    ))?;
    let model_id = state
        .orchestrator
        .model_for(&engine_id, eren_shared::ModelTier::Complex);

    let reply = eren_core::runs::utility::utility_run(
        engine,
        model_id,
        eren_core::previews::recipe_writer::prompt(&survey),
        Some(eren_shared::ReasoningEffort::High),
        std::time::Duration::from_secs(240),
    )
    .await
    .map_err(internal)?;

    let Some((kind, text)) = eren_core::previews::recipe_writer::extract(&reply) else {
        return Err((
            StatusCode::BAD_GATEWAY,
            "The agent returned neither a Dockerfile nor a compose file. Try again, \
             or write one yourself and put it in the project."
                .into(),
        ));
    };

    // Replaces any previous proposal, and resets approval: text nobody has read
    // must never inherit the approval given to different text.
    sqlx::query(
        "INSERT INTO preview_recipes (project_id, dockerfile, kind, status, edited, approved_at)
         VALUES ($1, $2, $3, 'proposed', FALSE, NULL)
         ON CONFLICT (project_id) DO UPDATE
            SET dockerfile = EXCLUDED.dockerfile, kind = EXCLUDED.kind,
                status = 'proposed', edited = FALSE, approved_at = NULL,
                created_at = now()",
    )
    .bind(project_id)
    .bind(&text)
    .bind(kind.as_str())
    .execute(&state.db.pool)
    .await
    .map_err(internal)?;

    Ok(Json(json!({
        "recipe": {
            "dockerfile": text,
            "kind": kind.as_str(),
            "status": "proposed",
            "edited": false,
        },
    })))
}

#[derive(serde::Deserialize)]
struct ApproveBody {
    /// The text being approved. Sent back in full rather than approving "the
    /// current proposal" by reference, so an edit and an approval are one act
    /// and there is no window where a different text gets the nod.
    dockerfile: String,
}

async fn approve_recipe(
    State(state): State<AppState>,
    caller: Caller,
    Path(project_id): Path<Uuid>,
    Json(body): Json<ApproveBody>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(project_id)).await?;
    let text = body.dockerfile.trim().to_string();
    // Re-derived from the text being approved rather than taken from the
    // proposal: a person may have rewritten a Dockerfile into a stack, and what
    // gets built has to match what was read.
    let Some((kind, _)) = eren_core::previews::recipe_writer::extract(&text) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "That is neither a Dockerfile (no FROM line) nor a compose file \
             (no services)."
                .into(),
        ));
    };
    let edited: bool = sqlx::query_scalar(
        "SELECT dockerfile IS DISTINCT FROM $2 FROM preview_recipes WHERE project_id = $1",
    )
    .bind(project_id)
    .bind(&text)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?
    .unwrap_or(true);

    sqlx::query(
        "INSERT INTO preview_recipes (project_id, dockerfile, kind, status, edited, approved_at)
         VALUES ($1, $2, $3, 'approved', $4, now())
         ON CONFLICT (project_id) DO UPDATE
            SET dockerfile = EXCLUDED.dockerfile, kind = EXCLUDED.kind,
                status = 'approved', edited = EXCLUDED.edited, approved_at = now()",
    )
    .bind(project_id)
    .bind(&text)
    .bind(kind.as_str())
    .bind(edited)
    .execute(&state.db.pool)
    .await
    .map_err(internal)?;

    Ok(Json(
        json!({ "approved": true, "edited": edited, "kind": kind.as_str() }),
    ))
}

/// Everything this project has running, plus what it is costing.
async fn list_for_project(
    State(state): State<AppState>,
    caller: Caller,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(project_id)).await?;
    let previews = eren_core::previews::list_for_project(&state.db, project_id)
        .await
        .map_err(internal)?;
    let limits = eren_core::previews::limits(&state.db).await;
    let (bytes, reclaimable) = eren_core::previews::disk(&state.db).await.unwrap_or((0, 0));
    // Counted across every project, not just this one: the cap is a property of
    // the machine, and a tab that says "2 of 3" while another project holds the
    // third would be lying.
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM previews WHERE status IN ('building','running')")
            .fetch_one(&state.db.pool)
            .await
            .unwrap_or(0);
    Ok(Json(json!({
        "previews": previews,
        "live": live,
        "maxLive": limits.max_live,
        "diskBytes": bytes,
        "reclaimable": reclaimable,
    })))
}

/// The project's base-branch preview — what a card's changes are compared to.
async fn current_base(
    State(state): State<AppState>,
    caller: Caller,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(project_id)).await?;
    let preview = eren_core::previews::get_base(&state.db, project_id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "preview": preview })))
}

async fn start_base(
    State(state): State<AppState>,
    caller: Caller,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(project_id)).await?;
    if !docker_ready(&state).await {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            "Docker isn't available on this machine.".into(),
        ));
    }
    let preview = eren_core::previews::start_base(&state.db, project_id)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(json!({ "preview": preview })))
}

async fn stop_base(
    State(state): State<AppState>,
    caller: Caller,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(project_id)).await?;
    let stopped = eren_core::previews::stop_base(&state.db, project_id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "stopped": stopped })))
}

async fn docker_ready(_state: &AppState) -> bool {
    matches!(eren_core::previews::docker::detect().await, Some(Ok(_)))
}

async fn current(
    State(state): State<AppState>,
    caller: Caller,
    Path(task_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(task_id)).await?;
    let preview = eren_core::previews::get(&state.db, task_id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "preview": preview })))
}

async fn start(
    State(state): State<AppState>,
    caller: Caller,
    Path(task_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(task_id)).await?;
    // Checked here rather than left to fail inside the build, so "you have no
    // Docker" is an immediate answer instead of a failed row.
    match eren_core::previews::docker::detect().await {
        None => {
            return Err((
                StatusCode::PRECONDITION_FAILED,
                "Docker isn't installed on this machine, so there is nothing to build with.".into(),
            ))
        }
        Some(Err(problem)) => {
            return Err((
                StatusCode::PRECONDITION_FAILED,
                format!("Docker isn't responding. {problem}"),
            ))
        }
        Some(Ok(_)) => {}
    }

    let preview = eren_core::previews::start(&state.db, task_id)
        .await
        // These are all things the user can act on — no Dockerfile, no
        // worktree — so they are the message, not a 500 with a log line.
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(json!({ "preview": preview })))
}

async fn stop(
    State(state): State<AppState>,
    caller: Caller,
    Path(task_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(task_id)).await?;
    let stopped = eren_core::previews::stop(&state.db, task_id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "stopped": stopped })))
}
