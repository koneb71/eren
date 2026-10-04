//! The gallery's own endpoints.
//!
//! Everything here is a person acting through the dashboard, so it all sits
//! behind the ordinary loopback checks like every other route, and touches
//! only apps and projects in the caller's own workspaces. What an *app*
//! may ask for is a different surface with a different gate, and does not live
//! here.

use super::{internal, ApiError};
use crate::auth::Caller;
use crate::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use eren_core::apps;
use eren_core::scope::Owned;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/apps", get(list).post(install))
        .route("/apps/generate", post(generate))
        .route("/apps/{id}", get(one).delete(uninstall))
        .route("/apps/{id}/manifest", put(set_manifest))
        .route("/apps/{id}/active", post(set_active))
        .route("/apps/{id}/builds", get(builds).post(change))
        .route("/apps/{id}/builds/{build}/revert", post(revert_build))
        .route("/apps/{id}/schema/apply", post(apply_schema))
        .route("/apps/{id}/schema/discard", post(discard_schema))
        .route("/apps/{id}/data/{model}", get(rows).post(add_row))
        .route(
            "/apps/{id}/data/{model}/{row}",
            axum::routing::patch(change_row).delete(drop_row),
        )
        .route("/apps/{id}/chart/{view}", get(chart))
        .route("/apps/{id}/run", get(container).post(start).delete(stop))
        .route(
            "/apps/{id}/dockerfile",
            get(dockerfile).post(approve_dockerfile),
        )
        .route("/apps/{id}/grants", get(grants).put(set_grants))
        .route("/apps/{id}/actions/{action}", post(run_action))
        .route("/apps/{id}/export", get(export))
        .route("/apps/import", post(import))
        .route("/projects/{id}/apps", get(repo_apps))
        .route("/projects/{id}/apps/sync", post(sync_app))
}

fn plan_json(plan: &apps::PendingPlan) -> Value {
    json!({
        "id": plan.id,
        "statements": plan.statements.iter().map(|s| json!({
            "sql": s.sql,
            "destructive": s.destructive,
            "why": s.why,
        })).collect::<Vec<_>>(),
    })
}

fn app_json(app: &apps::App) -> Value {
    // The menu rides along on the list the sidebar already fetches, rather than
    // costing a request per app to draw a nav item. A manifest that no longer
    // parses gets an empty one: the sidebar is not where someone should first
    // learn an app is broken.
    let menu = app.parsed().map_or_else(
        |_| Vec::new(),
        |m| {
            m.menu
                .iter()
                .map(|e| json!({ "label": e.label, "view": e.view }))
                .collect::<Vec<_>>()
        },
    );
    json!({
        "id": app.id,
        "projectId": app.project_id,
        "workspaceId": app.workspace_id,
        "slug": app.slug,
        "name": app.name,
        "icon": app.icon,
        "summary": app.summary,
        "brief": app.brief,
        "runtime": app.runtime.as_str(),
        "active": app.active,
        "path": app.path.to_string_lossy(),
        "menu": menu,
    })
}

/// A manifest problem is the person's to fix, not a server fault.
///
/// 400 with the parser's own text, which names the key — "models.expense.
/// fields.qty: unknown field type" is something to act on, where a 500 is not.
fn bad_manifest(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::BAD_REQUEST, e.to_string())
}

#[derive(Deserialize)]
pub struct WorkspaceFilter {
    pub workspace_id: Option<Uuid>,
}

async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Query(filter): Query<WorkspaceFilter>,
) -> Result<Json<Value>, ApiError> {
    let only = caller.workspace_filter(&state, filter.workspace_id).await?;
    // `apps::list` narrows to one workspace or none, so with no workspace asked
    // for it answers every one; the caller's own set is applied here.
    let apps: Vec<apps::App> = apps::list(&state.db, filter.workspace_id)
        .await
        .map_err(internal)?
        .into_iter()
        .filter(|a| only.as_ref().is_none_or(|ws| ws.contains(&a.workspace_id)))
        .collect();
    Ok(Json(
        json!({ "apps": apps.iter().map(app_json).collect::<Vec<_>>() }),
    ))
}

/// One app, with everything a screen needs to draw it.
///
/// The parsed manifest travels alongside the text, so the browser never has to
/// read YAML and cannot end up with a different idea of what the app declares
/// than the server has. A manifest that no longer parses still returns the app
/// — with the error — because the way out of a broken manifest is the editor,
/// and a 500 would take that away.
async fn one(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let app = load(&state, &caller, id).await?;
    let mut body = app_json(&app);
    body["manifest"] = json!(app.manifest);
    match app.parsed() {
        Ok(parsed) => body["declares"] = apps::render::manifest_json(&parsed),
        Err(e) => body["manifestError"] = json!(e.to_string()),
    }
    body["pending"] = apps::pending_plan(&state.db, id)
        .await
        .map_err(internal)?
        .as_ref()
        .map_or(Value::Null, plan_json);
    Ok(Json(body))
}

#[derive(Deserialize)]
struct Install {
    workspace_id: Uuid,
    manifest: String,
    #[serde(default)]
    brief: String,
}

async fn install(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<Install>,
) -> Result<Json<Value>, ApiError> {
    caller
        .require(&state, Owned::Workspace(body.workspace_id))
        .await?;
    // Validated here as well as inside `install`, so a manifest an agent got
    // wrong comes back as a 400 naming the key rather than a 500 naming
    // nothing. The check inside is what makes the guarantee; this is what makes
    // the message.
    apps::manifest::parse(&body.manifest).map_err(bad_manifest)?;
    let app = apps::install(&state.db, body.workspace_id, &body.manifest, &body.brief)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(app_json(&app)))
}

#[derive(Deserialize)]
struct Generate {
    description: String,
    engine: Option<String>,
    /// Omitted means a module — the safe runtime, and the one that needs no
    /// Docker on the machine.
    runtime: Option<String>,
}

/// Write a manifest from a description, and hand it back unsaved.
///
/// Returned rather than installed, deliberately. The whole reason a module is
/// YAML instead of code is that a person can read it before it becomes real —
/// installing it for them would spend that and give nothing back. Same shape as
/// `/api/agents/generate`.
///
/// The result is parsed here before it is returned, so a manifest the model got
/// wrong arrives with the error attached and the text still editable, rather
/// than looking fine until the install button fails.
async fn generate(
    State(state): State<AppState>,
    _caller: Caller,
    Json(body): Json<Generate>,
) -> Result<Json<Value>, ApiError> {
    let description = body.description.trim();
    if description.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "say what the app is for".into()));
    }
    let runtime = match body.runtime.as_deref() {
        None => apps::Runtime::Module,
        Some(r) => apps::Runtime::parse(r)
            .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("there is no {r} runtime")))?,
    };

    let default_engine = state.orchestrator.default_engine();
    let engine_id = body.engine.as_deref().unwrap_or(&default_engine);
    let engine = state.orchestrator.engine(engine_id).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("unknown engine {engine_id}"),
        )
    })?;
    let model_id = state
        .orchestrator
        .model_for(engine_id, eren_shared::ModelTier::Complex);

    // No tools and no project directory: this writes text, and a run that
    // cannot touch the filesystem cannot get that wrong in an interesting way.
    let output = eren_core::runs::utility::utility_run(
        engine,
        model_id,
        apps::scaffold::manifest_prompt(description, runtime),
        Some(eren_shared::ReasoningEffort::High),
        std::time::Duration::from_secs(180),
    )
    .await
    .map_err(internal)?;

    let manifest = apps::scaffold::extract(&output);
    let error = apps::manifest::parse(&manifest)
        .err()
        .map(|e| e.to_string());
    Ok(Json(json!({ "manifest": manifest, "error": error })))
}

#[derive(Deserialize)]
struct SetManifest {
    manifest: String,
}

async fn set_manifest(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<SetManifest>,
) -> Result<Json<Value>, ApiError> {
    let app = load(&state, &caller, id).await?;
    let (_, outcome) = apps::set_manifest(&state.db, &app, &body.manifest)
        .await
        .map_err(bad_manifest)?;
    let app = load(&state, &caller, id).await?;
    let mut out = app_json(&app);
    out["manifest"] = json!(app.manifest);
    // The caller needs to know whether the tables followed. A manifest that
    // saved but whose columns are waiting on approval looks identical
    // otherwise, and the app would appear to have changed when it has not.
    out["applied"] = json!(outcome.applied.len());
    out["pending"] = outcome.pending.as_ref().map_or(Value::Null, plan_json);
    Ok(Json(out))
}

fn build_json(b: &apps::build::Build, revertible: Option<Uuid>) -> Value {
    json!({
        "id": b.id,
        "taskId": b.task_id,
        "brief": b.brief,
        "status": b.status,
        "error": b.error,
        "landedCommit": b.landed_commit,
        "createdAt": b.created_at.to_rfc3339(),
        // Answered by the server rather than derived in the browser: which
        // build may be undone is a rule about what `base_commit` can promise,
        // and two implementations of it would disagree exactly once.
        "revertible": revertible == Some(b.id),
    })
}

async fn builds(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    load(&state, &caller, id).await?;
    let builds = apps::build::list(&state.db, id).await.map_err(internal)?;
    let revertible = apps::build::revertible(&builds);
    Ok(Json(json!({
        "builds": builds.iter().map(|b| build_json(b, revertible)).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct Change {
    brief: String,
    engine: Option<String>,
}

/// Hand this app to an agent.
///
/// An ordinary card on the app's own project — the orchestrator gives it a
/// worktree and a diff like any other. What is different is recorded in
/// `app_builds`: where the branch stood before it started, which is what makes
/// the automatic landing undoable.
async fn change(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<Change>,
) -> Result<Json<Value>, ApiError> {
    let app = load(&state, &caller, id).await?;
    let brief = body.brief.trim();
    if brief.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "say what should change".into()));
    }
    // One change at a time. Two started together would record the same
    // `base_commit`, and undoing the second would then reset past the first as
    // well — see `apps::build::in_progress`.
    if let Some(running) = apps::build::in_progress(&state.db, app.id)
        .await
        .map_err(internal)?
    {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "this app is already being changed — \"{running}\" has not finished. \
                 Wait for it, or cancel its card."
            ),
        ));
    }

    // Read before the card exists. Once the run has landed there is no way to
    // ask git where the branch was, and an undo needs to know.
    let base = apps::build::base_commit(&app).await;

    let engine = body
        .engine
        .clone()
        .unwrap_or_else(|| state.orchestrator.default_engine());
    let prompt = apps::scaffold::build_prompt(&app.manifest, app.runtime, brief);
    let title = apps::build::commit_message(brief)
        .strip_prefix("eren: ")
        .unwrap_or(brief)
        .to_string();

    // Auto-edit rather than the machine default, which is Reviewed. The agent
    // is editing a worktree of a folder Eren created, the result lands with a
    // real undo, and a dialog per line of YAML is the gate nobody reads — the
    // same argument `runtime.rs` makes for owning the Dockerfile. Not full-auto:
    // that would also stop asking about everything which is *not* an edit.
    const MODE: eren_shared::PermissionMode = eren_shared::PermissionMode::AutoEdit;

    // Refused on the click rather than as a failed run several seconds later,
    // which would leave a card and a build row to clean up. Existence is
    // checked separately because `vet_engine` answers "can this engine honour
    // that mode" and says *nothing* about an engine it has never heard of —
    // for an unknown id it returns None, which reads as approval.
    if state.orchestrator.engine(&engine).is_none() {
        return Err((StatusCode::BAD_REQUEST, format!("unknown engine {engine}")));
    }
    // The mode is Eren's choice here, not the person's, so an engine that
    // cannot honour it is Eren asking for something impossible.
    if let Some(reason) = state.orchestrator.vet_engine(&engine, MODE) {
        return Err((StatusCode::CONFLICT, reason));
    }

    // Created in the backlog and only moved to In Progress once the run is
    // actually queued, the same order `tasks::create` uses. Inserting it as
    // running would leave a card that nothing is working on if the enqueue
    // failed — and, here, a build row stuck on `running` forever with it.
    let task_id: Uuid = sqlx::query_scalar(
        "INSERT INTO tasks (project_id, title, prompt, model_tier, engine, board_column,
                            permission_mode)
         VALUES ($1, $2, $3, 'complex', $4, 'backlog', 'auto_edit') RETURNING id",
    )
    .bind(app.project_id)
    .bind(&title)
    .bind(&prompt)
    .bind(&engine)
    .fetch_one(&state.db.pool)
    .await
    .map_err(internal)?;

    let build_id = apps::build::record(&state.db, app.id, task_id, brief, base.as_deref())
        .await
        .map_err(internal)?;

    let run_id = state
        .orchestrator
        .enqueue_task(task_id)
        .await
        .map_err(super::run_refused)?;
    sqlx::query("UPDATE tasks SET board_column='running' WHERE id=$1")
        .bind(task_id)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    Ok(Json(
        json!({ "buildId": build_id, "taskId": task_id, "runId": run_id }),
    ))
}

/// Put the app back the way it was before its most recent change.
async fn revert_build(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, build)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    load(&state, &caller, id).await?;
    // `revert` finds the app from the build, so a build of some other app
    // would undo that one instead — and the check above would have been on
    // the wrong app.
    let owner = apps::build::get(&state.db, build)
        .await
        .map_err(internal)?
        .map(|b| b.app_id);
    if owner != Some(id) {
        return Err((StatusCode::NOT_FOUND, "no such build".into()));
    }
    let app = apps::build::revert(&state.db, build)
        .await
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
    let mut out = app_json(&app);
    out["manifest"] = json!(app.manifest);
    Ok(Json(out))
}

#[derive(Deserialize)]
struct PlanId {
    plan_id: Uuid,
}

/// Run a migration a person has read.
///
/// The plan id is required rather than "whatever is pending", so approving a
/// screen someone has been looking at cannot silently apply a different
/// migration that replaced it while they read.
async fn apply_schema(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<PlanId>,
) -> Result<Json<Value>, ApiError> {
    load(&state, &caller, id).await?;
    let applied = apps::apply_plan(&state.db, id, body.plan_id)
        .await
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(json!({ "applied": applied.len() })))
}

async fn discard_schema(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<PlanId>,
) -> Result<Json<Value>, ApiError> {
    load(&state, &caller, id).await?;
    apps::discard_plan(&state.db, id, body.plan_id)
        .await
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SetActive {
    active: bool,
}

async fn set_active(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<SetActive>,
) -> Result<Json<Value>, ApiError> {
    load(&state, &caller, id).await?;
    apps::set_active(&state.db, id, body.active)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "active": body.active })))
}

/// Remove an app, its tables and its folder.
///
/// The only destructive verb here. Deactivating is what "I am done with this
/// for now" means, and it keeps everything; this is the one that does not, so
/// the dashboard asks before calling it.
async fn uninstall(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::App(id)).await?;
    apps::uninstall(&state.db, id).await.map_err(internal)?;
    Ok(Json(json!({ "ok": true })))
}

// ── An app's own rows ───────────────────────────────────────────────────────
//
// No scope gates these. The tables exist because this app declared them, hold
// only what it put there, and are dropped with it — asking permission to use
// the thing you installed would be ceremony.

/// A data problem is the caller's to fix: an undeclared field, a value that is
/// not a number, a filter that is not a filter. 400 with the text, which names
/// which field and what was sent.
fn bad_data(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::BAD_REQUEST, e.to_string())
}

/// Resolve an app and one of its models together.
///
/// Both come from the manifest, so nothing after this point is holding a name
/// that arrived in the URL — which is the property the whole data layer rests
/// on. See `apps::query` for why.
async fn model_of(
    state: &AppState,
    caller: &Caller,
    id: Uuid,
    model: &str,
) -> Result<(apps::App, apps::manifest::Model), ApiError> {
    let app = load(state, caller, id).await?;
    let parsed = app.parsed().map_err(bad_manifest)?;
    let model = apps::data::model_of(&parsed, model)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?
        .clone();
    Ok((app, model))
}

/// Read the query string as a list of pairs.
///
/// A list, not a struct: `where` may appear several times and a struct keeps
/// only the last one, which would silently drop every filter but one and
/// return the wrong rows without erroring.
fn raw_query(pairs: Vec<(String, String)>) -> apps::query::Raw {
    let mut raw = apps::query::Raw::default();
    for (key, value) in pairs {
        match key.as_str() {
            "where" => raw.filters.push(value),
            "order" => raw.order = Some(value),
            "limit" => raw.limit = value.parse().ok(),
            "offset" => raw.offset = value.parse().ok(),
            _ => {}
        }
    }
    raw
}

async fn rows(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, model)): Path<(Uuid, String)>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Json<Value>, ApiError> {
    let (app, model) = model_of(&state, &caller, id, &model).await?;
    let raw = raw_query(pairs);
    let rows = apps::data::list(&state.db, &app.schema, &model, &raw)
        .await
        .map_err(bad_data)?;
    let total = apps::data::count(&state.db, &app.schema, &model, &raw)
        .await
        .map_err(bad_data)?;
    Ok(Json(json!({ "rows": rows, "total": total })))
}

async fn add_row(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, model)): Path<(Uuid, String)>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, ApiError> {
    let (app, model) = model_of(&state, &caller, id, &model).await?;
    apps::data::create(&state.db, &app.schema, &model, &body)
        .await
        .map(Json)
        .map_err(bad_data)
}

async fn change_row(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, model, row)): Path<(Uuid, String, Uuid)>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, ApiError> {
    let (app, model) = model_of(&state, &caller, id, &model).await?;
    apps::data::update(&state.db, &app.schema, &model, row, &body)
        .await
        .map_err(bad_data)?
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "no such row".into()))
}

async fn drop_row(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, model, row)): Path<(Uuid, String, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let (app, model) = model_of(&state, &caller, id, &model).await?;
    let gone = apps::data::delete(&state.db, &app.schema, &model, row)
        .await
        .map_err(bad_data)?;
    if gone {
        Ok(Json(json!({ "ok": true })))
    } else {
        Err((StatusCode::NOT_FOUND, "no such row".into()))
    }
}

/// The buckets behind a chart view.
///
/// Grouped in Postgres. A year of entries drawn as twelve bars should send
/// twelve numbers, not a year of entries.
async fn chart(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, view)): Path<(Uuid, String)>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Json<Value>, ApiError> {
    let app = load(&state, &caller, id).await?;
    let parsed = app.parsed().map_err(bad_manifest)?;
    let view = parsed
        .view(&view)
        .ok_or((StatusCode::NOT_FOUND, "no such view".to_string()))?;
    let apps::manifest::ViewSpec::Chart {
        group_by, measure, ..
    } = &view.spec
    else {
        return Err((StatusCode::BAD_REQUEST, "that view is not a chart".into()));
    };
    let model = parsed
        .model(&view.model)
        .ok_or((StatusCode::NOT_FOUND, "no such model".to_string()))?;

    let buckets = apps::render::chart(
        &state.db,
        &app.schema,
        model,
        group_by,
        measure,
        &raw_query(pairs),
    )
    .await
    .map_err(bad_data)?;
    Ok(Json(json!({ "buckets": buckets })))
}

// ── Container apps ──────────────────────────────────────────────────────────

/// Refuse plainly when this app has nothing to run.
///
/// A module is drawn by the dashboard, so "start the container" is not a
/// slower version of anything — it is a category error, and saying so beats a
/// Docker message about a missing Dockerfile.
fn container_app(app: &apps::App) -> Result<(), ApiError> {
    if app.runtime.is_container() {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            format!(
                "\"{}\" is a module — Eren draws it, so there is nothing to run.",
                app.name
            ),
        ))
    }
}

/// Docker, in the two shapes the UI needs: a flag to disable a button, and a
/// sentence saying which of the two problems it is. "Not installed" and "the
/// daemon is not answering" are completely different fixes.
async fn docker_state() -> Value {
    match docker_problem().await {
        None => json!({ "usable": true, "problem": Value::Null }),
        Some(problem) => json!({ "usable": false, "problem": problem }),
    }
}

async fn docker_problem() -> Option<String> {
    use eren_core::previews::docker;
    docker::explain(&docker::detect().await)
}

/// Whether this app's container is up, and where.
async fn container(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let app = load(&state, &caller, id).await?;
    container_app(&app)?;
    let preview = eren_core::previews::get_base(&state.db, app.project_id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        // The slug, not a URL: the port to put in it is the one Eren is being
        // *served* on, which the browser knows and the server does not.
        "slug": app.slug,
        "preview": preview,
        "docker": docker_state().await,
    })))
}

/// Build and run it, or wake it if the image is still here.
async fn start(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let app = load(&state, &caller, id).await?;
    container_app(&app)?;
    if !app.active {
        return Err((StatusCode::CONFLICT, "switch this app on first".into()));
    }
    if let Some(problem) = docker_problem().await {
        return Err((StatusCode::PRECONDITION_FAILED, problem));
    }
    let preview = eren_core::previews::start_base(&state.db, app.project_id)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(json!({ "slug": app.slug, "preview": preview })))
}

async fn stop(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let app = load(&state, &caller, id).await?;
    eren_core::previews::stop_base(&state.db, app.project_id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "ok": true })))
}

/// The Dockerfile this app would build from, and whether it needs reading.
async fn dockerfile(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    use eren_core::apps::runtime::{self, Build};
    let app = load(&state, &caller, id).await?;
    container_app(&app)?;

    let committed = tokio::fs::read_to_string(app.path.join("Dockerfile"))
        .await
        .ok();
    Ok(Json(
        match runtime::drift(app.runtime, committed.as_deref()) {
            Build::Owned(text) => json!({ "text": text, "drifted": false, "sha": Value::Null }),
            Build::Drifted { text, sha } => {
                let approved: Option<String> =
                    sqlx::query_scalar("SELECT dockerfile_sha256 FROM apps WHERE id = $1")
                        .bind(id)
                        .fetch_one(&state.db.pool)
                        .await
                        .map_err(internal)?;
                json!({
                    "text": text,
                    // Drifted *and unapproved* is what the gate reacts to. An edit
                    // someone already read is not a question any more.
                    "drifted": approved.as_deref() != Some(sha.as_str()),
                    "sha": sha,
                })
            }
            Build::None => json!({ "text": Value::Null, "drifted": false, "sha": Value::Null }),
        },
    ))
}

#[derive(Deserialize)]
struct ApproveDockerfile {
    sha: String,
}

/// Say that a person has read the Dockerfile as it now stands.
///
/// The hash, not a flag: approval attaches to the text, so the next rewrite
/// does not inherit the reading someone gave this one. Same rule as approving
/// a preview recipe.
async fn approve_dockerfile(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<ApproveDockerfile>,
) -> Result<Json<Value>, ApiError> {
    let app = load(&state, &caller, id).await?;
    container_app(&app)?;
    let committed = tokio::fs::read_to_string(app.path.join("Dockerfile"))
        .await
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                "this app has no Dockerfile".to_string(),
            )
        })?;

    // Re-derived from the file rather than trusted from the request, so what is
    // approved is what is on disk right now — not what the screen was showing
    // when it was opened.
    let actual = eren_core::apps::digest(committed.trim());
    if actual != body.sha {
        return Err((
            StatusCode::CONFLICT,
            "the Dockerfile changed while you were reading it — look again".into(),
        ));
    }
    sqlx::query("UPDATE apps SET dockerfile_sha256 = $2, approved_at = now() WHERE id = $1")
        .bind(id)
        .bind(&actual)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "approved": actual })))
}

// ── Grants ──────────────────────────────────────────────────────────────────

/// What the app asks for, what it has, and what each one means.
///
/// All three together, because a permissions screen that shows only what is
/// granted cannot show what is being asked for, and the question is the point.
async fn grants(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let app = load(&state, &caller, id).await?;
    let held = apps::grants::list(&state.db, id).await.map_err(internal)?;
    let requested = app.parsed().map(|m| m.scopes).unwrap_or_default();

    Ok(Json(json!({
        "requested": requested.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        "granted": held.iter().map(|g| json!({
            "scope": g.scope.as_str(),
            "grantedAt": g.granted_at,
            "lastUsedAt": g.last_used_at,
        })).collect::<Vec<_>>(),
        "all": apps::scope::ALL.iter().map(|s| json!({
            "scope": s.as_str(),
            "blurb": s.blurb(),
            "write": s.is_write(),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct SetGrants {
    scopes: Vec<String>,
}

async fn set_grants(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<SetGrants>,
) -> Result<Json<Value>, ApiError> {
    load(&state, &caller, id).await?;
    let mut scopes = Vec::new();
    for text in &body.scopes {
        // An unknown scope is refused rather than dropped: a client sending one
        // has a bug, and quietly granting the subset it got right would hide it.
        scopes.push(apps::Scope::parse(text).ok_or((
            StatusCode::BAD_REQUEST,
            format!("\"{text}\" is not a permission Eren has"),
        ))?);
    }
    apps::grants::set(&state.db, id, &scopes)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "granted": body.scopes })))
}

#[derive(Deserialize)]
struct RunAction {
    model: String,
    row: Option<Uuid>,
}

/// Press a button.
///
/// A step needing a scope nobody granted does not fail the request — it comes
/// back as `needsScope`, so the screen can offer to grant it instead of showing
/// an error about something the person is allowed to fix.
async fn run_action(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, action)): Path<(Uuid, String)>,
    Json(body): Json<RunAction>,
) -> Result<Json<Value>, ApiError> {
    let (app, model) = model_of(&state, &caller, id, &body.model).await?;
    let manifest = app.parsed().map_err(bad_manifest)?;

    let out = apps::run::run(
        &state.db,
        &state.orchestrator,
        &app,
        &manifest,
        &action,
        &model,
        body.row,
    )
    .await
    .map_err(bad_data)?;

    Ok(Json(json!({
        "messages": out.messages,
        "goto": out.goto,
        "deleted": out.deleted,
        "needsScope": out.needs_scope,
    })))
}

// ── Taking an app elsewhere ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ExportWhat {
    /// `true` is a *Move* export. Absent is *Share* — no rows.
    #[serde(default)]
    pub data: bool,
}

/// Download an app as a bundle.
///
/// Served as a file rather than as JSON in a response body, because the thing
/// a person does next is send it to someone.
async fn export(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Query(what): Query<ExportWhat>,
) -> Result<axum::response::Response, ApiError> {
    let app = load(&state, &caller, id).await?;
    let text = apps::export(&state.db, &app, what.data)
        .await
        .map_err(internal)?;

    let filename = format!("{}.erenapp", app.slug);
    Ok(axum::response::Response::builder()
        .header("content-type", "application/json; charset=utf-8")
        // Quoted, and the slug is already a DNS label, so there is nothing in
        // it that could end the header early.
        .header(
            "content-disposition",
            format!("attachment; filename=\"{filename}\""),
        )
        .body(axum::body::Body::from(text))
        .map_err(internal)?)
}

#[derive(Deserialize)]
struct Import {
    workspace_id: Uuid,
    bundle: String,
}

async fn import(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<Import>,
) -> Result<Json<Value>, ApiError> {
    caller
        .require(&state, Owned::Workspace(body.workspace_id))
        .await?;
    let app = apps::import(&state.db, body.workspace_id, &body.bundle)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(app_json(&app)))
}

/// Apps a project offers under `.eren/apps/`.
async fn repo_apps(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    let row: Option<(String, Uuid)> =
        sqlx::query_as("SELECT path, workspace_id FROM projects WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db.pool)
            .await
            .map_err(internal)?;
    let (path, workspace_id) = row.ok_or((StatusCode::NOT_FOUND, "no such project".to_string()))?;

    let found = apps::sync::scan(&state.db, workspace_id, std::path::Path::new(&path))
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        "apps": found.iter().map(|f| json!({
            "dir": f.dir,
            "name": f.name,
            "summary": f.summary,
            "error": f.error,
            "installedAs": f.installed_as,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct SyncOne {
    dir: String,
}

/// Install a repo app, or update the one already here.
async fn sync_app(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<SyncOne>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    let row: Option<(String, Uuid)> =
        sqlx::query_as("SELECT path, workspace_id FROM projects WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db.pool)
            .await
            .map_err(internal)?;
    let (path, workspace_id) = row.ok_or((StatusCode::NOT_FOUND, "no such project".to_string()))?;

    let app = apps::sync::adopt(
        &state.db,
        workspace_id,
        std::path::Path::new(&path),
        &body.dir,
    )
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(app_json(&app)))
}

/// The app, once it is known to be the caller's: someone else's answers as a
/// missing one. Every `/apps/{id}` route comes through here (or through
/// `model_of`, which does), so none can skip the check.
async fn load(state: &AppState, caller: &Caller, id: Uuid) -> Result<apps::App, ApiError> {
    caller.require(state, Owned::App(id)).await?;
    apps::get(&state.db, id)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such app".into()))
}
