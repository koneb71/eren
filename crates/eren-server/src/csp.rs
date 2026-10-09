//! The dashboard's content-security policy.
//!
//! One policy for every response the dashboard router serves, appended by
//! `refuse_to_be_framed` beside whatever stricter policy a handler set (a
//! browser enforces every policy it receives). It is built from what the
//! bundle actually needs, and nothing more:
//!
//! - **scripts**: the bundle's own files, plus a hash for each inline
//!   `<script>` in the served `index.html` — there is one, the theme-before-
//!   first-paint block, and Vite keeps it byte for byte. The hashes are read
//!   from the file at start, so an edit to that script cannot leave a stale
//!   constant here; a second inline script is refused by the design scan.
//!   No `'unsafe-eval'`: nothing in the bundle needs it.
//! - **styles**: `'unsafe-inline'`, because tiptap, Monaco and xterm inject
//!   `<style>` elements and write `style=""` attributes at run time, and the
//!   knowledge base's sanitised HTML carries filtered `style` attributes.
//! - **images**: Eren itself, `data:` (Monaco's CSS) and `blob:` (attachment
//!   previews) — and **no other origin**. Knowledge-base images and attachments
//!   are served from Eren; an `<img src="https://…">` in a stored page or in a
//!   model's markdown is the classic prompt-injection exfiltration channel (the
//!   URL carries whatever the model was talked into putting there, fetched by
//!   the operator's browser) and a tracking pixel, so it does not load.
//! - **frames**: the app iframe on `*.app.localhost`, and the video embed
//!   hosts the knowledge base's sanitiser allows — the same list, so the two
//!   cannot drift. **connect**: Eren, and the `*.app.localhost` probe the apps
//!   page makes to learn whether the browser resolves those names.
//! - `frame-ancestors 'none'`, as before: the dashboard is one-click
//!   irreversible actions and must not be framed. Previews and apps are
//!   served outside this layer and are meant to be framed.

use std::path::Path;

/// `'sha256-…'` for every inline `<script>` body in `html`, in order.
///
/// A script with a `src` is not inline. The body is hashed exactly as it sits
/// between the tags — whitespace included — which is what the browser hashes.
pub fn inline_script_hashes(html: &str) -> Vec<String> {
    use sha2::{Digest, Sha256};
    let mut hashes = vec![];
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        let tag_end = match rest[start..].find('>') {
            Some(i) => start + i,
            None => break,
        };
        let tag = &rest[start..tag_end];
        let after = &rest[tag_end + 1..];
        let Some(close) = after.find("</script>") else {
            break;
        };
        if !tag.contains("src=") {
            let body = &after[..close];
            hashes.push(format!(
                "'sha256-{}'",
                base64_std(&Sha256::digest(body.as_bytes()))
            ));
        }
        rest = &after[close + "</script>".len()..];
    }
    hashes
}

/// The policy, given the inline script hashes the served page needs.
pub fn policy(script_hashes: &[String]) -> String {
    let scripts = if script_hashes.is_empty() {
        "'self'".to_string()
    } else {
        format!("'self' {}", script_hashes.join(" "))
    };
    let embeds = eren_core::kb::EMBED_HOSTS
        .iter()
        .map(|h| format!("https://{h}"))
        .collect::<Vec<_>>()
        .join(" ");
    [
        "default-src 'self'".to_string(),
        format!("script-src {scripts}"),
        "style-src 'self' 'unsafe-inline'".to_string(),
        "img-src 'self' data: blob:".to_string(),
        "font-src 'self' data:".to_string(),
        "connect-src 'self' http://*.app.localhost:*".to_string(),
        format!("frame-src http://*.app.localhost:* {embeds}"),
        "worker-src 'self'".to_string(),
        "media-src 'self' https:".to_string(),
        "object-src 'none'".to_string(),
        "base-uri 'self'".to_string(),
        "form-action 'self'".to_string(),
        "frame-ancestors 'none'".to_string(),
    ]
    .join("; ")
}

/// The policy for the page at `<dist>/index.html` — the file the server
/// serves — or, with no such file (tests, a server with no dashboard), the
/// policy with no inline script allowed at all.
pub fn for_dist(dist: &Path) -> String {
    let html = std::fs::read_to_string(dist.join("index.html")).unwrap_or_default();
    policy(&inline_script_hashes(&html))
}

/// Standard base64 with padding, as a CSP hash is spelled.
fn base64_std(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The repository's own page: the hash the policy carries for it.
    fn index_html() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/index.html");
        std::fs::read_to_string(path).expect("web/index.html")
    }

    #[test]
    fn base64_is_the_standard_alphabet_with_padding() {
        assert_eq!(base64_std(b""), "");
        assert_eq!(base64_std(b"f"), "Zg==");
        assert_eq!(base64_std(b"fo"), "Zm8=");
        assert_eq!(base64_std(b"foo"), "Zm9v");
        assert_eq!(base64_std(b"foobar"), "Zm9vYmFy");
    }

    /// The theme script is the one inline script, and this is its hash — the
    /// number a browser computes, so a drift here is a dashboard that does
    /// not set its theme.
    #[test]
    fn the_pages_one_inline_script_is_hashed() {
        let hashes = inline_script_hashes(&index_html());
        assert_eq!(
            hashes,
            ["'sha256-xu4p/a5Bv0YmOOHeakaIKPAQojrTPl+4XXtYPwRInss='"]
        );
    }

    #[test]
    fn a_script_with_a_src_is_not_inline() {
        let html = r#"<script type="module" crossorigin src="/assets/index.js"></script>
                      <script>alert(1)</script><script src=x></script>"#;
        let hashes = inline_script_hashes(html);
        assert_eq!(hashes.len(), 1);
        assert!(inline_script_hashes("<p>no scripts</p>").is_empty());
        assert!(inline_script_hashes("<script>unterminated").is_empty());
    }

    #[test]
    fn the_policy_names_every_directive_and_the_embed_hosts() {
        let p = policy(&["'sha256-abc='".to_string()]);
        for directive in [
            "default-src 'self'",
            "script-src 'self' 'sha256-abc='",
            "style-src 'self' 'unsafe-inline'",
            "img-src 'self' data: blob:",
            "font-src 'self' data:",
            "connect-src 'self' http://*.app.localhost:*",
            "frame-src http://*.app.localhost:*",
            "worker-src 'self'",
            "object-src 'none'",
            "base-uri 'self'",
            "form-action 'self'",
            "frame-ancestors 'none'",
        ] {
            assert!(p.contains(directive), "{directive} missing from {p}");
        }
        for host in eren_core::kb::EMBED_HOSTS {
            assert!(p.contains(&format!("https://{host}")), "{host}");
        }
        // No eval, and no other origin for images.
        assert!(!p.contains("unsafe-eval"));
        assert!(!p.contains("img-src 'self' data: blob: http"));
        // With nothing inline, nothing inline is allowed.
        assert!(policy(&[]).contains("script-src 'self';"));
    }

    #[test]
    fn a_missing_dist_yields_the_policy_with_no_inline_script() {
        let dir = tempfile::tempdir().unwrap();
        assert!(for_dist(dir.path()).contains("script-src 'self';"));
        std::fs::write(dir.path().join("index.html"), index_html()).unwrap();
        assert!(for_dist(dir.path()).contains("'sha256-xu4p/"));
    }
}
