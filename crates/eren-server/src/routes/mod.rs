pub mod activity;
pub mod agents;
pub mod apps;
pub mod attachments;
pub mod audit;
pub mod auth;
pub mod budgets;
pub mod chat;
pub mod checks;
pub mod engines;
pub mod files;
pub mod fs;
pub mod github;
pub mod goals;
pub mod inbox;
pub mod kb;
pub mod manager;
pub mod mcp_servers;
pub mod orgs;
pub mod previews;
pub mod projects;
pub mod pull_requests;
pub mod repo_map;
pub mod research;
pub mod reviews;
pub mod revisions;
pub mod routines;
pub mod rules;
pub mod search;
pub mod settings;
pub mod skills;
pub mod spaces;
pub mod spend;
pub mod tasks;
pub mod teams;
pub mod terminal;
pub mod usage;
pub mod workflows;
pub mod workspaces;

use crate::AppState;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

pub type ApiError = (StatusCode, String);

/// A run a door refused for a reason the person can act on — the card is
/// already running, a follow-up has nothing to act on, the agent is paused —
/// is a 409 that says so. Anything else is a fault.
pub fn run_refused(e: anyhow::Error) -> ApiError {
    if e.is::<eren_core::runs::orchestrator::AlreadyRunning>()
        || e.is::<eren_core::runs::follow_up::FollowUpRefusal>()
        || e.is::<eren_core::agents::Unavailable>()
        || e.is::<eren_core::budgets::OverBudget>()
        || e.is::<eren_core::runs::orchestrator::NoTools>()
        || e.is::<eren_core::runs::orchestrator::CantHonour>()
    {
        (axum::http::StatusCode::CONFLICT, e.to_string())
    } else {
        internal(e)
    }
}

/// [`run_refused`] for a door whose other failures are the caller's fault
/// (a 400) rather than a fault: a refusal still reads as a 409.
pub fn refused_or(status: StatusCode) -> impl Fn(anyhow::Error) -> ApiError {
    move |e| match run_refused(e) {
        (StatusCode::INTERNAL_SERVER_ERROR, message) => (status, message),
        refused => refused,
    }
}

/// The header a dashboard write carries. Its only job is to be un-settable by
/// a cross-origin simple request: there is no CORS layer, so a preflight for
/// it gets no `Access-Control-Allow-*` and the browser refuses to send the
/// real request. The value is not a secret and is checked against nothing.
/// Belt and braces behind the Origin check in `lib.rs`.
pub const WRITE_HEADER: &str = eren_shared::brand::WRITE_HEADER;

/// Whether a request carries the write header, under its current name or the
/// one scripts written before the rename send. Either is a header no
/// cross-origin page can set, which is all the gate asks of it.
pub fn has_write_header(headers: &axum::http::HeaderMap) -> bool {
    eren_shared::brand::WRITE_HEADERS
        .iter()
        .any(|h| headers.contains_key(*h))
}

/// Refuse a write that did not come from the dashboard. `what` says what the
/// endpoint does, so the refusal explains why it is gated.
pub fn require_write(headers: &axum::http::HeaderMap, what: &str) -> Result<(), ApiError> {
    if has_write_header(headers) {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            format!("{what}, so it needs the {WRITE_HEADER} header"),
        ))
    }
}

/// An answer to something waiting on a person, refused — said in HTTP.
pub fn answer_refused(e: eren_core::approvals::Refusal) -> ApiError {
    use eren_core::approvals::Refusal;
    match e {
        Refusal::NotFound(m) => (StatusCode::NOT_FOUND, m),
        Refusal::Conflict(m) => (StatusCode::CONFLICT, m),
        Refusal::Invalid(m) => (StatusCode::BAD_REQUEST, m),
        Refusal::Gated(e) => run_refused(e),
        Refusal::Internal(e) => internal(e),
    }
}

pub fn internal(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub fn api_router() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .merge(auth::router())
        .merge(workspaces::router())
        .merge(projects::router())
        .merge(apps::router())
        .merge(tasks::router())
        .merge(checks::router())
        .merge(reviews::router())
        .merge(goals::router())
        .merge(budgets::router())
        .merge(inbox::router())
        .merge(audit::router())
        .merge(revisions::router())
        .merge(agents::router())
        .merge(skills::router())
        .merge(teams::router())
        .merge(orgs::router())
        .merge(workflows::router())
        .merge(fs::router())
        .merge(files::router())
        .merge(attachments::router())
        .merge(search::router())
        .merge(chat::router())
        .merge(activity::router())
        .merge(mcp_servers::router())
        .merge(settings::router())
        .merge(engines::router())
        .merge(github::router())
        .merge(previews::router())
        .merge(pull_requests::router())
        .merge(usage::router())
        .merge(spend::router())
        .merge(kb::router())
        .merge(repo_map::router())
        .merge(research::router())
        .merge(manager::router())
        .merge(routines::router())
        .merge(rules::router())
        .merge(spaces::router())
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "name": eren_shared::brand::NAME }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_is_recognised_under_either_header_name() {
        // A script written before the rename sends the old name, and must
        // still be let through; a request with neither must not.
        for name in eren_shared::brand::WRITE_HEADERS {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(*name, "1".parse().unwrap());
            assert!(require_write(&headers, "x").is_ok(), "{name}");
        }
        assert!(require_write(&axum::http::HeaderMap::new(), "x").is_err());
    }

    /// Handlers reachable with no session while accounts are on — the ones
    /// `crate::auth::open_path` lets through — each with its reason.
    const PUBLIC: &[(&str, &str)] = &[
        ("health", "says the server is up and its name; nothing else"),
        ("status", "tells the sign-in page whether accounts are on"),
        ("login", "how a session starts"),
        ("signup", "how an account starts"),
        ("logout", "ends the session the cookie names, if any"),
    ];

    /// The handler names a module registers with `.route(…)`.
    fn handlers(src: &str) -> Vec<String> {
        let mut out = vec![];
        let mut rest = src;
        while let Some(at) = rest.find(".route(") {
            rest = &rest[at + ".route(".len()..];
            // The registration runs to the paren that closes `.route(`.
            let mut depth = 1;
            let end = rest
                .char_indices()
                .find(|&(_, c)| {
                    match c {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    depth == 0
                })
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            let reg = &rest[..end];
            for method in ["get(", "post(", "put(", "patch(", "delete(", "any("] {
                let mut r = reg;
                while let Some(i) = r.find(method) {
                    let before = r[..i].chars().last();
                    r = &r[i + method.len()..];
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                    let name: String = r
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                        .collect();
                    if let Some(last) = name.rsplit("::").next().filter(|n| !n.is_empty()) {
                        out.push(last.to_string());
                    }
                }
            }
        }
        out
    }

    /// Every route handler says who is calling. With accounts on, a handler
    /// that takes neither a `Caller` nor an `Admin` cannot check whose
    /// workspace it is touching — so it is a hole, whatever its query does.
    #[test]
    fn every_handler_takes_a_caller() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes");
        let mut missing = vec![];
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            // Whole files — a test module can sit between two handlers — but
            // not this one's tests, which register made-up handlers.
            let mut shipped = std::fs::read_to_string(&path).unwrap();
            if path.ends_with("routes/mod.rs") {
                shipped.truncate(shipped.find("#[cfg(test)]").unwrap_or(shipped.len()));
            }
            for name in handlers(&shipped) {
                if PUBLIC.iter().any(|(n, _)| *n == name) {
                    continue;
                }
                let Some(at) = shipped.find(&format!("fn {name}(")) else {
                    missing.push(format!(
                        "{}: {name} (not found)",
                        path.file_name().unwrap().to_string_lossy()
                    ));
                    continue;
                };
                let sig = &shipped[at..];
                let sig = &sig[..sig.find('{').unwrap_or(sig.len())];
                if !(sig.contains("Caller") || sig.contains("Admin")) {
                    missing.push(format!(
                        "{}: {name}",
                        path.file_name().unwrap().to_string_lossy()
                    ));
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "{} handlers take neither a Caller nor an Admin:\n{}",
            missing.len(),
            missing.join("\n")
        );
    }

    #[test]
    fn the_scan_finds_handlers_however_they_are_registered() {
        let src = r#"
            .route("/a/{id}", get(one).post(super::x::two).delete(three))
            .route("/b", axum::routing::post(four).layer(DefaultBodyLimit::max(MAX)))
        "#;
        assert_eq!(handlers(src), ["one", "two", "three", "four"]);
    }
}
