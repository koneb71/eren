//! Apps: things you install, switch on, and use without leaving the dashboard.
//!
//! Two runtimes behind one manifest. A **module** declares models, views and
//! actions, and Eren's own dashboard renders it — nothing arbitrary executes,
//! so there is no container, no iframe and nothing to approve. A **container**
//! app is the escape hatch for work that genuinely needs code, and carries the
//! apparatus that implies.
//!
//! An app is a *project*, on disk under `~/.eren/apps/<slug>`. That is what
//! makes this small: worktrees, diffs, the files editor, chat and the run
//! orchestrator already exist and already key on a project, so "change this
//! app" is a card and nothing had to be built twice.
//!
//! Neither runtime ever receives a database connection. An app declares what it
//! wants to store and reaches its rows through Eren, which is what keeps
//! agent-written code away from everything it did not declare.

pub mod bridge;
pub mod build;
pub mod bundle;
pub mod client_js;
pub mod data;
pub mod expr;
pub mod grants;
pub mod host;
pub mod introspect;
pub mod manifest;
pub mod query;
pub mod render;
pub mod run;
pub mod runtime;
pub mod scaffold;
pub mod schema;
pub mod scope;
pub mod skeleton;
pub mod sync;

pub use manifest::{Manifest, ManifestError, Runtime};
pub use schema::Stmt;
pub use scope::Scope;

use crate::Db;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The manifest's filename inside an app's folder.
pub const MANIFEST_FILE: &str = eren_shared::brand::APP_MANIFEST;

/// Where apps live.
///
/// Beside worktrees and attachments rather than among the user's own code: an
/// app is Eren's to create, move and delete, and putting them in someone's
/// projects directory would mean a `git status` somewhere unrelated growing
/// entries nobody asked for.
pub fn root() -> PathBuf {
    match eren_shared::brand::var("APPS_DIR") {
        Some(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => eren_shared::brand::home().join("apps"),
    }
}

/// The Postgres schema holding an app's tables.
///
/// A schema per app rather than prefixed tables in `public`, which is what Odoo
/// does for historical reasons. Uninstall becomes `DROP SCHEMA … CASCADE`,
/// export is scoped by construction, and two apps may both declare a `note`.
///
/// Built from the slug, which is already a validated DNS label, with dashes
/// folded to underscores. There is nothing in the result that needs quoting.
pub fn schema_name(slug: &str) -> String {
    let cleaned: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("app_{cleaned}")
}

/// A DNS-safe, unique label for an app.
///
/// Named for the app so the address bar says what you are looking at, and
/// suffixed from its id so two apps called "Notes" can coexist. Unlike a
/// preview's slug this is an *identity* — it decides which grants and which
/// tables a request gets — which is why the column is `UNIQUE`.
pub fn slug(name: &str, id: &Uuid) -> String {
    let mut label = String::new();
    let mut last_dash = true;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            label.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !label.is_empty() {
            label.push('-');
            last_dash = true;
        }
    }
    let trimmed: String = label.trim_matches('-').chars().take(32).collect();
    let trimmed = trimmed.trim_end_matches('-');
    let tail = &id.simple().to_string()[..6];
    if trimmed.is_empty() {
        format!("app-{tail}")
    } else {
        format!("{trimmed}-{tail}")
    }
}

/// The hash a manifest is recognised by.
///
/// Used to tell whether the copy in the folder and the copy in the database
/// have drifted — which they can, because a person may edit either.
pub fn digest(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// An app, as everything outside this module sees it.
#[derive(Debug, Clone)]
pub struct App {
    pub id: Uuid,
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    pub slug: String,
    pub name: String,
    pub icon: String,
    pub summary: String,
    pub brief: String,
    pub runtime: Runtime,
    pub active: bool,
    pub schema: String,
    pub manifest: String,
    pub path: PathBuf,
}

impl App {
    /// The parsed manifest.
    ///
    /// Reparsed rather than cached in the struct: the text is the record, and a
    /// stale parse would be a second source of truth for what the app is.
    pub fn parsed(&self) -> Result<Manifest, ManifestError> {
        manifest::parse(&self.manifest)
    }
}

fn row_to_app(r: &sqlx::postgres::PgRow) -> App {
    use sqlx::Row;
    App {
        id: r.get("id"),
        project_id: r.get("project_id"),
        workspace_id: r.get("workspace_id"),
        slug: r.get("slug"),
        name: r.get("name"),
        icon: r.get("icon"),
        summary: r.get("summary"),
        brief: r.get("brief"),
        runtime: Runtime::parse(r.get::<String, _>("runtime").as_str()).unwrap_or(Runtime::Module),
        active: r.get::<String, _>("state") == "active",
        schema: r.get("schema_name"),
        manifest: r.get("manifest"),
        path: PathBuf::from(r.get::<String, _>("path")),
    }
}

const SELECT_APP: &str = "SELECT a.id, a.project_id, a.workspace_id, a.slug, a.name, a.icon,
        a.summary, a.brief, a.runtime, a.state, a.schema_name, a.manifest, p.path
   FROM apps a JOIN projects p ON p.id = a.project_id";

pub async fn list(db: &Db, workspace_id: Option<Uuid>) -> anyhow::Result<Vec<App>> {
    let rows = sqlx::query(&format!(
        "{SELECT_APP} WHERE $1::uuid IS NULL OR a.workspace_id = $1
          ORDER BY a.name ASC"
    ))
    .bind(workspace_id)
    .fetch_all(&db.pool)
    .await?;
    Ok(rows.iter().map(row_to_app).collect())
}

pub async fn get(db: &Db, id: Uuid) -> anyhow::Result<Option<App>> {
    let row = sqlx::query(&format!("{SELECT_APP} WHERE a.id = $1"))
        .bind(id)
        .fetch_optional(&db.pool)
        .await?;
    Ok(row.as_ref().map(row_to_app))
}

pub async fn by_slug(db: &Db, slug: &str) -> anyhow::Result<Option<App>> {
    let row = sqlx::query(&format!("{SELECT_APP} WHERE a.slug = $1"))
        .bind(slug)
        .fetch_optional(&db.pool)
        .await?;
    Ok(row.as_ref().map(row_to_app))
}

/// Create an app from a manifest, folder and all.
///
/// The manifest is validated *before* anything is written, so a manifest an
/// agent got wrong leaves no directory, no project row and no repository
/// behind — a half-made app is worse than none, because it is something a
/// person then has to notice and clean up.
pub async fn install(
    db: &Db,
    workspace_id: Uuid,
    manifest_text: &str,
    brief: &str,
) -> anyhow::Result<App> {
    let parsed = manifest::parse(manifest_text)?;

    let id = Uuid::new_v4();
    let slug = slug(&parsed.name, &id);
    let path = root().join(&slug);
    let schema = schema_name(&slug);

    tokio::fs::create_dir_all(&path).await?;

    // A repository, for the same reason every project has one: a run edits in a
    // worktree, and the worktree is what makes the change reviewable and
    // revertible. Refused nesting is reported now rather than at the end of a
    // generation run someone paid for.
    let repo = crate::worktrees::manager::ensure_repo(&path, "main").await;
    if let crate::worktrees::manager::Vcs::None(reason) = repo {
        let _ = tokio::fs::remove_dir_all(&path).await;
        anyhow::bail!(
            "an app needs its own repository and this folder cannot have one — {reason}. \
             Set EREN_APPS_DIR to somewhere outside any repository."
        );
    }

    write_manifest(&path, manifest_text).await?;

    // A container app is created with a tree scaffolded *from its manifest* —
    // a router and one views/<screen>.html per menu entry, CRUD when the entry
    // names a model — for the same reason the schema is reconciled below
    // rather than on first use: an app that installed should be an app that
    // works. The floor case still holds: even an empty menu yields the files
    // its Dockerfile needs, because `COPY package*.json ./` fails outright
    // when nothing matches.
    for (name, body) in skeleton::files(&parsed) {
        let file = path.join(&name);
        if let Some(parent) = file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(file, body).await?;
    }

    // …and committed, because `ensure_repo` ran before any of it was written.
    // A file that is not committed is not on `main`, so `git worktree add`
    // hands the next agent asked to change this app an empty folder.
    commit(&path, &format!("Add {}", parsed.name)).await?;

    let mut tx = db.pool.begin().await?;
    let project_id: Uuid = sqlx::query_scalar(
        "INSERT INTO projects (workspace_id, path, name, default_branch, vcs, kind)
         VALUES ($1, $2, $3, 'main', 'git', 'app')
         ON CONFLICT (path) DO UPDATE SET name = EXCLUDED.name, kind = 'app'
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(path.to_string_lossy().as_ref())
    .bind(&parsed.name)
    .fetch_one(&mut *tx)
    .await?;

    let scopes: Vec<String> = parsed
        .scopes
        .iter()
        .map(|s| s.as_str().to_string())
        .collect();
    sqlx::query(
        "INSERT INTO apps (id, project_id, workspace_id, slug, name, icon, summary, brief,
                           runtime, state, schema_name, manifest, manifest_sha256,
                           requested_scopes, port)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'active', $10, $11, $12, $13, $14)",
    )
    .bind(id)
    .bind(project_id)
    .bind(workspace_id)
    .bind(&slug)
    .bind(&parsed.name)
    .bind(&parsed.icon)
    .bind(&parsed.summary)
    .bind(brief)
    .bind(parsed.runtime.as_str())
    .bind(&schema)
    .bind(manifest_text)
    .bind(digest(manifest_text))
    .bind(&scopes)
    .bind(parsed.port.map(i32::from))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let app = get(db, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the app vanished immediately after being created"))?;

    // A brand-new app has no tables, so every statement is a create and nothing
    // can be destructive. It applies here rather than on first use, so an app
    // that installed is an app that works.
    reconcile_schema(db, &app).await?;
    Ok(app)
}

/// Replace an app's manifest, in the database and in its folder, and reconcile
/// its tables with what the new one declares.
///
/// Both copies, always, and through one function — the same shape the Files tab
/// uses when a person saves. The database copy is what the renderer reads on
/// every page load; the folder copy is what git tracks and what export reads.
///
/// The returned outcome says whether the tables followed or are waiting: a
/// manifest that only adds is live immediately, and one that would destroy
/// something is saved but not yet enacted.
pub async fn set_manifest(
    db: &Db,
    app: &App,
    manifest_text: &str,
) -> anyhow::Result<(Manifest, SchemaOutcome)> {
    let parsed = manifest::parse(manifest_text)?;
    if parsed.runtime != app.runtime {
        anyhow::bail!(
            "this app is a {} app and the new manifest says {} — changing what runs an \
             app is not an edit, it is a different app",
            app.runtime.as_str(),
            parsed.runtime.as_str()
        );
    }
    write_manifest(&app.path, manifest_text).await?;
    // A no-op when this call *came* from a landed merge or a revert, where the
    // file on disk is already what git has.
    commit(&app.path, &format!("Update {}", parsed.name)).await?;

    let scopes: Vec<String> = parsed
        .scopes
        .iter()
        .map(|s| s.as_str().to_string())
        .collect();
    sqlx::query(
        "UPDATE apps SET manifest = $2, manifest_sha256 = $3, name = $4, icon = $5,
                         summary = $6, requested_scopes = $7, port = $8, updated_at = now()
          WHERE id = $1",
    )
    .bind(app.id)
    .bind(manifest_text)
    .bind(digest(manifest_text))
    .bind(&parsed.name)
    .bind(&parsed.icon)
    .bind(&parsed.summary)
    .bind(&scopes)
    .bind(parsed.port.map(i32::from))
    .execute(&db.pool)
    .await?;

    // Read the app back before reconciling: the plan is computed from the
    // manifest that was just stored, not the one this call was handed.
    let updated = get(db, app.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("this app was removed while it was being edited"))?;
    let outcome = reconcile_schema(db, &updated).await?;
    Ok((parsed, outcome))
}

async fn write_manifest(path: &Path, text: &str) -> anyhow::Result<()> {
    tokio::fs::write(path.join(MANIFEST_FILE), text).await?;
    Ok(())
}

/// Commit what is in an app's folder.
///
/// Called after every write Eren makes to it, so the branch and the folder
/// never disagree. Two things depend on that being true: a build's worktree is
/// cut from `main` and would otherwise open on a stale manifest, and
/// `squash_merge` checks out the base branch in this repository, which a dirty
/// tree can refuse.
///
/// Best-effort by design — a folder that cannot be committed is worth a warning
/// and not worth failing an install over, since the manifest in the database is
/// what the app actually runs on.
pub(crate) async fn commit(path: &Path, message: &str) -> anyhow::Result<()> {
    if let Err(e) = crate::worktrees::manager::commit_all(path, message).await {
        tracing::warn!(path = %path.display(), error = %e, "could not commit an app's folder");
    }
    Ok(())
}

/// A migration waiting to be read.
#[derive(Debug, Clone)]
pub struct PendingPlan {
    pub id: Uuid,
    pub statements: Vec<Stmt>,
}

/// What reconciling an app's schema did, or wants permission to do.
#[derive(Debug, Clone, Default)]
pub struct SchemaOutcome {
    /// Statements that ran. Only ever safe ones.
    pub applied: Vec<Stmt>,
    /// The whole plan, held back because part of it destroys something.
    pub pending: Option<PendingPlan>,
}

/// Bring an app's tables in line with what its manifest declares.
///
/// Additive plans run immediately: creating a table or adding a nullable column
/// cannot lose anything, and asking about them would be a dialog that always
/// gets the same answer — which is how a gate stops being read.
///
/// A plan containing *anything* destructive runs **nothing** and waits, whole.
/// Not the safe half now and the rest on approval: the statements are ordered
/// and a half-applied migration is a schema in a state no manifest describes,
/// and recomputing the remainder later would mean running SQL nobody saw.
pub async fn reconcile_schema(db: &Db, app: &App) -> anyhow::Result<SchemaOutcome> {
    let parsed = app.parsed()?;
    let live = introspect::tables(db, &app.schema).await?;
    let plan = schema::plan(&app.schema, &parsed.models, &live);

    if !schema::needs_approval(&plan) {
        run(db, &plan).await?;
        // Anything outstanding was computed against a manifest that has now
        // been superseded, so leaving it would offer a stale question.
        sqlx::query(
            "UPDATE app_schema_plans SET status = 'discarded'
              WHERE app_id = $1 AND status = 'pending'",
        )
        .bind(app.id)
        .execute(&db.pool)
        .await?;
        return Ok(SchemaOutcome {
            applied: plan,
            pending: None,
        });
    }

    // One outstanding question per app: a new proposal replaces the old rather
    // than queueing behind it, or approving the older one would apply a
    // migration computed against a manifest that has since changed.
    let mut tx = db.pool.begin().await?;
    sqlx::query(
        "UPDATE app_schema_plans SET status = 'discarded'
          WHERE app_id = $1 AND status = 'pending'",
    )
    .bind(app.id)
    .execute(&mut *tx)
    .await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app_schema_plans (app_id, statements, destructive)
         VALUES ($1, $2, TRUE) RETURNING id",
    )
    .bind(app.id)
    .bind(serde_json::to_value(&plan)?)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(SchemaOutcome {
        applied: Vec::new(),
        pending: Some(PendingPlan {
            id,
            statements: plan,
        }),
    })
}

/// The migration this app is waiting on, if any.
pub async fn pending_plan(db: &Db, app_id: Uuid) -> anyhow::Result<Option<PendingPlan>> {
    let row = sqlx::query(
        "SELECT id, statements FROM app_schema_plans
          WHERE app_id = $1 AND status = 'pending'",
    )
    .bind(app_id)
    .fetch_optional(&db.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    use sqlx::Row;
    Ok(Some(PendingPlan {
        id: row.get("id"),
        statements: serde_json::from_value(row.get("statements"))?,
    }))
}

/// Run a plan a person has read.
///
/// The stored statements, not freshly derived ones — what was approved is what
/// executes. All of it in one transaction, so a migration that fails half way
/// leaves the schema as it was rather than in a shape nothing describes.
///
/// Scoped to the app the request names: a plan id alone would let one app's
/// page apply another app's migration.
pub async fn apply_plan(db: &Db, app_id: Uuid, plan_id: Uuid) -> anyhow::Result<Vec<Stmt>> {
    // Claimed, not read: the status check is in the WHERE of the write, so of
    // two approvals racing — the app page and the inbox, a double click — one
    // runs the DDL and the other is told it has been dealt with.
    let statements: serde_json::Value = sqlx::query_scalar(
        "UPDATE app_schema_plans SET status = 'applying'
          WHERE id = $1 AND app_id = $2 AND status = 'pending'
        RETURNING statements",
    )
    .bind(plan_id)
    .bind(app_id)
    .fetch_optional(&db.pool)
    .await?
    .ok_or_else(|| anyhow::anyhow!("that change has already been dealt with"))?;
    let statements: Vec<Stmt> = match serde_json::from_value(statements) {
        Ok(s) => s,
        Err(e) => {
            settle_plan(db, plan_id, "failed", Some(&e.to_string())).await?;
            return Err(e.into());
        }
    };

    if let Err(e) = run(db, &statements).await {
        settle_plan(db, plan_id, "failed", Some(&e.to_string())).await?;
        return Err(e);
    }
    settle_plan(db, plan_id, "applied", None).await?;
    Ok(statements)
}

/// Record how a claimed plan went — only against the claim, so nothing but
/// the apply that took it can write its outcome.
async fn settle_plan(
    db: &Db,
    plan_id: Uuid,
    status: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE app_schema_plans
            SET status = $2, error = $3,
                applied_at = CASE WHEN $2 = 'applied' THEN now() ELSE applied_at END
          WHERE id = $1 AND status = 'applying'",
    )
    .bind(plan_id)
    .bind(status)
    .bind(error)
    .execute(&db.pool)
    .await?;
    Ok(())
}

/// Turn down a proposed migration. The app keeps the tables it has.
///
/// Only a pending plan, and only this app's: discarding one that was already
/// applied or failed would rewrite its record into something that never
/// happened.
pub async fn discard_plan(db: &Db, app_id: Uuid, plan_id: Uuid) -> anyhow::Result<()> {
    let done = sqlx::query(
        "UPDATE app_schema_plans SET status = 'discarded'
          WHERE id = $1 AND app_id = $2 AND status = 'pending'",
    )
    .bind(plan_id)
    .bind(app_id)
    .execute(&db.pool)
    .await?;
    if done.rows_affected() == 0 {
        anyhow::bail!("that change has already been dealt with");
    }
    Ok(())
}

async fn run(db: &Db, statements: &[Stmt]) -> anyhow::Result<()> {
    if statements.is_empty() {
        return Ok(());
    }
    let mut tx = db.pool.begin().await?;
    for stmt in statements {
        // Raw execute: this is DDL, which cannot take bound parameters. What
        // makes it safe is upstream — every identifier in it came from the
        // manifest's charset check, and every statement was built here rather
        // than supplied.
        sqlx::query(&stmt.sql)
            .execute(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("{}\n  while running: {}", e, stmt.sql))?;
    }
    tx.commit().await?;
    Ok(())
}

/// Pack an app up to go somewhere else.
///
/// `with_data` is the *Move* export; without it this is *Share*. The choice is
/// the caller's rather than a default, because "here, try my app" and "put this
/// on my laptop" are different sentences and only one of them means to include
/// what is in your tables.
pub async fn export(db: &Db, app: &App, with_data: bool) -> anyhow::Result<String> {
    let parsed = app.parsed()?;
    let ddl = schema::plan(&app.schema, &parsed.models, &[])
        .iter()
        .map(|s| format!("{};", s.sql))
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut rows = Vec::new();
    if with_data {
        for name in bundle::model_order(&parsed.models) {
            let Some(model) = parsed.model(&name) else {
                continue;
            };
            // Everything, in id order so two exports of an unchanged app are
            // the same bytes and a bundle in git diffs to nothing.
            let all = data::list(
                db,
                &app.schema,
                model,
                &query::Raw {
                    order: Some("id".into()),
                    limit: Some(query::MAX_LIMIT),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.0))?;
            rows.push((name, all));
        }
    }

    Ok(bundle::write(&app.manifest, &parsed, &ddl, &rows))
}

/// Install an app from a bundle, rows and all.
///
/// The tables are derived from the manifest by the ordinary install path — the
/// `schema` in a bundle is documentation and is never executed, so a bundle
/// someone hand-edited cannot introduce a statement no manifest asked for.
pub async fn import(db: &Db, workspace_id: Uuid, text: &str) -> anyhow::Result<App> {
    let parsed_bundle = bundle::read(text)?;
    // Parsed here so the error is the manifest parser's, naming the key, rather
    // than a second opinion from the bundle reader.
    let parsed = manifest::parse(&parsed_bundle.manifest)?;
    let app = install(db, workspace_id, &parsed_bundle.manifest, "imported").await?;

    for (model_name, rows) in &parsed_bundle.rows {
        let Some(model) = parsed.model(model_name) else {
            continue;
        };
        for row in rows {
            // Ids and timestamps are kept: a `ref:` holds the id of another
            // row, so re-minting them would quietly break every link in the
            // bundle. This is Eren loading its own export, not an app
            // writing — but the values are still bound and the columns still
            // come from the manifest.
            if let Err(e) = data::insert_verbatim(db, &app.schema, model, row).await {
                // One bad row should not lose the other nine hundred, and the
                // app is already installed and usable without it.
                tracing::warn!(app = %app.slug, model = %model_name, "skipped a row: {e}");
            }
        }
    }
    Ok(app)
}

/// Switch an app on or off.
///
/// Off means out of the sidebar and, for a container, stopped. It does **not**
/// mean the data is gone: deactivating has to stay free, or nobody will use the
/// switch and the toggle stops being worth having. Losing rows is what
/// `uninstall` is for, and it asks.
pub async fn set_active(db: &Db, id: Uuid, active: bool) -> anyhow::Result<()> {
    sqlx::query("UPDATE apps SET state = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(if active { "active" } else { "inactive" })
        .execute(&db.pool)
        .await?;
    Ok(())
}

/// Remove an app, its tables, and its folder.
///
/// The only destructive verb, and the only thing that drops a schema. The
/// database goes first: a folder deleted while the rows survive leaves an app
/// nothing can render and nothing can repair, whereas rows outliving a folder
/// by a moment are merely untidy.
pub async fn uninstall(db: &Db, id: Uuid) -> anyhow::Result<()> {
    let Some(app) = get(db, id).await? else {
        return Ok(());
    };
    // Identifier, not a bound parameter — DDL cannot take one. Safe because a
    // schema name is derived from a slug, which is generated here from a
    // validated charset and never from anything a manifest supplies.
    sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", app.schema))
        .execute(&db.pool)
        .await?;
    // Deleting the project cascades to the app row, its builds and its grants.
    sqlx::query("DELETE FROM projects WHERE id = $1")
        .bind(app.project_id)
        .execute(&db.pool)
        .await?;
    if app.path.starts_with(root()) {
        let _ = tokio::fs::remove_dir_all(&app.path).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_is_a_legal_dns_label_whatever_the_name_was() {
        let id = Uuid::nil();
        assert!(slug("Expenses", &id).starts_with("expenses-"));
        assert!(slug("My  Great   App!", &id).starts_with("my-great-app-"));
        // A name of nothing but punctuation still has to produce a hostname,
        // and a hostname may not start with a dash.
        let odd = slug("!!!", &id);
        assert!(odd.starts_with("app-"), "{odd}");
        for s in [slug("Expenses", &id), slug("!!!", &id), slug("A-B-", &id)] {
            assert!(!s.starts_with('-') && !s.ends_with('-'), "{s}");
            assert!(s.len() <= 63, "{s}");
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{s}"
            );
        }
    }

    #[test]
    fn two_apps_with_one_name_do_not_collide() {
        let name = "Notes";
        let a = slug(name, &Uuid::new_v4());
        let b = slug(name, &Uuid::new_v4());
        assert_ne!(a, b);
    }

    #[test]
    fn a_schema_name_needs_no_quoting() {
        // It is interpolated into DROP SCHEMA, so this is the property that
        // matters more than how pretty it looks.
        let s = schema_name("my-app-a1b2c3");
        assert_eq!(s, "app_my_app_a1b2c3");
        assert!(s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'));
    }

    #[test]
    fn the_digest_changes_when_the_text_does() {
        assert_eq!(digest("a"), digest("a"));
        assert_ne!(digest("a"), digest("a "));
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb;

    /// Two approvals of one plan — the app page and the inbox, or a double
    /// click — run its DDL once. The second is told it has been dealt with,
    /// and nothing it does can rewrite the first one's outcome.
    #[tokio::test]
    async fn a_plan_is_applied_once() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (ws, project) = t.project(dir.path(), false).await;
        let app: Uuid = sqlx::query_scalar(
            "INSERT INTO apps (project_id, workspace_id, slug, name, schema_name, manifest,
                               manifest_sha256)
             VALUES ($1, $2, 'probe', 'Probe', 'app_probe', '', '') RETURNING id",
        )
        .bind(project)
        .bind(ws)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        let plan: Uuid = sqlx::query_scalar(
            "INSERT INTO app_schema_plans (app_id, statements, destructive)
             VALUES ($1, $2, TRUE) RETURNING id",
        )
        .bind(app)
        .bind(serde_json::json!([
            { "sql": "CREATE TABLE plan_probe (id int)", "destructive": true, "why": "probe" }
        ]))
        .fetch_one(&t.db.pool)
        .await
        .unwrap();

        let (a, b) = tokio::join!(apply_plan(&t.db, app, plan), apply_plan(&t.db, app, plan));
        let (won, lost) = match (a, b) {
            (Ok(s), Err(e)) | (Err(e), Ok(s)) => (s, e),
            (a, b) => panic!("expected one apply and one refusal, got {a:?} and {b:?}"),
        };
        assert_eq!(won.len(), 1);
        assert!(
            lost.to_string().contains("already been dealt with"),
            "{lost}"
        );
        let status: String =
            sqlx::query_scalar("SELECT status FROM app_schema_plans WHERE id = $1")
                .bind(plan)
                .fetch_one(&t.db.pool)
                .await
                .unwrap();
        assert_eq!(status, "applied");
        assert!(apply_plan(&t.db, app, plan).await.is_err());
        assert!(discard_plan(&t.db, app, plan).await.is_err());
        assert!(pending_plan(&t.db, app).await.unwrap().is_none());
        t.finish().await;
    }
}
