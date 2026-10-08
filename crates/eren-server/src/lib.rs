// `json!` expands one recursion level per key, and the board card — the widest
// object this crate builds — has outgrown the default 128.
#![recursion_limit = "256"]

pub mod access;
pub mod app_bridge;
pub mod audit_layer;
pub mod auth;
pub mod mcp;
pub mod preview_proxy;
pub mod routes;
pub mod ws;

use axum::http::{HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::Router;
use eren_core::runs::permissions::PermissionBroker;
use eren_core::{Db, EventBus, Orchestrator};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub bus: EventBus,
    pub orchestrator: Arc<Orchestrator>,
    pub permissions: PermissionBroker,
    /// Object storage for knowledge-base attachments. `None` when it isn't
    /// configured, which is a normal state: articles work without it, and the
    /// upload endpoint says so rather than failing obscurely.
    pub storage: Option<eren_core::storage::Storage>,
    /// Serializes file saves from the Files tab.
    ///
    /// A save is read-hash-compare-write, and without a lock two of them can
    /// interleave so the second lands on bytes the first never saw — exactly
    /// the clobber `baseHash` exists to prevent. Saves are human-paced, so one
    /// lock for all of them is free.
    pub file_writes: Arc<tokio::sync::Mutex<()>>,
    /// Who may reach the server from another machine: the extra host names,
    /// and the token they must present. Empty for a loopback-only server.
    pub access: Arc<access::Access>,
    /// Whether accounts are on (an admin exists), and the sign-in throttle.
    /// See [`auth`].
    pub accounts: Arc<auth::Accounts>,
}

pub fn app(state: AppState) -> Router {
    let mut router = Router::new()
        // After routing, so the ledger sees each route's template and ids.
        .nest(
            "/api",
            routes::api_router().route_layer(middleware::from_fn_with_state(
                state.clone(),
                audit_layer::record,
            )),
        )
        // The agent CLIs are always on this machine, so nothing else is
        // answered here — not a token holder, not a signed-in account.
        .nest(
            "/mcp",
            mcp::mcp_router().route_layer(middleware::from_fn(mcp::this_machine_only)),
        )
        .route("/ws", axum::routing::get(ws::ws_handler))
        // At the root, not under /api: the dashboard dials /ws/terminal/…,
        // and a WebSocket handshake that lands on the SPA fallback gets a 200
        // page instead of a 101 and dies silently.
        .merge(routes::terminal::router());
    // The capability bridge exists on app hostnames only, where the proxy
    // layer above answers it and never calls through to here. Saying so
    // explicitly, because the SPA fallback would otherwise serve `index.html`
    // with a 200 for `/__eren/me` on localhost — no capability leaked, but a
    // very confusing thing to find while probing. Under every prefix the
    // bridge answers to.
    for prefix in eren_shared::brand::BRIDGE_PREFIXES {
        router = router
            .route(&format!("/{prefix}"), axum::routing::any(no_bridge_here))
            .route(
                &format!("/{prefix}/{{*rest}}"),
                axum::routing::any(no_bridge_here),
            );
    }

    // Dashboard assets: EREN_WEB_DIST overrides; defaults to ./web/dist
    // for dev checkouts. (v1.0 embeds these in the binary via rust-embed.)
    let dist = eren_shared::brand::var("WEB_DIST").unwrap_or_else(|| "web/dist".into());
    if std::path::Path::new(&dist).join("index.html").exists() {
        let serve = tower_http::services::ServeDir::new(&dist).fallback(
            tower_http::services::ServeFile::new(std::path::Path::new(&dist).join("index.html")),
        );
        router = router.fallback_service(serve);
    }

    router
        .layer(middleware::from_fn_with_state(
            state.clone(),
            reject_non_local_callers,
        ))
        // Inside the preview proxy and outside the loopback check, so every
        // dashboard response carries it — including the 403 above — and no
        // proxied response does. A preview must stay framable; the dashboard
        // must not.
        .layer(middleware::from_fn(refuse_to_be_framed))
        // Outside the loopback check on purpose: a preview hostname is handled
        // here in full and never reaches the dashboard router, so widening what
        // `Host` values are accepted does not widen what can reach the API.
        // See `preview_proxy` for why the suffix match is safe.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            preview_proxy::route_previews,
        ))
        // Who is calling. With accounts off, everyone is the one local
        // person and the token below decides who reaches this; with accounts
        // on, this is the gate and the token steps aside.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ))
        // Outermost: from another machine, nothing — not the API, the socket,
        // a preview or an app's bridge — answers without the access token,
        // while accounts are off.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            access::require_token,
        ))
        .with_state(state)
}

async fn no_bridge_here() -> (StatusCode, &'static str) {
    (
        StatusCode::NOT_FOUND,
        "The app bridge is served on an app's own hostname, not here.",
    )
}

/// Refuse to be displayed inside anyone else's frame.
///
/// Eren has no authentication, so every control on this page acts the moment
/// it is clicked: a mid-run permission prompt's **Allow**, a Dockerfile
/// approval, a squash-merge. Nothing stopped another page from loading the
/// dashboard in an invisible iframe, positioning it under something innocuous,
/// and collecting one of those clicks — the textbook clickjack, against a UI
/// made of one-click irreversible actions.
///
/// Both headers, because they are not the same check: `frame-ancestors` is the
/// one browsers actually honour now, and `X-Frame-Options` still covers what
/// predates it. Neither reaches a preview or an app, which are *meant* to be
/// embedded — that is what the layer's position buys.
async fn refuse_to_be_framed(
    req: Request<axum::body::Body>,
    next: Next,
) -> axum::response::Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(
        axum::http::header::X_FRAME_OPTIONS,
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    res
}

/// The host part of a `Host` or `Origin` value, without the port.
///
/// Written out rather than `split(':').next()`, which the previous version used
/// and which is wrong for IPv6: `"[::1]:4820".split(':').next()` is `"["`, so
/// the `[::1]` arm of the old allowlist had never once matched.
pub(crate) fn bare_host(value: &str) -> &str {
    let authority = value.rsplit_once("://").map_or(value, |(_, rest)| rest);
    let authority = authority.split('/').next().unwrap_or("");
    match authority.strip_prefix('[') {
        // IPv6 literal: the port, if any, follows the closing bracket.
        Some(rest) => match rest.split_once(']') {
            Some((inner, _)) => {
                // Return the bracketed form, which is how a Host header spells it.
                let end = inner.len() + 2;
                &authority[..end.min(authority.len())]
            }
            None => authority,
        },
        None => authority.split(':').next().unwrap_or(""),
    }
}

/// The `host[:port]` part of a `Host` or `Origin` value, lowercased.
fn authority(value: &str) -> String {
    let authority = value.rsplit_once("://").map_or(value, |(_, rest)| rest);
    authority
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// True when a page at `origin` may talk to the server it reached as `host`.
///
/// Same-origin: the page must have been served by the very authority it is
/// calling. A loopback host is not enough on its own, because loopback is
/// where previews live — agent-written, unreviewed code published at
/// `http://127.0.0.1:{port}` — and a page there matched the old
/// port-agnostic rule and could open `/ws/terminal`, a shell on this machine.
///
/// The dev checkout still works, and not by exception: `vite dev` proxies
/// `/api` and `/ws` without `changeOrigin`, so the request arrives with
/// `Host: localhost:5173` and `Origin: http://localhost:5173` — the same
/// authority. Comparing against the `Host` rather than the port this process
/// bound also survives Docker publishing 4820 on some other host port.
///
/// A name in `EREN_ALLOWED_HOSTS` counts as this server's own, under the same
/// same-authority rule: the page at `http://192.168.1.20:4820` may call the
/// server it was served from, and nothing at any other port may.
fn origin_may_call(origin: &str, host: &str, access: &access::Access) -> bool {
    // `Origin: null` is what a sandboxed iframe and some redirect chains send.
    // It is not a local page; it is the absence of one.
    origin != "null"
        && access.allows_host(bare_host(origin))
        && authority(origin) == authority(host)
}

/// Refuse callers that are not this machine's own dashboard.
///
/// Two checks, against two different attacks.
///
/// **Host** is the DNS-rebinding defence this has always had: the server binds
/// 127.0.0.1, and it also declines to answer to a name it does not recognise —
/// loopback's, and whatever `EREN_ALLOWED_HOSTS` adds (see [`access`]).
///
/// **Origin** is new, and closes a hole that was open. Eren has no
/// authentication of any kind, so until now any page on the internet could open
/// `ws://localhost:4820/ws` and read every run's transcript — prompts, file
/// contents, costs — and could POST to the mutating endpoints that take no JSON
/// body. Verified before the fix: a WebSocket upgrade carrying
/// `Origin: https://evil.example` was answered with `101 Switching Protocols`.
///
/// A missing `Origin` is allowed, and has to be: the spawned agent CLIs call
/// `/mcp`, and `eren doctor` and any curl call `/api`, none of them browsers and
/// none of them sending one. That is not a weakness — a program that can set
/// arbitrary headers is not the attacker this check is for. The attacker here is a
/// web page, and browsers attach `Origin` to exactly the cross-origin requests
/// that matter.
async fn reject_non_local_callers(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    // Both decisions are made before `req` is handed on, so neither borrow of
    // its headers is still alive at the move.
    let (host_ok, origin_ok) = {
        let headers = req.headers();
        let str_of = |name| {
            headers
                .get(name)
                .and_then(|v: &HeaderValue| v.to_str().ok())
        };
        let host = str_of(axum::http::header::HOST).unwrap_or_default();
        (
            state.access.allows_host(bare_host(host)),
            str_of(axum::http::header::ORIGIN)
                .is_none_or(|o| origin_may_call(o, host, &state.access)),
        )
    };

    if host_ok && origin_ok {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

/// What binding to an address exposes, and whether Eren should do it.
///
/// The whole design rests on one assumption — that only this machine can reach
/// the server — and that assumption is what pays for having no login. Binding
/// anywhere but loopback spends it, unless the access token (see [`access`])
/// stands in for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    /// Loopback. Only this machine can connect; the kernel enforces it.
    Local,
    /// Reachable from the network, and every other machine needs the token.
    Protected,
    /// Reachable from the network with no token, and somebody said so on purpose.
    Network,
    /// Reachable from the network with no token, and nobody said so.
    Unacknowledged,
}

/// The variable that acknowledges what binding wide without a token means.
pub const TRUST_NETWORK: &str = "EREN_TRUST_NETWORK";
/// The variable that sets, or turns off, the access token.
pub const ACCESS_TOKEN: &str = "EREN_ACCESS_TOKEN";

/// Whether [`TRUST_NETWORK`] is set — under either spelling — to anything but
/// empty, `0`, `false`, `no` or `off`. Those spell "not set" in every `.env`
/// written by hand, and reading one of them as trust would start an
/// unauthenticated server because somebody wrote down that they did not want one.
pub fn network_trusted() -> bool {
    eren_shared::brand::var("TRUST_NETWORK").is_some_and(|v| trusted_value(&v))
}

fn trusted_value(v: &str) -> bool {
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

/// Decide what this bind address means.
///
/// ## Why an acknowledgement rather than a check on the caller
///
/// The obvious guard is "refuse callers whose peer address is not loopback",
/// and it does not work. Bound to loopback the kernel already guarantees it, so
/// the check is a no-op; bound wide, the legitimate caller is *also* remote —
/// in the shipped container the browser arrives via the Docker gateway — so the
/// check cannot tell the person who set this up from anybody else on their
/// network.
///
/// What the `Host` allowlist does and does not do matters here, because the
/// comment beside the bind used to claim it "still guards it either way". It
/// guards **browsers**: a page on the internet cannot make one send
/// `Host: localhost` to another machine. It does not guard anything else, and
/// it is not meant to — `curl -H 'Host: localhost'` from across the room sets
/// that header itself, and a missing `Origin` is deliberately allowed so the
/// spawned agent CLIs can reach `/mcp`.
///
/// So the realistic failure is not an attack, it is an accident: somebody
/// copies `EREN_BIND=0.0.0.0` to reach the dashboard from their phone and
/// does not know there is no password. This makes that a decision rather than a
/// side effect.
///
/// The token changes the answer. With it, a wide bind no longer means "anyone
/// who can reach the port", so it needs no acknowledgement; without it
/// (`EREN_ACCESS_TOKEN=off`), it means exactly that, and still does.
pub fn exposure(bind: std::net::IpAddr, acknowledged: bool, token: bool) -> Exposure {
    if bind.is_loopback() {
        // Includes 127.0.0.0/8 and ::1. `0.0.0.0` is *not* loopback: it is
        // every interface, loopback among them.
        Exposure::Local
    } else if token {
        Exposure::Protected
    } else if acknowledged {
        Exposure::Network
    } else {
        Exposure::Unacknowledged
    }
}

/// What to tell somebody who bound wide without saying so.
pub fn unacknowledged_message(bind: std::net::IpAddr) -> String {
    format!(
        "refusing to start: EREN_BIND is {bind}, which is reachable from your \
         network, and {ACCESS_TOKEN}=off turns off the access token, so there is \
         no authentication of any kind — anyone who can reach this port can read \
         every run's transcript, browse your files and start agents on your \
         machine. The Host-header check does not stop this; it only stops a web \
         page, and any other program sets that header itself.\n\n\
         If that is what you want, set {TRUST_NETWORK}=1 as well. If it is not, \
         leave {ACCESS_TOKEN} unset so other machines need the access link, or \
         leave EREN_BIND unset and reach the dashboard over an SSH tunnel."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use access::LOCAL_HOSTS;

    use std::net::IpAddr;

    #[test]
    fn only_loopback_needs_no_saying_so() {
        for local in ["127.0.0.1", "127.0.0.2", "::1"] {
            let ip: IpAddr = local.parse().unwrap();
            assert_eq!(exposure(ip, false, false), Exposure::Local, "{local}");
            // Nothing from another machine can arrive, so no token is needed.
            assert_eq!(exposure(ip, false, true), Exposure::Local, "{local}");
        }
    }

    #[test]
    fn every_address_the_network_can_reach_has_to_be_acknowledged() {
        // `0.0.0.0` is the one people reach for, and it is not loopback — it is
        // every interface, loopback among them. The shipped Dockerfile sets it.
        for wide in ["0.0.0.0", "::", "192.168.1.5", "10.0.0.7"] {
            let ip: IpAddr = wide.parse().unwrap();
            assert_eq!(
                exposure(ip, false, false),
                Exposure::Unacknowledged,
                "{wide}"
            );
            assert_eq!(exposure(ip, true, false), Exposure::Network, "{wide}");
            // The token stands in for the acknowledgement.
            assert_eq!(exposure(ip, false, true), Exposure::Protected, "{wide}");
            assert_eq!(exposure(ip, true, true), Exposure::Protected, "{wide}");
        }
    }

    /// The refusal has to say what is actually at stake, because the person
    /// reading it is about to decide whether to override it.
    #[test]
    fn the_refusal_names_the_thing_that_is_missing() {
        let message = unacknowledged_message("0.0.0.0".parse().unwrap());
        assert!(message.contains("no authentication"), "{message}");
        // And corrects the belief that used to sit next to the bind.
        assert!(
            message.contains("Host-header check does not stop this"),
            "{message}"
        );
        // Names the way out, and a safer alternative to it.
        assert!(message.contains(TRUST_NETWORK), "{message}");
        // The name in the message is the name that is read.
        assert_eq!(TRUST_NETWORK, eren_shared::brand::env_name("TRUST_NETWORK"));
        assert!(message.contains("SSH tunnel"), "{message}");
        // And the safer way to be reachable at all.
        assert!(message.contains(ACCESS_TOKEN), "{message}");
        assert_eq!(ACCESS_TOKEN, eren_shared::brand::env_name("ACCESS_TOKEN"));
    }

    /// `EREN_TRUST_NETWORK=false` is somebody saying no, not yes.
    #[test]
    fn saying_no_to_trusting_the_network_is_not_saying_yes() {
        for no in ["", " ", "0", "false", "FALSE", "no", "off"] {
            assert!(!trusted_value(no), "{no:?}");
        }
        for yes in ["1", "true", "yes", "anything"] {
            assert!(trusted_value(yes), "{yes:?}");
        }
    }

    #[test]
    fn ipv6_hosts_keep_their_brackets_and_lose_their_port() {
        // The bug this replaces: the old `split(':').next()` returned "[".
        assert_eq!(bare_host("[::1]:4820"), "[::1]");
        assert_eq!(bare_host("[::1]"), "[::1]");
        assert!(LOCAL_HOSTS.contains(&bare_host("[::1]:4820")));
    }

    #[test]
    fn ordinary_hosts_lose_their_port() {
        assert_eq!(bare_host("localhost:4820"), "localhost");
        assert_eq!(bare_host("127.0.0.1:4820"), "127.0.0.1");
        assert_eq!(bare_host("localhost"), "localhost");
    }

    #[test]
    fn origins_are_stripped_to_their_host() {
        assert_eq!(bare_host("http://localhost:5173"), "localhost");
        assert_eq!(bare_host("https://evil.example"), "evil.example");
        assert_eq!(bare_host("http://[::1]:4820"), "[::1]");
    }

    #[test]
    fn the_dashboard_and_the_dev_server_may_call_themselves() {
        assert!(origin_may_call(
            "http://localhost:4820",
            "localhost:4820",
            &access::Access::default()
        ));
        assert!(origin_may_call(
            "http://127.0.0.1:4820",
            "127.0.0.1:4820",
            &access::Access::default()
        ));
        // `vite dev` forwards the browser's own Host, so its page and the
        // request it proxies share an authority.
        assert!(origin_may_call(
            "http://localhost:5173",
            "localhost:5173",
            &access::Access::default()
        ));
        assert!(origin_may_call(
            "http://[::1]:4820",
            "[::1]:4820",
            &access::Access::default()
        ));
    }

    #[test]
    fn an_allowed_host_may_call_itself_and_nothing_else_gains() {
        let access = access::Access::new(vec!["192.168.1.20".into()], None);
        assert!(origin_may_call(
            "http://192.168.1.20:4820",
            "192.168.1.20:4820",
            &access
        ));
        // Same rule as loopback: another port on that address is not the dashboard.
        assert!(!origin_may_call(
            "http://192.168.1.20:5173",
            "192.168.1.20:4820",
            &access
        ));
        // And it is not allowed unless someone listed it.
        assert!(!origin_may_call(
            "http://192.168.1.20:4820",
            "192.168.1.20:4820",
            &access::Access::default()
        ));
    }

    /// The hole this closes: a preview is agent-written code served from a
    /// loopback port, and a loopback origin used to be all it took.
    #[test]
    fn a_preview_on_another_loopback_port_may_not() {
        assert!(!origin_may_call(
            "http://127.0.0.1:53817",
            "127.0.0.1:4820",
            &access::Access::default()
        ));
        assert!(!origin_may_call(
            "http://localhost:5173",
            "localhost:4820",
            &access::Access::default()
        ));
        assert!(!origin_may_call(
            "http://localhost:4820",
            "127.0.0.1:4820",
            &access::Access::default()
        ));
    }

    #[test]
    fn everything_else_is_not() {
        assert!(!origin_may_call(
            "https://evil.example",
            "localhost:4820",
            &access::Access::default()
        ));
        assert!(!origin_may_call(
            "null",
            "localhost:4820",
            &access::Access::default()
        ));
        // A suffix match would let a deployment's own page call the API.
        assert!(!origin_may_call(
            "http://my-preview.localhost:4820",
            "my-preview.localhost:4820",
            &access::Access::default()
        ));
        // And a lookalike registered on the public internet must not pass.
        assert!(!origin_may_call(
            "https://localhost.evil.example",
            "localhost.evil.example",
            &access::Access::default()
        ));
    }
}
