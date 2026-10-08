//! Browse and edit the files of a project checkout, or of a card's worktree.
//!
//! This used to say "a viewer, not an editor: nothing here writes". It writes
//! now — but the invariant that mattered is intact and worth restating, because
//! it is easy to read the change as a weakening of it: **the dashboard writes
//! when a person asks it to. Agents still never touch the working copy** — they
//! work in isolated worktrees, which is what makes a run reviewable.
//!
//! `fs.rs` sandboxes to $HOME for "load folder"; this sandboxes to one tree's
//! own root, which is stricter and independent of it.
//!
//! ## The four gates on a write
//!
//! Before any of them, the project or card must be the caller's
//! (`Caller::require`). With accounts off that passes for everyone and the
//! server binds loopback with no login, so every one of these is load bearing:
//!
//! 1. **No path may contain a `.git` component.** `SKIP_DIRS` hides `.git` from
//!    *listing* only; a crafted `path=.git/hooks/pre-commit` would otherwise go
//!    straight through `resolve`. Eren itself runs `git checkout` and
//!    `git merge` in that repo during squash-merge, which would execute the
//!    hook — remote code execution reached from a page someone merely visited.
//!    A worktree is no safer: its `.git` is a *file* pointing into the main
//!    repo's `.git/worktrees/<name>`, whose `commondir` shares `hooks/`.
//! 2. **The root must be one Eren may open.** A checkout must satisfy
//!    `fs::may_open` — under `EREN_BROWSE_ROOT`, or one of Eren's own apps or
//!    spaces folders — which is the same answer loading a folder and the
//!    terminal give. It gates *reads* too: a project row is data, not a
//!    grant, and a row outside every root (one made before `projects.rs`
//!    sandboxed its door, or a database write) used to read any file the
//!    server could, up to 512 KB each. A worktree must satisfy
//!    `WorktreeManager::manages`, the same check that gates full-auto, so "is
//!    this tree ours" has exactly one answer.
//! 3. **`baseHash` must match what is on disk.** Compare-and-write under a
//!    mutex, so a save cannot land on top of bytes the caller never saw.
//! 4. **A header no cross-origin request can set.** There is no CORS layer, so
//!    a preflight for `X-Eren-Write` gets no `Access-Control-Allow-*` and the
//!    browser refuses to send the real request. Belt and braces behind the
//!    `Origin` check in `lib.rs`.

use super::{internal, ApiError};
use crate::auth::Caller;
use crate::AppState;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use eren_core::scope::Owned;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/projects/{id}/files", get(list_project))
        .route("/projects/{id}/files/search", get(search))
        .route("/projects/{id}/file", get(read_project).put(write_project))
        // A card's worktree — the tree its diff is computed from, so a fix
        // made here is a fix in the change you are about to merge.
        .route("/tasks/{id}/files", get(list_task))
        .route("/tasks/{id}/file", get(read_task).put(write_task))
}

/// Anything larger is reported as too-large rather than streamed into a
/// browser tab that would choke on it.
const MAX_FILE_BYTES: u64 = 512 * 1024;

/// Directories that are never useful in a source browser and are large
/// enough that walking them hurts.
const SKIP_DIRS: [&str; 3] = [".git", "node_modules", "target"];

/// Resolve a project-relative path inside `root`, rejecting anything that
/// escapes it. Canonicalizing both sides means `..` and symlinks that point
/// outside the checkout are caught, not just literal `../` in the string.
fn resolve(root: &Path, rel: &str) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).ok()?;
    let canonical = std::fs::canonicalize(root.join(rel.trim_start_matches('/'))).ok()?;
    canonical.starts_with(&root).then_some(canonical)
}

/// Path of `full` relative to `root`, using forward slashes — this is the
/// form the API speaks, so clients never see absolute host paths.
fn relative(root: &Path, full: &Path) -> String {
    std::fs::canonicalize(root)
        .ok()
        .and_then(|r| full.strip_prefix(&r).ok().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| full.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

/// Where a tree lives on disk, and whether we may write to it.
///
/// A project tree that may be read may be written (gate 2 above answers both);
/// a worktree may be written only if Eren manages it.
struct Root {
    path: PathBuf,
    /// `Err` carries the sentence to show. Computed up front so a caller
    /// cannot forget to ask.
    writable: Result<(), String>,
}

/// A project's checkout — only one Eren may open at all (gate 2), which is
/// then also writable.
async fn project_tree(state: &AppState, id: Uuid) -> Result<Root, ApiError> {
    let path = project_root(state, id).await?;
    super::fs::may_open(&path).map_err(|why| (StatusCode::FORBIDDEN, why))?;
    Ok(Root {
        path,
        writable: Ok(()),
    })
}

/// A card's worktree. Writable only if the worktree manager owns it.
///
/// `browse_root` deliberately does *not* apply: `~/.eren` sits outside it in
/// a container, and `manages` is both tighter and the answer already used to
/// decide whether a run may go full-auto.
async fn task_tree(state: &AppState, id: Uuid) -> Result<Root, ApiError> {
    let stored: Option<String> = sqlx::query_scalar("SELECT worktree_path FROM tasks WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such task".to_string()))?;

    let Some(stored) = stored else {
        return Err((
            StatusCode::NOT_FOUND,
            "this task has no worktree — a project without version control \
             edits in place, so its files are on the project's own Files tab."
                .into(),
        ));
    };

    let path = PathBuf::from(stored);
    // Canonicalize *before* asking whether we manage it: a `worktree_path`
    // containing a symlink would otherwise pass a `starts_with` on raw text.
    let writable = match std::fs::canonicalize(&path) {
        Ok(canonical) if state.orchestrator.worktrees.manages(&canonical) => Ok(()),
        Ok(_) => Err("this worktree is not one Eren manages".to_string()),
        Err(_) => Err("this worktree is gone from disk".to_string()),
    };
    Ok(Root { path, writable })
}

async fn project_root(state: &AppState, id: Uuid) -> Result<PathBuf, ApiError> {
    let row = sqlx::query("SELECT path FROM projects WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such project".to_string()))?;
    Ok(PathBuf::from(row.get::<String, _>("path")))
}

#[derive(Deserialize)]
struct PathQuery {
    /// Project-relative; absent or empty means the project root.
    path: Option<String>,
}

async fn list_project(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    let tree = project_tree(&state, id).await?;
    list_in(&tree.path, q).await
}

async fn list_task(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    let tree = task_tree(&state, id).await?;
    list_in(&tree.path, q).await
}

async fn read_project(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    let tree = project_tree(&state, id).await?;
    read_in(&tree, q).await
}

async fn read_task(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    let tree = task_tree(&state, id).await?;
    read_in(&tree, q).await
}

async fn list_in(root: &Path, q: PathQuery) -> Result<Json<Value>, ApiError> {
    let root = root.to_path_buf();
    let root = &root;
    let rel = q.path.unwrap_or_default();
    let Some(dir) = resolve(&root, &rel) else {
        return Err((StatusCode::FORBIDDEN, "path is outside the project".into()));
    };
    if !dir.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "not a directory".into()));
    }

    let mut entries: Vec<Value> = vec![];
    let mut read = tokio::fs::read_dir(&dir).await.map_err(internal)?;
    while let Ok(Some(entry)) = read.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        let is_dir = file_type.is_dir();
        if is_dir && SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let size = if is_dir {
            None
        } else {
            entry.metadata().await.ok().map(|m| m.len())
        };
        entries.push(json!({
            "name": name,
            "path": relative(&root, &entry.path()),
            "kind": if is_dir { "dir" } else { "file" },
            "size": size,
        }));
    }
    // Directories first, then case-insensitive by name.
    entries.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                v["kind"].as_str().unwrap_or("") != "dir",
                v["name"].as_str().unwrap_or("").to_lowercase(),
            )
        };
        key(a).cmp(&key(b))
    });

    let rel_dir = relative(&root, &dir);
    let parent = if rel_dir.is_empty() {
        None
    } else {
        Some(
            Path::new(&rel_dir)
                .parent()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default(),
        )
    };
    Ok(Json(
        json!({ "path": rel_dir, "parent": parent, "entries": entries }),
    ))
}

async fn read_in(tree: &Root, q: PathQuery) -> Result<Json<Value>, ApiError> {
    let root = &tree.path;
    let rel = q.path.unwrap_or_default();
    if rel.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "path is required".into()));
    }
    let Some(file) = resolve(&root, &rel) else {
        return Err((StatusCode::FORBIDDEN, "path is outside the project".into()));
    };
    let meta = tokio::fs::metadata(&file).await.map_err(internal)?;
    if meta.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "that is a directory".into()));
    }
    let rel_path = relative(&root, &file);
    // `hash` is the editor's whole concurrency story, so it is null exactly
    // when the content is — no hash, no save button, and no way to reach a
    // save path for a file whose bytes the client never received.
    let read_only = tree.writable.as_ref().err().cloned();
    if meta.len() > MAX_FILE_BYTES {
        return Ok(Json(json!({
            "path": rel_path, "size": meta.len(), "tooLarge": true,
            "binary": false, "content": Value::Null, "hash": Value::Null,
            "readOnly": read_only,
        })));
    }

    let bytes = tokio::fs::read(&file).await.map_err(internal)?;
    // A NUL byte is the same heuristic git uses to call a blob binary.
    let binary = bytes.contains(&0) || String::from_utf8(bytes.clone()).is_err();
    Ok(Json(json!({
        "path": rel_path,
        "size": meta.len(),
        "tooLarge": false,
        "binary": binary,
        "content": if binary { Value::Null } else { json!(String::from_utf8_lossy(&bytes)) },
        "hash": if binary { Value::Null } else { json!(content_hash(&bytes)) },
        "readOnly": read_only,
    })))
}

/// The version token for a file: sha256 of its exact bytes, hex.
///
/// A content hash rather than mtime or size. The knowledge base needed a stored
/// `body_version` because its pointer could fall out of step with its text; a
/// file has no such pointer — the bytes *are* the version. mtime is worse than
/// it looks here: `git checkout` and `git worktree` rewrite it for reasons
/// unrelated to content, and two writes inside one filesystem tick are
/// indistinguishable. Size alone is collidable by a single edited character.
fn content_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[derive(Deserialize)]
struct WriteBody {
    /// Tree-relative, same vocabulary as every other path here.
    path: String,
    content: String,
    /// What the caller believes is on disk. `None` means "I am creating this".
    /// There is no way to opt out — an escape hatch is the check not existing.
    #[serde(default)]
    base_hash: Option<String>,
}

async fn write_project(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
    headers: HeaderMap,
    Json(body): Json<WriteBody>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    let tree = project_tree(&state, id).await?;
    write_in(&state, &tree, headers, body).await
}

async fn write_task(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
    headers: HeaderMap,
    Json(body): Json<WriteBody>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    let tree = task_tree(&state, id).await?;
    write_in(&state, &tree, headers, body).await
}

async fn write_in(
    state: &AppState,
    tree: &Root,
    headers: HeaderMap,
    body: WriteBody,
) -> Result<Json<Value>, ApiError> {
    // Gate 4.
    if !super::has_write_header(&headers) {
        return Err((
            StatusCode::BAD_REQUEST,
            "not a write from the dashboard".into(),
        ));
    }
    // Gate 2.
    if let Err(why) = &tree.writable {
        return Err((StatusCode::FORBIDDEN, why.clone()));
    }
    // Gates 1 and 3.
    let target =
        resolve_for_write(&tree.path, &body.path).map_err(|why| (StatusCode::BAD_REQUEST, why))?;

    let bytes = body.content.as_bytes();
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "this file is larger than Eren will serve, so it will not save it either".into(),
        ));
    }

    // Held across read-compare-write. Saves are human-paced, so one global
    // lock costs nothing and closes the window between checking the hash and
    // replacing the bytes; a per-path map would be more code and no more
    // correct.
    let _guard = state.file_writes.lock().await;

    let path = match &target {
        WriteTarget::Existing(p) => {
            let current = tokio::fs::read(p).await.map_err(internal)?;
            let current_hash = content_hash(&current);
            match &body.base_hash {
                Some(sent) if *sent == current_hash => {}
                Some(_) => {
                    return Err((
                        StatusCode::CONFLICT,
                        serde_json::to_string(&json!({
                            "error": "this file changed on disk since you opened it",
                            "currentHash": current_hash,
                            "currentContent": String::from_utf8_lossy(&current),
                        }))
                        .unwrap_or_else(|_| "this file changed on disk".into()),
                    ));
                }
                None => {
                    return Err((StatusCode::CONFLICT, "a file already exists there".into()));
                }
            }
            p.clone()
        }
        WriteTarget::New(p) => {
            if body.base_hash.is_some() {
                return Err((StatusCode::CONFLICT, "that file no longer exists".into()));
            }
            p.clone()
        }
    };

    write_atomically(&path, bytes).await.map_err(internal)?;

    Ok(Json(json!({
        "path": relative(&tree.path, &path),
        "size": bytes.len(),
        "hash": content_hash(bytes),
    })))
}

/// Write via a temp file in the same directory, then rename over the target.
///
/// The permission copy is not decoration: without it, saving a shell script or
/// a git hook silently strips its executable bit, and the failure shows up
/// somewhere else entirely.
///
/// The temp file is the one path in a save that `resolve_for_write` never
/// sees, so it has to be safe by construction. It used to be a predictable
/// `.{name}.eren-tmp` opened with `File::create`, which follows symlinks: an
/// agent could plant that name in its worktree pointing at the main repo's
/// `.git/hooks/post-merge`, and the next person to save `name` wrote their
/// bytes — and the exec bit — into a hook the squash-merge then ran. Now the
/// name is unguessable and the open is `create_new` (`O_EXCL`), which refuses
/// anything already there, a symlink included; the mode is set on the open
/// handle, never by path.
async fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let tmp = dir.join(format!(".{name}.{}.eren-tmp", Uuid::new_v4().simple()));

    let mode = tokio::fs::metadata(path)
        .await
        .ok()
        .map(|m| m.permissions());

    // Opened outside the cleanup block: if this fails, whatever is at `tmp`
    // is not ours, and removing it is not ours to do either.
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await?;

    let result = async {
        {
            use tokio::io::AsyncWriteExt;
            f.write_all(bytes).await?;
            if let Some(mode) = mode {
                f.set_permissions(mode).await?;
            }
            f.sync_all().await?;
        }
        drop(f);
        tokio::fs::rename(&tmp, path).await
    }
    .await;

    if result.is_err() {
        // Do not leave a dotfile behind in someone's checkout.
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// Where a save is allowed to land.
#[derive(Debug)]
enum WriteTarget {
    /// A file that already exists. Carries the *canonical* path, so writing
    /// "through" an in-repo symlink edits what it points at rather than
    /// replacing the link with a regular file.
    Existing(PathBuf),
    New(PathBuf),
}

/// Validate a destination that may not exist yet.
///
/// `resolve` cannot do this: `canonicalize` fails on a path with no file at the
/// end of it, which its own test asserts. Nor can it simply be relaxed —
/// `fs.rs` already explains why on `safe_dir_name`: a parent that canonicalizes
/// perfectly is meaningless if `../../..` can be appended to it. So the parent
/// is resolved and the final component is validated separately.
fn resolve_for_write(root: &Path, rel: &str) -> Result<WriteTarget, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err("a path is required".into());
    }
    if rel.contains('\0') {
        return Err("that is not a path".into());
    }
    if rel.ends_with('/') || rel.ends_with('\\') {
        return Err("that is a directory, not a file".into());
    }
    // Gate 1. Checked on the *requested* text as well as the resolved path,
    // because a path that escapes into `.git` via a symlink still lands there.
    for part in Path::new(rel).components() {
        let text = part.as_os_str().to_string_lossy();
        if text.eq_ignore_ascii_case(".git") {
            return Err("eren will not write inside .git".into());
        }
        if text == ".." {
            return Err("a path may not climb out with \"..\"".into());
        }
    }

    if let Some(existing) = resolve(root, rel) {
        if inside_git(&existing) {
            return Err("eren will not write inside .git".into());
        }
        let meta = std::fs::metadata(&existing).map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err("that is not a regular file".into());
        }
        return Ok(WriteTarget::Existing(existing));
    }

    // Nothing there yet. The parent must already exist inside the root —
    // deliberately no mkdir: a file editor that creates directories from a
    // path it was handed is a traversal amplifier with nothing to show for it.
    let (parent_rel, name) = match rel.rsplit_once('/') {
        Some((p, n)) => (p, n),
        None => ("", rel),
    };
    let name = safe_file_name(name)?;
    let parent =
        resolve(root, parent_rel).ok_or_else(|| "that folder does not exist".to_string())?;
    if !parent.is_dir() {
        return Err("that folder does not exist".into());
    }
    // The resolved parent, not just the requested text: a folder that is a
    // symlink to `.git/hooks` passes gate 1 and would let a save create a
    // hook that git then runs.
    if inside_git(&parent) {
        return Err("eren will not write inside .git".into());
    }
    let target = parent.join(name);
    // `symlink_metadata` and not `exists()`: a *dangling* symlink reports as
    // absent, and `File::create` on one writes wherever it points.
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err("something is already at that path".into());
    }
    Ok(WriteTarget::New(target))
}

/// Whether a resolved path has a `.git` component anywhere in it.
fn inside_git(path: &Path) -> bool {
    path.components().any(|part| {
        part.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(".git")
    })
}

/// The final component of a path being created.
///
/// Modelled on `fs::safe_dir_name`, with one deliberate difference: a leading
/// dot is allowed. That rule exists there because the folder browser hides
/// dotfiles, and its own comment says it is not a security rule — whereas
/// `.gitignore` and `.env.example` are ordinary files people edit.
fn safe_file_name(raw: &str) -> Result<&str, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("a file name is required".into());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("that file name contains a path separator".into());
    }
    if name == "." || name == ".." {
        return Err("that is not a file name".into());
    }
    Ok(name)
}

/// Caps for the @-mention crawl. A debounced keystroke must never be able to
/// walk a monorepo, so all three are deliberately small.
const SEARCH_MAX_RESULTS: usize = 20;
const SEARCH_MAX_VISITED: usize = 20_000;
const SEARCH_MAX_DEPTH: usize = 12;

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

/// Rank a project-relative path against a lowercased query. `None` = no match,
/// lower = better. Basename hits beat path hits, and ties break on path length
/// so shallow files float up.
fn score_path(rel: &str, q: &str) -> Option<u32> {
    let lower = rel.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    // Cheap tiebreaker: shorter paths first, capped so it can't outweigh tier.
    let len_penalty = (rel.len().min(200) / 8) as u32;

    let tier = if name == q {
        0
    } else if name.starts_with(q) {
        100
    } else if name.contains(q) {
        200
    } else if lower.contains(q) {
        300
    } else if is_subsequence(&lower, q) {
        400
    } else {
        return None;
    };
    Some(tier + len_penalty)
}

/// Every char of `needle` appearing in order within `haystack` — what makes
/// "wsapi" find "web/src/lib/api.ts".
fn is_subsequence(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|c| chars.any(|h| h == c))
}

async fn search(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(project_id): UrlPath<Uuid>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(project_id)).await?;
    let root = project_tree(&state, project_id).await?.path;
    let needle = q.q.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Ok(Json(json!({ "files": [], "truncated": false })));
    }
    let Some(root) = resolve(&root, "") else {
        return Err((StatusCode::NOT_FOUND, "project directory is missing".into()));
    };

    // Blocking walk on a worker thread: std::fs in a BFS with an explicit
    // queue, so depth is bounded without recursion.
    let (hits, truncated) = tokio::task::spawn_blocking(move || {
        let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::from([(root.clone(), 0)]);
        let mut scored: Vec<(u32, String, String)> = vec![];
        let mut visited = 0usize;
        let mut truncated = false;

        while let Some((dir, depth)) = queue.pop_front() {
            if depth > SEARCH_MAX_DEPTH {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                visited += 1;
                if visited > SEARCH_MAX_VISITED {
                    truncated = true;
                    break;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                // `file_type` on a DirEntry does not follow the link.
                let Ok(ft) = entry.file_type() else { continue };
                // Never follow symlinks: `resolve` guards a caller-supplied
                // path, but a crawl that followed one out of the project
                // would leak outside paths into the picker.
                if ft.is_symlink() {
                    continue;
                }
                if ft.is_dir() {
                    if SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                        continue;
                    }
                    queue.push_back((entry.path(), depth + 1));
                    continue;
                }
                let rel = relative(&root, &entry.path());
                if let Some(score) = score_path(&rel, &needle) {
                    scored.push((score, rel, name));
                }
            }
            if truncated {
                break;
            }
        }

        scored.sort_unstable();
        if scored.len() > SEARCH_MAX_RESULTS {
            scored.truncate(SEARCH_MAX_RESULTS);
            truncated = true;
        }
        (scored, truncated)
    })
    .await
    .map_err(internal)?;

    Ok(Json(json!({
        "files": hits.into_iter()
            .map(|(_, path, name)| json!({ "path": path, "name": name }))
            .collect::<Vec<_>>(),
        "truncated": truncated,
    })))
}

#[cfg(test)]
mod tests {
    use super::{
        content_hash, relative, resolve, resolve_for_write, safe_file_name, score_path,
        write_atomically, WriteTarget,
    };
    use std::path::Path;

    /// A scratch tree per test, so they cannot see each other's files.
    fn scratch(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("eren-write-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        root
    }

    /// The temp file of an old save had a predictable name and was opened
    /// following symlinks, so a link planted under that name redirected a
    /// person's save — bytes and exec bit — to wherever it pointed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_planted_temp_name_cannot_redirect_a_save() {
        let root = scratch("planted-tmp");
        let hook = root.join("post-merge");
        std::fs::write(&hook, "untouched").unwrap();
        std::os::unix::fs::symlink(&hook, root.join("src/.main.rs.eren-tmp")).unwrap();

        write_atomically(&root.join("src/main.rs"), b"saved")
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&hook).unwrap(), "untouched");
        assert_eq!(
            std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
            "saved"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_save_keeps_the_exec_bit_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("exec-bit");
        let script = root.join("src/run.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        write_atomically(&script, b"#!/bin/sh\necho hi\n")
            .await
            .unwrap();

        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let leftovers: Vec<_> = std::fs::read_dir(root.join("src"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".eren-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// Eren runs `git checkout` and `git merge` in these repos during
    /// squash-merge, so a writable `.git/hooks/pre-commit` is remote code
    /// execution reached from a page someone merely visited.
    ///
    /// **Do not delete this test.** If it starts failing, the write path has
    /// stopped being safe to expose, not the other way round.
    #[test]
    fn nothing_may_be_written_inside_dot_git() {
        let root = scratch("dotgit");
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::fs::write(root.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n").unwrap();

        for attempt in [
            ".git/hooks/pre-commit",
            ".git/config",
            ".git/hooks/new-hook",
            "src/../.git/config",
            // Case-insensitive filesystems are the common case on macOS.
            ".GIT/config",
            ".Git/hooks/pre-push",
        ] {
            assert!(
                resolve_for_write(&root, attempt).is_err(),
                "{attempt} must be refused"
            );
        }
    }

    /// A folder that is a symlink into `.git` passes the check on the
    /// requested text, so the resolved parent of a new file is checked too —
    /// otherwise a save through `src/hooks -> .git/hooks` creates a hook.
    #[cfg(unix)]
    #[test]
    fn a_new_file_through_a_link_into_dot_git_is_refused() {
        let root = scratch("dotgit-link");
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::os::unix::fs::symlink(root.join(".git/hooks"), root.join("src/hooks")).unwrap();
        assert!(resolve_for_write(&root, "src/hooks/post-checkout").is_err());
        assert!(!root.join(".git/hooks/post-checkout").exists());
    }

    #[test]
    fn a_new_file_needs_a_folder_that_already_exists() {
        let root = scratch("newfile");
        // Beside a file that exists: fine.
        assert!(matches!(
            resolve_for_write(&root, "src/lib.rs"),
            Ok(WriteTarget::New(_))
        ));
        // Into a folder that does not: refused rather than created. A file
        // editor that runs mkdir -p on a supplied path is a traversal
        // amplifier with nothing to show for it.
        assert!(resolve_for_write(&root, "nope/deeper/x.rs").is_err());
        assert!(!root.join("nope").exists());
    }

    #[test]
    fn an_existing_file_resolves_to_its_canonical_self() {
        let root = scratch("existing");
        match resolve_for_write(&root, "src/main.rs") {
            Ok(WriteTarget::Existing(p)) => {
                assert_eq!(p, std::fs::canonicalize(root.join("src/main.rs")).unwrap());
            }
            other => panic!("expected the existing file, got {other:?}"),
        }
        // A directory is not a file.
        assert!(resolve_for_write(&root, "src").is_err());
    }

    #[test]
    fn a_write_path_may_not_contain_dot_dot_even_when_it_would_land_inside() {
        // `resolve` allows this for reads, and the fs browser's own test
        // asserts `Documents/..` resolves. Writes are stricter on purpose:
        // the UI builds paths from listings and `relative()` hands back
        // normalised ones, so a `..` arriving here is never something this
        // dashboard asked for.
        let root = scratch("dotdot");
        assert!(resolve_for_write(&root, "src/../src/main.rs").is_err());
        assert!(resolve(&root, "src/../src/main.rs").is_some());
    }

    #[test]
    fn no_path_may_leave_the_tree() {
        let root = scratch("escape");
        for attempt in ["../outside.rs", "src/../../outside.rs", "/etc/passwd"] {
            assert!(
                resolve_for_write(&root, attempt).is_err(),
                "{attempt} must be refused"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_followed_for_targets_inside_and_refused_outside() {
        let root = scratch("symlink");
        // Out of the tree: the classic escape.
        std::os::unix::fs::symlink("/etc/passwd", root.join("escape.txt")).unwrap();
        assert!(resolve_for_write(&root, "escape.txt").is_err());

        // Inside the tree: writing "through" it must edit the target, not
        // replace the link with a regular file.
        std::os::unix::fs::symlink(root.join("src/main.rs"), root.join("alias.rs")).unwrap();
        match resolve_for_write(&root, "alias.rs") {
            Ok(WriteTarget::Existing(p)) => {
                assert_eq!(p, std::fs::canonicalize(root.join("src/main.rs")).unwrap());
            }
            other => panic!("expected the link's target, got {other:?}"),
        }

        // A *dangling* link reports as absent to `exists()`, and creating over
        // it would write wherever it points.
        std::os::unix::fs::symlink("/tmp/eren-nonexistent-xyz", root.join("dangling.rs")).unwrap();
        assert!(resolve_for_write(&root, "dangling.rs").is_err());
    }

    #[test]
    fn file_names_may_start_with_a_dot_but_may_not_be_paths() {
        // Unlike safe_dir_name: .gitignore and .env.example are ordinary files
        // people edit, and that rule is explicitly not a security one.
        assert!(safe_file_name(".gitignore").is_ok());
        assert!(safe_file_name(".env.example").is_ok());
        assert!(safe_file_name("a/b").is_err());
        assert!(safe_file_name("..").is_err());
        assert!(safe_file_name(".").is_err());
        assert!(safe_file_name("   ").is_err());
    }

    #[test]
    fn the_hash_is_of_the_exact_bytes() {
        assert_eq!(content_hash(b""), content_hash(b""));
        assert_ne!(content_hash(b"a\n"), content_hash(b"a\r\n"));
        // A single edited character has to move it — this is the whole point
        // of hashing rather than comparing sizes.
        assert_ne!(content_hash(b"hello"), content_hash(b"hellp"));
        assert_eq!(content_hash(b"abc").len(), 64);
    }

    #[test]
    fn resolve_keeps_paths_inside_the_project() {
        let root = std::env::temp_dir().join("eren-files-test");
        let nested = root.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("main.rs"), "fn main() {}").unwrap();

        // Empty path is the root itself.
        assert_eq!(
            resolve(&root, "").unwrap(),
            std::fs::canonicalize(&root).unwrap()
        );
        assert!(resolve(&root, "src/main.rs").is_some());
        // A leading slash is treated as project-relative, not absolute.
        assert!(resolve(&root, "/src/main.rs").is_some());
        // Traversal that lands back inside is fine…
        assert!(resolve(&root, "src/../src").is_some());
        // …but escaping the project is not.
        assert!(resolve(&root, "..").is_none());
        assert!(resolve(&root, "../../etc/passwd").is_none());
        // Nonexistent paths are rejected (canonicalize fails).
        assert!(resolve(&root, "src/nope.rs").is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn score_prefers_basename_matches_over_path_matches() {
        let exact = score_path("web/src/lib/api.ts", "api.ts").unwrap();
        let prefix = score_path("web/src/lib/api-client.ts", "api").unwrap();
        let substring = score_path("web/src/lib/myapi.ts", "api").unwrap();
        let in_path = score_path("web/api/src/thing.ts", "api").unwrap();
        assert!(exact < prefix, "exact basename should win");
        assert!(prefix < substring, "prefix beats mid-name substring");
        assert!(substring < in_path, "basename beats directory-only match");
    }

    #[test]
    fn shorter_paths_break_ties() {
        let shallow = score_path("api.ts", "api.ts").unwrap();
        let deep = score_path("a/b/c/d/e/f/g/h/api.ts", "api.ts").unwrap();
        assert!(shallow < deep);
    }

    #[test]
    fn subsequence_matches_rank_last_but_still_match() {
        // "wsapi" -> web/src/lib/api.ts
        let sub = score_path("web/src/lib/api.ts", "wsapi").unwrap();
        let direct = score_path("web/src/lib/api.ts", "api").unwrap();
        assert!(direct < sub);
        assert_eq!(score_path("web/src/lib/api.ts", "zzq"), None);
    }

    #[test]
    fn score_is_case_insensitive_on_the_path_side() {
        // The handler lowercases the query; the path side must match that.
        assert!(score_path("Web/Src/API.ts", "api.ts").is_some());
    }

    #[test]
    fn relative_strips_the_root_and_uses_forward_slashes() {
        let root = std::env::temp_dir().join("eren-files-rel");
        std::fs::create_dir_all(root.join("a")).unwrap();
        let canonical = std::fs::canonicalize(&root).unwrap();
        assert_eq!(relative(&root, &canonical), "");
        assert_eq!(relative(&root, &canonical.join("a")), "a");
        assert_eq!(
            relative(&root, Path::new(&canonical).join("a/b").as_path()),
            "a/b"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
