use super::{internal, require_write, ApiError};
use crate::auth::Caller;
use crate::AppState;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use eren_core::scope::Owned;
use eren_core::worktrees::manager::{self, ensure_repo_state, Vcs};
use eren_shared::{ReasoningEffort, TierChoice};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/projects", get(list).post(create))
        .route("/projects/space", post(create_space))
        .route("/projects/{id}", get(one).patch(update).delete(unload))
        .route("/projects/{id}/brain", get(get_brain).put(put_brain))
        .route("/projects/{id}/brain/revisions", get(brain_revisions))
        .route("/projects/{id}/storage", get(storage))
        .route("/projects/{id}/worktrees", get(worktrees))
        .route("/projects/{id}/worktrees/reclaim", post(reclaim_worktrees))
        .route("/projects/{id}/checkout", get(checkout))
        .route("/projects/{id}/checkout/stash", post(stash_checkout))
        .route("/projects/{id}/checkout/commit", post(commit_checkout))
        .route("/projects/{id}/checkout/pull", post(pull_checkout))
        .route("/projects/{id}/checkout/push", post(push_checkout))
}

const PROJECT_COLUMNS: &str = "id, path, name, default_branch, workspace_id, vcs, vcs_note, \
     full_auto_opt_in, kind, github_repo, default_engine, default_tier, default_effort";

fn project_json(r: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": r.get::<Uuid, _>("id"),
        "path": r.get::<String, _>("path"),
        "name": r.get::<String, _>("name"),
        "defaultBranch": r.get::<String, _>("default_branch"),
        "workspaceId": r.get::<Uuid, _>("workspace_id"),
        "vcs": r.get::<String, _>("vcs"),
        "vcsNote": r.get::<Option<String>, _>("vcs_note"),
        "fullAutoOptIn": r.get::<bool, _>("full_auto_opt_in"),
        "kind": r.get::<String, _>("kind"),
        // Resolved lazily on first use and cached, so this is NULL until some
        // GitHub feature has asked. Absent means "not asked yet", never "not a
        // GitHub project".
        "githubRepo": r.get::<Option<String>, _>("github_repo"),
        // Null means inherit, everywhere. A project that pins nothing behaves
        // exactly as it did before these existed.
        "defaultEngine": r.get::<Option<String>, _>("default_engine"),
        "defaultTier": r.get::<Option<String>, _>("default_tier"),
        "defaultEffort": r.get::<Option<String>, _>("default_effort"),
    })
}

/// One project by id, whatever kind it is.
///
/// Deliberately without the `kind = 'repo'` filter the list carries. An app is
/// a project, and its folder is the only place its source lives — so the files
/// editor has to be able to reach it. Filtered here as well would mean a page
/// that renders with a header reading "Project" and every setting silently
/// defaulted, which is what happened while this route did not exist.
async fn one(
    State(state): State<AppState>,
    caller: Caller,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    let row = sqlx::query(&format!(
        "SELECT {PROJECT_COLUMNS} FROM projects WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "no such project".to_string()))?;

    // The point of use for the repository's identity: this is what the project
    // page fetches, so a GitHub project learns its own name the first time
    // somebody opens it, and every surface after that reads a column. Resolving
    // is a `git remote get-url` at most once per project, and never for a
    // project that has no remote to read.
    let mut body = project_json(&row);
    if row.get::<Option<String>, _>("github_repo").is_none() {
        if let Some(slug) = eren_core::github::repo::resolve(&state.db, id).await {
            body["githubRepo"] = Value::String(slug);
        }
    }
    Ok(Json(body))
}

/// Take a project out of Eren. Its folder stays exactly where it is.
///
/// "Remove" next to a filesystem path reads as "delete my code", so this is
/// called unload everywhere it is offered and the confirm says outright that
/// nothing on disk is touched. What goes is Eren's own record — the cards,
/// their runs and comments, the chats — plus the worktrees and `eren/*`
/// branches Eren created, which is the one thing it *would* leave behind.
///
/// Until this existed, the only way to remove a project was to delete its whole
/// workspace, which has no UI either. Load the wrong folder once and it was in
/// the sidebar forever.
async fn unload(
    State(state): State<AppState>,
    caller: Caller,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    // Same shape as deleting a card: a live agent owns its worktree, and
    // pulling the project row out from under a running process leaves it
    // writing into a directory nothing will ever look at.
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM runs r JOIN tasks t ON t.id = r.task_id
             WHERE t.project_id = $1 AND r.status NOT IN ('completed','failed','canceled'))",
    )
    .bind(id)
    .fetch_one(&state.db.pool)
    .await
    .map_err(internal)?;
    if live {
        return Err((
            StatusCode::CONFLICT,
            "an agent is still working in this project — cancel its run first".into(),
        ));
    }

    // Before the row goes: afterwards nothing knows the repository's path, and
    // a worktree whose repo cannot be named can never be removed by git again.
    // Best-effort, and the unload proceeds either way — a folder left behind is
    // recoverable, a project you cannot remove is not.
    if let Ok(held) = held_for(&state, id).await {
        let (path, _) = git_project(&state, id).await?;
        let repo = std::path::PathBuf::from(&path);
        for h in &held {
            let wt = eren_core::worktrees::manager::Worktree {
                path: h.path.clone(),
                branch: h.branch.clone(),
            };
            if let Err(e) = state.orchestrator.worktrees.remove(&repo, &wt).await {
                tracing::warn!(branch = %h.branch, error = %e, "could not remove worktree while unloading");
            }
        }
    }

    // Containers outlive rows too.
    if let Err(e) = eren_core::previews::stop_for_project(&state.db, id).await {
        tracing::warn!(project = %id, error = %e, "could not stop previews while unloading");
    }

    let done = sqlx::query("DELETE FROM projects WHERE id = $1")
        .bind(id)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    if done.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "no such project".into()));
    }
    Ok(Json(json!({ "unloaded": true })))
}

/// The project's Brain — the standing context every run in it starts with.
async fn get_brain(
    State(state): State<AppState>,
    caller: Caller,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    let brain = eren_core::brain::load(&state.db, id)
        .await
        .map_err(internal)?;
    // An unwritten brain is an empty one, not a 404: the editor opens on it.
    Ok(Json(json!({
        "body": brain.as_ref().map(|b| b.body.clone()).unwrap_or_default(),
        "enabled": brain.as_ref().map(|b| b.enabled).unwrap_or(true),
        "hash": brain.as_ref().map(|b| b.hash.clone()).unwrap_or_else(|| eren_core::brain::hash("")),
        "updatedAt": brain.as_ref().and_then(|b| b.updated_at),
        "maxChars": eren_core::brain::MAX_CHARS,
    })))
}

#[derive(Deserialize)]
struct BrainBody {
    body: String,
    #[serde(default = "yes")]
    enabled: bool,
    /// What the editor loaded. Absent only for a first write; a mismatch is a
    /// 409 rather than an overwrite — the same rule the files editor and the
    /// wiki carry, and the answer to two tabs open on the same text.
    hash: Option<String>,
}

fn yes() -> bool {
    true
}

async fn put_brain(
    State(state): State<AppState>,
    caller: Caller,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(body): Json<BrainBody>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    use eren_core::brain::SaveError;
    match eren_core::brain::save(
        &state.db,
        id,
        &body.body,
        body.enabled,
        body.hash.as_deref(),
    )
    .await
    {
        Ok(b) => Ok(Json(json!({
            "body": b.body,
            "enabled": b.enabled,
            "hash": b.hash,
            "updatedAt": b.updated_at,
        }))),
        // Carries what is there now, so the editor can show the difference
        // rather than only refusing.
        Err(SaveError::Stale(now)) => Err((
            StatusCode::CONFLICT,
            format!(
                "this was edited somewhere else since you opened it. Reload to see the \
                 current text before saving over it.\n\n{}",
                now.body
            ),
        )),
        Err(SaveError::Secret(why)) => Err((StatusCode::BAD_REQUEST, why)),
        Err(SaveError::Other(e)) => Err(internal(e)),
    }
}

async fn brain_revisions(
    State(state): State<AppState>,
    caller: Caller,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    let rows = eren_core::brain::revisions(&state.db, id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        "revisions": rows.iter().map(|(id, body, at)| json!({
            "id": id, "body": body, "savedAt": at,
        })).collect::<Vec<_>>(),
    })))
}

/// Everything this project is holding, in one answer.
///
/// The parts existed already and were scattered: checkouts in the Files tab,
/// preview images in the Previews tab, and the per-run leftovers nowhere at
/// all. Nobody could answer "what is this costing me" without visiting two
/// tabs and knowing to ask — which is how 2.9 GB of worktrees went unnoticed
/// until somebody measured the directory by hand.
///
/// Composed from the existing accounting rather than a second implementation:
/// `manager::inventory` and `previews::disk` are the same calls the two tabs
/// make, so this page cannot disagree with them.
async fn storage(
    State(state): State<AppState>,
    caller: Caller,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    let held = held_for(&state, id).await.unwrap_or_default();

    // Preview rows for this project, with what each is holding. Images are
    // Eren's own now, so their size is a real number rather than the 0 B the
    // panel used to report for every compose stack.
    let previews = sqlx::query(
        "SELECT p.id, p.status, p.image_kept, t.title
           FROM previews p LEFT JOIN tasks t ON t.id = p.task_id
          WHERE p.project_id = $1 AND p.status IN ('building','running','idle','failed')",
    )
    .bind(id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;

    let (image_bytes, image_reclaimable) =
        eren_core::previews::disk(&state.db).await.unwrap_or((0, 0));

    // Kept, and said so rather than shown as a number with a dead button next
    // to it. Run history is what a reconnecting client replays from, so
    // trimming it is the operator's standing decision — `EREN_EVENT_RETENTION_DAYS`
    // (`eren_core::retention`), not a button here.
    let history: (i64, i64) = sqlx::query_as(
        "SELECT count(*), coalesce(sum(pg_column_size(e.payload)), 0)
           FROM events e
           JOIN runs r ON r.id = e.run_id
           JOIN tasks t ON t.id = r.task_id
          WHERE t.project_id = $1",
    )
    .bind(id)
    .fetch_one(&state.db.pool)
    .await
    .unwrap_or((0, 0));

    let checkout_bytes: u64 = held.iter().map(|h| h.bytes).sum();
    Ok(Json(json!({
        "checkouts": {
            "bytes": checkout_bytes,
            "count": held.len(),
            "reclaimable": held.iter().filter(|h| h.reclaimable()).count(),
            "reclaimableBytes": held.iter().filter(|h| h.reclaimable()).map(|h| h.bytes).sum::<u64>(),
            "items": held.iter().map(|h| json!({
                "branch": h.branch,
                "bytes": h.bytes,
                "reclaimable": h.reclaimable(),
                "keptBecause": h.kept_because(),
            })).collect::<Vec<_>>(),
        },
        "previews": {
            // Workspace-wide, and labelled as such by the caller: Docker
            // images are not attributable to one project without asking Docker
            // per image, and a per-project figure that was really a global one
            // would be a worse lie than an honest global.
            "bytes": image_bytes,
            "reclaimable": image_reclaimable,
            "items": previews.iter().map(|r| json!({
                "id": r.get::<Uuid, _>("id"),
                "status": r.get::<String, _>("status"),
                "imageKept": r.get::<bool, _>("image_kept"),
                "title": r.get::<Option<String>, _>("title"),
            })).collect::<Vec<_>>(),
        },
        "history": { "events": history.0, "bytes": history.1 },
        "total": checkout_bytes + image_bytes,
    })))
}

/// What this project's finished cards are still holding on disk.
///
/// Previews got a disk figure and a reclaim link; worktrees are the larger
/// artifact and got neither, so they accumulated silently — 2.9 GB across 22
/// directories on the machine this was written on, every one of them belonging
/// to a card that had already landed.
async fn worktrees(
    State(state): State<AppState>,
    caller: Caller,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    let held = held_for(&state, id).await?;

    // Which card each directory belongs to, so the list reads as work rather
    // than as paths. A directory with no row is exactly the kind this sweep
    // exists for — a bake-off variant whose run cascaded away, or a workflow
    // fan-out worktree whose id was never written down at all.
    let owners = sqlx::query(
        "SELECT worktree_path, title FROM tasks
         WHERE project_id = $1 AND worktree_path IS NOT NULL",
    )
    .bind(id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;

    let rows: Vec<Value> = held
        .iter()
        .map(|h| {
            let title = owners
                .iter()
                .find(|r| r.get::<String, _>("worktree_path") == h.path.to_string_lossy())
                .map(|r| r.get::<String, _>("title"));
            json!({
                "path": h.path,
                "branch": h.branch,
                "bytes": h.bytes,
                "dirty": h.dirty,
                "landed": h.landed,
                "reclaimable": h.reclaimable(),
                "keptBecause": h.kept_because(),
                "title": title,
            })
        })
        .collect();

    Ok(Json(json!({
        "worktrees": rows,
        "bytes": held.iter().map(|h| h.bytes).sum::<u64>(),
        "reclaimable": held.iter().filter(|h| h.reclaimable()).count(),
        "reclaimableBytes": held.iter().filter(|h| h.reclaimable()).map(|h| h.bytes).sum::<u64>(),
    })))
}

/// Release the ones that can be proved safe, and say why each of the rest stays.
///
/// Explicit rather than automatic, for the reason `previews::reclaim_disk` is:
/// disk you can see and choose to free is a different thing from disk something
/// frees behind you.
async fn reclaim_worktrees(
    State(state): State<AppState>,
    caller: Caller,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    let (path, _) = git_project(&state, id).await?;
    let repo = std::path::PathBuf::from(&path);
    let held = held_for(&state, id).await?;

    // A card whose agent is working right now owns its worktree, whatever git
    // thinks of the branch.
    let busy: Vec<String> = sqlx::query_scalar(
        "SELECT t.worktree_path FROM tasks t
         WHERE t.project_id = $1 AND t.worktree_path IS NOT NULL
           AND EXISTS (SELECT 1 FROM runs r WHERE r.task_id = t.id
                       AND r.status NOT IN ('completed','failed','canceled'))",
    )
    .bind(id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;

    let mut freed = 0u64;
    let mut released: Vec<Value> = Vec::new();
    let mut kept: Vec<Value> = Vec::new();
    for h in &held {
        let running = busy.iter().any(|p| *p == h.path.to_string_lossy());
        let why = if running {
            Some("its agent is still working")
        } else {
            h.kept_because()
        };
        if let Some(why) = why {
            kept.push(json!({ "branch": h.branch, "why": why }));
            continue;
        }
        let wt = eren_core::worktrees::manager::Worktree {
            path: h.path.clone(),
            branch: h.branch.clone(),
        };
        match state.orchestrator.worktrees.remove(&repo, &wt).await {
            Ok(()) => {
                freed += h.bytes;
                released.push(json!({ "branch": h.branch, "bytes": h.bytes }));
            }
            Err(e) => {
                tracing::warn!(branch = %h.branch, error = %e, "could not reclaim worktree");
                kept.push(json!({ "branch": h.branch, "why": "git would not remove it" }));
            }
        }
    }

    // The rows that named what was just removed would otherwise point at
    // nothing, and the Files tab reads them to build its tree picker.
    for r in &released {
        sqlx::query(
            "UPDATE tasks SET worktree_path=NULL, branch=NULL WHERE project_id=$1 AND branch=$2",
        )
        .bind(id)
        .bind(r["branch"].as_str().unwrap_or_default())
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    }

    Ok(Json(
        json!({ "released": released, "kept": kept, "bytes": freed }),
    ))
}

async fn held_for(
    state: &AppState,
    id: Uuid,
) -> Result<Vec<eren_core::worktrees::manager::Held>, ApiError> {
    let row = sqlx::query("SELECT path, vcs, default_branch FROM projects WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such project".to_string()))?;
    if row.get::<String, _>("vcs") != "git" {
        return Ok(vec![]);
    }
    manager::inventory(
        &state.orchestrator.worktrees,
        std::path::Path::new(&row.get::<String, _>("path")),
        &row.get::<String, _>("default_branch"),
    )
    .await
    .map_err(internal)
}

/// The project's own checkout, and what is standing in a merge's way.
///
/// The merge guard has always been able to *name* these files; nothing could
/// read them, so "commit or stash them and merge again" was an instruction you
/// could only follow in a terminal. Read-only, and it runs the same query the
/// guard does, so the two cannot disagree about which files count.
async fn checkout(
    State(state): State<AppState>,
    caller: Caller,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    let (path, vcs) = git_project(&state, id).await?;
    if !vcs {
        // Not an error: a project that edits in place has no merge to block,
        // so the honest answer is an empty one.
        return Ok(Json(json!({ "vcs": false, "branch": null, "dirty": [] })));
    }
    let repo = std::path::Path::new(&path);
    let (branch, dirty) = manager::checkout_status(repo).await.map_err(internal)?;
    // (behind, ahead) against the upstream; null when there is none — the UI
    // renders that as "publish", not as zeroes.
    let standing = manager::ahead_behind(repo).await;
    let has_remote = manager::remote_url(repo, "origin").await.is_some();
    Ok(Json(json!({
        "vcs": true,
        "path": path,
        "branch": branch,
        "behind": standing.map(|(b, _)| b),
        "ahead": standing.map(|(_, a)| a),
        "hasRemote": has_remote,
        "dirty": dirty.iter().map(|f| json!({
            "index": f.index.to_string(),
            "worktree": f.worktree.to_string(),
            "path": f.path,
        })).collect::<Vec<_>>(),
    })))
}

/// Set the checkout's changes aside. Reversible, and the response says how.
async fn stash_checkout(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_write(&headers, "this changes your checkout")?;
    caller.require(&state, Owned::Project(id)).await?;
    let path = dirty_git_project(&state, id).await?;
    let repo = std::path::Path::new(&path);
    manager::stash(repo, "eren: set aside so a card could merge")
        .await
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(json!({
        "stashed": true,
        "undo": "git stash pop",
    })))
}

/// Commit the checkout's changes as their own commit, under the person's name.
///
/// Their work, so their commit — never folded into the card's, which is the
/// whole reason the merge refuses in the first place.
async fn commit_checkout(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, ApiError> {
    require_write(&headers, "this commits in your checkout")?;
    caller.require(&state, Owned::Project(id)).await?;
    let path = dirty_git_project(&state, id).await?;
    let repo = std::path::Path::new(&path);
    // The editor's commit box sends a message; the older merge-unblock button
    // sends nothing and keeps its old wording.
    let message = body
        .as_ref()
        .and_then(|Json(b)| b.get("message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("Work in progress")
        .to_string();
    let wrote = manager::commit_all(repo, &message)
        .await
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(json!({
        "committed": wrote,
        "undo": "git reset --soft HEAD~1",
    })))
}

/// Fast-forward the checkout from its upstream. A pull that would merge or
/// rebase is refused by git itself (`--ff-only`), and that message comes back
/// verbatim — whose history wins is not a button's decision.
async fn pull_checkout(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_write(&headers, "this changes your checkout")?;
    caller.require(&state, Owned::Project(id)).await?;
    let (path, vcs) = git_project(&state, id).await?;
    if !vcs {
        return Err((
            StatusCode::CONFLICT,
            "this project has no git repository".into(),
        ));
    }
    let out = manager::pull_ff(std::path::Path::new(&path))
        .await
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(json!({ "pulled": true, "detail": out.trim() })))
}

/// Push the current branch, publishing it if it has never been pushed.
async fn push_checkout(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_write(&headers, "this pushes your checkout")?;
    caller.require(&state, Owned::Project(id)).await?;
    let (path, vcs) = git_project(&state, id).await?;
    if !vcs {
        return Err((
            StatusCode::CONFLICT,
            "this project has no git repository".into(),
        ));
    }
    let out = manager::push_current(std::path::Path::new(&path))
        .await
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(json!({ "pushed": true, "detail": out.trim() })))
}

/// A project that has a repository, or an explanation of why the request makes
/// no sense for it.
async fn git_project(state: &AppState, id: Uuid) -> Result<(String, bool), ApiError> {
    let row = sqlx::query("SELECT path, vcs FROM projects WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such project".to_string()))?;
    Ok((row.get("path"), row.get::<String, _>("vcs") == "git"))
}

/// The same, for the two routes that change something: they need a repository
/// *and* something to act on. Refusing an empty checkout rather than writing an
/// empty commit or an empty stash keeps a double-click from doing anything.
async fn dirty_git_project(state: &AppState, id: Uuid) -> Result<String, ApiError> {
    let (path, vcs) = git_project(state, id).await?;
    if !vcs {
        return Err((
            StatusCode::BAD_REQUEST,
            "this project has no version control, so there is nothing to commit or stash".into(),
        ));
    }
    let (_, dirty) = manager::checkout_status(std::path::Path::new(&path))
        .await
        .map_err(internal)?;
    if dirty.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            "your checkout has no uncommitted changes — nothing to set aside".into(),
        ));
    }
    Ok(path)
}

#[derive(Deserialize)]
pub struct WorkspaceFilter {
    pub workspace_id: Option<Uuid>,
    /// Which kinds to list. Absent means 'repo' — the original behaviour, and
    /// what the Projects page, the sidebar and every board flow expect.
    /// `space` lists document spaces; `chat` lists what a conversation can be
    /// scoped to (repos and spaces, never apps).
    pub kind: Option<String>,
}

async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Query(filter): Query<WorkspaceFilter>,
) -> Result<Json<Value>, ApiError> {
    let workspaces = caller.workspace_filter(&state, filter.workspace_id).await?;
    // Apps are projects too — that is what gives them worktrees, diffs and
    // previews — but they belong in the gallery, not in a list of the
    // repositories someone works in. Spaces are the third kind: document
    // folders with no git, listed only where a chat can be pointed at them.
    let kinds = match filter.kind.as_deref() {
        None | Some("repo") => "('repo')",
        Some("space") => "('space')",
        Some("chat") => "('repo','space')",
        Some(other) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown kind filter {other}"),
            ))
        }
    };
    let rows = sqlx::query(&format!(
        "SELECT {PROJECT_COLUMNS} FROM projects
         WHERE kind IN {kinds} AND ($1::uuid[] IS NULL OR workspace_id = ANY($1))
         ORDER BY created_at DESC"
    ))
    .bind(workspaces)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;
    let projects: Vec<Value> = rows.iter().map(project_json).collect();
    Ok(Json(json!({ "projects": projects })))
}

#[derive(Deserialize)]
struct CreateSpace {
    workspace_id: Uuid,
    name: String,
}

/// Create a *space*: a project that is a folder of documents, not a repo.
///
/// It gets a managed folder under `~/.eren/spaces/<slug>` the way apps get
/// one under `~/.eren/apps/` — the person did not hand us a path, so the
/// path is ours to manage. `vcs='none'` is what already means "no worktrees,
/// no branches, runs happen in place"; `kind='space'` is what keeps it out of
/// the Projects page, the board flows and app lookups, all of which filter on
/// kind. What a space is *for*: pointing a chat at a folder of documents —
/// drop files in, ask questions about them.
async fn create_space(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<CreateSpace>,
) -> Result<Json<Value>, ApiError> {
    caller
        .require(&state, Owned::Workspace(body.workspace_id))
        .await?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "a space needs a name".into()));
    }
    let slug = {
        let s: String = name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        if s.is_empty() {
            "space".to_string()
        } else {
            s.chars().take(40).collect()
        }
    };
    // Suffix on collision rather than ON CONFLICT-reuse: two spaces named
    // "notes" are two spaces, not one folder shared by surprise.
    let base = super::fs::spaces_root();
    let mut dir = base.join(&slug);
    let mut n = 2;
    while dir.exists() {
        dir = base.join(format!("{slug}-{n}"));
        n += 1;
    }
    tokio::fs::create_dir_all(&dir).await.map_err(internal)?;

    let row = sqlx::query(&format!(
        "INSERT INTO projects (workspace_id, path, name, kind, vcs, vcs_note)
         VALUES ($1, $2, $3, 'space', 'none', 'a document space — not a repository')
         RETURNING {PROJECT_COLUMNS}"
    ))
    .bind(body.workspace_id)
    .bind(dir.to_string_lossy().into_owned())
    .bind(name)
    .fetch_one(&state.db.pool)
    .await
    .map_err(internal)?;
    Ok(Json(project_json(&row)))
}


#[derive(Deserialize)]
struct CreateProject {
    workspace_id: Uuid,
    path: String,
    name: Option<String>,
    default_branch: Option<String>,
}

async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<CreateProject>,
) -> Result<Json<Value>, ApiError> {
    caller
        .require(&state, Owned::Workspace(body.workspace_id))
        .await?;
    let path = std::path::Path::new(&body.path);
    if !path.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{} is not a directory", body.path),
        ));
    }
    // The same sandbox the folder browser, `mkdir` and `git-init` keep: a
    // project is loaded from under the browse root and nowhere else. This
    // door used to take any directory that existed, and a project at `/` is
    // every file the server can read, in the Files tab and in the terminal.
    // Apps, spaces and clones are made by their own routes under folders
    // Eren manages, and `fs::may_open` admits those too.
    if super::fs::sandboxed(&super::fs::browse_root(), path).is_none() {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "{} is outside the folder Eren is allowed to browse. Set {} if that is \
                 deliberate.",
                body.path,
                eren_shared::brand::env_name("BROWSE_ROOT")
            ),
        ));
    }
    let mine = caller.workspaces(&state).await?;
    let taken_elsewhere = || {
        (
            StatusCode::CONFLICT,
            format!(
                "{} is already a project in another account — a folder can belong to one account only",
                body.path
            ),
        )
    };
    // Asked before anything touches the folder (initializing a repository
    // below), not only by the insert's guard, which is what closes the race.
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT workspace_id FROM projects WHERE path = $1")
            .bind(&body.path)
            .fetch_optional(&state.db.pool)
            .await
            .map_err(internal)?;
    if let (Some(owner), Some(mine)) = (owner, &mine) {
        if !mine.contains(&owner) {
            return Err(taken_elsewhere());
        }
    }
    let default_branch = body.default_branch.unwrap_or_else(|| "main".into());

    // Initialize a repository if this folder needs one. A worktree is what
    // keeps an agent out of the user's files and what makes review possible,
    // so it's worth creating a repo to get one — but a folder that can't have
    // one still becomes a project, with its runs happening in place.
    // Record the branch the repository is actually on. Assuming "main" for a
    // repo that uses something else makes every later `worktree add` fail.
    let repo = ensure_repo_state(path, &default_branch).await;
    let default_branch = repo.branch;
    let (vcs, vcs_note) = match repo.vcs {
        Vcs::Git => ("git", None),
        Vcs::None(reason) => ("none", Some(reason)),
    };

    let name = body.name.unwrap_or_else(|| {
        path.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".into())
    });
    // Loading a folder that is already a project moves it here — but only
    // from one of the caller's own workspaces. `path` is unique across the
    // machine, so without the `WHERE` loading another account's folder would
    // take their project, cards and all. That case updates nothing and returns
    // no row, answered in words rather than as a 404: the person is looking at
    // the folder on disk and needs to know why it will not load.
    let row = sqlx::query(
        "INSERT INTO projects (workspace_id, path, name, default_branch, vcs, vcs_note)
         VALUES ($1,$2,$3,$4,$5,$6)
         ON CONFLICT (path) DO UPDATE SET name = EXCLUDED.name,
             workspace_id = EXCLUDED.workspace_id,
             vcs = EXCLUDED.vcs, vcs_note = EXCLUDED.vcs_note
         WHERE $7::uuid[] IS NULL OR projects.workspace_id = ANY($7)
         RETURNING id, (xmax = 0) AS fresh",
    )
    .bind(body.workspace_id)
    .bind(&body.path)
    .bind(&name)
    .bind(&default_branch)
    .bind(vcs)
    .bind(&vcs_note)
    .bind(&mine)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?
    .ok_or_else(taken_elsewhere)?;
    // A repository that just became a project starts with its owner's rules
    // (`eren_core::rules`). Not one being loaded again, and not a folder with
    // no repository, where a commit is not possible.
    let rules = if row.get::<bool, _>("fresh") && vcs == "git" {
        Some(eren_core::rules::seed_project(&state.db, body.workspace_id, path).await)
    } else {
        None
    };
    Ok(Json(json!({
        "id": row.get::<Uuid, _>("id"),
        "name": name,
        "vcs": vcs,
        "vcsNote": vcs_note,
        "rules": rules,
    })))
}

#[derive(Deserialize)]
struct UpdateProject {
    /// Let agents work in this project without stopping to ask.
    ///
    /// This is the switch the FullAuto gate has always read and nothing has
    /// ever been able to set — which is why every run prompted for every
    /// action with no way to turn it off.
    full_auto_opt_in: Option<bool>,
    default_branch: Option<String>,
    /// What to call it. The folder's basename until somebody says otherwise —
    /// two clones of the same repository are both called `cli`, and only the
    /// person looking at them knows which is which.
    name: Option<String>,
    /// What new cards here start on. `Some(None)` clears the pin and goes back
    /// to inheriting; the field being absent leaves it alone. Typed so a client
    /// that invents a tier is a 422 rather than a row the orchestrator has to
    /// shrug at when a run actually starts.
    #[serde(default, deserialize_with = "double_option")]
    default_engine: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    default_tier: Option<Option<TierChoice>>,
    #[serde(default, deserialize_with = "double_option")]
    default_effort: Option<Option<ReasoningEffort>>,
}

/// Tell "absent" apart from "explicitly null".
///
/// `COALESCE` cannot express "clear this", and without the distinction there is
/// no way to un-pin a project once pinned — the same shape `chats` needed for
/// its own inheritable settings.
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(d).map(Some)
}

async fn update(
    State(state): State<AppState>,
    caller: Caller,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(body): Json<UpdateProject>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(id)).await?;
    // Rejected rather than trimmed to nothing: a project with a blank name is
    // an unclickable row in the sidebar.
    let name = match body.name.as_deref().map(str::trim) {
        Some("") => {
            return Err((StatusCode::BAD_REQUEST, "a project needs a name".into()));
        }
        other => other.map(str::to_string),
    };

    // Working without prompts is only safe because the run happens in an
    // eren-managed worktree you review before merging. A project with no
    // version control edits your files directly, so there is nothing to
    // review and nothing to roll back — refuse rather than quietly downgrade
    // it later and leave the toggle looking enabled.
    if body.full_auto_opt_in == Some(true) {
        let vcs: String = sqlx::query_scalar("SELECT vcs FROM projects WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db.pool)
            .await
            .map_err(internal)?
            .ok_or((StatusCode::NOT_FOUND, "no such project".to_string()))?;
        if vcs != "git" {
            return Err((
                StatusCode::BAD_REQUEST,
                "this project has no version control, so runs edit your files directly —                  there is no isolated worktree to review or roll back. Working without                  prompts needs a git project."
                    .into(),
            ));
        }
    }

    // `PROJECT_COLUMNS` and `project_json`, not a hand-written pair.
    //
    // This used to return its own shorter list, which omitted `kind` and
    // `github_repo` — and the client types the response as a whole `Project`
    // and puts it straight into state. So one click on the autonomy toggle made
    // the GitHub chip and the Import-issues button disappear until a reload,
    // because the fields behind them came back absent rather than unchanged.
    // `$n IS NOT DISTINCT FROM` would be neater but cannot express "leave it";
    // the paired boolean is what carries absent-versus-null through to SQL.
    let row = sqlx::query(&format!(
        "UPDATE projects
            SET full_auto_opt_in = COALESCE($2, full_auto_opt_in),
                default_branch   = COALESCE($3, default_branch),
                name             = COALESCE($4, name),
                default_engine   = CASE WHEN $5 THEN $6  ELSE default_engine END,
                default_tier     = CASE WHEN $7 THEN $8  ELSE default_tier   END,
                default_effort   = CASE WHEN $9 THEN $10 ELSE default_effort END
          WHERE id = $1
      RETURNING {PROJECT_COLUMNS}"
    ))
    .bind(id)
    .bind(body.full_auto_opt_in)
    .bind(body.default_branch.as_deref())
    .bind(name.as_deref())
    .bind(body.default_engine.is_some())
    .bind(body.default_engine.clone().flatten())
    .bind(body.default_tier.is_some())
    .bind(body.default_tier.flatten().map(|t| t.as_str().to_string()))
    .bind(body.default_effort.is_some())
    .bind(
        body.default_effort
            .flatten()
            .map(|e| e.as_str().to_string()),
    )
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "no such project".to_string()))?;

    Ok(Json(project_json(&row)))
}

#[cfg(test)]
mod tests {
    /// `create` is the one door a person hands an arbitrary path to, and it
    /// has to meet the sandbox before it inserts: the row is what the Files
    /// tab and the terminal later open.
    #[test]
    fn create_sandboxes_the_path_before_it_inserts() {
        let src = include_str!("projects.rs");
        let start = src.find("async fn create(").expect("create exists");
        let body = &src[start..];
        let end = body.find("\n}\n").map(|i| i + 3).unwrap_or(body.len());
        let body = &body[..end];
        let sandbox = body
            .find("fs::sandboxed(&super::fs::browse_root()")
            .expect("create asks the browse-root sandbox");
        let insert = body
            .find("INSERT INTO projects")
            .expect("create inserts the project");
        assert!(sandbox < insert, "the sandbox check comes before the insert");
    }
}
