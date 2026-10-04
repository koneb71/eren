//! Changing an app with an agent, and undoing it.
//!
//! An app is a project with its own repository, so a change to one is an
//! ordinary card: the orchestrator gives it a worktree, runs the agent, and
//! produces a diff, exactly as it does for the user's own code. Nothing here
//! reimplements any of that. What this module adds is the two things an app
//! needs that a repository does not.
//!
//! **It lands by itself.** A card against your code stops in review because a
//! person has to decide whether the change belongs in their project. An app's
//! change has nowhere else to go — reviewing a diff that *is* the whole app is
//! what running the app is for, and asking someone to read a patch before they
//! can see whether "make the header blue" worked turns a gallery back into a
//! task board. The repository being landed onto is `~/.eren/apps/<slug>`,
//! which Eren created and owns; no diff of the user's code is ever merged
//! without them.
//!
//! **So the undo has to be real**, and that is what `base_commit` is for. It is
//! recorded before the run starts, because after the fact there is no way to
//! ask git where the branch stood.
//!
//! Two things landing deliberately does *not* do:
//!
//! * It does not apply destructive DDL. The manifest is read back off disk and
//!   pushed through [`super::set_manifest`], so the ordinary
//!   additive-applies / destructive-waits gate runs unchanged. An agent that
//!   drops a column still cannot do it silently.
//! * It does not touch a card whose merge conflicted. That card stays in
//!   `review`, where the diff view and the Merge button already work — the
//!   failure mode falls back onto the flow every other task uses rather than
//!   into something built specially for it.

use super::App;
use crate::worktrees::manager::{self, Worktree, WorktreeManager};
use crate::Db;
use sqlx::Row;
use uuid::Uuid;

/// One attempt to change an app.
#[derive(Debug, Clone)]
pub struct Build {
    pub id: Uuid,
    pub app_id: Uuid,
    /// The card that did the work. `None` once that card has been deleted —
    /// history outlives it.
    pub task_id: Option<Uuid>,
    pub brief: String,
    pub base_commit: Option<String>,
    pub landed_commit: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

const SELECT_BUILD: &str = "SELECT id, app_id, task_id, brief, base_commit, landed_commit,
        status, error, created_at
   FROM app_builds";

fn row_to_build(r: &sqlx::postgres::PgRow) -> Build {
    Build {
        id: r.get("id"),
        app_id: r.get("app_id"),
        task_id: r.get("task_id"),
        brief: r.get("brief"),
        base_commit: r.get("base_commit"),
        landed_commit: r.get("landed_commit"),
        status: r.get("status"),
        error: r.get("error"),
        created_at: r.get("created_at"),
    }
}

/// Where an app's branch stands right now.
///
/// Read before the card is created rather than when it finishes: by then the
/// commit it started from is only recoverable by guessing.
pub async fn base_commit(app: &App) -> Option<String> {
    manager::head(&app.path).await
}

/// Whether a change to this app is already under way.
///
/// **One at a time, and the reason is the undo.** Two builds started together
/// both record the *same* `base_commit`, because neither has landed yet. The
/// second to land is then the newest landed build, so undoing it resets past
/// the first one as well — silently discarding a change nobody asked to lose,
/// which is precisely what [`revertible`] exists to prevent and cannot detect
/// once the rows are written.
pub async fn in_progress(db: &Db, app_id: Uuid) -> anyhow::Result<Option<String>> {
    let brief: Option<String> = sqlx::query_scalar(
        "SELECT brief FROM app_builds
          WHERE app_id = $1 AND status = 'running'
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(app_id)
    .fetch_optional(&db.pool)
    .await?;
    Ok(brief)
}

/// Note that a card is going to change this app.
pub async fn record(
    db: &Db,
    app_id: Uuid,
    task_id: Uuid,
    brief: &str,
    base_commit: Option<&str>,
) -> anyhow::Result<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app_builds (app_id, task_id, brief, base_commit, status)
         VALUES ($1, $2, $3, $4, 'running') RETURNING id",
    )
    .bind(app_id)
    .bind(task_id)
    .bind(brief)
    .bind(base_commit)
    .fetch_one(&db.pool)
    .await?;
    Ok(id)
}

/// Every build of an app, newest first.
pub async fn list(db: &Db, app_id: Uuid) -> anyhow::Result<Vec<Build>> {
    let rows = sqlx::query(&format!(
        "{SELECT_BUILD} WHERE app_id = $1 ORDER BY created_at DESC LIMIT 50"
    ))
    .bind(app_id)
    .fetch_all(&db.pool)
    .await?;
    Ok(rows.iter().map(row_to_build).collect())
}

pub async fn get(db: &Db, id: Uuid) -> anyhow::Result<Option<Build>> {
    let row = sqlx::query(&format!("{SELECT_BUILD} WHERE id = $1"))
        .bind(id)
        .fetch_optional(&db.pool)
        .await?;
    Ok(row.as_ref().map(row_to_build))
}

/// Which build, if any, can still be undone.
///
/// **Only the newest landed one.** `base_commit` says where the branch stood
/// before *that* build, so resetting to an older one would throw away every
/// build since without ever saying so. A conflicted or failed build changed
/// nothing to undo, and a reverted one has been undone already.
///
/// Pure, and takes the list newest-first as [`list`] returns it, so the rule is
/// tested here rather than asserted inside a component.
pub fn revertible(builds: &[Build]) -> Option<Uuid> {
    let newest = builds.iter().find(|b| b.status == "landed")?;
    // A build that never recorded where main was cannot promise to put it back.
    newest.base_commit.as_ref()?;
    Some(newest.id)
}

/// The commit message a landed build leaves behind.
pub fn commit_message(brief: &str) -> String {
    let first = brief
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let short: String = first.chars().take(72).collect();
    if short.is_empty() {
        "eren: change this app".to_string()
    } else {
        format!("eren: {short}")
    }
}

/// What the run's outcome means for the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// Merge the worktree onto main and re-read the manifest.
    Land,
    /// The run did not produce anything worth landing.
    Failed,
}

/// Read a terminal run status as a settlement.
///
/// Pure and separate from the database work so the table can be tested: there
/// is no DB harness here, and "which statuses land" is exactly the rule worth
/// pinning.
pub fn settlement_for(status: eren_shared::RunStatus) -> Option<Settlement> {
    use eren_shared::RunStatus::*;
    match status {
        Completed => Some(Settlement::Land),
        Failed | Canceled => Some(Settlement::Failed),
        // Not terminal — the run is still going, so there is nothing to settle.
        _ => None,
    }
}

/// Finish whatever build this task was doing.
///
/// **A no-op for every task that is not an app build**, which is what makes it
/// safe to call from the orchestrator on every completed run. The caller treats
/// a failure here as a warning: a build that did not land must never turn a
/// completed run into a failed one.
pub async fn settle(
    db: &Db,
    worktrees: &WorktreeManager,
    task_id: Uuid,
    status: eren_shared::RunStatus,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    let Some(settlement) = settlement_for(status) else {
        return Ok(());
    };
    let Some(row) = sqlx::query(
        "SELECT b.id, b.app_id, b.brief, t.worktree_path, t.branch
           FROM app_builds b JOIN tasks t ON t.id = b.task_id
          WHERE b.task_id = $1 AND b.status = 'running'",
    )
    .bind(task_id)
    .fetch_optional(&db.pool)
    .await?
    else {
        return Ok(());
    };

    let build_id: Uuid = row.get("id");
    if settlement == Settlement::Failed {
        return mark(db, build_id, "failed", reason, None).await;
    }

    let app = super::get(db, row.get::<Uuid, _>("app_id"))
        .await?
        .ok_or_else(|| anyhow::anyhow!("this app was uninstalled while it was being changed"))?;

    // No worktree means the run edited in place, which an app's project never
    // does — but a row can say anything, and merging nothing is not an error
    // worth inventing a status for.
    let (Some(path), Some(branch)): (Option<String>, Option<String>) =
        (row.get("worktree_path"), row.get("branch"))
    else {
        return mark(
            db,
            build_id,
            "landed",
            None,
            manager::head(&app.path).await.as_deref(),
        )
        .await;
    };

    let worktree = Worktree {
        path: path.into(),
        branch,
    };
    let brief: String = row.get("brief");
    if let Err(e) = worktrees
        .squash_merge(&app.path, &worktree, "main", &commit_message(&brief))
        .await
    {
        // The card stays where the orchestrator left it — in review, with a
        // working diff and Merge button. Nothing further to build for this.
        return mark(db, build_id, "conflicted", Some(&e.to_string()), None).await;
    }

    let landed = manager::head(&app.path).await;

    // The files are merged either way. What can still go wrong is the manifest
    // they contain, and rolling the files back over a syntax error would throw
    // away work that is mostly right — so the build is landed, with the problem
    // recorded, and the app page's broken-manifest banner takes over.
    let note = adopt_manifest(db, &app).await.err().map(|e| e.to_string());
    mark(db, build_id, "landed", note.as_deref(), landed.as_deref()).await?;

    // Reviewing is what this run replaced, so the card is done.
    sqlx::query("UPDATE tasks SET board_column='done' WHERE id=$1")
        .bind(task_id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

/// Put an app back the way it was before its newest build.
///
/// Refuses anything but the newest landed build, for the reason [`revertible`]
/// gives. `reset --hard` discards uncommitted edits in the app's folder — the
/// caller says so before asking.
pub async fn revert(db: &Db, id: Uuid) -> anyhow::Result<App> {
    let build = get(db, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no such build"))?;
    let builds = list(db, build.app_id).await?;
    if revertible(&builds) != Some(id) {
        anyhow::bail!(
            "only the most recent landed change can be undone — anything older would \
             silently throw away the changes made after it"
        );
    }
    let base = build.base_commit.as_deref().ok_or_else(|| {
        anyhow::anyhow!("this build never recorded where the app stood before it")
    })?;
    let app = super::get(db, build.app_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("this app is no longer installed"))?;

    manager::reset_hard(&app.path, base).await?;
    // Recorded the moment the folder is back, and before the manifest is
    // adopted: from here on the files *are* reverted, and a status still
    // reading `landed` because the step after this one failed would offer the
    // undo a second time and describe the app as something it no longer is.
    sqlx::query("UPDATE app_builds SET status='reverted' WHERE id=$1")
        .bind(id)
        .execute(&db.pool)
        .await?;

    // Same gate as landing: the files are back, and the tables follow through
    // the ordinary plan. Reverting an added column is a *drop*, so it waits.
    adopt_manifest(db, &app).await?;

    super::get(db, build.app_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("this app is no longer installed"))
}

/// Take the manifest now on disk as the app's manifest.
///
/// Everything a landing or a revert changes about the *app*, as opposed to the
/// folder, happens here — through the same function a person's edit uses, so
/// there is one path that reconciles a schema and one gate in front of it.
async fn adopt_manifest(db: &Db, app: &App) -> anyhow::Result<()> {
    let text = tokio::fs::read_to_string(eren_shared::brand::app_manifest(&app.path))
        .await
        .map_err(|_| anyhow::anyhow!("the change removed {}", super::MANIFEST_FILE))?;
    if text.trim() == app.manifest.trim() {
        return Ok(());
    }
    super::set_manifest(db, app, &text).await?;
    Ok(())
}

async fn mark(
    db: &Db,
    id: Uuid,
    status: &str,
    error: Option<&str>,
    landed_commit: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE app_builds
            SET status = $2, error = $3, landed_commit = $4,
                landed_at = CASE WHEN $2 = 'landed' THEN now() ELSE landed_at END
          WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(error)
    .bind(landed_commit)
    .execute(&db.pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eren_shared::RunStatus;

    fn build(status: &str, base: Option<&str>) -> Build {
        Build {
            id: Uuid::new_v4(),
            app_id: Uuid::nil(),
            task_id: None,
            brief: String::new(),
            base_commit: base.map(str::to_string),
            landed_commit: None,
            status: status.to_string(),
            error: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn only_the_newest_landed_build_can_be_undone() {
        // The bug this prevents is silent and unrecoverable: resetting to an
        // older build's `base_commit` throws away every build after it, and
        // git does not ask twice.
        let newest = build("landed", Some("aaa"));
        let older = build("landed", Some("bbb"));
        let list = vec![newest.clone(), older];
        assert_eq!(revertible(&list), Some(newest.id));
    }

    #[test]
    fn a_build_that_changed_nothing_offers_no_undo() {
        for status in ["running", "conflicted", "failed", "reverted"] {
            assert_eq!(revertible(&[build(status, Some("aaa"))]), None, "{status}");
        }
        assert_eq!(revertible(&[]), None);
    }

    #[test]
    fn a_landed_build_that_never_recorded_a_base_offers_no_undo() {
        // Better than a button that cannot keep its promise: an app installed
        // before its repository had a commit has nothing to go back to.
        assert_eq!(revertible(&[build("landed", None)]), None);
    }

    #[test]
    fn a_newer_failure_does_not_hide_the_undo_underneath_it() {
        // A change that failed left the folder exactly as the last landed one
        // did, so that one is still the newest thing to undo.
        let landed = build("landed", Some("aaa"));
        let list = vec![
            build("failed", None),
            build("conflicted", None),
            landed.clone(),
        ];
        assert_eq!(revertible(&list), Some(landed.id));
    }

    #[test]
    fn only_a_completed_run_lands() {
        assert_eq!(settlement_for(RunStatus::Completed), Some(Settlement::Land));
        assert_eq!(settlement_for(RunStatus::Failed), Some(Settlement::Failed));
        assert_eq!(
            settlement_for(RunStatus::Canceled),
            Some(Settlement::Failed)
        );
        // A run still going settles nothing — the build stays `running` and
        // the row is picked up when it actually finishes.
        assert_eq!(settlement_for(RunStatus::Running), None);
        assert_eq!(settlement_for(RunStatus::Queued), None);
    }

    #[test]
    fn a_commit_message_is_one_readable_line() {
        assert_eq!(
            commit_message("add a total column"),
            "eren: add a total column"
        );
        // A brief is a textarea, so it arrives with newlines in it.
        assert_eq!(
            commit_message("add a total\n\nand a chart"),
            "eren: add a total"
        );
        assert_eq!(commit_message("\n  make it blue  \n"), "eren: make it blue");
        assert_eq!(commit_message("   "), "eren: change this app");
        let long = commit_message(&"x".repeat(200));
        assert!(long.len() < 90, "a subject line that long is a paragraph");
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb;

    /// A build whose run never gets to the end of `execute_task_run` — here,
    /// cancelled while still queued — ends with it, rather than reading
    /// `running` forever and refusing every later change to the app.
    #[tokio::test]
    async fn a_build_ends_with_a_run_that_never_got_going() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let orch = t.orchestrator(dir.path());
        orch.set_queue_paused(true).await.unwrap();
        let (ws, project) = t.project(dir.path(), false).await;
        let app: Uuid = sqlx::query_scalar(
            "INSERT INTO apps (project_id, workspace_id, slug, name, schema_name, manifest,
                               manifest_sha256)
             VALUES ($1, $2, 'built', 'Built', 'app_built', '', '') RETURNING id",
        )
        .bind(project)
        .bind(ws)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        let card = t.card(project, "make the header blue").await;
        let build = record(&t.db, app, card, "make the header blue", Some("abc"))
            .await
            .unwrap();
        assert!(in_progress(&t.db, app).await.unwrap().is_some());

        let run = orch.enqueue_task(card).await.unwrap();
        orch.cancel_idle(run).await.unwrap();

        let b = get(&t.db, build).await.unwrap().unwrap();
        assert_eq!(b.status, "failed");
        assert!(in_progress(&t.db, app).await.unwrap().is_none());
        t.finish().await;
    }
}
