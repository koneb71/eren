//! Who is calling, once accounts are on.
//!
//! Accounts are off until an admin exists (`eren admin create`; see
//! `eren_core::users`). Off, this layer only marks every request as
//! [`Caller::Local`] — the single person a loopback-first Eren was built for,
//! who owns everything — and [`crate::access`] decides who gets in, exactly
//! as before. On, the access token is no longer consulted: every request
//! needs a session cookie, from this machine as from any other, except
//!
//! - what a browser needs to *reach* the sign-in page: the dashboard's static
//!   files (anything outside `/api`, `/ws` and `/mcp`) and `/api/auth/…`'s
//!   status, sign-in and sign-up;
//! - `/mcp` from a loopback peer: the spawned agent CLIs, which carry no
//!   cookie and are identified by the live run in their URL, as before.
//!
//! A preview's or an app's hostname reaches this layer as an ordinary path,
//! so [`crate::preview_proxy`] asks again for itself: a loopback peer or a
//! signed-in owner, nobody else.
//!
//! "Loopback" is the TCP peer, never a header — the same rule as the token.
//!
//! An account whose password the admin reset reaches nothing under `/api` or
//! `/ws` but `/api/auth/…` until it chooses a new one — the terminal and the
//! event stream included, not only the REST routes.

use crate::routes::{internal, ApiError};
use crate::AppState;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use eren_core::scope::Owned;
use eren_core::users::User;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The cookie a browser keeps its session in.
pub const COOKIE: &str = "eren_session";

/// How often an Eren with accounts off looks again, so `eren admin create`
/// from another terminal takes effect without a restart.
const RECHECK: Duration = Duration::from_secs(5);

/// Who made a request.
#[derive(Debug, Clone)]
pub enum Caller {
    /// Accounts are off: the one person this machine serves, who owns every
    /// workspace and may change every setting — Eren as it always was.
    Local,
    /// A signed-in account.
    User(User),
    /// Accounts are on and there is no session: only the paths listed at the
    /// top of this module let one through, and no handler that takes a
    /// [`Caller`] ever sees one.
    Anonymous,
}

impl Caller {
    pub fn user(&self) -> Option<&User> {
        match self {
            Caller::User(u) => Some(u),
            _ => None,
        }
    }

    pub fn user_id(&self) -> Option<Uuid> {
        self.user().map(|u| u.id)
    }

    /// May change what belongs to the machine rather than to a workspace.
    pub fn is_admin(&self) -> bool {
        match self {
            Caller::Local => true,
            Caller::User(u) => u.is_admin,
            Caller::Anonymous => false,
        }
    }

    /// Refuse unless `what` lives in a workspace this caller owns. A thing
    /// that is someone else's answers exactly as one that does not exist —
    /// a 404 — so ids cannot be probed.
    pub async fn require(&self, state: &AppState, what: Owned) -> Result<(), ApiError> {
        match self {
            Caller::Local => Ok(()),
            Caller::Anonymous => Err((StatusCode::UNAUTHORIZED, "Sign in to Eren first.".into())),
            Caller::User(u) => {
                if eren_core::scope::owned_by(&state.db, what, u.id)
                    .await
                    .map_err(internal)?
                {
                    Ok(())
                } else {
                    Err((StatusCode::NOT_FOUND, "not found".into()))
                }
            }
        }
    }

    /// [`Caller::require`] for an id that may be absent.
    pub async fn require_opt<F: FnOnce(Uuid) -> Owned>(
        &self,
        state: &AppState,
        id: Option<Uuid>,
        what: F,
    ) -> Result<(), ApiError> {
        match id {
            Some(id) => self.require(state, what(id)).await,
            None => Ok(()),
        }
    }

    /// The workspaces this caller owns; `None` means every workspace (accounts
    /// off).
    pub async fn workspaces(&self, state: &AppState) -> Result<Option<Vec<Uuid>>, ApiError> {
        match self {
            Caller::Local => Ok(None),
            Caller::Anonymous => Ok(Some(vec![])),
            Caller::User(u) => Ok(Some(
                eren_core::users::workspaces(&state.db, u.id)
                    .await
                    .map_err(internal)?,
            )),
        }
    }

    /// What a list may show: the one workspace asked for (checked), or —
    /// when none was — every workspace this caller owns. `None` means no
    /// filter at all (accounts off, none asked for). Bind it as
    /// `$n::uuid[]` and filter `($n::uuid[] IS NULL OR workspace_id = ANY($n))`.
    pub async fn workspace_filter(
        &self,
        state: &AppState,
        asked: Option<Uuid>,
    ) -> Result<Option<Vec<Uuid>>, ApiError> {
        match asked {
            Some(ws) => {
                self.require(state, Owned::Workspace(ws)).await?;
                Ok(Some(vec![ws]))
            }
            None => self.workspaces(state).await,
        }
    }

    /// The ledger's name for this caller.
    pub fn actor(&self) -> eren_core::audit::Actor {
        match self {
            Caller::User(u) => eren_core::audit::Actor::User(u.id),
            _ => eren_core::audit::Actor::Api,
        }
    }
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "Sign in to Eren first.").into_response()
}

/// A handler that takes a `Caller` is never reached anonymously.
impl FromRequestParts<AppState> for Caller {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _: &AppState) -> Result<Self, Self::Rejection> {
        match parts.extensions.get::<Caller>() {
            Some(Caller::Anonymous) | None => Err(unauthorized()),
            Some(c) => Ok(c.clone()),
        }
    }
}

/// A caller who may change the machine's settings: the admin, or anyone
/// while accounts are off.
#[derive(Debug, Clone)]
pub struct Admin(pub Caller);

impl FromRequestParts<AppState> for Admin {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let caller = Caller::from_request_parts(parts, state).await?;
        if caller.is_admin() {
            Ok(Admin(caller))
        } else {
            Err((
                StatusCode::FORBIDDEN,
                "Only the admin can change this; it belongs to the whole machine.",
            )
                .into_response())
        }
    }
}

/// Whether accounts are on, and the sign-in throttle.
#[derive(Debug, Default)]
pub struct Accounts {
    on: AtomicBool,
    checked: Mutex<Option<Instant>>,
    throttle: Throttle,
}

impl Accounts {
    /// Ask the database now; used once at boot.
    pub async fn load(db: &eren_core::Db) -> anyhow::Result<Self> {
        let me = Self::default();
        me.on
            .store(eren_core::users::accounts_on(db).await?, Ordering::Relaxed);
        *me.checked.lock().unwrap() = Some(Instant::now());
        Ok(me)
    }

    /// Accounts, once on, stay on: the admin cannot be deleted or disabled.
    /// While off, the database is asked again at most every [`RECHECK`].
    pub async fn is_on(&self, db: &eren_core::Db) -> bool {
        if self.on.load(Ordering::Relaxed) {
            return true;
        }
        let due = {
            let mut checked = self.checked.lock().unwrap();
            let due = checked.is_none_or(|t| t.elapsed() >= RECHECK);
            if due {
                *checked = Some(Instant::now());
            }
            due
        };
        if due {
            match eren_core::users::accounts_on(db).await {
                Ok(true) => {
                    tracing::info!("an admin exists: accounts are on, every request now signs in");
                    self.on.store(true, Ordering::Relaxed);
                    return true;
                }
                Ok(false) => {}
                // Fail closed would lock a single-person install out of its
                // own dashboard over a hiccup; the token still guards it.
                Err(e) => tracing::warn!(error = %e, "could not ask whether accounts are on"),
            }
        }
        false
    }

    pub fn throttle(&self) -> &Throttle {
        &self.throttle
    }
}

/// Failed sign-ins, per name and address: after [`FREE_TRIES`], each further
/// failure doubles the wait, up to [`MAX_WAIT`]. In memory — a restart forgets
/// it, which costs a guesser a restart they cannot cause.
#[derive(Debug, Default)]
pub struct Throttle(Mutex<HashMap<(String, Option<IpAddr>), (u32, Instant)>>);

pub const FREE_TRIES: u32 = 5;
pub const MAX_WAIT: Duration = Duration::from_secs(15 * 60);

fn wait_after(failures: u32) -> Duration {
    if failures < FREE_TRIES {
        return Duration::ZERO;
    }
    let exp = (failures - FREE_TRIES).min(10);
    Duration::from_secs(1u64 << exp).min(MAX_WAIT)
}

impl Throttle {
    /// How long until this name may be tried again from here, if at all.
    pub fn wait(&self, name: &str, ip: Option<IpAddr>) -> Option<Duration> {
        let map = self.0.lock().unwrap();
        let (failures, last) = map.get(&(name.to_ascii_lowercase(), ip))?;
        let wait = wait_after(*failures);
        let elapsed = last.elapsed();
        (elapsed < wait).then(|| wait - elapsed)
    }

    pub fn failed(&self, name: &str, ip: Option<IpAddr>) {
        let mut map = self.0.lock().unwrap();
        // Bounded: a flood of made-up names cannot grow this without limit.
        if map.len() > 10_000 {
            map.retain(|_, (_, last)| last.elapsed() < MAX_WAIT);
        }
        let entry = map
            .entry((name.to_ascii_lowercase(), ip))
            .or_insert((0, Instant::now()));
        entry.0 += 1;
        entry.1 = Instant::now();
    }

    pub fn succeeded(&self, name: &str, ip: Option<IpAddr>) {
        self.0
            .lock()
            .unwrap()
            .remove(&(name.to_ascii_lowercase(), ip));
    }
}

/// The session token a request carries, if any.
pub fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

/// The `Set-Cookie` that signs a browser in, or (with `None`) out.
pub fn cookie(token: Option<&str>) -> HeaderValue {
    let value = match token {
        Some(t) => format!(
            "{COOKIE}={t}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
            i64::from(eren_core::sessions::TTL_DAYS) * 86_400
        ),
        None => format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
    };
    HeaderValue::from_str(&value).expect("a hex token is a valid header value")
}

/// What may be reached with no session while accounts are on.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Open {
    /// Needs a session.
    No,
    /// Reachable by anyone who can reach the server.
    Yes,
    /// Reachable from this machine only: the agents' MCP calls.
    FromLoopback,
}

/// Whether `path` is `prefix` or a path below it — a path segment, not a
/// string prefix, so `/apidocs` is not under `/api`.
fn under(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

pub(crate) fn open_path(path: &str) -> Open {
    const PUBLIC_API: [&str; 5] = [
        "/api/health",
        "/api/auth/status",
        "/api/auth/login",
        "/api/auth/signup",
        "/api/auth/logout",
    ];
    if PUBLIC_API.contains(&path) {
        return Open::Yes;
    }
    if under(path, "/mcp") {
        return Open::FromLoopback;
    }
    if under(path, "/api") || under(path, "/ws") {
        return Open::No;
    }
    // The dashboard's files (the sign-in page is one of them). A preview or
    // app hostname lands here too, and `preview_proxy` gates it itself.
    Open::Yes
}

/// Paths an account that must change its password may still reach.
fn while_changing_password(path: &str) -> bool {
    path.starts_with("/api/auth/")
}

/// What a temporary password does not open: the API and the sockets — the
/// terminal is a shell and `/ws` is every transcript — until the account has
/// chosen a password of its own. The dashboard's files stay reachable, since
/// the page that changes the password is one of them.
pub(crate) fn gated_while_changing_password(path: &str) -> bool {
    (under(path, "/api") || under(path, "/ws")) && !while_changing_password(path)
}

/// The middleware: decides the [`Caller`] and puts it in the request.
pub async fn require_session(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    if !state.accounts.is_on(&state.db).await {
        req.extensions_mut().insert(Caller::Local);
        return next.run(req).await;
    }
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    let user = match session_token(req.headers()) {
        Some(token) => match eren_core::sessions::lookup(&state.db, token).await {
            Ok(user) => user,
            Err(e) => {
                tracing::warn!(error = %e, "could not look a session up");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "could not check the session",
                )
                    .into_response();
            }
        },
        None => None,
    };
    let path = req.uri().path().to_string();
    match user {
        Some(user) => {
            if user.must_change_password && gated_while_changing_password(&path) {
                return (
                    StatusCode::FORBIDDEN,
                    "Choose a new password first: the admin reset this one.",
                )
                    .into_response();
            }
            req.extensions_mut().insert(Caller::User(user));
            next.run(req).await
        }
        None => {
            let open = match open_path(&path) {
                Open::Yes => true,
                Open::FromLoopback => peer.is_some_and(|ip| ip.to_canonical().is_loopback()),
                Open::No => false,
            };
            if open {
                req.extensions_mut().insert(Caller::Anonymous);
                next.run(req).await
            } else {
                unauthorized()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sign_in_page_and_the_agents_are_reachable_and_nothing_else() {
        for p in [
            "/",
            "/index.html",
            "/assets/app.js",
            "/projects/x",
            "/api/auth/login",
            "/api/health",
        ] {
            assert_eq!(open_path(p), Open::Yes, "{p}");
        }
        for p in ["/mcp", "/mcp/run/abc"] {
            assert_eq!(open_path(p), Open::FromLoopback, "{p}");
        }
        for p in [
            "/api/tasks",
            "/api/auth/users",
            "/api/auth/password",
            "/ws",
            "/ws/terminal/x",
            "/api",
        ] {
            assert_eq!(open_path(p), Open::No, "{p}");
        }
        // A prefix is a path segment, not a string prefix.
        assert_eq!(open_path("/apidocs"), Open::Yes);
        assert_eq!(open_path("/mcpx"), Open::Yes);
    }

    /// A temporary password opens the page that replaces it and nothing
    /// else — not the API, and not the sockets behind it.
    #[test]
    fn a_temporary_password_opens_nothing_but_the_way_to_change_it() {
        for gated in [
            "/api/tasks",
            "/api",
            "/ws",
            "/ws/terminal/x",
            "/ws/terminal",
        ] {
            assert!(gated_while_changing_password(gated), "{gated}");
        }
        for open in [
            "/api/auth/password",
            "/api/auth/logout",
            "/",
            "/assets/app.js",
            "/projects/x",
            "/mcp/run/x",
        ] {
            assert!(!gated_while_changing_password(open), "{open}");
        }
    }

    #[test]
    fn the_session_is_read_from_its_own_cookie_only() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "eren_access=nope; eren_session=abc123; theme=dark"
                .parse()
                .unwrap(),
        );
        assert_eq!(session_token(&h), Some("abc123"));
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "xeren_session=abc".parse().unwrap());
        assert_eq!(session_token(&h), None);
    }

    #[test]
    fn the_cookie_is_http_only_and_signing_out_expires_it() {
        let on = cookie(Some("abc"));
        let on = on.to_str().unwrap();
        assert!(on.starts_with("eren_session=abc;"));
        assert!(on.contains("HttpOnly") && on.contains("SameSite=Lax"));
        assert!(cookie(None).to_str().unwrap().contains("Max-Age=0"));
    }

    #[test]
    fn a_few_wrong_passwords_are_free_and_then_each_one_costs_more() {
        let t = Throttle::default();
        let ip = Some("192.168.1.9".parse().unwrap());
        for _ in 0..FREE_TRIES {
            assert!(t.wait("bea", ip).is_none());
            t.failed("Bea", ip);
        }
        assert!(t.wait("bea", ip).is_some());
        // Another address, or another name, is not held up by it.
        assert!(t
            .wait("bea", Some("192.168.1.10".parse().unwrap()))
            .is_none());
        assert!(t.wait("cal", ip).is_none());
        t.succeeded("bea", ip);
        assert!(t.wait("bea", ip).is_none());

        assert_eq!(wait_after(FREE_TRIES), Duration::from_secs(1));
        assert_eq!(wait_after(FREE_TRIES + 3), Duration::from_secs(8));
        assert_eq!(wait_after(FREE_TRIES + 40), MAX_WAIT);
    }

    #[test]
    fn with_accounts_off_the_one_local_person_is_the_admin() {
        assert!(Caller::Local.is_admin());
        assert!(!Caller::Anonymous.is_admin());
    }
}
