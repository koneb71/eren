//! Settings that belong to the machine rather than to a workspace.
//!
//! Today that means which model each complexity tier routes to. The tiers
//! themselves (easy / medium / complex) are how tasks, agents and workflow
//! steps express "how hard is this" — but which model answers that question
//! depends on the user's plan and appetite, and used to be hard-coded.

use super::{internal, ApiError};
use crate::auth::{Admin, Caller};
use crate::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use eren_shared::{
    is_known_model_for, EngineTierEffort, EngineTierMapping, ModelTier, PermissionMode,
    ReasoningEffort, TierMapping, MODEL_CHOICES,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings/models", get(get_models).put(set_models))
        .route(
            "/settings/permissions",
            get(get_permissions).put(set_permissions),
        )
        .route("/settings/effort", get(get_effort).put(set_effort))
        .route(
            "/settings/permissions/apply-to-agents",
            axum::routing::post(apply_to_agents),
        )
        .route("/settings/attention", get(get_attention).put(set_attention))
        .route(
            "/settings/unattended",
            get(get_unattended).put(set_unattended),
        )
}

async fn get_models(State(state): State<AppState>, _caller: Caller) -> Json<Value> {
    let mapping = state.orchestrator.tier_mapping();
    let efforts = state.orchestrator.tier_efforts();
    // One column per engine that is actually installed. An engine the user
    // doesn't have is not worth asking them to configure.
    let engines = state
        .orchestrator
        .engines()
        .into_iter()
        .filter(|e| e.id() != "mock")
        .map(|e| {
            let current = mapping.for_engine(e.id());
            // What Reset actually goes back to, which is the mapping derived
            // from this install's own models when there is one. The built-in
            // constant is the fallback rather than the answer: it names
            // Anthropic ids to a Google-only OpenCode, and nothing at all to
            // a local runtime — and a "default" the engine cannot run makes
            // the Reset button write a value the save then refuses.
            let defaults = state
                .orchestrator
                .derived_defaults(e.id())
                .unwrap_or_else(|| EngineTierMapping::defaults_for(e.id()));
            json!({
                "id": e.id(),
                "label": e.label(),
                // A fixed catalog is only honest for an engine that has one.
                // OpenCode fronts 75+ providers, so its field is free text and
                // the picker says so rather than offering a stale list.
                "fixedCatalog": e.capabilities().fixed_model_catalog,
                // A fixed catalog is the engine's own when its install
                // reported one — Gemini's aliases, Amp's modes — and Claude
                // Code's, the one catalog Eren ships, when it did not.
                "choices": match (e.capabilities().fixed_model_catalog, state.orchestrator.engine_info(e.id())) {
                    (false, _) => vec![],
                    (true, Some(info)) if !info.models.is_empty() => info
                        .models
                        .iter()
                        .map(|id| json!({ "id": id, "label": id, "blurb": null }))
                        .collect::<Vec<_>>(),
                    (true, _) => MODEL_CHOICES.iter().map(|m| json!({
                        "id": m.id, "label": m.label, "blurb": m.blurb,
                    })).collect::<Vec<_>>(),
                },
                // What this install can actually reach, straight from the CLI.
                // Suggestions, not a whitelist: a local model the CLI doesn't
                // enumerate is still a legitimate thing to type.
                "available": state
                    .orchestrator
                    .engine_info(e.id())
                    .map(|i| i.models.clone())
                    .unwrap_or_default(),
                "providers": state
                    .orchestrator
                    .engine_info(e.id())
                    .map(|i| i.providers.clone())
                    .unwrap_or_default(),
                "tiers": tiers_json(&current),
                "defaults": tiers_json(&defaults),
                // How hard each tier thinks on this engine. Null means the tier
                // pins nothing and inherits — the shipped state, and a real
                // answer rather than an unconfigured one.
                "efforts": efforts_json(&efforts.for_engine(e.id())),
            })
        })
        .collect::<Vec<_>>();
    Json(json!({ "engines": engines }))
}

fn tiers_json(m: &TierMapping) -> Value {
    json!({
        "easy": m.model_for(ModelTier::Easy),
        "medium": m.model_for(ModelTier::Medium),
        "complex": m.model_for(ModelTier::Complex),
    })
}

fn efforts_json(m: &BTreeMap<ModelTier, ReasoningEffort>) -> Value {
    json!({
        "easy": m.get(&ModelTier::Easy).map(|e| e.as_str()),
        "medium": m.get(&ModelTier::Medium).map(|e| e.as_str()),
        "complex": m.get(&ModelTier::Complex).map(|e| e.as_str()),
    })
}

#[derive(Deserialize)]
struct ModelsBody {
    /// `{engine_id: {easy, medium, complex}}`.
    engines: BTreeMap<String, TierRow>,
    /// The same shape, but every level is nullable and null means "inherit".
    /// Separate from `engines` so a client that only routes models can leave
    /// it out entirely and change nothing.
    #[serde(default)]
    efforts: BTreeMap<String, EffortRow>,
}

#[derive(Deserialize, Default)]
struct EffortRow {
    #[serde(default)]
    easy: Option<ReasoningEffort>,
    #[serde(default)]
    medium: Option<ReasoningEffort>,
    #[serde(default)]
    complex: Option<ReasoningEffort>,
}

#[derive(Deserialize)]
struct TierRow {
    easy: String,
    medium: String,
    complex: String,
}

async fn set_models(
    State(state): State<AppState>,
    _admin: Admin,
    Json(body): Json<ModelsBody>,
) -> Result<Json<Value>, ApiError> {
    let mut mapping = BTreeMap::new();
    for (engine, row) in body.engines {
        if state.orchestrator.engine(&engine).is_none() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("\"{engine}\" is not an engine this machine has"),
            ));
        }
        // Checked here as well as in the orchestrator so the message names the
        // offending field rather than just the id.
        for (field, model) in [
            ("easy", &row.easy),
            ("medium", &row.medium),
            ("complex", &row.complex),
        ] {
            if !is_known_model_for(&engine, model) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("\"{model}\" is not a model {engine} can run (field {field})"),
                ));
            }
        }
        mapping.insert(
            engine,
            TierMapping(
                [
                    (ModelTier::Easy, row.easy),
                    (ModelTier::Medium, row.medium),
                    (ModelTier::Complex, row.complex),
                ]
                .into_iter()
                .collect(),
            ),
        );
    }

    // Validated in the same pass as the models, so an unknown engine is one
    // rejection rather than a half-applied save.
    let mut effort_map = BTreeMap::new();
    for (engine, row) in body.efforts {
        if state.orchestrator.engine(&engine).is_none() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("\"{engine}\" is not an engine this machine has"),
            ));
        }
        let tiers: BTreeMap<ModelTier, ReasoningEffort> = [
            (ModelTier::Easy, row.easy),
            (ModelTier::Medium, row.medium),
            (ModelTier::Complex, row.complex),
        ]
        .into_iter()
        // A tier that pins nothing is stored as absent, not as a null — so
        // "inherit" has one representation and the map stays readable.
        .filter_map(|(tier, effort)| effort.map(|e| (tier, e)))
        .collect();
        if !tiers.is_empty() {
            effort_map.insert(engine, tiers);
        }
    }

    state
        .orchestrator
        .set_tier_mapping(EngineTierMapping(mapping))
        .await
        .map_err(internal)?;
    state
        .orchestrator
        .set_tier_efforts(EngineTierEffort(effort_map))
        .await
        .map_err(internal)?;

    // Runs already in flight keep the model they started with — a model is
    // chosen when a run starts, and the CLI cannot be asked to switch mid
    // conversation.
    Ok(Json(json!({ "saved": true })))
}

/// How much freedom new work gets by default.
async fn get_permissions(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    // How many agents pin their own mode. A bound agent's preset overrides
    // the default, so this number is exactly "how many agents will ignore
    // the setting above" — worth showing rather than leaving to be discovered
    // when a run stops to ask anyway. Counted among the caller's own agents:
    // someone else's are neither theirs to see nor to clear.
    let mine = caller.workspaces(&state).await?;
    let overriding: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agents WHERE permission_preset IS NOT NULL
            AND ($1::uuid[] IS NULL OR workspace_id = ANY($1))",
    )
    .bind(mine)
    .fetch_one(&state.db.pool)
    .await
    .unwrap_or(0);

    Ok(Json(json!({
        "agentsOverriding": overriding,
        "defaultMode": state.orchestrator.default_permission_mode().await,
        "modes": [
            { "id": "reviewed",  "label": "Ask me first",
              "blurb": "Every file edit and command waits for your approval." },
            { "id": "auto_edit", "label": "Edit freely, ask before commands",
              "blurb": "File changes go ahead; shell commands still stop for you." },
            { "id": "full_auto", "label": "Don't ask",
              "blurb": "Work straight through. Needs a git project that opted in, because the run is isolated in a worktree you review before merging." },
        ],
    })))
}

#[derive(Deserialize)]
struct PermissionsBody {
    default_mode: PermissionMode,
}

#[derive(Deserialize)]
struct EffortBody {
    /// Absent or null returns every run to its CLI's own default rather than
    /// pinning one, which is a real choice and the one this ships with.
    default_effort: Option<ReasoningEffort>,
}

async fn get_effort(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    // Agents carrying their own budget outrank this, exactly as they do for
    // permissions — worth counting here rather than leaving it to be discovered
    // when a card thinks harder or less hard than the setting says. Among the
    // caller's own agents, as for permissions.
    let mine = caller.workspaces(&state).await?;
    let overriding: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agents WHERE effort IS NOT NULL
            AND ($1::uuid[] IS NULL OR workspace_id = ANY($1))",
    )
    .bind(mine)
    .fetch_one(&state.db.pool)
    .await
    .unwrap_or(0);
    Ok(Json(json!({
        "agentsOverriding": overriding,
        "defaultEffort": state.orchestrator.default_effort().await,
        "levels": [
            { "id": "low",    "label": "Low",     "blurb": "Fast and cheap. Fine for mechanical edits." },
            { "id": "medium", "label": "Medium",  "blurb": "A balance. Sensible for most work." },
            { "id": "high",   "label": "High",    "blurb": "Thinks before acting. Worth it for design and debugging." },
            { "id": "xhigh",  "label": "Extra high", "blurb": "Slower and dearer; for genuinely hard problems." },
            { "id": "max",    "label": "Maximum", "blurb": "As much as the CLI will give. Expect long waits." }
        ]
    })))
}

async fn set_effort(
    State(state): State<AppState>,
    _admin: Admin,
    Json(body): Json<EffortBody>,
) -> Result<Json<Value>, ApiError> {
    state
        .orchestrator
        .set_default_effort(body.default_effort)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "defaultEffort": body.default_effort })))
}

async fn set_permissions(
    State(state): State<AppState>,
    _admin: Admin,
    Json(body): Json<PermissionsBody>,
) -> Result<Json<Value>, ApiError> {
    state
        .orchestrator
        .set_default_permission_mode(body.default_mode)
        .await
        .map_err(internal)?;
    // Note this is only the *default* for new cards. "Don't ask" still needs
    // the per-project opt-in, and the orchestrator downgrades it outside an
    // eren-managed worktree regardless of what is stored here.
    Ok(Json(json!({ "defaultMode": body.default_mode })))
}

/// Clear every agent's own preset so they all follow the workspace default.
///
/// Without this, changing the default appears to do nothing: most cards have
/// an agent, and an agent's preset wins. Rather than silently rewriting
/// agents when the default changes — which would quietly widen what a
/// carefully-configured agent may do — this is an explicit action the user
/// takes once.
async fn apply_to_agents(
    State(state): State<AppState>,
    _admin: Admin,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    super::require_write(&headers, "this clears every agent's own permission preset")?;
    let cleared = sqlx::query(
        "UPDATE agents SET permission_preset = NULL WHERE permission_preset IS NOT NULL",
    )
    .execute(&state.db.pool)
    .await
    .map_err(internal)?
    .rows_affected();
    Ok(Json(json!({ "cleared": cleared })))
}

/// What Eren does about runs that stop showing signs of life. See
/// `eren_core::reaper`.
async fn get_unattended(State(state): State<AppState>, _caller: Caller) -> Json<Value> {
    let u = eren_core::reaper::load(&state.db).await;
    Json(json!({ "unattended": u, "maxSilenceMinutes": eren_core::reaper::MAX_SILENCE_MINUTES }))
}

pub(crate) async fn set_unattended(
    State(state): State<AppState>,
    Admin(caller): Admin,
    headers: HeaderMap,
    Json(body): Json<eren_core::reaper::Unattended>,
) -> Result<Json<Value>, ApiError> {
    super::require_write(&headers, "this decides when Eren stops and restarts runs")?;
    eren_core::revisions::keep(
        &state.db,
        eren_core::revisions::EntityKind::Unattended,
        "unattended",
    )
    .await;
    eren_core::reaper::save(&state.db, body)
        .await
        .map_err(internal)?;
    Ok(get_unattended(State(state), caller).await)
}

/// The most dangerous write in the app.
///
/// The stored value is a shell command this server will execute, so anything
/// that can reach this endpoint has remote code execution. It carries the same
/// header gate every dashboard write carries — see `super::require_write`.
///
/// Read by every account, since the events and the window shape what each
/// person's runs do; the command itself — often a webhook URL with a token in
/// it — is the admin's, like its revision history, and is withheld from
/// everyone else.
async fn get_attention(State(state): State<AppState>, caller: Caller) -> Json<Value> {
    let a = eren_core::attention::load(&state.db).await;
    Json(attention_json(&a, None, caller.is_admin()))
}

#[derive(Deserialize)]
pub(crate) struct AttentionBody {
    enabled: Option<bool>,
    command: Option<String>,
    events: Option<Vec<String>>,
    hook_timeout_secs: Option<i64>,
    wait_secs: Option<i64>,
}

pub(crate) async fn set_attention(
    State(state): State<AppState>,
    _admin: Admin,
    headers: HeaderMap,
    Json(body): Json<AttentionBody>,
) -> Result<Json<Value>, ApiError> {
    super::require_write(
        &headers,
        "this endpoint stores a command this machine will run",
    )?;
    let current = eren_core::attention::load(&state.db).await;
    let next = eren_core::attention::Attention {
        enabled: body.enabled.unwrap_or(current.enabled),
        command: body.command.unwrap_or(current.command),
        events: match body.events {
            Some(list) => list
                .iter()
                .filter_map(|e| eren_core::attention::Event::parse(e))
                .collect(),
            None => current.events,
        },
        hook_timeout_secs: body.hook_timeout_secs.unwrap_or(current.hook_timeout_secs),
        wait_secs: body.wait_secs.unwrap_or(current.wait_secs),
    };
    // Warned about rather than refused. Someone pasting a webhook URL with a
    // token in it should be told it is stored in plain text in the database,
    // not blocked from setting up their own notifications.
    let warning =
        eren_shared::looks_like_secret(&next.command).map(|f| eren_shared::secrets::refusal(&f));

    eren_core::revisions::keep(
        &state.db,
        eren_core::revisions::EntityKind::Attention,
        "attention",
    )
    .await;
    let saved = eren_core::attention::save(&state.db, next)
        .await
        .map_err(internal)?;
    Ok(Json(attention_json(&saved, warning, true)))
}

fn attention_json(
    a: &eren_core::attention::Attention,
    warning: Option<String>,
    show_command: bool,
) -> Value {
    json!({
        "enabled": a.enabled,
        "command": if show_command { Value::String(a.command.clone()) } else { Value::Null },
        "events": a.events.iter().map(|e| e.as_str()).collect::<Vec<_>>(),
        "hookTimeoutSecs": a.hook_timeout_secs,
        "waitSecs": a.wait_secs,
        "maxWaitSecs": eren_core::attention::MAX_WINDOW_SECS,
        // The names the hook will find in its environment, so the settings
        // screen can show them rather than making someone read the source.
        "envNames": eren_core::attention::ENV_NAMES,
        "warning": warning,
    })
}
