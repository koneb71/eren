//! Reach a preview by name instead of by port.
//!
//! `http://fix-the-login-a1b2c3.preview.localhost:4820` reaches the container
//! that card is running in. Two things this buys that a port cannot:
//!
//! * **The link survives.** A port is asked of the OS each time a preview
//!   starts, so a rebuild or a wake hands back a different one — the tab you
//!   left open points at nothing, or at whatever took the port meanwhile.
//! * **Previews stop sharing a cookie jar.** Cookies ignore ports, so two
//!   previews on `127.0.0.1` are one origin as far as the browser's cookie
//!   store is concerned: log into one and the other is logged in too. Distinct
//!   hostnames are what actually keeps them apart.
//!
//! The same machinery serves installed apps at `.app.localhost`, which is a
//! different thing wearing the same shape: a preview is a branch under review
//! and gets nothing, while an app may hold grants. Telling them apart is
//! `apps::host::classify`, and its confusion matrix is pinned there.
//!
//! `*.localhost` resolves to loopback without any `/etc/hosts` entry — checked
//! on this machine, not assumed — so this needs no setup and no DNS.
//!
//! ## Why this is not a hole in the dashboard's front door
//!
//! `reject_non_local_callers` refuses any request whose `Host` is not loopback,
//! which is what stops a container or a page on another origin from driving
//! Eren's API. This runs *before* that check and handles preview hosts
//! entirely, never calling into the dashboard router — so a preview hostname
//! can reach a preview and nothing else. The suffix match is on the whole host,
//! so `evil.preview.localhost.attacker.com` does not match, and the label
//! itself may not contain a dot.
//!
//! ## Who may reach one, once accounts are on
//!
//! [`crate::auth`] lets every path through on a hostname it cannot tell from
//! the dashboard's, so this module asks for itself: a preview or an app
//! answers a loopback TCP peer — this machine, where `*.localhost` resolves —
//! or a signed-in account whose workspace it is in. Anyone else gets a `401`,
//! or a `404` for someone else's, exactly as the API would answer. Without
//! this, a wide bind with accounts on served every preview and every app's
//! bridge to whoever sent the right `Host` header.

use crate::auth::Caller;
use crate::AppState;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use axum::middleware::Next;
use eren_core::apps;
use eren_core::scope::Owned;
use sqlx::Row;
use std::net::{IpAddr, SocketAddr};
use uuid::Uuid;

/// Headers that describe one hop and must not be copied to the next.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// A `Set-Cookie` line with any `Domain` attribute removed.
///
/// Cookies ignore ports but they do not ignore domains, and a preview that
/// sends `Domain=localhost` has its cookie sent to *every other preview* and to
/// the dashboard — which is the exact sharing this module's distinct hostnames
/// exist to prevent. Stripping the attribute makes the cookie host-only, so it
/// comes back to the preview that set it and nowhere else.
///
/// The first `;`-separated segment is the cookie itself, never an attribute, so
/// a cookie genuinely named `domain` survives. Everything kept is copied
/// verbatim, spacing included: this rewrites one attribute, it does not
/// normalise a header the browser is about to parse.
pub fn host_only(set_cookie: &str) -> String {
    let kept: Vec<&str> = set_cookie
        .split(';')
        .enumerate()
        .filter(|(i, part)| {
            let name = part.split('=').next().unwrap_or("").trim();
            !(*i > 0 && name.eq_ignore_ascii_case("domain"))
        })
        .map(|(_, part)| part)
        .collect();
    kept.join(";")
}

/// Cookies the dashboard sets, which are never a preview's business.
const OWN_COOKIES: [&str; 2] = [crate::access::COOKIE, crate::auth::COOKIE];

/// A `Cookie` header line without Eren's own cookies. They are host-only to
/// the dashboard's host, so a browser does not send them to a preview's
/// hostname today; this keeps that true of a client that does.
fn without_own_cookies(line: &str) -> String {
    line.split(';')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .filter(|c| {
            let name = c.split('=').next().unwrap_or("").trim();
            !OWN_COOKIES.contains(&name)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn forwardable(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str().to_ascii_lowercase();
        // `host` is rewritten by the client to the upstream authority; copying
        // ours would tell the container it is being served at a name it does
        // not know. `authorization` is the access token when a script sends
        // one, and a preview is an unreviewed branch's code: it gets neither
        // that nor the dashboard's cookies.
        if n == "host" || n == "authorization" || HOP_BY_HOP.contains(&n.as_str()) {
            continue;
        }
        if n == "cookie" {
            let kept = value.to_str().map(without_own_cookies).unwrap_or_default();
            if !kept.is_empty() {
                if let Ok(v) = HeaderValue::from_str(&kept) {
                    out.append(name.clone(), v);
                }
            }
            continue;
        }
        out.insert(name.clone(), value.clone());
    }
    out
}

/// Serve preview and app hostnames; hand everything else to the dashboard.
///
/// Two arms, and the difference between them is the whole point: a
/// `.preview.localhost` name is a branch under review and gets nothing but its
/// own container, while a `.app.localhost` name is an installed app and may
/// hold grants. `apps::host::classify` is what keeps them apart, and its
/// confusion matrix is pinned there.
pub async fn route_previews(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response<Body> {
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    let Some((kind, slug)) = apps::host::classify(&host) else {
        return next.run(req).await;
    };
    let slug = slug.to_string();

    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    match gate(req.extensions().get::<Caller>(), peer) {
        Gate::Pass => {}
        Gate::Refuse => return plain(StatusCode::UNAUTHORIZED, "Sign in to Eren first."),
        Gate::Owner(caller) => {
            if let Err((status, message)) = require_owner(&state, &caller, kind, &slug).await {
                return plain(status, message);
            }
        }
    }

    if kind == apps::host::HostKind::App {
        let path = req.uri().path().to_string();
        // The reserved prefix is answered here and never forwarded, so the
        // container cannot see it, cannot serve it, and cannot learn it was
        // asked for.
        match apps::host::bridge_path(&path) {
            Some(Err(apps::host::Traversal)) => {
                return plain(StatusCode::BAD_REQUEST, "that path is not allowed")
            }
            Some(Ok(segments)) => {
                let owned: Vec<String> = segments.into_iter().map(str::to_string).collect();
                return crate::app_bridge::handle(&state, &slug, owned, req).await;
            }
            None => {}
        }
        // A name Eren answers to itself never reaches an app, so the probe
        // gets an answer whether or not one exists.
        if apps::host::RESERVED.contains(&slug.as_str()) {
            return plain(
                StatusCode::NOT_FOUND,
                format!("\"{slug}\" is a name Eren keeps for itself."),
            );
        }
    }

    match proxy(&state, kind, &slug, req).await {
        Ok(response) => response,
        Err(message) => plain(StatusCode::BAD_GATEWAY, message),
    }
}

/// What [`route_previews`] must check before it answers.
#[derive(Debug)]
enum Gate {
    Pass,
    /// A signed-in account: only what lives in a workspace it owns.
    Owner(Caller),
    Refuse,
}

/// Decided by the TCP peer, never a header. With accounts off,
/// [`crate::auth`] marks every request [`Caller::Local`] and the access token
/// has already decided who got this far.
fn gate(caller: Option<&Caller>, peer: Option<IpAddr>) -> Gate {
    if peer.is_some_and(|ip| ip.to_canonical().is_loopback()) {
        return Gate::Pass;
    }
    match caller {
        Some(Caller::Local) => Gate::Pass,
        Some(c @ Caller::User(_)) => Gate::Owner(c.clone()),
        Some(Caller::Anonymous) | None => Gate::Refuse,
    }
}

/// Refuse unless the preview or app at `slug` is in one of the caller's
/// workspaces. A name that matches nothing passes, and [`proxy`] then says
/// nothing is running there — the same words someone else's gets.
async fn require_owner(
    state: &AppState,
    caller: &Caller,
    kind: apps::host::HostKind,
    slug: &str,
) -> Result<(), (StatusCode, String)> {
    let (sql, owned): (&str, fn(Uuid) -> Owned) = match kind {
        apps::host::HostKind::Preview => (
            "SELECT id FROM previews WHERE slug = $1 ORDER BY created_at DESC LIMIT 1",
            Owned::Preview,
        ),
        apps::host::HostKind::App => ("SELECT id FROM apps WHERE slug = $1", Owned::App),
    };
    let id: Option<Uuid> = sqlx::query_scalar(sql)
        .bind(slug)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match id {
        Some(id) => caller.require(state, owned(id)).await.map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                format!("Nothing is running at \"{slug}\"."),
            )
        }),
        None => Ok(()),
    }
}

async fn proxy(
    state: &AppState,
    kind: apps::host::HostKind,
    slug: &str,
    req: Request<Body>,
) -> Result<Response<Body>, String> {
    // Only a *running* container answers. One that is idle or stopped gets a
    // page saying so rather than a connection refused, which is the difference
    // between "it went to sleep" and "eren is broken".
    //
    // A preview is found by its own slug; an app by the slug on the `apps` row,
    // whose live container is the project's base preview. Keeping the app's
    // name off the preview row is what makes it survive a rebuild.
    let row = match kind {
        apps::host::HostKind::Preview => {
            sqlx::query(
                "SELECT status, host_port FROM previews WHERE slug = $1
              ORDER BY created_at DESC LIMIT 1",
            )
            .bind(slug)
            .fetch_optional(&state.db.pool)
            .await
        }
        apps::host::HostKind::App => {
            sqlx::query(
                "SELECT v.status, v.host_port
               FROM apps a
               JOIN previews v ON v.project_id = a.project_id AND v.task_id IS NULL
              WHERE a.slug = $1
              ORDER BY v.created_at DESC LIMIT 1",
            )
            .bind(slug)
            .fetch_optional(&state.db.pool)
            .await
        }
    }
    .map_err(|e| format!("could not look up this address: {e}"))?;

    let thing = match kind {
        apps::host::HostKind::Preview => "preview",
        apps::host::HostKind::App => "app",
    };
    let Some(row) = row else {
        return Ok(plain(
            StatusCode::NOT_FOUND,
            format!("Nothing is running at \"{slug}\". The {thing} may have been stopped."),
        ));
    };
    let status: String = row.get("status");
    let Some(port) = row
        .get::<Option<i32>, _>("host_port")
        .filter(|_| status == "running")
    else {
        return Ok(plain(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "This {thing} is {status}. Open it in Eren and start it again — \
                 if its image is still here that takes a few seconds."
            ),
        ));
    };

    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    // Not always 127.0.0.1: a containerised Eren reaches the host's
    // previews by name. See `previews::docker::dialled`.
    let url = format!(
        "http://{}{path}",
        eren_core::previews::docker::dialled(port as u16)
    );
    let method = req.method().clone();
    let headers = forwardable(req.headers());

    // Buffered rather than streamed. A preview serves pages and assets, and
    // the simplicity is worth more here than the memory would be — noted
    // rather than hidden, because a preview that streams a large download
    // would hold it all in memory on the way through.
    let body = axum::body::to_bytes(req.into_body(), 32 * 1024 * 1024)
        .await
        .map_err(|_| "the request body was too large to proxy".to_string())?;

    let upstream = reqwest::Client::new()
        .request(method, &url)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|e| format!("This preview's container is not answering on port {port}. ({e})"))?;

    let mut out = Response::builder().status(upstream.status());
    for (name, value) in upstream.headers() {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) {
            continue;
        }
        // An app is *meant* to be embedded — that is the whole feature — so a
        // framework helpfully sending `X-Frame-Options: DENY` would leave the
        // gallery showing a blank box. Dropped for apps only; a preview keeps
        // whatever it sent, because nothing embeds a preview.
        if kind == apps::host::HostKind::App && lower == "x-frame-options" {
            continue;
        }
        // The one header rewritten on the way back. A `Domain` attribute would
        // hand this preview's cookie to every other one and to the dashboard.
        if lower == "set-cookie" {
            if let Ok(text) = value.to_str() {
                if let Ok(fixed) = HeaderValue::from_str(&host_only(text)) {
                    out = out.header(name, fixed);
                    continue;
                }
            }
        }
        out = out.header(name, value);
    }
    // Appended rather than replacing whatever the app sent: multiple CSP
    // headers all apply and the intersection wins, so an app with a stricter
    // policy of its own keeps it.
    if kind == apps::host::HostKind::App {
        out = out.header(
            axum::http::header::CONTENT_SECURITY_POLICY,
            crate::app_bridge::csp(),
        );
    }

    let bytes = upstream
        .bytes()
        .await
        .map_err(|e| format!("this {thing}'s response could not be read: {e}"))?;
    out.body(Body::from(bytes))
        .map_err(|e| format!("could not assemble the proxied response: {e}"))
}

fn plain(status: StatusCode, message: impl Into<String>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(
            HeaderName::from_static("content-type"),
            "text/plain; charset=utf-8",
        )
        .body(Body::from(message.into()))
        .expect("a plain-text response is always well formed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_accounts_on_only_this_machine_or_a_signed_in_owner_gets_through() {
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        let lan: IpAddr = "192.168.1.9".parse().unwrap();
        // This machine, whatever it carries: `*.localhost` resolves here.
        for peer in [loopback, mapped] {
            assert!(matches!(
                gate(Some(&Caller::Anonymous), Some(peer)),
                Gate::Pass
            ));
        }
        // Another machine with no session is refused, whatever its `Host`
        // header claims; so is a request no caller was decided for.
        assert!(matches!(
            gate(Some(&Caller::Anonymous), Some(lan)),
            Gate::Refuse
        ));
        assert!(matches!(gate(None, Some(lan)), Gate::Refuse));
        assert!(matches!(gate(Some(&Caller::Anonymous), None), Gate::Refuse));
        // Accounts off: the token already decided.
        assert!(matches!(gate(Some(&Caller::Local), Some(lan)), Gate::Pass));
    }

    #[test]
    fn strips_headers_that_describe_only_this_hop() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "card-a.preview.localhost".parse().unwrap());
        headers.insert("connection", "keep-alive".parse().unwrap());
        headers.insert("accept", "text/html".parse().unwrap());
        headers.insert("cookie", "session=abc".parse().unwrap());
        headers.insert("authorization", "Bearer the-access-token".parse().unwrap());
        let out = forwardable(&headers);
        assert!(out.get("host").is_none());
        assert!(out.get("connection").is_none());
        // The preview's own cookies must survive, or nothing with a login works.
        assert_eq!(out.get("cookie").unwrap(), "session=abc");
        assert_eq!(out.get("accept").unwrap(), "text/html");
        // The access token is the dashboard's, never a branch's.
        assert!(out.get("authorization").is_none());
    }

    /// Eren's own cookies stay on Eren's side of the proxy; the preview's
    /// own are delivered untouched.
    #[test]
    fn the_dashboards_cookies_do_not_reach_a_preview() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            "eren_access=tok; session=abc; eren_session=sess; theme=dark"
                .parse()
                .unwrap(),
        );
        let out = forwardable(&headers);
        assert_eq!(out.get("cookie").unwrap(), "session=abc; theme=dark");

        let mut only_ours = HeaderMap::new();
        only_ours.insert("cookie", "eren_session=sess".parse().unwrap());
        assert!(forwardable(&only_ours).get("cookie").is_none());
    }

    #[test]
    fn a_domain_attribute_does_not_survive_the_trip_back() {
        // The bug: `Domain=localhost` matches every subdomain, so one preview's
        // session cookie was sent to every other preview and to the dashboard.
        assert_eq!(
            host_only("session=abc; Domain=localhost; Path=/"),
            "session=abc; Path=/"
        );
        // Spelling it differently must not get past the check.
        assert_eq!(host_only("a=1; domain = .localhost"), "a=1");
        assert_eq!(
            host_only("a=1; DOMAIN=.LocalHost; HttpOnly"),
            "a=1; HttpOnly"
        );
    }

    #[test]
    fn everything_else_about_a_cookie_is_left_exactly_as_it_was() {
        // Not a normaliser: attribute order and spacing reach the browser
        // unchanged, because this rewrites one attribute and nothing else.
        let untouched = "session=abc; Path=/; Secure; SameSite=Lax; Max-Age=600";
        assert_eq!(host_only(untouched), untouched);
        assert_eq!(host_only("bare"), "bare");
        // The first segment is the cookie, never an attribute — a cookie that
        // happens to be called `domain` is a cookie, and keeps its value.
        assert_eq!(host_only("domain=abc; Path=/"), "domain=abc; Path=/");
    }
}
