//! Every mutating `/api` request, into the ledger.
//!
//! A layer rather than a call in each handler, because there are well over a
//! hundred mutating routes and a ledger that depends on each remembering is a
//! ledger with holes. It runs after routing, so it knows the route's template
//! (`/api/tasks/{id}/merge`) and its path ids — and it never reads the body,
//! which carries prompts, file contents and the occasional secret.
//!
//! The handful of POSTs that change nothing (a search, a preview, a probe) are
//! listed in [`QUIET`], each with its reason; the test below keeps that list
//! honest.

use crate::AppState;
use axum::extract::{MatchedPath, Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use eren_core::audit::{self, Actor, Entry};
use serde_json::json;

/// POSTs that change nothing, and why. Matched against the template after
/// `/api`.
pub const QUIET: &[(&str, &str)] = &[
    (
        "/inbox/read",
        "marking an item read is not an action on anything",
    ),
    (
        "/routines/preview",
        "computes when a schedule would fire; stores nothing",
    ),
    ("/projects/{id}/map/search", "a search"),
    (
        "/agents/generate",
        "drafts an agent for the person to edit; saves nothing",
    ),
    (
        "/apps/generate",
        "drafts an app for the person to edit; saves nothing",
    ),
    (
        "/skills/generate",
        "drafts skills for the person to edit; saves nothing",
    ),
    (
        "/skills/{id}/try",
        "runs a skill against sample text; saves nothing",
    ),
    ("/mcp-servers/{id}/test", "probes a server; saves nothing"),
];

pub fn is_mutation(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

fn quiet(method: &Method, template: &str) -> bool {
    *method == Method::POST
        && QUIET
            .iter()
            .any(|(t, _)| template.strip_prefix("/api").unwrap_or(template) == *t)
}

/// What a request was done to: the first literal segment after `/api`, and the
/// value of the first path parameter. Pure, for the tests.
///
/// Both sides are taken without the `/api` prefix: inside the nested router
/// the request's path has had it stripped while the matched template keeps
/// it, and zipping the two unaligned put every id one segment off.
pub fn entity(template: &str, path: &str) -> (Option<String>, Option<String>, serde_json::Value) {
    let unprefixed = |s: &'_ str| -> String {
        s.strip_prefix("/api")
            .unwrap_or(s)
            .trim_start_matches('/')
            .to_string()
    };
    let (template, path) = (unprefixed(template), unprefixed(path));
    let t: Vec<&str> = template.split('/').collect();
    let p: Vec<&str> = path.split('/').collect();
    let kind = t
        .iter()
        .skip_while(|s| **s == "api")
        .find(|s| !s.starts_with('{'))
        .map(|s| s.to_string());
    let mut params = serde_json::Map::new();
    let mut first = None;
    for (seg, val) in t.iter().zip(p.iter()) {
        if let Some(name) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            if first.is_none() {
                first = Some(val.to_string());
            }
            params.insert(name.to_string(), json!(val));
        }
    }
    (kind, first, serde_json::Value::Object(params))
}

pub async fn record(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    if !is_mutation(&method) {
        return next.run(req).await;
    }
    let template = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    if quiet(&method, &template) {
        return next.run(req).await;
    }
    let path = req.uri().path().to_string();
    let actor = req
        .extensions()
        .get::<crate::auth::Caller>()
        .map(|c| c.actor())
        .unwrap_or(Actor::Api);
    let res = next.run(req).await;
    let status = res.status();
    let (kind, id, params) = entity(&template, &path);
    let mut e = Entry::new(actor, format!("{method} {template}"))
        .summary(format!(
            "{method} {} → {}",
            template.strip_prefix("/api").unwrap_or(&template),
            status.as_u16()
        ))
        .detail(json!({ "status": status.as_u16(), "params": params }));
    if let (Some(k), Some(i)) = (kind.as_deref(), id) {
        e = e.on(k, i);
    } else if let Some(k) = kind {
        e.entity_kind = Some(k);
    }
    // After the response is built and off the request's path: the ledger
    // must never slow or fail the thing it records.
    let db = state.db.clone();
    tokio::spawn(async move { audit::record(&db, e).await });
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_are_not_recorded_and_writes_are() {
        assert!(!is_mutation(&Method::GET));
        assert!(!is_mutation(&Method::HEAD));
        for m in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(is_mutation(&m));
        }
    }

    #[test]
    fn a_request_names_what_it_was_done_to() {
        let (kind, id, params) = entity("/api/tasks/{id}/merge", "/api/tasks/abc/merge");
        assert_eq!(
            (kind.as_deref(), id.as_deref()),
            (Some("tasks"), Some("abc"))
        );
        assert_eq!(params["id"], "abc");
        // What the layer actually sees: the nest strips `/api` from the path
        // but not from the template.
        let (kind, id, _) = entity("/api/tasks/{id}/comments", "/tasks/abc/comments");
        assert_eq!(
            (kind.as_deref(), id.as_deref()),
            (Some("tasks"), Some("abc"))
        );
        let (kind, id, _) = entity("/api/queue/pause", "/api/queue/pause");
        assert_eq!((kind.as_deref(), id), (Some("queue"), None));
    }

    #[test]
    fn only_the_listed_posts_go_unrecorded_and_each_says_why() {
        for (t, why) in QUIET {
            assert!(!why.trim().is_empty(), "{t} needs a reason");
            assert!(quiet(&Method::POST, &format!("/api{t}")));
            assert!(
                !quiet(&Method::DELETE, &format!("/api{t}")),
                "only a POST can be quiet"
            );
        }
        assert!(!quiet(&Method::POST, "/api/tasks/{id}/merge"));
    }
}
