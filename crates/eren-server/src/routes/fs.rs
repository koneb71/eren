//! Local folder browser for "load folder". The server runs on the user's own
//! machine; browsing is still sandboxed to one root and directory names only.
//!
//! This module also holds the one answer to "is this a folder Eren may open?"
//! ([`may_open`]): under the browse root, or one of Eren's own managed folders
//! (apps, spaces). Loading a folder as a project, reading its files, and
//! opening a terminal in it all ask the same question here, so they cannot
//! disagree — which is how, before, a project could be made at `/` and read
//! from, while only *writing* to it was refused.

use super::{internal, ApiError};
use crate::auth::Caller;
use crate::AppState;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use eren_core::worktrees::manager::{ensure_repo, Vcs};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/fs/list", get(list))
        .route("/fs/mkdir", post(mkdir))
        .route("/fs/git-init", post(git_init))
}

/// The only directory tree the browser may show.
///
/// `$HOME` when run normally. In a container `$HOME` is the container's, not
/// yours, so `EREN_BROWSE_ROOT` points it at wherever your code is mounted.
pub(crate) fn browse_root() -> PathBuf {
    eren_shared::brand::var_os("BROWSE_ROOT")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Canonicalize `requested` and require it to live under `root`.
/// Returns None for anything that escapes (symlinks included).
pub(crate) fn sandboxed(root: &Path, requested: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(requested).ok()?;
    let root_canonical = std::fs::canonicalize(root).ok()?;
    canonical.starts_with(&root_canonical).then_some(canonical)
}

/// Where spaces live: folders of documents Eren makes and manages itself.
pub(crate) fn spaces_root() -> PathBuf {
    eren_shared::brand::home().join("spaces")
}

/// Every tree Eren may open a project in: the browse root, and its own
/// managed folders. Apps and spaces sit under `~/.eren`, which in a container
/// is outside `EREN_BROWSE_ROOT`; they are Eren's to make, so they are Eren's
/// to open.
pub(crate) fn managed_roots() -> Vec<PathBuf> {
    vec![browse_root(), eren_core::apps::root(), spaces_root()]
}

/// The canonical path of `requested` if it lives under any of `roots`. A
/// root that does not exist yet (no space has been made) is simply not a
/// match; it cannot panic and it cannot widen anything.
pub(crate) fn opens_under(roots: &[PathBuf], requested: &Path) -> Option<PathBuf> {
    roots.iter().find_map(|root| sandboxed(root, requested))
}

/// Whether Eren may open `requested` — list it, read it, write it, open a
/// shell in it. `Err` carries the sentence to show, with the remedy.
pub(crate) fn may_open(requested: &Path) -> Result<PathBuf, String> {
    opens_under(&managed_roots(), requested).ok_or_else(|| {
        format!(
            "this project's folder is outside the folder Eren is allowed to open \
             (EREN_BROWSE_ROOT, or Eren's own apps and spaces folders). Set \
             {} if that is deliberate.",
            eren_shared::brand::env_name("BROWSE_ROOT")
        )
    })
}

#[derive(Deserialize)]
struct ListQuery {
    path: Option<String>,
}

async fn list(_caller: Caller, Query(q): Query<ListQuery>) -> Result<Json<Value>, ApiError> {
    let home = browse_root();
    let requested = q.path.map(PathBuf::from).unwrap_or_else(|| home.clone());
    let Some(path) = sandboxed(&home, &requested) else {
        return Err((
            StatusCode::FORBIDDEN,
            "that path is outside the folder Eren is allowed to browse".into(),
        ));
    };

    let mut dirs: Vec<Value> = vec![];
    let mut entries = tokio::fs::read_dir(&path).await.map_err(internal)?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let dir_path = entry.path();
        dirs.push(json!({
            "name": name,
            "path": dir_path.to_string_lossy(),
            "isGitRepo": dir_path.join(".git").exists(),
        }));
    }
    dirs.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });

    let parent = path
        .parent()
        .and_then(|p| sandboxed(&home, p))
        .map(|p| p.to_string_lossy().into_owned());
    Ok(Json(json!({
        "path": path.to_string_lossy(),
        "parent": parent,
        "isGitRepo": path.join(".git").exists(),
        "dirs": dirs,
    })))
}

#[derive(Deserialize)]
struct MkdirBody {
    /// Existing directory to create inside. Must already be browsable.
    parent: String,
    name: String,
}

/// Create a folder so a project can be started from nothing.
///
/// Browsing could only ever *find* a folder, so starting something new meant
/// leaving the app for a terminal. The new directory is created inside the
/// sandbox and returned, ready to be loaded as a project.
async fn mkdir(_caller: Caller, Json(body): Json<MkdirBody>) -> Result<Json<Value>, ApiError> {
    let home = browse_root();
    let Some(parent) = sandboxed(&home, Path::new(&body.parent)) else {
        return Err((
            StatusCode::FORBIDDEN,
            "that path is outside the folder Eren is allowed to browse".into(),
        ));
    };

    let name = safe_dir_name(&body.name).map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let path = parent.join(name);
    if path.exists() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("\"{name}\" already exists here"),
        ));
    }
    tokio::fs::create_dir(&path).await.map_err(internal)?;

    Ok(Json(json!({
        "path": path.to_string_lossy(),
        "name": name,
        "isGitRepo": false,
    })))
}

/// One new directory, not a path.
///
/// Rejecting separators and `..` is what keeps the sandbox check meaningful:
/// the parent is canonicalized and verified, but `../../..` appended to a
/// perfectly valid parent lands anywhere at all.
pub(crate) fn safe_dir_name(raw: &str) -> Result<&str, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("a folder name is required".into());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("use a single folder name, not a path".into());
    }
    if name.starts_with('.') {
        // Not a security rule — the browser hides dotfiles, so this would
        // create a folder the user then can't find.
        return Err("a folder starting with \".\" would be hidden from the browser".into());
    }
    Ok(name)
}

#[derive(Deserialize)]
struct GitInitBody {
    path: String,
}

async fn git_init(_caller: Caller, Json(body): Json<GitInitBody>) -> Result<Json<Value>, ApiError> {
    let home = browse_root();
    let Some(path) = sandboxed(&home, Path::new(&body.path)) else {
        return Err((
            StatusCode::FORBIDDEN,
            "that path is outside the folder Eren is allowed to browse".into(),
        ));
    };
    if path.join(".git").exists() {
        return Err((StatusCode::BAD_REQUEST, "already a git repository".into()));
    }
    // `init` alone leaves an unborn HEAD, and `git worktree add … main` then
    // fails with "invalid reference: main" — so the first task run on a freshly
    // initialized project used to die. The first commit must also include any
    // existing files, or the worktree would be handed none of them.
    match ensure_repo(&path, "main").await {
        Vcs::Git => Ok(Json(json!({
            "initialized": true,
            "path": path.to_string_lossy(),
        }))),
        Vcs::None(reason) => Err((StatusCode::BAD_REQUEST, reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::{opens_under, safe_dir_name, sandboxed};
    use std::path::Path;

    /// The one answer every door asks: under any managed root, and nowhere
    /// else — not through a symlink, not through `..`, not a root that is
    /// not there.
    #[test]
    fn opens_under_any_managed_root_and_nowhere_else() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let elsewhere = dir.path().join("elsewhere");
        for d in [&a, &b, &elsewhere] {
            std::fs::create_dir_all(d.join("inner")).unwrap();
        }
        let missing = dir.path().join("not-made-yet");
        let roots = vec![a.clone(), missing.clone(), b.clone()];

        assert!(opens_under(&roots, &a.join("inner")).is_some());
        assert!(opens_under(&roots, &b.join("inner")).is_some());
        assert!(opens_under(&roots, &b).is_some());
        // A missing root is skipped, never matched and never a panic.
        assert!(opens_under(&roots, &missing).is_none());
        // Outside every root, however it is spelled.
        assert!(opens_under(&roots, &elsewhere).is_none());
        assert!(opens_under(&roots, &a.join("..").join("elsewhere")).is_none());
        assert!(opens_under(&roots, dir.path()).is_none());
        #[cfg(unix)]
        {
            let link = a.join("link");
            std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
            assert!(opens_under(&roots, &link).is_none(), "a symlink out is out");
        }
        assert!(opens_under(&[], &a).is_none());
    }

    #[test]
    fn a_folder_name_may_not_be_a_path() {
        assert_eq!(safe_dir_name("  my-app "), Ok("my-app"));
        assert_eq!(safe_dir_name("Fine Name 2"), Ok("Fine Name 2"));

        // The escapes that would defeat the sandboxed parent.
        for bad in ["../evil", "..", "a/b", "nested\\win", "/etc"] {
            assert!(safe_dir_name(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(safe_dir_name("   ").is_err());
        assert!(safe_dir_name(".hidden").is_err());
    }

    #[test]
    fn sandbox_accepts_home_subdirs_and_rejects_escapes() {
        let home = std::env::var("HOME").unwrap();
        let home = Path::new(&home);

        // A directory this test makes, rather than one it hopes for. The
        // previous version reached for `Documents`, which meant the test
        // passed on a laptop and failed in any container without one — and a
        // test that depends on the machine is a test people learn to ignore.
        let scratch = home.join(".eren-fs-sandbox-test");
        std::fs::create_dir_all(&scratch).expect("a directory under HOME");

        // Home itself is allowed.
        assert!(sandboxed(home, home).is_some());
        // Traversal back into home is fine after canonicalization…
        assert!(sandboxed(home, &scratch.join("..")).is_some());
        // …and into the directory itself.
        assert!(sandboxed(home, &scratch).is_some());
        // …but escaping is not.
        assert!(sandboxed(home, Path::new("/tmp")).is_none());
        assert!(sandboxed(home, &home.join("..")).is_none());
        // Nonexistent paths are rejected (canonicalize fails).
        assert!(sandboxed(home, &home.join("definitely-not-a-real-dir-xyz")).is_none());

        let _ = std::fs::remove_dir(&scratch);
    }
}
