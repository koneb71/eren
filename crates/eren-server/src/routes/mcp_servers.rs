//! Managing the MCP servers a user connects.
//!
//! The interesting endpoint is `/test`: an MCP server that is misconfigured
//! fails silently inside a run, forty minutes in, as an agent that quietly
//! never used the tool. Checking at the point of configuration is the only
//! place the feedback is cheap.

use super::{internal, require_write, ApiError};
use crate::auth::Caller;
use crate::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use eren_core::mcp_servers::{check_env, masked_headers, merge_masked_headers, slug_name, MASK};
use eren_core::scope::Owned;
use eren_shared::env_guard;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mcp-servers", get(list).post(create))
        .route("/mcp-servers/{id}", patch(update).delete(remove))
        .route("/mcp-servers/{id}/test", post(test))
        .route(
            "/agents/{id}/mcp-servers",
            get(for_agent).put(set_for_agent),
        )
}

const TRANSPORTS: &[&str] = &["stdio", "http", "sse"];

#[derive(Deserialize)]
struct ListQuery {
    workspace_id: Uuid,
}

#[derive(Deserialize)]
struct ServerBody {
    workspace_id: Option<Uuid>,
    name: Option<String>,
    transport: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<Value>,
    url: Option<String>,
    headers: Option<Value>,
    enabled: Option<bool>,
}

fn bad(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn server_json(r: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": r.get::<Uuid, _>("id"),
        "name": r.get::<String, _>("name"),
        "transport": r.get::<String, _>("transport"),
        "command": r.get::<Option<String>, _>("command"),
        "args": r.get::<Vec<String>, _>("args"),
        "env": r.get::<Value, _>("env"),
        "url": r.get::<Option<String>, _>("url"),
        // Names only: the values are a server's credential, stored for the
        // run and never returned. `env` is shown as it is — its keys are
        // refused when auth-shaped, and the dashboard edits it as text.
        "headers": masked_headers(&r.get::<Value, _>("headers")),
        "enabled": r.get::<bool, _>("enabled"),
        // What the model will see its tools called, so the UI can show it
        // rather than leaving the naming rule to be discovered.
        "toolPrefix": format!("mcp__{}", r.get::<String, _>("name")),
    })
}

const COLUMNS: &str = "id, name, transport, command, args, env, url, headers, enabled";

/// Whether any header value in `headers` is the mask.
fn sent_a_mask(headers: Option<&Value>) -> bool {
    headers
        .and_then(Value::as_object)
        .is_some_and(|m| m.values().any(|v| v.as_str() == Some(MASK)))
}

async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    caller
        .require(&state, Owned::Workspace(q.workspace_id))
        .await?;
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM mcp_servers WHERE workspace_id = $1 ORDER BY name"
    ))
    .bind(q.workspace_id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;
    Ok(Json(
        json!({ "servers": rows.iter().map(server_json).collect::<Vec<_>>() }),
    ))
}

/// Validate the parts a run can't recover from: a name that would break the
/// tool namespace, a transport we don't write, and the auth env vars that
/// this app exists to never touch.
fn validate(body: &ServerBody) -> Result<(String, String), ApiError> {
    let name = slug_name(body.name.as_deref().unwrap_or(""));
    if name.is_empty() {
        return Err(bad("a name is required (letters, digits and underscores)"));
    }
    let transport = body.transport.clone().unwrap_or_else(|| "stdio".into());
    if !TRANSPORTS.contains(&transport.as_str()) {
        return Err(bad(format!(
            "transport must be one of {}",
            TRANSPORTS.join(", ")
        )));
    }
    if transport == "stdio" {
        if body.command.as_deref().unwrap_or("").trim().is_empty() {
            return Err(bad("a stdio server needs a command to run"));
        }
    } else if body.url.as_deref().unwrap_or("").trim().is_empty() {
        return Err(bad(format!("an {transport} server needs a url")));
    }
    if let Some(env) = &body.env {
        check_env(env).map_err(|e| bad(e.to_string()))?;
    }
    Ok((name, transport))
}

async fn create(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Json(body): Json<ServerBody>,
) -> Result<Json<Value>, ApiError> {
    require_write(&headers, "this stores a command this machine will run")?;
    let workspace_id = body
        .workspace_id
        .ok_or_else(|| bad("workspace_id is required"))?;
    caller
        .require(&state, Owned::Workspace(workspace_id))
        .await?;
    let (name, transport) = validate(&body)?;
    if sent_a_mask(body.headers.as_ref()) {
        return Err(bad(format!(
            "a header value cannot be {MASK}: there is nothing stored yet to keep"
        )));
    }

    let row = sqlx::query(&format!(
        "INSERT INTO mcp_servers (workspace_id, name, transport, command, args, env, url, headers)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING {COLUMNS}"
    ))
    .bind(workspace_id)
    .bind(&name)
    .bind(&transport)
    .bind(body.command.as_deref().map(str::trim))
    .bind(body.args.clone().unwrap_or_default())
    .bind(body.env.clone().unwrap_or_else(|| json!({})))
    .bind(body.url.as_deref().map(str::trim))
    .bind(body.headers.clone().unwrap_or_else(|| json!({})))
    .fetch_one(&state.db.pool)
    .await
    .map_err(|e| {
        if e.to_string().contains("mcp_servers_workspace_id_name_key") {
            bad(format!(
                "a server called \"{name}\" already exists in this workspace"
            ))
        } else {
            internal(e)
        }
    })?;

    Ok(Json(server_json(&row)))
}

async fn update(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<ServerBody>,
) -> Result<Json<Value>, ApiError> {
    require_write(&headers, "this changes a command this machine will run")?;
    caller.require(&state, Owned::McpServer(id)).await?;
    if let Some(env) = &body.env {
        check_env(env).map_err(|e| bad(e.to_string()))?;
    }
    // A sent header equal to the mask means "keep what is stored under that
    // name" — the form a reader was given — so the stored row is read first.
    let headers = match body.headers.clone() {
        Some(sent) => {
            let stored: Option<Value> =
                sqlx::query_scalar("SELECT headers FROM mcp_servers WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&state.db.pool)
                    .await
                    .map_err(internal)?;
            Some(merge_masked_headers(
                sent,
                &stored.unwrap_or_else(|| json!({})),
            ))
        }
        None => None,
    };
    // COALESCE so a patch of one field doesn't blank the rest. `name` is
    // slugged only when present; an empty slug would silently rename the
    // server to nothing and orphan every agent's tool prefix.
    let name = body
        .name
        .as_deref()
        .map(slug_name)
        .filter(|n| !n.is_empty());

    let row = sqlx::query(&format!(
        "UPDATE mcp_servers SET
             name      = COALESCE($2, name),
             transport = COALESCE($3, transport),
             command   = COALESCE($4, command),
             args      = COALESCE($5, args),
             env       = COALESCE($6, env),
             url       = COALESCE($7, url),
             headers   = COALESCE($8, headers),
             enabled   = COALESCE($9, enabled)
         WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(name)
    .bind(
        body.transport
            .as_ref()
            .filter(|t| TRANSPORTS.contains(&t.as_str())),
    )
    .bind(body.command.as_deref().map(str::trim))
    .bind(body.args.clone())
    .bind(body.env.clone())
    .bind(body.url.as_deref().map(str::trim))
    .bind(headers)
    .bind(body.enabled)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "no such server".to_string()))?;

    Ok(Json(server_json(&row)))
}

async fn remove(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_write(&headers, "this removes a server agents are launched with")?;
    caller.require(&state, Owned::McpServer(id)).await?;
    sqlx::query("DELETE FROM mcp_servers WHERE id = $1")
        .bind(id)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "deleted": true })))
}

/// Ask the server to list its tools, and report what came back.
///
/// This speaks MCP directly rather than going through the CLI: the question
/// is whether *this configuration* connects, and a wrong answer wrapped in a
/// failed agent run is far harder to read.
async fn test(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_write(&headers, "this runs the server's command on this machine")?;
    caller.require(&state, Owned::McpServer(id)).await?;
    let row = sqlx::query(&format!("SELECT {COLUMNS} FROM mcp_servers WHERE id = $1"))
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such server".to_string()))?;

    let transport: String = row.get("transport");
    let name: String = row.get("name");
    let result = match transport.as_str() {
        "stdio" => {
            probe_stdio(
                row.get::<Option<String>, _>("command").unwrap_or_default(),
                row.get::<Vec<String>, _>("args"),
                row.get::<Value, _>("env"),
            )
            .await
        }
        _ => {
            probe_http(
                row.get::<Option<String>, _>("url").unwrap_or_default(),
                row.get::<Value, _>("headers"),
            )
            .await
        }
    };

    Ok(Json(match result {
        Ok(tools) => json!({
            "ok": true,
            "tools": tools,
            "toolPrefix": format!("mcp__{name}"),
        }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }))
}

/// Minimal MCP handshake over stdio: initialize, then tools/list.
async fn probe_stdio(
    command: String,
    args: Vec<String>,
    env: Value,
) -> anyhow::Result<Vec<String>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    check_env(&env)?;
    let mut child = env_guard::command(&command)
        .args(&args)
        .envs(
            env.as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not start `{command}`: {e}"))?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");

    for msg in [
        initialize_request(),
        initialized_notification(),
        tools_request(),
    ] {
        stdin.write_all(format!("{msg}\n").as_bytes()).await?;
    }
    stdin.flush().await?;

    // Read until the tools/list reply or the server gives up on us.
    let deadline = tokio::time::Duration::from_secs(20);
    let read = async {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            if let Some(tools) = tools_from_reply(&line) {
                return Ok(tools);
            }
        }
        Err(anyhow::anyhow!(
            "the server closed without listing its tools"
        ))
    };
    let tools = tokio::time::timeout(deadline, read)
        .await
        .map_err(|_| anyhow::anyhow!("no response within 20s"))??;
    let _ = child.kill().await;
    Ok(tools)
}

/// Streamable-HTTP MCP: one POST carrying the same handshake.
async fn probe_http(url: String, headers: Value) -> anyhow::Result<Vec<String>> {
    let client = reqwest::Client::new();
    let mut req = client
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(map) = headers.as_object() {
        for (k, v) in map {
            if let Some(v) = v.as_str() {
                req = req.header(k.as_str(), v);
            }
        }
    }
    let body = client
        .execute(
            req.body(format!("{}\n", initialize_request()))
                .build()
                .map_err(|e| anyhow::anyhow!("bad request: {e}"))?,
        )
        .await
        .map_err(|e| anyhow::anyhow!("could not reach {url}: {e}"))?;

    if !body.status().is_success() {
        anyhow::bail!("{url} answered {}", body.status());
    }
    // A successful initialize is the useful signal; tool discovery over HTTP
    // needs the session id the server just issued, which is more ceremony
    // than a connectivity check warrants.
    Ok(vec![])
}

fn initialize_request() -> String {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "eren", "version": env!("CARGO_PKG_VERSION") }
        }
    })
    .to_string()
}

fn initialized_notification() -> String {
    json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string()
}

fn tools_request() -> String {
    json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }).to_string()
}

/// Pull tool names out of a `tools/list` reply, ignoring anything else the
/// server says on the way past.
fn tools_from_reply(line: &str) -> Option<Vec<String>> {
    let msg: Value = serde_json::from_str(line.trim()).ok()?;
    if msg.get("id")? != 2 {
        return None;
    }
    Some(
        msg.get("result")?
            .get("tools")?
            .as_array()?
            .iter()
            .filter_map(|t| t.get("name")?.as_str().map(str::to_string))
            .collect(),
    )
}

async fn for_agent(
    State(state): State<AppState>,
    caller: Caller,
    Path(agent_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Agent(agent_id)).await?;
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT server_id FROM agent_mcp_servers WHERE agent_id = $1")
            .bind(agent_id)
            .fetch_all(&state.db.pool)
            .await
            .map_err(internal)?;
    Ok(Json(json!({ "serverIds": ids })))
}

#[derive(Deserialize)]
struct AgentServers {
    server_ids: Vec<Uuid>,
}

/// Replace the whole set for an agent — the UI edits it as a list of
/// checkboxes, and a diffing API would only invite the two to disagree.
async fn set_for_agent(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(agent_id): Path<Uuid>,
    Json(body): Json<AgentServers>,
) -> Result<Json<Value>, ApiError> {
    require_write(&headers, "this decides what an agent is launched with")?;
    caller.require(&state, Owned::Agent(agent_id)).await?;
    // Each server too: handing an agent someone else's server would run
    // their command, with their headers, inside this agent's runs.
    for id in &body.server_ids {
        caller.require(&state, Owned::McpServer(*id)).await?;
    }
    let mut tx = state.db.pool.begin().await.map_err(internal)?;
    sqlx::query("DELETE FROM agent_mcp_servers WHERE agent_id = $1")
        .bind(agent_id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    if !body.server_ids.is_empty() {
        sqlx::query(
            "INSERT INTO agent_mcp_servers (agent_id, server_id)
             SELECT $1, unnest($2::uuid[])",
        )
        .bind(agent_id)
        .bind(&body.server_ids)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    }
    tx.commit().await.map_err(internal)?;
    Ok(Json(json!({ "serverIds": body.server_ids })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_come_out_of_a_tools_list_reply() {
        let line = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[
            {"name":"browser_navigate"},{"name":"browser_click"}]}}"#;
        assert_eq!(
            tools_from_reply(line).unwrap(),
            ["browser_navigate", "browser_click"]
        );
    }

    #[test]
    fn other_traffic_on_the_wire_is_ignored() {
        // The initialize reply and any log notifications arrive first.
        assert!(tools_from_reply(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#).is_none());
        assert!(
            tools_from_reply(r#"{"jsonrpc":"2.0","method":"notifications/message"}"#).is_none()
        );
        assert!(tools_from_reply("not json at all").is_none());
    }

    #[test]
    fn a_tools_reply_with_no_tools_is_still_a_success() {
        let line = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#;
        assert_eq!(tools_from_reply(line).unwrap().len(), 0);
    }
}

#[cfg(test)]
mod mask_tests {
    use super::*;

    /// Every row a client is shown goes through `server_json`, and
    /// `server_json` masks the headers: the credential never leaves.
    #[test]
    fn what_a_client_is_shown_has_its_headers_masked() {
        let src = include_str!("mcp_servers.rs");
        let start = src.find("fn server_json(").expect("server_json exists");
        let end = src[start..]
            .find("\n}\n")
            .map(|i| start + i)
            .unwrap_or(src.len());
        let body = &src[start..end];
        assert!(
            body.contains("masked_headers("),
            "server_json returns raw headers"
        );
        // And nothing else builds a row for a client.
        assert_eq!(src.matches("\"headers\": r.get").count(), 0);
    }

    #[test]
    fn a_mask_is_recognised_wherever_it_sits() {
        assert!(sent_a_mask(Some(&json!({ "Authorization": MASK }))));
        assert!(sent_a_mask(Some(&json!({ "A": "x", "B": MASK }))));
        assert!(!sent_a_mask(Some(&json!({ "A": "x" }))));
        assert!(!sent_a_mask(Some(&json!([MASK]))));
        assert!(!sent_a_mask(None));
    }
}
