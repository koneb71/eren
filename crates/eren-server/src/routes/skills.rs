//! Skills: named ways of doing something, and the harness that lets you try
//! one before it does real work.

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
use std::time::Duration;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/skills", get(list).post(create))
        .route("/skills/generate", post(generate))
        .route("/skills/{id}", patch(update).delete(remove))
        .route("/skills/{id}/try", post(try_it))
        // Installing is project-shaped even though the library is
        // workspace-shaped: the files land in one checkout, and that checkout
        // is what an agent reads.
        .route("/projects/{id}/skills/install", post(install))
}

#[derive(Deserialize)]
struct WorkspaceFilter {
    workspace_id: Uuid,
}

async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Query(q): Query<WorkspaceFilter>,
) -> Result<Json<Value>, ApiError> {
    caller
        .require(&state, Owned::Workspace(q.workspace_id))
        .await?;
    let skills = eren_core::skills::list(&state.db, q.workspace_id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "skills": skills })))
}

#[derive(Deserialize)]
pub(crate) struct SkillBody {
    workspace_id: Option<Uuid>,
    /// Create it as a personal skill — the caller's, offered in every one of
    /// their workspaces — rather than one of `workspace_id`'s. Read only on
    /// create; where a skill lives does not change afterwards.
    personal: Option<bool>,
    name: Option<String>,
    description: Option<String>,
    instructions: Option<String>,
    must_not: Option<String>,
    enabled: Option<bool>,
}

/// Everything a person types here ends up in a prompt, so it gets the same
/// check the Brain does before anything is stored.
fn no_secrets(body: &SkillBody) -> Result<(), ApiError> {
    for text in [&body.instructions, &body.must_not, &body.description] {
        if let Some(found) = text.as_deref().and_then(eren_shared::looks_like_secret) {
            return Err((
                StatusCode::BAD_REQUEST,
                eren_shared::secrets::refusal(&found),
            ));
        }
    }
    Ok(())
}

async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<SkillBody>,
) -> Result<Json<Value>, ApiError> {
    let personal = body.personal.unwrap_or(false);
    let workspace_id = match (personal, body.workspace_id) {
        (true, _) => None,
        (false, Some(ws)) => {
            caller.require(&state, Owned::Workspace(ws)).await?;
            Some(ws)
        }
        (false, None) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "workspace_id is required".to_string(),
            ))
        }
    };
    let name = body.name.as_deref().unwrap_or("").trim().to_string();
    no_secrets(&body)?;

    // One `@` namespace with agents, so this refusal has to name the reason —
    // "that already exists" would leave somebody hunting through the wrong list.
    // A personal skill is named in every workspace its owner has, so it has
    // to be free in each.
    let free = match workspace_id {
        Some(ws) => eren_core::skills::check_name_free(&state.db, ws, &name, None).await,
        None => {
            eren_core::skills::check_personal_name_free(&state.db, caller.user_id(), &name, None)
                .await
        }
    };
    if let Err(why) = free.map_err(internal)? {
        return Err((StatusCode::CONFLICT, why));
    }

    let row = sqlx::query(
        "INSERT INTO skills (workspace_id, owner_id, name, description, instructions, must_not, enabled)
         VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING id",
    )
    .bind(workspace_id)
    .bind(if personal { caller.user_id() } else { None })
    .bind(&name)
    .bind(body.description.as_deref().unwrap_or(""))
    .bind(body.instructions.as_deref().unwrap_or(""))
    .bind(body.must_not.as_deref().unwrap_or(""))
    .bind(body.enabled.unwrap_or(true))
    .fetch_one(&state.db.pool)
    .await
    .map_err(internal)?;

    let id: Uuid = sqlx::Row::get(&row, "id");
    one(&state, id).await
}

pub(crate) async fn update(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<SkillBody>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Skill(id)).await?;
    no_secrets(&body)?;

    if let Some(name) = body.name.as_deref().map(str::trim) {
        let (workspace_id, owner): (Option<Uuid>, Option<Uuid>) =
            sqlx::query_as("SELECT workspace_id, owner_id FROM skills WHERE id=$1")
                .bind(id)
                .fetch_optional(&state.db.pool)
                .await
                .map_err(internal)?
                .ok_or((StatusCode::NOT_FOUND, "no such skill".to_string()))?;
        let free = match workspace_id {
            Some(ws) => eren_core::skills::check_name_free(&state.db, ws, name, Some(id)).await,
            None => {
                eren_core::skills::check_personal_name_free(&state.db, owner, name, Some(id)).await
            }
        };
        if let Err(why) = free.map_err(internal)? {
            return Err((StatusCode::CONFLICT, why));
        }
    }

    eren_core::revisions::keep(
        &state.db,
        eren_core::revisions::EntityKind::Skill,
        &id.to_string(),
    )
    .await;

    sqlx::query(
        "UPDATE skills SET name = COALESCE($2, name),
                           description = COALESCE($3, description),
                           instructions = COALESCE($4, instructions),
                           must_not = COALESCE($5, must_not),
                           enabled = COALESCE($6, enabled),
                           updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(body.name.as_deref().map(str::trim))
    .bind(body.description.as_deref())
    .bind(body.instructions.as_deref())
    .bind(body.must_not.as_deref())
    .bind(body.enabled)
    .execute(&state.db.pool)
    .await
    .map_err(internal)?;
    one(&state, id).await
}

async fn remove(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Skill(id)).await?;
    eren_core::revisions::keep(
        &state.db,
        eren_core::revisions::EntityKind::Skill,
        &id.to_string(),
    )
    .await;
    sqlx::query("DELETE FROM skills WHERE id=$1")
        .bind(id)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct GenerateBody {
    /// What kind of job the skills are for.
    description: String,
    /// Engine id from `/api/engines`. Omitted means the machine default.
    engine: Option<String>,
    /// Which tier writes them; resolves through the person's own settings.
    model_tier: Option<ModelTier>,
}

const GENERATE_PROMPT: &str = r##"You are writing skills for a multi-agent coding platform.
A skill is a named, reusable method for ONE particular kind of job — how that job is done here,
like "how we write a database migration" or "our release checklist". It is applied only when a
person names it (@its-name), so it must be narrow: never general coding advice.

Based on the user's need below, output ONLY a JSON array of 1 to 3 skill definitions — no prose,
no markdown fences. Each element:
{"name": "kebab-case, 2-4 words, e.g. write-migration",
 "description": "one line: when to reach for this skill",
 "instructions": "the method: concrete numbered steps and the standard the result must meet, 5-15 lines",
 "must_not": "things never to do in this job, one per line, or an empty string"}
Never include credentials, tokens, or private URLs.

User's need: "##;

/// Drafts skills for a person to read, edit and save through `create`,
/// which checks the workspace and the name then. Touches no workspace and
/// saves nothing — which is why the ledger leaves it out (`audit_layer::QUIET`).
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
    // Medium by default: writing a method down is ordinary work, and the
    // person can ask for more in the wizard.
    let tier = body.model_tier.unwrap_or(ModelTier::Medium);
    let model_id = state.orchestrator.model_for(engine_id, tier);
    let prompt = format!("{GENERATE_PROMPT}{}\"", body.description.trim());
    let output = utility_run(
        engine,
        model_id,
        prompt,
        Some(ReasoningEffort::Medium),
        Duration::from_secs(180),
    )
    .await
    .map_err(internal)?;
    match extract_json(&output) {
        Ok(Value::Array(drafts)) => Ok(Json(json!({ "drafts": drafts }))),
        Ok(single @ Value::Object(_)) => Ok(Json(json!({ "drafts": [single] }))),
        _ => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("model output was not valid JSON:\n{output}"),
        )),
    }
}

async fn one(state: &AppState, id: Uuid) -> Result<Json<Value>, ApiError> {
    let skill = eren_core::skills::get(&state.db, id)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such skill".to_string()))?;
    Ok(Json(serde_json::to_value(skill).unwrap_or_default()))
}

#[derive(Deserialize)]
struct TryBody {
    /// The low-risk prompt to try it against.
    prompt: String,
    model_tier: Option<ModelTier>,
}

/// Try a skill before it does real work.
///
/// The source this feature is modelled on is blunt about it — *"a skill should
/// be tested with a simple prompt before it becomes part of real work"*, with a
/// prompt that avoids file operations, shell commands and credentials. This is
/// that, and it is safe by construction rather than by asking nicely:
/// `utility_run` has **no tools at all**, no worktree, no repository and no MCP
/// wiring, so whatever the skill says to do, there is nothing here to do it to.
/// What comes back is text, and it is shown rather than stored.
///
/// Which is also the honest limit, and the UI says so: this tells you how the
/// skill *reads*, not what it would do to your files.
async fn try_it(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<TryBody>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Skill(id)).await?;
    if body.prompt.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "give it something to try — a short, harmless request".into(),
        ));
    }
    let skill = eren_core::skills::get(&state.db, id)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such skill".to_string()))?;

    let engine_id = state.orchestrator.default_engine();
    let engine = state
        .orchestrator
        .engine(&engine_id)
        .ok_or((StatusCode::BAD_REQUEST, format!("no engine {engine_id}")))?;
    let tier = body.model_tier.unwrap_or(ModelTier::Medium);
    let model_id = state.orchestrator.model_for(&engine_id, tier);

    // Assembled exactly as a real run would assemble it — the point of a test
    // is that it exercises the same text, fence and all.
    let prompt = eren_core::skills::augment_prompt(body.prompt.trim(), Some(&skill));

    let output = utility_run(
        engine,
        model_id,
        prompt.clone(),
        Some(ReasoningEffort::Low),
        Duration::from_secs(120),
    )
    .await
    .map_err(internal)?;

    Ok(Json(json!({
        "output": output,
        // Shown alongside, because half of what a test tells you is whether the
        // skill says what you thought it said.
        "prompt": prompt,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallBody {
    /// `owner/repo`, a GitHub URL, or a skills.sh page. Normalised and
    /// refused in the core rather than passed on for the installer to fail at.
    reference: String,
}

/// Install a skill from a registry into this project, and mirror it.
///
/// Slow on purpose — `npx` fetches a package and then a repository, which
/// takes as long as it takes. The button says so rather than the request
/// pretending to be quick and the person pressing it twice.
async fn install(
    State(state): State<AppState>,
    caller: Caller,
    Path(project_id): Path<Uuid>,
    Json(body): Json<InstallBody>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(project_id)).await?;
    let out = eren_core::skills::install::install(&state.db, project_id, &body.reference)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::to_value(out).map_err(internal)?))
}
