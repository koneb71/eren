//! Your rules for agents: read and saved here, written into each new
//! repository project as AGENTS.md and CLAUDE.md (see `eren_core::rules`).
//!
//! Per person: a signed-in account reads and writes its own; with accounts off
//! there is one local person, and these are theirs.

use super::{internal, require_write, ApiError};
use crate::auth::Caller;
use crate::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn router() -> Router<AppState> {
    Router::new().route("/rules", get(read).put(save))
}

async fn read(State(state): State<AppState>, caller: Caller) -> Result<Json<Value>, ApiError> {
    let text = eren_core::rules::get(&state.db, caller.user_id())
        .await
        .map_err(internal)?;
    Ok(Json(
        json!({ "text": text, "maxChars": eren_core::rules::MAX_CHARS }),
    ))
}

#[derive(Deserialize)]
struct RulesBody {
    text: String,
}

async fn save(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Json(body): Json<RulesBody>,
) -> Result<Json<Value>, ApiError> {
    require_write(&headers, "your rules are committed into every new project")?;
    // Committed into repositories, and from there pushed: the one place a
    // pasted token would travel furthest.
    if let Some(found) = eren_shared::looks_like_secret(&body.text) {
        return Err((
            StatusCode::BAD_REQUEST,
            eren_shared::secrets::refusal(&found),
        ));
    }
    eren_core::rules::set(&state.db, caller.user_id(), &body.text)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(json!({ "saved": true })))
}
