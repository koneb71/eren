//! Who may reach this server, from where.
//!
//! Eren was built for one caller — the browser on this machine — and that is
//! what lets it go without a login. Two settings widen that, for someone who
//! wants the dashboard on their phone or another computer on their network:
//!
//! - **`EREN_ALLOWED_HOSTS`** — names the server answers to besides loopback,
//!   such as `192.168.1.20` or `mybox.local`. The `Host` and `Origin` checks in
//!   `reject_non_local_callers` exist so that a web page cannot reach Eren
//!   through a browser; this list is the names *you* reach it by, and nothing
//!   else gets added.
//! - **The access token** — required of every caller that is not this machine,
//!   whatever address Eren listens on. A browser is given it once by an
//!   access link (`/?access=…`), which trades it for a cookie and redirects the
//!   token out of the address bar; a script sends `Authorization: Bearer …`.
//!   Generated on first start and kept in `~/.eren/access_token` (0600), or set
//!   with `EREN_ACCESS_TOKEN`; `EREN_ACCESS_TOKEN=off` goes back to the old,
//!   unauthenticated behaviour, which on a wide bind then has to be
//!   acknowledged with `EREN_TRUST_NETWORK`.
//!
//! The token exists on a loopback bind too, and it is not pedantry. A
//! loopback-bound port is reached by more than this machine's own processes:
//! Docker Desktop delivers a container's connection to `host.docker.internal`
//! with the gateway's address as the peer, and a preview container is an
//! unreviewed branch's code. The Host check does not stop it — any program
//! sets that header itself — so the token is what does. Nothing changes for
//! this machine: a loopback peer is never asked.
//!
//! "Not this machine" is the TCP peer, never a header: a header is whatever
//! the caller says. The spawned agent CLIs reach `/mcp` over loopback, so they
//! never need the token and never see it — it is one of Eren's own secrets,
//! stripped from every child by `env_guard`. A reverse proxy on this machine
//! makes every caller look local, which is why SECURITY.md says not to put one
//! in front of Eren without its own authentication.

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

/// Host names every Eren answers to: this machine's own.
pub(crate) const LOCAL_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

/// The cookie a browser keeps the token in.
pub const COOKIE: &str = "eren_access";
/// The query parameter an access link carries the token in.
pub const QUERY: &str = "access";
/// Shorter than this is guessable enough to refuse.
const MIN_TOKEN_LEN: usize = 16;

/// Who may call: the configured names, and the token if one is required.
#[derive(Debug, Clone, Default)]
pub struct Access {
    hosts: Vec<String>,
    token: Option<String>,
}

impl Access {
    pub fn new(hosts: Vec<String>, token: Option<String>) -> Self {
        Self { hosts, token }
    }

    /// Whether the server answers to this host (no port; lowercase compared).
    pub fn allows_host(&self, bare: &str) -> bool {
        let bare = bare.to_ascii_lowercase();
        LOCAL_HOSTS.contains(&bare.as_str()) || self.hosts.contains(&bare)
    }

    /// The names from `EREN_ALLOWED_HOSTS`, in the order given.
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    /// The token callers from other machines must present, if any.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

/// Read `EREN_ALLOWED_HOSTS`: names separated by commas or spaces.
///
/// A name is a host, never a pattern: no port, no path, no wildcard. An IPv6
/// address is accepted bare and stored bracketed, which is how a `Host`
/// header spells it. Refused rather than skipped, so a typo is heard at start.
pub fn parse_hosts(raw: &str) -> Result<Vec<String>, String> {
    let mut hosts = vec![];
    for entry in raw
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|e| !e.is_empty())
    {
        let entry = entry.to_ascii_lowercase();
        let host = if let Ok(v6) = entry.parse::<std::net::Ipv6Addr>() {
            format!("[{v6}]")
        } else if entry.starts_with('[') && entry.ends_with(']') {
            let inner = &entry[1..entry.len() - 1];
            match inner.parse::<std::net::Ipv6Addr>() {
                Ok(v6) => format!("[{v6}]"),
                Err(_) => return Err(format!("\"{entry}\" is not an IPv6 address")),
            }
        } else if entry
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            && !entry.starts_with('.')
            && !entry.ends_with('.')
        {
            entry
        } else {
            return Err(format!(
                "\"{entry}\" is not a host name — give a name or an address on its \
                 own, without a port, a path or a wildcard"
            ));
        };
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    Ok(hosts)
}

/// What `EREN_ACCESS_TOKEN` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSetting {
    /// Unset: use the one in the home folder, making it the first time.
    Generated,
    /// A token chosen by the person running Eren.
    Given(String),
    /// `off`: no token, which a wide bind then has to acknowledge.
    Off,
}

/// Read `EREN_ACCESS_TOKEN`.
///
/// A given token must be long enough not to be guessed, and URL-safe, because
/// it rides in an access link and a cookie as it is.
pub fn token_setting(raw: Option<&str>) -> Result<TokenSetting, String> {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(TokenSetting::Generated);
    };
    if raw.eq_ignore_ascii_case("off") {
        return Ok(TokenSetting::Off);
    }
    if raw.len() < MIN_TOKEN_LEN {
        return Err(format!(
            "the access token must be at least {MIN_TOKEN_LEN} characters; \
             leave it unset and Eren makes a long random one"
        ));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
    {
        return Err(
            "the access token may use letters, digits and - _ . ~ only, because it \
             travels in a link"
                .into(),
        );
    }
    Ok(TokenSetting::Given(raw.to_string()))
}

/// The token kept at `path`, made — random, 0600 — if there is none yet.
///
/// Created with its final permissions in one step, so there is no moment when
/// another account on the machine could read it.
pub fn load_or_create_token(path: &Path) -> std::io::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim().to_string();
        if existing.len() >= MIN_TOKEN_LEN {
            return Ok(existing);
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Two v4 UUIDs: 244 random bits from the OS, hex, URL-safe as it is.
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options.open(path)?.write_all(token.as_bytes())?;
    Ok(token)
}

/// The token this server runs with, from its setting: the one given, the one
/// in `path` (made if there is none yet), or none at all when it was turned
/// off. Asked on every bind, not only a wide one — see the module comment.
pub fn token_for(setting: TokenSetting, path: &Path) -> std::io::Result<Option<String>> {
    match setting {
        TokenSetting::Given(t) => Ok(Some(t)),
        TokenSetting::Generated => load_or_create_token(path).map(Some),
        TokenSetting::Off => Ok(None),
    }
}

/// What to do with one request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Let it through.
    Pass,
    /// An access link with the right token: keep it in a cookie and send the
    /// browser on to the same place without it.
    SignIn { location: String },
    /// Another machine, without the token.
    Refuse,
}

/// Decide a request. `peer` is `None` only where no connection info exists,
/// which is never in `serve` — and is refused, so a missing peer fails closed.
pub(crate) fn judge(
    token: Option<&str>,
    peer: Option<IpAddr>,
    headers: &HeaderMap,
    uri: &Uri,
) -> Verdict {
    let Some(token) = token else {
        return Verdict::Pass;
    };
    if peer.is_some_and(|ip| ip.to_canonical().is_loopback()) {
        return Verdict::Pass;
    }
    if let Some(given) = query_token(uri) {
        return if same(given, token) {
            Verdict::SignIn {
                location: without_token(uri),
            }
        } else {
            Verdict::Refuse
        };
    }
    let from_cookie = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))
        .any(|v| same(v, token));
    let from_bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|v| same(v.trim(), token));
    if from_cookie || from_bearer {
        Verdict::Pass
    } else {
        Verdict::Refuse
    }
}

/// The token an access link carries, if it carries one.
fn query_token(uri: &Uri) -> Option<&str> {
    uri.query()?
        .split('&')
        .find_map(|pair| pair.strip_prefix(QUERY)?.strip_prefix('='))
}

/// The same path and query, minus the token.
fn without_token(uri: &Uri) -> String {
    let rest: Vec<&str> = uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some(QUERY))
        .collect();
    if rest.is_empty() {
        uri.path().to_string()
    } else {
        format!("{}?{}", uri.path(), rest.join("&"))
    }
}

/// Compare without stopping at the first difference, so the time a refusal
/// takes says nothing about how much of a guess was right.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The middleware: outermost, so nothing — the API, the socket, a preview or
/// an app's bridge — is reachable from another machine without the token.
pub async fn require_token(
    State(state): State<crate::AppState>,
    req: Request,
    next: Next,
) -> Response {
    // With accounts on, every caller signs in instead (see `crate::auth`).
    if state.accounts.is_on(&state.db).await {
        return next.run(req).await;
    }
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    match judge(state.access.token(), peer, req.headers(), req.uri()) {
        Verdict::Pass => next.run(req).await,
        Verdict::SignIn { location } => {
            let token = state.access.token().unwrap_or_default();
            let mut res = StatusCode::SEE_OTHER.into_response();
            let h = res.headers_mut();
            // Lax rather than Strict: an access link opened from a message
            // or a QR code is a cross-site navigation, and a Strict cookie
            // would not ride along on the redirect that follows it. What Lax
            // still lets through — a top-level GET from another site — the
            // attacker cannot read, and every request that changes anything
            // also passes the Origin check.
            let cookie =
                format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=34560000");
            if let Ok(v) = HeaderValue::from_str(&cookie) {
                h.insert(header::SET_COOKIE, v);
            }
            if let Ok(v) = HeaderValue::from_str(&location) {
                h.insert(header::LOCATION, v);
            }
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            h.insert(
                header::REFERRER_POLICY,
                HeaderValue::from_static("no-referrer"),
            );
            res
        }
        Verdict::Refuse => refusal(req.headers()),
    }
}

fn refusal(headers: &HeaderMap) -> Response {
    let wants_page = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));
    let mut res = if wants_page {
        (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            REFUSAL_PAGE,
        )
            .into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            "This Eren needs its access token from other machines: open the access \
             link `eren serve` printed, or send Authorization: Bearer <token>.",
        )
            .into_response()
    };
    res.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"eren\""),
    );
    res
}

/// Shown to a browser on another machine that has not been given the link.
/// Says where to find it, and nothing about the token itself.
const REFUSAL_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark"><title>Eren · access link needed</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:34rem;margin:15vh auto;padding:0 1rem}code{font-size:.9em}</style>
</head><body>
<h1>This device needs Eren's access link</h1>
<p>Eren is running on another computer, and it only lets devices in with its access link.
On that computer, look where <code>eren serve</code> is running — it prints a link for each
address it answers to — or read the token from <code>~/.eren/access_token</code>, and open
<code>http://&lt;address&gt;:&lt;port&gt;/?access=&lt;token&gt;</code> here once.</p>
<p>This device will be remembered after that.</p>
</body></html>"#;

/// A best guess at this machine's address on its network, for telling someone
/// what to put in `EREN_ALLOWED_HOSTS`.
///
/// A UDP socket "connected" to a documentation address sends nothing; it only
/// makes the OS choose the interface it would route through.
pub fn guess_lan_address() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("192.0.2.1", 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    fn lan() -> Option<IpAddr> {
        Some("192.168.1.30".parse().unwrap())
    }

    fn with(name: header::HeaderName, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, value.parse().unwrap());
        h
    }

    /// `EREN_ACCESS_TOKEN=off` is the only way to have no token at all, and
    /// then a wide bind has to be acknowledged (`crate::exposure`).
    #[test]
    fn with_the_token_off_everything_passes() {
        assert_eq!(
            judge(None, lan(), &HeaderMap::new(), &uri("/api/tasks")),
            Verdict::Pass
        );
    }

    /// The token is on whatever the bind: a loopback-bound port can still be
    /// reached by a container through a gateway, and that peer must be asked.
    #[test]
    fn the_token_is_on_whatever_the_bind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access_token");
        let generated = token_for(TokenSetting::Generated, &path).unwrap().unwrap();
        assert!(generated.len() >= 60);
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), generated);

        let given = token_for(
            TokenSetting::Given(TOKEN.into()),
            &dir.path().join("unused"),
        )
        .unwrap();
        assert_eq!(given.as_deref(), Some(TOKEN));
        assert!(!dir.path().join("unused").exists());

        assert_eq!(
            token_for(TokenSetting::Off, &dir.path().join("off")).unwrap(),
            None
        );
        assert!(!dir.path().join("off").exists());
    }

    #[test]
    fn this_machine_never_needs_it() {
        // The spawned CLIs call /mcp from here and must never be asked.
        for local in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            let peer = Some(local.parse().unwrap());
            assert_eq!(
                judge(Some(TOKEN), peer, &HeaderMap::new(), &uri("/mcp/run/x")),
                Verdict::Pass,
                "{local}"
            );
        }
    }

    #[test]
    fn another_machine_is_refused_without_it() {
        for path in ["/", "/api/tasks", "/ws?run_id=1", "/mcp"] {
            assert_eq!(
                judge(Some(TOKEN), lan(), &HeaderMap::new(), &uri(path)),
                Verdict::Refuse,
                "{path}"
            );
        }
        // A missing peer fails closed rather than counting as local.
        assert_eq!(
            judge(Some(TOKEN), None, &HeaderMap::new(), &uri("/")),
            Verdict::Refuse
        );
    }

    #[test]
    fn an_access_link_signs_the_browser_in_and_drops_the_token_from_the_address() {
        assert_eq!(
            judge(
                Some(TOKEN),
                lan(),
                &HeaderMap::new(),
                &uri(&format!("/?access={TOKEN}"))
            ),
            Verdict::SignIn {
                location: "/".into()
            }
        );
        assert_eq!(
            judge(
                Some(TOKEN),
                lan(),
                &HeaderMap::new(),
                &uri(&format!("/projects/p?tab=board&access={TOKEN}&x=1"))
            ),
            Verdict::SignIn {
                location: "/projects/p?tab=board&x=1".into()
            }
        );
        // A wrong link is refused, not passed through to the dashboard.
        assert_eq!(
            judge(Some(TOKEN), lan(), &HeaderMap::new(), &uri("/?access=nope")),
            Verdict::Refuse
        );
    }

    #[test]
    fn the_cookie_or_a_bearer_header_lets_it_through() {
        let cookie = with(header::COOKIE, &format!("theme=dark; {COOKIE}={TOKEN}"));
        assert_eq!(
            judge(Some(TOKEN), lan(), &cookie, &uri("/api/tasks")),
            Verdict::Pass
        );
        let bearer = with(header::AUTHORIZATION, &format!("Bearer {TOKEN}"));
        assert_eq!(
            judge(Some(TOKEN), lan(), &bearer, &uri("/api/tasks")),
            Verdict::Pass
        );

        let wrong = with(header::COOKIE, &format!("{COOKIE}={}", &TOKEN[1..]));
        assert_eq!(
            judge(Some(TOKEN), lan(), &wrong, &uri("/")),
            Verdict::Refuse
        );
        // A cookie whose name merely starts the same way is not ours.
        let lookalike = with(header::COOKIE, &format!("{COOKIE}x={TOKEN}"));
        assert_eq!(
            judge(Some(TOKEN), lan(), &lookalike, &uri("/")),
            Verdict::Refuse
        );
    }

    #[test]
    fn allowed_hosts_are_names_never_patterns() {
        assert_eq!(
            parse_hosts("192.168.1.20, MyBox.local  fe80::1").unwrap(),
            ["192.168.1.20", "mybox.local", "[fe80::1]"]
        );
        assert_eq!(parse_hosts("").unwrap(), Vec::<String>::new());
        for bad in [
            "*.local",
            "box.local:4820",
            "http://box",
            "a/b",
            ".local",
            "[nope]",
        ] {
            assert!(parse_hosts(bad).is_err(), "{bad}");
        }
        let access = Access::new(parse_hosts("mybox.local").unwrap(), None);
        assert!(access.allows_host("MYBOX.local"));
        assert!(access.allows_host("localhost"));
        assert!(!access.allows_host("evil.example"));
    }

    #[test]
    fn a_given_token_must_be_long_and_link_safe() {
        assert_eq!(token_setting(None).unwrap(), TokenSetting::Generated);
        assert_eq!(token_setting(Some("  ")).unwrap(), TokenSetting::Generated);
        assert_eq!(token_setting(Some("OFF")).unwrap(), TokenSetting::Off);
        assert_eq!(
            token_setting(Some(TOKEN)).unwrap(),
            TokenSetting::Given(TOKEN.into())
        );
        assert!(token_setting(Some("short")).is_err());
        assert!(token_setting(Some("has spaces in it, sixteen+")).is_err());
        assert!(token_setting(Some("amp&ersand-sixteen-chars")).is_err());
    }

    #[test]
    fn the_generated_token_is_kept_private_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("access_token");
        let first = load_or_create_token(&path).unwrap();
        assert!(first.len() >= 60, "{first}");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(load_or_create_token(&path).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Deleting it is how to rotate it.
        std::fs::remove_file(&path).unwrap();
        assert_ne!(load_or_create_token(&path).unwrap(), first);
    }
}
