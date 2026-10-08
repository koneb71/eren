//! Hand-rolled MCP-over-HTTP endpoint implementing exactly what Claude
//! Code's `--permission-prompt-tool mcp__eren__approve` needs: initialize,
//! tools/list, and tools/call for a single `approve` tool. When the engine
//! asks for permission, the call parks in the PermissionBroker until the
//! user answers in the dashboard.

pub mod chat_tools;
pub mod org_tools;
pub mod run_tools;

use crate::AppState;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use eren_core::runs::permissions::Decision;
use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};
use uuid::Uuid;

/// Whether a peer is this machine: the only place an agent CLI ever calls from.
pub(crate) fn from_this_machine(peer: Option<IpAddr>) -> bool {
    peer.is_some_and(|ip| ip.to_canonical().is_loopback())
}

/// The MCP endpoints answer loopback peers and nobody else, whatever the
/// account state or the token. Their callers are identified by the run id in
/// the URL alone — there is no `Caller` here — and the spawned CLIs that hold
/// those ids are always on this machine. A signed-in account or a token
/// holder on another machine has no business here, and with a run id it could
/// park a prompt that stops that run or answer a review ahead of its reviewer.
pub async fn this_machine_only(req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    if from_this_machine(peer) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            "Eren's MCP endpoints answer this machine only.",
        )
            .into_response()
    }
}

pub fn mcp_router() -> Router<AppState> {
    Router::new()
        .route("/run/{run_id}", post(rpc))
        .route("/chat/{chat_id}/{run_id}", post(chat_tools::rpc))
        .route("/org/{run_id}/{step_id}", post(org_tools::rpc))
}

async fn rpc(
    State(state): State<AppState>,
    Path(run_id): Path<Uuid>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");

    // Notifications (no id) just get acknowledged.
    let Some(id) = id else {
        return (StatusCode::ACCEPTED, Json(Value::Null));
    };

    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "eren", "version": env!("CARGO_PKG_VERSION") }
        }),
        "ping" => json!({}),
        "tools/list" => {
            let mut tools = vec![json!({
                "name": "approve",
                "description": "Ask the Eren dashboard user to approve a tool call.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "tool_name": { "type": "string" },
                        "input": { "type": "object" },
                        "tool_use_id": { "type": "string" }
                    },
                    "required": ["tool_name", "input"]
                }
            })];
            // A lookup failure leaves the permission prompt alone: the run
            // can still ask, it just has nothing else from us.
            if let Ok(ctx) = run_tools::context(&state, run_id).await {
                tools.extend(run_tools::tools(&ctx));
            }
            json!({ "tools": tools })
        }
        "tools/call"
            if !matches!(
                req.pointer("/params/name").and_then(Value::as_str),
                None | Some("approve")
            ) =>
        {
            let name = req
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            let outcome = match run_tools::context(&state, run_id).await {
                Ok(ctx) => run_tools::call(&state, run_id, &ctx, name, args).await,
                Err(e) => Err(e),
            };
            log_tool(&state, run_id, name, &outcome);
            match outcome {
                Ok(payload) => {
                    json!({ "content": [{ "type": "text", "text": payload.to_string() }] })
                }
                Err(message) => json!({
                    "content": [{ "type": "text", "text": json!({ "error": message }).to_string() }],
                    "isError": true
                }),
            }
        }
        "tools/call" => {
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            let tool_name = args
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let input = args.get("input").cloned().unwrap_or(json!({}));
            let tool_name_for_log = tool_name.clone();

            // Eren's own toolbox is asked about by name and let through:
            // each tool in it was built to be safe for any run to call, and a
            // person asked to approve "comment" every time would learn to
            // click Allow without reading.
            let decision = if run_tools::is_own(&tool_name) {
                Decision::Allowed
            } else {
                state
                    .permissions
                    .request(run_id, tool_name, input.clone())
                    .await
            };

            if !run_tools::is_own(&tool_name_for_log) {
                let e = eren_core::audit::Entry::new(
                    eren_core::audit::Actor::Agent(run_id),
                    format!("permission {tool_name_for_log}"),
                )
                .on("runs", run_id)
                .summary(format!(
                    "asked to use {tool_name_for_log} → {}",
                    match &decision {
                        Decision::Allowed => "allowed",
                        Decision::Denied => "denied",
                        Decision::Unanswered { .. } => "unanswered",
                        Decision::RunGone => "run gone",
                    }
                ));
                let db = state.db.clone();
                tokio::spawn(async move { eren_core::audit::record(&db, e).await });
            }

            // The permission-prompt-tool contract: content[0].text is a
            // JSON-encoded {behavior, updatedInput|message}.
            let payload = match &decision {
                Decision::Allowed => json!({ "behavior": "allow", "updatedInput": input }),
                other => json!({ "behavior": "deny", "message": refusal(other) }),
            };
            json!({
                "content": [{ "type": "text", "text": payload.to_string() }]
            })
        }
        _ => {
            return (
                StatusCode::OK,
                Json(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {method}") }
                })),
            );
        }
    };

    (
        StatusCode::OK,
        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
    )
}

/// An agent's tool call, into the ledger: the tool's name and whether it was
/// refused — never its input, which can carry anything the model wrote.
pub(crate) fn log_tool(
    state: &AppState,
    run_id: Uuid,
    tool: &str,
    outcome: &Result<Value, String>,
) {
    let summary = match outcome {
        Ok(_) => format!("{tool} → ok"),
        Err(e) => format!(
            "{tool} → refused: {}",
            e.chars().take(120).collect::<String>()
        ),
    };
    let e = eren_core::audit::Entry::new(
        eren_core::audit::Actor::Agent(run_id),
        format!("tool {tool}"),
    )
    .on("runs", run_id)
    .summary(summary);
    let db = state.db.clone();
    tokio::spawn(async move { eren_core::audit::record(&db, e).await });
}

/// Refuse a tool call from a run that is over, or that is not the one the
/// URL pairs it with. A CLI can outlive its run — stopped, orphaned by a
/// restart — and nothing it asks for afterwards should happen.
pub(crate) async fn still_running(
    state: &AppState,
    run_id: Uuid,
    belongs: &str,
    owner: Uuid,
) -> Result<(), String> {
    // `belongs` is a literal from the call sites, never request text.
    let status: Option<String> = sqlx::query_scalar(&format!(
        "SELECT status FROM runs WHERE id = $1 AND {belongs} = $2"
    ))
    .bind(run_id)
    .bind(owner)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(|e| e.to_string())?;
    match status.as_deref() {
        None => Err("this run is not part of that conversation".into()),
        Some("completed" | "failed" | "canceled") => {
            Err("this run has ended — its tools are closed".into())
        }
        Some(_) => Ok(()),
    }
}

/// What to say when the answer is not "allow".
///
/// The wire protocol has only allow and deny, so everything here travels as a
/// denial — but only one of these is a person refusing. An engine told it was
/// refused works around the refusal and spends real money doing it, so the
/// message is where the difference has to survive. `Unanswered` and `RunGone`
/// both say plainly that the run is over, because the broker has already
/// stopped it and any further work would be thrown away.
fn refusal(decision: &Decision) -> &'static str {
    match decision {
        // Unreachable: the caller matches Allowed before getting here.
        Decision::Allowed => "allowed",
        Decision::Denied => "denied by the Eren user",
        Decision::Unanswered { .. } => {
            "nobody answered this request, so Eren stopped the run. \
             This is not a refusal — no one saw it. Do not work around it; stop here."
        }
        Decision::RunGone => "this run was cancelled while the request was outstanding; stop here.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn only_a_loopback_peer_is_this_machine() {
        for local in ["127.0.0.1", "127.0.0.2", "::1", "::ffff:127.0.0.1"] {
            assert!(from_this_machine(Some(local.parse().unwrap())), "{local}");
        }
        for away in [
            "192.168.1.9",
            "172.17.0.1",
            "10.0.0.2",
            "::ffff:192.168.1.9",
        ] {
            assert!(!from_this_machine(Some(away.parse().unwrap())), "{away}");
        }
        // No peer at all fails closed.
        assert!(!from_this_machine(None));
    }

    #[test]
    fn the_engine_is_never_told_a_person_refused_something_nobody_saw() {
        let unanswered = refusal(&Decision::Unanswered {
            waited: Duration::from_secs(86_400),
        });
        // The exact sentence this replaced was "denied by Eren user", which
        // described a decision that had not happened.
        assert!(!unanswered.contains("denied"), "{unanswered}");
        assert!(unanswered.contains("not a refusal"), "{unanswered}");

        assert!(refusal(&Decision::Denied).contains("denied"));
        assert!(refusal(&Decision::RunGone).contains("cancelled"));
    }

    #[test]
    fn every_ending_tells_the_engine_something_different() {
        let all = [
            refusal(&Decision::Denied),
            refusal(&Decision::Unanswered {
                waited: Duration::from_secs(1),
            }),
            refusal(&Decision::RunGone),
        ];
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len(), "two endings read the same");
    }
}
