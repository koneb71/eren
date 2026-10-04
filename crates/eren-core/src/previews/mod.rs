//! Run a card's branch and look at it.
//!
//! A card in review is a diff, and reading a diff is a poor way to answer "does
//! this look right". This builds the branch's own Dockerfile and publishes one
//! container on one loopback port, so the question can be answered by looking.
//!
//! Deliberately *not* a deployment feature. It manages no standing
//! environments assigned no hostnames until slice 2 gave them one; a preview exists because you are
//! looking at a card right now.
//!
//! It does outlive an Eren restart, but only as far as the container itself
//! does — `reconcile` keeps the row and the container agreeing rather than
//! killing one to match the other. The container carries `--restart no`, so a
//! Docker restart ends it and the next reconcile records that.
//!
//! ## What keeps this from eating the machine
//!
//! The machine running these previews is also running the user's editor, their
//! other projects, and Eren itself. So: one container per card enforced by a
//! partial unique index, hard memory/CPU/pid caps (`docker::run`), loopback-only
//! publishing, and reconciliation at boot that reads *Docker* rather than the
//! database. Nothing here creates a network or a named volume, which is what
//! keeps a preview from colliding with the compose stacks the user already runs.

pub mod compose;
pub mod docker;
pub mod recipe;
pub mod recipe_writer;

use crate::db::Db;
use serde_json::Value;
use sqlx::Row;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// A preview as the API reports it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub id: Uuid,
    /// None for a project's base-branch preview — the thing a card's changes
    /// are meant to be compared against.
    pub task_id: Option<Uuid>,
    pub status: String,
    pub url: Option<String>,
    pub host_port: Option<i32>,
    pub container_port: Option<i32>,
    /// True when nothing in the Dockerfile said which port to publish, so the
    /// number above is a guess. Surfaced because the symptom of a wrong guess
    /// is a blank page, which looks like a bug in the branch.
    pub port_assumed: bool,
    /// The card has been worked on since this was built, so it is serving
    /// history. Reported rather than acted on: killing what someone is looking
    /// at is worse than telling them it is old, and "what did it look like
    /// before?" is a real question.
    pub stale: bool,
    /// The image is still on disk, so starting this again is a wake of a few
    /// seconds rather than a rebuild of several minutes. Worth saying: the
    /// button means something different in each case.
    pub can_wake: bool,
    /// The name this answers to. Only the label — the dashboard builds the URL,
    /// because the port to put in it is the one Eren is being *served* on and
    /// the core has no business knowing that. The browser already does.
    pub slug: Option<String>,
    /// A whole compose stack rather than one container. Worth saying: the
    /// build takes longer, and what is running is more than what you opened.
    pub is_stack: bool,
    pub error: Option<String>,
}

/// How many previews may run at once.
///
/// Each one is capped at 2 GB and 2 CPUs, so this is the number that decides
/// whether a laptop keeps working while you review a column of cards. Three is
/// deliberately low — the failure it prevents is not "a preview is slow", it is
/// "the machine running your editor stops responding". This is the default;
/// `limits()` reads a workspace override from `settings.preview_limits`, so a
/// machine with room to spare can say so.
pub const MAX_LIVE: i64 = 3;

/// The hostname suffix previews answer on. Duplicated nowhere: the server's
/// proxy imports this, so the name it routes and the name the UI shows cannot
/// drift apart.
pub const PREVIEW_SUFFIX: &str = ".preview.localhost";

/// How long a preview nobody has looked at stays up.
pub const IDLE_MINUTES: i64 = 30;

/// How many *apps* may run at once, separately from previews.
///
/// Two, not three: an app is something you keep open, so the number that
/// matters is how many you use side by side rather than how many branches you
/// are reviewing.
pub const MAX_LIVE_APPS: i64 = 2;

/// The two numbers that decide whether previews are safe to forget about.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub max_live: i64,
    /// Zero means never idle-stop, which is a real choice for someone who
    /// leaves one open all day on purpose.
    pub idle_minutes: i64,
    /// Apps get their own budget, and the reasoning above about the machine
    /// being full does still apply — but a preview is looked at once and an app
    /// is opened dozens of times a day, so making them compete means the thing
    /// you use every day loses to three branches you have finished reviewing.
    pub max_live_apps: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_live: MAX_LIVE,
            idle_minutes: IDLE_MINUTES,
            max_live_apps: MAX_LIVE_APPS,
        }
    }
}

pub async fn limits(db: &Db) -> Limits {
    let stored: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'preview_limits'")
            .fetch_optional(&db.pool)
            .await
            .ok()
            .flatten();
    let d = Limits::default();
    let Some(v) = stored else { return d };
    Limits {
        // Clamped rather than trusted: a zero here would make the Build button
        // permanently refuse, and there is no way back from that in the UI.
        max_live: v
            .get("max_live")
            .and_then(|x| x.as_i64())
            .unwrap_or(d.max_live)
            .clamp(1, 20),
        idle_minutes: v
            .get("idle_minutes")
            .and_then(|x| x.as_i64())
            .unwrap_or(d.idle_minutes)
            .clamp(0, 24 * 60),
        max_live_apps: v
            .get("max_live_apps")
            .and_then(|x| x.as_i64())
            .unwrap_or(d.max_live_apps)
            .clamp(1, 20),
    }
}

pub async fn set_limits(db: &Db, next: Limits) -> anyhow::Result<Limits> {
    let next = Limits {
        max_live: next.max_live.clamp(1, 20),
        idle_minutes: next.idle_minutes.clamp(0, 24 * 60),
        max_live_apps: next.max_live_apps.clamp(1, 20),
    };
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES ('preview_limits', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(serde_json::json!({
        "max_live": next.max_live,
        "idle_minutes": next.idle_minutes,
        "max_live_apps": next.max_live_apps,
    }))
    .execute(&db.pool)
    .await?;
    Ok(next)
}

/// Where this task's work lives on disk.
///
/// The most recent run that produced a worktree, falling back to the project's
/// own checkout for a project with no version control — those tasks edit in
/// place, so the checkout *is* the branch.
async fn source_path(db: &Db, task_id: Uuid) -> anyhow::Result<Option<PathBuf>> {
    let row = sqlx::query(
        "SELECT COALESCE(
                    (SELECT r.worktree_path FROM runs r
                      WHERE r.task_id = t.id AND r.worktree_path IS NOT NULL
                      ORDER BY r.created_at DESC LIMIT 1),
                    p.path
                ) AS path
           FROM tasks t JOIN projects p ON p.id = t.project_id
          WHERE t.id = $1",
    )
    .bind(task_id)
    .fetch_optional(&db.pool)
    .await?;
    Ok(row
        .and_then(|r| r.get::<Option<String>, _>("path"))
        .map(PathBuf::from))
}

/// A free loopback port, asked of the OS rather than picked from a range.
///
/// There is a gap between letting go of this and Docker binding it, so this is
/// a hint and not a reservation — Docker fails loudly if it loses the race, and
/// the row records that instead of a preview that silently isn't there.
fn free_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// How a source directory gets built.
///
/// Compose wins when both are present: a project with a stack *and* a root
/// Dockerfile almost always uses that Dockerfile as one service of the stack,
/// so building it alone gives you a front end talking to nothing.
enum How {
    Stack(compose::Plan, PathBuf),
    Single(Option<String>),
}

/// How an app builds, when this project is one.
///
/// It lives here rather than in the apps routes so that
/// `POST /api/projects/{id}/preview` cannot reach an app's container by a
/// different door and skip the gate below.
///
/// Eren owns the Dockerfile for each runtime, so the common case has nothing
/// to approve. When the repository's copy differs from ours, *that* is what
/// gets built — and it is agent-written code whose `RUN` lines execute on this
/// machine, so it waits until someone has read it. Approval attaches to the
/// text's hash, exactly as it does for a preview recipe, so the rewrite on the
/// next build does not inherit the reading someone gave this one.
async fn how_app(
    db: &Db,
    source: &Path,
    project_id: Uuid,
) -> anyhow::Result<Option<(How, recipe::Port)>> {
    use crate::apps::runtime::{self, Build};

    let row = sqlx::query(
        "SELECT a.runtime, a.dockerfile_sha256, a.name
           FROM apps a WHERE a.project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(&db.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };

    let runtime = crate::apps::Runtime::parse(row.get::<String, _>("runtime").as_str())
        .unwrap_or(crate::apps::Runtime::Module);
    if !runtime.is_container() {
        let name: String = row.get("name");
        anyhow::bail!("\"{name}\" is a module — Eren draws it, so there is no container to build.");
    }

    let committed = tokio::fs::read_to_string(source.join("Dockerfile"))
        .await
        .ok();
    let text = match runtime::drift(runtime, committed.as_deref()) {
        Build::None => return Ok(None),
        Build::Owned(ours) => ours.to_string(),
        Build::Drifted { text, sha } => {
            let approved: Option<String> = row.get("dockerfile_sha256");
            if approved.as_deref() != Some(sha.as_str()) {
                anyhow::bail!(
                    "this app's Dockerfile has been changed from the one Eren wrote. \
                     Read it and approve it before it is built — its RUN lines execute \
                     on this machine."
                );
            }
            text
        }
    };

    let port = recipe::plan(Some(&text)).map_err(|e| anyhow::anyhow!(e.message()))?;
    // Fed on stdin like an approved recipe, so building never adds a file to
    // the tree or changes what is committed.
    Ok(Some((How::Single(Some(text)), port)))
}

async fn how(db: &Db, source: &Path, project_id: Uuid) -> anyhow::Result<(How, recipe::Port)> {
    // Before anything in the tree: an app's build is Eren's, not the
    // repository's, and a stray compose file in a generated app must not
    // quietly become how it runs.
    if let Some(build) = how_app(db, source, project_id).await? {
        return Ok(build);
    }

    for name in compose::COMPOSE_FILES {
        let path = source.join(name);
        let Ok(text) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        let plan = compose::plan(&text).map_err(|e| anyhow::anyhow!(e.message()))?;
        let port = recipe::Port {
            number: plan.container_port,
            source: if plan.port_assumed {
                recipe::PortSource::Assumed
            } else {
                recipe::PortSource::Exposed
            },
        };
        // The directory, not the file: it is what every relative build context
        // and bind mount inside resolves against.
        let dir = path.parent().unwrap_or(source).to_path_buf();
        return Ok((How::Stack(plan, dir), port));
    }

    if let Some(own) = tokio::fs::read_to_string(source.join("Dockerfile"))
        .await
        .ok()
    {
        let port = recipe::plan(Some(&own)).map_err(|e| anyhow::anyhow!(e.message()))?;
        return Ok((How::Single(None), port));
    }

    // Nothing in the branch. An approved recipe is the last resort — and only
    // an approved one, since a proposal is agent-written code nobody has read.
    let row = sqlx::query(
        "SELECT dockerfile, kind FROM preview_recipes
          WHERE project_id = $1 AND status = 'approved'",
    )
    .bind(project_id)
    .fetch_optional(&db.pool)
    .await?;
    let Some(row) = row else {
        return Err(anyhow::anyhow!(recipe::NoRecipe::NoDockerfile.message()));
    };

    let text: String = row.get("dockerfile");
    let kind = recipe_writer::Kind::parse(&row.get::<String, _>("kind"))
        .unwrap_or(recipe_writer::Kind::Dockerfile);
    match kind {
        // An agent-written stack goes through exactly the same treatment as one
        // found in the repo: ports stripped, everything namespaced. Nothing
        // about having written it ourselves makes it safer.
        recipe_writer::Kind::Compose => {
            let plan = compose::plan(&text).map_err(|e| anyhow::anyhow!(e.message()))?;
            let port = recipe::Port {
                number: plan.container_port,
                source: if plan.port_assumed {
                    recipe::PortSource::Assumed
                } else {
                    recipe::PortSource::Exposed
                },
            };
            Ok((How::Stack(plan, source.to_path_buf()), port))
        }
        recipe_writer::Kind::Dockerfile => {
            let port = recipe::plan(Some(&text)).map_err(|e| anyhow::anyhow!(e.message()))?;
            Ok((How::Single(Some(text)), port))
        }
    }
}

/// Which preview: a card's, or a project's base branch.
///
/// One enum rather than two parallel families of function. Everything after the
/// lookup — liveness, touching, waking, stopping — is identical, and the last
/// time two copies of this kind of logic existed in Eren they drifted.
#[derive(Debug, Clone, Copy)]
enum Which {
    Card(Uuid),
    Base(Uuid),
}

async fn find(db: &Db, which: Which) -> anyhow::Result<Option<Uuid>> {
    Ok(match which {
        Which::Card(task_id) => sqlx::query_scalar(
            "SELECT id FROM previews WHERE task_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(task_id),
        Which::Base(project_id) => sqlx::query_scalar(
            "SELECT id FROM previews WHERE project_id = $1 AND task_id IS NULL
              ORDER BY created_at DESC LIMIT 1",
        )
        .bind(project_id),
    }
    .fetch_optional(&db.pool)
    .await?)
}

/// The current preview for a card, alive or not.
pub async fn get(db: &Db, task_id: Uuid) -> anyhow::Result<Option<Preview>> {
    match find(db, Which::Card(task_id)).await? {
        Some(id) => by_id(db, id).await,
        None => Ok(None),
    }
}

/// The current preview of a project's base branch.
pub async fn get_base(db: &Db, project_id: Uuid) -> anyhow::Result<Option<Preview>> {
    match find(db, Which::Base(project_id)).await? {
        Some(id) => by_id(db, id).await,
        None => Ok(None),
    }
}

async fn by_id(db: &Db, id: Uuid) -> anyhow::Result<Option<Preview>> {
    let row = sqlx::query(
        "SELECT p.id, p.task_id, p.status, p.host_port, p.container_port,
                p.port_assumed, p.error, p.container_id, p.image_kept, p.slug,
                p.compose_file,
                -- Any run that started after this was built means the branch
                -- has moved on. A base preview has no card, so it is never
                -- stale in this sense: main moving is not something a run did.
                COALESCE((SELECT true FROM runs r
                           WHERE r.task_id = p.task_id AND r.created_at > p.created_at
                           LIMIT 1), false) AS stale
           FROM previews p WHERE p.id = $1",
    )
    .bind(id)
    .fetch_optional(&db.pool)
    .await?;

    // Reading the card is the only "someone is looking at this" signal there
    // is — the drawer does not poll a running preview, and the browser tab
    // showing the container never talks to Eren at all. It is a coarse
    // signal, so the idle window is measured in tens of minutes rather than
    // minutes: the cost of being wrong is a rebuild, not lost work.
    if let Some(r) = &row {
        if r.get::<String, _>("status") == "running" {
            let _ = sqlx::query("UPDATE previews SET last_seen_at = now() WHERE id = $1")
                .bind(id)
                .execute(&db.pool)
                .await;
            // A container can die on its own — out of memory, a crash minutes
            // in — and nothing would notice until the next restart, leaving a
            // link to a closed port. Checked when someone reads it, which is
            // exactly when it matters.
            //
            // A stack has to be asked differently: compose names its containers
            // `<project>-<service>-1`, so looking for our own container name
            // finds nothing and would kill a healthy stack on first read.
            let alive = if r.get::<Option<String>, _>("compose_file").is_some() {
                docker::compose_running(&recipe::container_name(&id)).await
            } else {
                let container: Option<String> = r.get("container_id");
                let name = container.unwrap_or_else(|| recipe::container_name(&id));
                docker::is_running(&name).await
            };
            if !alive {
                sqlx::query(
                    "UPDATE previews SET status='stopped', stopped_at=now(),
                            image_kept=FALSE,
                            error=COALESCE(error, 'its container stopped on its own')
                      WHERE id=$1 AND status='running'",
                )
                .bind(id)
                .execute(&db.pool)
                .await?;
                docker::remove_image(&recipe::image_tag(&id)).await;
                return Box::pin(by_id(db, id)).await;
            }
        }
    }

    Ok(row.map(|r| {
        let host_port = r.get::<Option<i32>, _>("host_port");
        let status = r.get::<String, _>("status");
        Preview {
            id: r.get("id"),
            task_id: r.get("task_id"),
            // A URL only while there is something on the other end of it.
            url: host_port
                .filter(|_| status == "running")
                .map(|p| format!("http://{}", docker::published(p as u16))),
            host_port,
            container_port: r.get("container_port"),
            port_assumed: r.get("port_assumed"),
            stale: r.get("stale"),
            can_wake: r.get::<bool, _>("image_kept") && status != "running",
            slug: r
                .get::<Option<String>, _>("slug")
                .filter(|_| status == "running"),
            is_stack: r.get::<Option<String>, _>("compose_file").is_some(),
            error: r.get("error"),
            status,
        }
    }))
}

/// Every preview this project has, base branch first.
///
/// One query rather than one per card: the tab shows them together, and the
/// point of showing them together is that they compete for the same three
/// slots and the same disk.
pub async fn list_for_project(db: &Db, project_id: Uuid) -> anyhow::Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT DISTINCT ON (p.task_id)
                p.id, p.task_id, p.status, p.host_port, p.container_port,
                p.port_assumed, p.error, p.image_kept, p.slug, p.compose_file,
                t.title AS task_title,
                COALESCE((SELECT true FROM runs r
                           WHERE r.task_id = p.task_id AND r.created_at > p.created_at
                           LIMIT 1), false) AS stale
           FROM previews p
           LEFT JOIN tasks t ON t.id = p.task_id
          WHERE p.project_id = $1
            -- 'stopped' included. Leaving it out meant a preview you stopped
            -- from this very tab vanished off it, and could only be started
            -- again from the card drawer — a row that disappears when you use
            -- the button on it reads as a delete.
            AND p.status IN ('building','running','idle','failed','stopped')
          -- One row per target, newest first. Without the DISTINCT a card that
          -- failed and was then rebuilt appears twice — two rows with the same
          -- name and different states, which reads as a bug in the thing being
          -- previewed rather than in this list.
          ORDER BY p.task_id NULLS FIRST, p.created_at DESC",
    )
    .bind(project_id)
    .fetch_all(&db.pool)
    .await?;

    let mut out: Vec<Value> = rows
        .iter()
        .map(|r| {
            let status: String = r.get("status");
            let host_port = r.get::<Option<i32>, _>("host_port");
            serde_json::json!({
                "id": r.get::<Uuid, _>("id"),
                "taskId": r.get::<Option<Uuid>, _>("task_id"),
                // The base preview has no card, so it names itself.
                "title": r.get::<Option<String>, _>("task_title")
                    .unwrap_or_else(|| "main".to_string()),
                "status": status,
                "url": host_port.filter(|_| status == "running")
                    .map(|p| format!("http://{}", docker::published(p as u16))),
                "hostPort": host_port,
                "containerPort": r.get::<Option<i32>, _>("container_port"),
                "portAssumed": r.get::<bool, _>("port_assumed"),
                "stale": r.get::<bool, _>("stale"),
                "canWake": r.get::<bool, _>("image_kept") && status != "running",
                "slug": r.get::<Option<String>, _>("slug")
                    .filter(|_| status == "running"),
                "isStack": r.get::<Option<String>, _>("compose_file").is_some(),
                "error": r.get::<Option<String>, _>("error"),
            })
        })
        .collect();
    // Base branch first: it is the thing everything else is compared against.
    // Done here because `DISTINCT ON` dictates the SQL ordering.
    out.sort_by_key(|v| v.get("taskId").is_some_and(|t| !t.is_null()));
    Ok(out)
}

/// Preview the branch cards merge into.
///
/// The project's own checkout rather than a worktree, so it is whatever `main`
/// is right now. Everything downstream — build, ports, naming, idle-stop,
/// reconciliation — is the per-card path unchanged; only where the source comes
/// from and what the row is keyed to differ.
pub async fn start_base(db: &Db, project_id: Uuid) -> anyhow::Result<Preview> {
    let (path, name): (String, String) =
        sqlx::query_as("SELECT path, name FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_optional(&db.pool)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no such project"))?;
    let source = PathBuf::from(&path);
    if !source.exists() {
        anyhow::bail!("this project's folder is gone from disk ({path})");
    }

    let (build, port) = how(db, &source, project_id).await?;

    // An app counts against the app budget and a branch against the preview
    // one. The original reasoning — the machine being full does not care what a
    // container is for — still holds for two things looked at once; it stops
    // holding when one of them is opened dozens of times a day and the other
    // is a branch someone has finished reviewing.
    let limits = limits(db).await;
    let is_app: bool = sqlx::query_scalar("SELECT kind = 'app' FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_optional(&db.pool)
        .await?
        .unwrap_or(false);
    let (live, cap, what) = if is_app {
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM previews v JOIN projects p ON p.id = v.project_id
              WHERE v.status IN ('building','running')
                AND p.kind = 'app' AND v.project_id <> $1",
        )
        .bind(project_id)
        .fetch_one(&db.pool)
        .await?;
        (live, limits.max_live_apps, "apps")
    } else {
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM previews v JOIN projects p ON p.id = v.project_id
              WHERE v.status IN ('building','running')
                AND p.kind <> 'app' AND (v.task_id IS NOT NULL OR v.project_id <> $1)",
        )
        .bind(project_id)
        .fetch_one(&db.pool)
        .await?;
        (live, limits.max_live, "previews")
    };
    if live >= cap {
        anyhow::bail!("{live} {what} are already running, which is the limit. Stop one first.");
    }

    if let Some(woken) = wake(db, Which::Base(project_id), port.number).await? {
        return Ok(woken);
    }

    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO previews
             (task_id, project_id, status, container_port, port_assumed, source_path)
         VALUES (NULL, $1, 'building', $2, $3, $4)
         ON CONFLICT DO NOTHING
         RETURNING id",
    )
    .bind(project_id)
    .bind(port.number as i32)
    .bind(port.source == recipe::PortSource::Assumed)
    .bind(&path)
    .fetch_optional(&db.pool)
    .await?;

    let Some(id) = id else {
        return get_base(db, project_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("a preview is already starting for this project"));
    };

    // Named for the project rather than a card, and keyed to the project so the
    // name survives rebuilds exactly as a card's does.
    sqlx::query("UPDATE previews SET slug = $2 WHERE id = $1")
        .bind(id)
        .bind(recipe::slug(&format!("{name} main"), &project_id))
        .execute(&db.pool)
        .await?;

    let pool = db.pool.clone();
    tokio::spawn(async move {
        if let Err(e) = build_and_run(&pool, id, &source, port.number, build).await {
            fail(&pool, id, &e).await;
        }
    });

    get_base(db, project_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("preview vanished immediately after being created"))
}

/// Begin a preview: claim the row, then build and run in the background.
///
/// Returns as soon as the row exists, because a build takes minutes and the
/// button should not hang for them. Everything after that is reported through
/// the row's status.
pub async fn start(db: &Db, task_id: Uuid) -> anyhow::Result<Preview> {
    let Some(source) = source_path(db, task_id).await? else {
        anyhow::bail!("no such task");
    };
    if !source.exists() {
        anyhow::bail!(
            "this card's worktree is gone from disk ({}) — re-run the task to recreate it",
            source.display()
        );
    }
    // A stack if the branch has one, otherwise its Dockerfile, otherwise an
    // approved recipe — and only an *approved* one, since a proposal is
    // agent-written code nobody has read and `RUN` executes on this machine.
    // Read before the row is claimed, so a branch with nothing to build says
    // so immediately rather than leaving a failed row behind.
    let project_id: Uuid = sqlx::query_scalar("SELECT project_id FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(&db.pool)
        .await?;
    let (build, port) = how(db, &source, project_id).await?;

    // Checked before the row is claimed, and named in the message: "too many
    // previews" with no way to see which ones is a dead end, and the cards
    // holding the slots are usually ones the person has forgotten about.
    let live: Vec<String> = sqlx::query_scalar(
        "SELECT t.title FROM previews p JOIN tasks t ON t.id = p.task_id
          WHERE p.status IN ('building','running') AND p.task_id <> $1
          ORDER BY p.created_at",
    )
    .bind(task_id)
    .fetch_all(&db.pool)
    .await?;
    let limits = limits(db).await;
    if live.len() as i64 >= limits.max_live {
        anyhow::bail!(
            "{} previews are already running, which is the limit — each one holds \
             2 GB and 2 CPUs of this machine. Stop one first: {}.",
            live.len(),
            live.join(", ")
        );
    }

    // Wake before rebuild. An idle-stopped preview kept its image, so putting
    // it back is a `docker run` rather than a `docker build` — seconds instead
    // of minutes, and byte-for-byte what was there before rather than a fresh
    // build of a branch that may have moved on in the meantime.
    if let Some(woken) = wake(db, Which::Card(task_id), port.number).await? {
        return Ok(woken);
    }

    // The partial unique index is what makes a double-click safe: the second
    // insert loses rather than starting a second container nothing points at.
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO previews
             (task_id, project_id, status, container_port, port_assumed, source_path)
         VALUES ($1, $2, 'building', $3, $4, $5)
         ON CONFLICT DO NOTHING
         RETURNING id",
    )
    .bind(task_id)
    .bind(project_id)
    .bind(port.number as i32)
    .bind(port.source == recipe::PortSource::Assumed)
    .bind(source.to_string_lossy().to_string())
    .fetch_optional(&db.pool)
    .await?;

    let Some(id) = id else {
        // Already building or running — hand back what is there rather than
        // reporting an error for what is arguably success.
        return get(db, task_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("a preview is already starting for this card"));
    };

    // Named after the card and keyed to the card, so a rebuild keeps the name.
    // Written as a second statement only because the insert above may have lost
    // its race, in which case there is no row of ours to name.
    let title: String = sqlx::query_scalar("SELECT title FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(&db.pool)
        .await
        .unwrap_or_default();
    sqlx::query("UPDATE previews SET slug = $2 WHERE id = $1")
        .bind(id)
        .bind(recipe::slug(&title, &task_id))
        .execute(&db.pool)
        .await?;

    let pool = db.pool.clone();
    tokio::spawn(async move {
        if let Err(e) = build_and_run(&pool, id, &source, port.number, build).await {
            fail(&pool, id, &e).await;
        }
    });

    get(db, task_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("preview vanished immediately after being created"))
}

/// Where a preview's build output is kept.
///
/// A file rather than a database column: build logs are megabytes, they are
/// only read while someone is looking at that one preview, and a row that big
/// would be carried by every query that touches the table.
pub fn build_log_path(id: Uuid) -> PathBuf {
    eren_shared::brand::home()
        .join("previews")
        .join(format!("{}.log", recipe::container_name(&id)))
}

/// Where a failed stack's own output is kept.
///
/// Written at the moment of failure, because the containers are removed
/// immediately afterwards and `compose logs` cannot speak for a project that
/// is gone. This file is the only remaining record of why.
pub fn output_log_path(id: Uuid) -> PathBuf {
    eren_shared::brand::home()
        .join("previews")
        .join(format!("{}.out", recipe::container_name(&id)))
}

/// What this preview has printed: how it was built, and what it has said since.
///
/// Both halves matter and they fail differently. A build log explains an image
/// that never came out; runtime output explains a container that started and
/// then refused to serve — which is the case the tail on the card cannot show,
/// because by then the build succeeded.
pub async fn logs(db: &Db, preview_id: Uuid) -> anyhow::Result<(String, String)> {
    let row = sqlx::query("SELECT container_id, compose_file, status FROM previews WHERE id = $1")
        .bind(preview_id)
        .fetch_optional(&db.pool)
        .await?;
    let Some(row) = row else {
        anyhow::bail!("no such preview");
    };

    let build = tokio::fs::read_to_string(build_log_path(preview_id))
        .await
        .unwrap_or_default();

    let name = recipe::container_name(&preview_id);
    let mut runtime = if row.get::<Option<String>, _>("compose_file").is_some() {
        docker::compose_logs(&name, 400).await
    } else {
        let container = row
            .get::<Option<String>, _>("container_id")
            .unwrap_or_else(|| name.clone());
        docker::logs(&container, 400).await
    };
    // Docker has nothing to say about containers that are gone, so fall back to
    // what was kept when they died. Otherwise the failure people most want to
    // read is the one with no output at all.
    if runtime.trim().is_empty() {
        runtime = tokio::fs::read_to_string(output_log_path(preview_id))
            .await
            .unwrap_or_default();
    }
    Ok((build, runtime))
}

/// The rewritten file a preview project name implies.
fn compose_path_for_name(project: &str) -> PathBuf {
    eren_shared::brand::home()
        .join("previews")
        .join(format!("{project}.yaml"))
}

/// Where a preview's rewritten compose file lives.
///
/// Under Eren's own directory, never in the branch: a preview must not add a
/// file to the diff someone is reviewing, and the rewrite is Eren's business
/// rather than the project's.
async fn compose_file_for(id: Uuid, contents: &str) -> std::io::Result<PathBuf> {
    let dir = eren_shared::brand::home().join("previews");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}.yaml", recipe::container_name(&id)));
    tokio::fs::write(&path, contents).await?;
    Ok(path)
}

/// Put an idle preview back, if its image is still there.
///
/// Returns `None` when there is nothing to wake, so the caller falls through to
/// a normal build. Checks Docker for the image rather than trusting the column:
/// `docker image prune` is a thing people run, and Eren is not told.
async fn wake(db: &Db, which: Which, container_port: u16) -> anyhow::Result<Option<Preview>> {
    let row = match which {
        Which::Card(task_id) => sqlx::query(
            "SELECT id, image, container_port FROM previews
              WHERE task_id = $1 AND status = 'idle' AND image_kept
              ORDER BY created_at DESC LIMIT 1",
        )
        .bind(task_id),
        Which::Base(project_id) => sqlx::query(
            "SELECT id, image, container_port FROM previews
              WHERE project_id = $1 AND task_id IS NULL
                AND status = 'idle' AND image_kept
              ORDER BY created_at DESC LIMIT 1",
        )
        .bind(project_id),
    }
    .fetch_optional(&db.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };

    let id: Uuid = row.get("id");
    let tag = row
        .get::<Option<String>, _>("image")
        .unwrap_or_else(|| recipe::image_tag(&id));
    if !docker::image_exists(&tag).await {
        // Someone reclaimed it behind our back. Say so in the row rather than
        // silently rebuilding under a status that claims to be a wake.
        sqlx::query("UPDATE previews SET image_kept = FALSE WHERE id = $1")
            .bind(id)
            .execute(&db.pool)
            .await?;
        return Ok(None);
    }

    // A woken preview gets a new port: the old one may well have been taken by
    // something else in the meantime, and a stale port in the row would send
    // someone to whatever is there now.
    let host_port = free_port()?;
    let port = row
        .get::<Option<i32>, _>("container_port")
        .map(|p| p as u16)
        .unwrap_or(container_port);
    // The old container is gone by definition, but its name may linger if the
    // idle stop lost a race.
    docker::remove(&recipe::container_name(&id)).await;
    let container = docker::run(&tag, &recipe::container_name(&id), host_port, port)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    sqlx::query(
        "UPDATE previews SET status='running', container_id=$2, host_port=$3,
                             started_at=now(), last_seen_at=now(), error=NULL
          WHERE id=$1",
    )
    .bind(id)
    .bind(&container)
    .bind(host_port as i32)
    .execute(&db.pool)
    .await?;

    tracing::info!(preview=%id, "woke an idle preview from its kept image");
    by_id(db, id).await
}

/// Stop previews nobody has looked at for a while, keeping their images.
///
/// The container is what costs memory; the image only costs disk. Keeping the
/// image is what makes this safe to do automatically — the cost of being wrong
/// is a few seconds when you come back, not a rebuild.
pub async fn sweep_idle(db: &Db) -> anyhow::Result<u64> {
    let limits = limits(db).await;
    if limits.idle_minutes == 0 {
        return Ok(0);
    }
    let rows = sqlx::query(
        "UPDATE previews SET status='idle', stopped_at=now(), image_kept=TRUE,
                error='stopped after nobody looked at it for a while'
          WHERE status='running'
            AND COALESCE(last_seen_at, started_at, created_at)
                  < now() - make_interval(mins => $1::int)
      RETURNING id, container_id",
    )
    .bind(limits.idle_minutes as i32)
    .fetch_all(&db.pool)
    .await?;

    for row in &rows {
        let id: Uuid = row.get("id");
        if let Some(c) = row.get::<Option<String>, _>("container_id") {
            docker::remove(&c).await;
        }
        docker::remove(&recipe::container_name(&id)).await;
    }
    if !rows.is_empty() {
        tracing::info!(
            count = rows.len(),
            "idle previews stopped; images kept for a fast wake"
        );
    }
    Ok(rows.len() as u64)
}

/// The sweeper loop. One minute is far finer than the idle window, which keeps
/// the check cheap and the stop close to when it was earned.
pub async fn idle_loop(db: Db) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        if let Err(e) = sweep_idle(&db).await {
            tracing::warn!(error=%e, "idle preview sweep failed");
        }
    }
}

/// What previews are costing on disk, and what could be reclaimed.
pub async fn disk(db: &Db) -> anyhow::Result<(u64, i64)> {
    let reclaimable: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM previews WHERE image_kept AND status <> 'running'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap_or(0);
    Ok((docker::image_disk_bytes().await, reclaimable))
}

/// Drop every kept image that nothing is running from.
///
/// Explicit rather than automatic: reclaiming an image turns the next wake back
/// into a full rebuild, and that is a trade the person looking at the disk
/// figure should get to make.
pub async fn reclaim_disk(db: &Db) -> anyhow::Result<u64> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE previews SET image_kept = FALSE
          WHERE image_kept AND status <> 'running' RETURNING id",
    )
    .fetch_all(&db.pool)
    .await?;
    for id in &ids {
        // The single-Dockerfile image…
        docker::remove_image(&recipe::image_tag(id)).await;
        // …and every image a *stack* built, which until images were namespaced
        // this could not name and so never removed. That is why the disk figure
        // read 0 B: the images it could see were the only ones it could delete,
        // and compose built neither.
        for tag in docker::images_for(&recipe::container_name(id)).await {
            docker::remove_image(&tag).await;
        }
    }
    Ok(ids.len() as u64)
}

async fn build_and_run(
    pool: &sqlx::PgPool,
    id: Uuid,
    source: &Path,
    container_port: u16,
    build: How,
) -> Result<(), String> {
    let tag = recipe::image_tag(&id);
    let name = recipe::container_name(&id);
    if let Some(dir) = build_log_path(id).parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }

    // A stack takes a different path entirely: compose builds, starts and wires
    // its own services, so there is no single image and no `docker run`.
    if let How::Stack(plan, project_dir) = &build {
        let host_port = free_port().map_err(|e| format!("no free port: {e}"))?;
        let rendered = compose_file_for(id, &plan.render(&id, docker::published(host_port)))
            .await
            .map_err(|e| format!("could not write the compose file: {e}"))?;
        // Recorded *before* the stack comes up, not after. A stack that fails
        // half way is still a stack: without this the row does not know it, so
        // its logs fall through to the single-container path and `stop` cannot
        // reach it through compose — leaving containers nothing can clean up.
        sqlx::query("UPDATE previews SET compose_file=$2, compose_dir=$3 WHERE id=$1")
            .bind(id)
            .bind(rendered.to_string_lossy().to_string())
            .bind(project_dir.to_string_lossy().to_string())
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;

        let project = recipe::container_name(&id);
        if let Err(e) =
            docker::compose_up(&rendered, project_dir, &project, &build_log_path(id)).await
        {
            // Compose's own stderr is progress noise — "Container Starting",
            // "exited (1)" — and never says *why*. The reason is in whatever
            // the service printed on its way down, so that is what the card
            // gets. This is the difference between "exited (1)" and
            // "host not found in upstream".
            // Kept before the stack is torn down, because `compose logs` has
            // nothing to say about a project that no longer exists — and the
            // whole point of a failed preview is being able to read why.
            let said = docker::compose_logs(&project, 400).await;
            if !said.is_empty() {
                let _ = tokio::fs::write(output_log_path(id), &said).await;
            }
            // A stack that never came up is not worth keeping: its containers
            // are useless and nothing offers to stop them, so they would sit
            // there until the next restart swept them.
            docker::compose_down(&rendered, project_dir, &project).await;

            let tail: String = said
                .lines()
                .rev()
                .take(6)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            return Err(if tail.is_empty() {
                e
            } else {
                format!("{e}\n\n{tail}")
            });
        }

        sqlx::query(
            "UPDATE previews SET status='running', host_port=$2, started_at=now(),
                                 last_seen_at=now(), image_kept=FALSE
              WHERE id=$1 AND status='building'",
        )
        .bind(id)
        .bind(host_port as i32)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
        return Ok(());
    }

    let How::Single(recipe_text) = &build else {
        unreachable!("the stack arm returns above")
    };
    docker::build(source, &tag, recipe_text.as_deref(), &build_log_path(id)).await?;

    // Asked for after the build, not before: a build takes minutes, and a port
    // reserved at the start of one is a port held against the rest of the
    // machine for no reason.
    let host_port = free_port().map_err(|e| format!("no free port: {e}"))?;
    let container = docker::run(&tag, &name, host_port, container_port).await?;

    // A container that starts and exits immediately — a bad CMD, a missing
    // env var — reports success from `docker run` and then is not there. Give
    // it a moment and check, so "running" means running.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    if !docker::is_running(&container).await {
        let why = docker::logs(&container, 20).await;
        docker::remove(&container).await;
        docker::remove_image(&tag).await;
        return Err(if why.is_empty() {
            "the container exited immediately and logged nothing".into()
        } else {
            format!("the container exited immediately:\n{why}")
        });
    }

    sqlx::query(
        "UPDATE previews SET status='running', container_id=$2, image=$3,
                             host_port=$4, started_at=now(), last_seen_at=now(),
                             image_kept=TRUE
          WHERE id=$1 AND status='building'",
    )
    .bind(id)
    .bind(&container)
    .bind(&tag)
    .bind(host_port as i32)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

async fn fail(pool: &sqlx::PgPool, id: Uuid, why: &str) {
    if let Err(e) =
        sqlx::query("UPDATE previews SET status='failed', error=$2, stopped_at=now() WHERE id=$1")
            .bind(id)
            .bind(why)
            .execute(pool)
            .await
    {
        // Losing this write means a row stuck at "building" forever, which the
        // boot sweep will clear — but it is worth knowing it happened.
        tracing::warn!(preview=%id, error=%e, "could not record a preview failure");
    }
}

/// Stop a task's preview and let go of its image.
///
/// Removal is best effort against Docker but authoritative in the database: a
/// container the user already killed by hand must still leave the row stopped,
/// or the card offers Stop forever and never Preview.
pub async fn stop(db: &Db, task_id: Uuid) -> anyhow::Result<bool> {
    stop_which(db, Which::Card(task_id)).await
}

/// Stop a project's base-branch preview.
pub async fn stop_base(db: &Db, project_id: Uuid) -> anyhow::Result<bool> {
    stop_which(db, Which::Base(project_id)).await
}

/// Stop everything this project is running — its base preview and every
/// card's.
///
/// For unloading a project: the rows cascade away with it, and a container
/// whose row is gone is one `reconcile` will only meet at the next boot, still
/// holding its port in the meantime.
pub async fn stop_for_project(db: &Db, project_id: Uuid) -> anyhow::Result<usize> {
    let tasks: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT task_id FROM previews
          WHERE project_id = $1 AND task_id IS NOT NULL
            AND status IN ('building','running','idle')",
    )
    .bind(project_id)
    .fetch_all(&db.pool)
    .await?;

    let mut stopped = 0;
    for task_id in tasks {
        // One failure must not leave the rest running.
        match stop(db, task_id).await {
            Ok(true) => stopped += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(%task_id, error = %e, "could not stop a card preview"),
        }
    }
    if stop_base(db, project_id).await.unwrap_or(false) {
        stopped += 1;
    }
    Ok(stopped)
}

async fn stop_which(db: &Db, which: Which) -> anyhow::Result<bool> {
    let row = match which {
        Which::Card(task_id) => sqlx::query(
            "UPDATE previews SET status='stopped', stopped_at=now(), image_kept=FALSE
              WHERE task_id=$1 AND status IN ('building','running','idle')
          RETURNING id, container_id, image, compose_file, compose_dir",
        )
        .bind(task_id),
        Which::Base(project_id) => sqlx::query(
            "UPDATE previews SET status='stopped', stopped_at=now(), image_kept=FALSE
              WHERE project_id=$1 AND task_id IS NULL
                AND status IN ('building','running','idle')
          RETURNING id, container_id, image, compose_file, compose_dir",
        )
        .bind(project_id),
    }
    .fetch_optional(&db.pool)
    .await?;
    let Some(row) = row else { return Ok(false) };

    // By name as well as by id: a row that failed between `docker run` and the
    // status write has no container id, but the container is named after the
    // row and is very much there.
    let id: Uuid = row.get("id");
    // A stack has no single container. `down --volumes` is safe here only
    // because compose namespaces everything under the preview's project name,
    // so it can reach nothing the user's own stacks created.
    if let (Some(file), Some(dir)) = (
        row.get::<Option<String>, _>("compose_file"),
        row.get::<Option<String>, _>("compose_dir"),
    ) {
        docker::compose_down(
            Path::new(&file),
            Path::new(&dir),
            &recipe::container_name(&id),
        )
        .await;
        let _ = tokio::fs::remove_file(&file).await;
    }
    if let Some(container) = row.get::<Option<String>, _>("container_id") {
        docker::remove(&container).await;
    }
    docker::remove(&recipe::container_name(&id)).await;
    if let Some(image) = row.get::<Option<String>, _>("image") {
        docker::remove_image(&image).await;
    } else {
        docker::remove_image(&recipe::image_tag(&id)).await;
    }
    // A stack has no single image either — it has one per service, and neither
    // branch above names any of them. So stopping a stack set `image_kept =
    // FALSE`, which is what `disk()` counts and `reclaim_disk()` selects on,
    // while the images stayed on disk: the row said they were gone, so nothing
    // ever looked for them again. `compose down` does not take images.
    //
    // Only reachable now that they are namespaced — before, the images were
    // called whatever the user's compose file called them, and there was no
    // safe way to name them here at all.
    for tag in docker::images_for(&recipe::container_name(&id)).await {
        docker::remove_image(&tag).await;
    }
    Ok(true)
}

/// Settle previews against what Docker actually has. Called once at boot.
///
/// Both directions matter, and only one of them is obvious:
///
/// * a row says running but the container is gone — mark it stopped, or the
///   card offers a dead link;
/// * a container is up but no row claims it — remove it, or every restart
///   leaves another orphan holding a port, and nothing in the UI will ever
///   mention it again.
///
/// The second is why this reads Docker rather than iterating the table.
pub async fn reconcile(db: &Db) -> anyhow::Result<(u64, usize)> {
    if docker::detect().await.is_none() {
        return Ok((0, 0));
    }
    let alive = docker::list_owned().await;

    let rows = sqlx::query(
        "SELECT id, container_id, compose_file, compose_dir
           FROM previews WHERE status IN ('building','running')",
    )
    .fetch_all(&db.pool)
    .await?;

    let stacks = docker::list_owned_stacks().await;
    let mut claimed = std::collections::HashSet::new();
    let mut claimed_stacks = std::collections::HashSet::new();
    let mut dead = Vec::new();
    let mut dead_stacks = Vec::new();
    for row in &rows {
        let id: Uuid = row.get("id");
        let name = recipe::container_name(&id);
        // A stack is asked of compose; a single container of docker. Getting
        // this the wrong way round declares a healthy stack dead, which is
        // exactly what the first version of the liveness check did.
        if let (Some(file), Some(dir)) = (
            row.get::<Option<String>, _>("compose_file"),
            row.get::<Option<String>, _>("compose_dir"),
        ) {
            if docker::compose_running(&name).await {
                claimed_stacks.insert(name);
            } else {
                dead.push(id);
                dead_stacks.push((file, dir, name));
            }
        } else if alive.contains(&name) && docker::is_running(&name).await {
            claimed.insert(name);
        } else {
            dead.push(id);
        }
    }

    // Whatever is left of a stack that died: its network and any container that
    // outlived its siblings.
    for (file, dir, name) in &dead_stacks {
        docker::compose_down(Path::new(file), Path::new(dir), name).await;
        let _ = tokio::fs::remove_file(file).await;
    }

    // Reclaim each dead row's image as well as its row. Settling the status
    // alone leaks a few hundred megabytes per preview, silently and forever —
    // the container is gone, so nothing else will ever mention the image
    // again. Derived from the id rather than read from the row because a
    // preview that died mid-build never recorded one.
    for id in &dead {
        // Under every name, so a preview from before the rename goes too.
        for (image, container) in recipe::every_name(id) {
            docker::remove(&container).await;
            docker::remove_image(&image).await;
            // A stack builds one image per service, all under the same prefix.
            for tag in docker::images_for(&container).await {
                docker::remove_image(&tag).await;
            }
        }
    }

    let settled = if dead.is_empty() {
        0
    } else {
        sqlx::query(
            "UPDATE previews SET status='stopped', stopped_at=now(),
                    error=COALESCE(error, 'its container is no longer running')
              WHERE id = ANY($1)",
        )
        .bind(&dead)
        .execute(&db.pool)
        .await?
        .rows_affected()
    };

    // A stack no live row claims. Taken down through compose rather than
    // container by container, so its network and volumes go with it.
    for name in stacks {
        if !claimed_stacks.contains(&name) {
            // By name alone. The rewritten file may already be gone, and an
            // earlier version passed it as the project *directory*, which
            // compose rejects — so the stack survived every sweep.
            docker::compose_down_project(&name).await;
            let _ = tokio::fs::remove_file(compose_path_for_name(&name)).await;
            // `compose down` takes containers, networks and volumes; images it
            // built are left. They share the project's name, which is exactly
            // what makes them findable now that they are namespaced.
            for tag in docker::images_for(&name).await {
                docker::remove_image(&tag).await;
            }
        }
    }

    // Anything wearing our label that no live row claims. The label is what
    // makes this safe — a container the user started themselves cannot match.
    let mut swept = 0usize;
    for name in alive {
        if !claimed.contains(&name) {
            docker::remove(&name).await;
            // The image is named from the same id the container is, so an
            // orphan with no row at all can still have its image reclaimed.
            if let Some(short) = name.strip_prefix("eren-preview-") {
                docker::remove_image(&format!("eren-preview:{short}")).await;
            }
            swept += 1;
        }
    }
    // Images belonging to no preview that could still be woken.
    //
    // The three sweeps above are keyed on containers and stacks; an image
    // outlives both. A stopped stack is the case that made this necessary — its
    // row says `image_kept = FALSE`, so `disk()` counts nothing and
    // `reclaim_disk()` selects nothing, while the images sit there. Anything
    // whose id is not a preview that can still be woken is unreachable by
    // definition: nothing will ever name it again.
    //
    // Safe only because the names are Eren's own. Before they were
    // namespaced, this sweep would have been deleting images called whatever
    // the user's compose file called them.
    let wakeable: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM previews WHERE status IN ('building','running','idle')")
            .fetch_all(&db.pool)
            .await
            .unwrap_or_default();
    let keep: std::collections::HashSet<String> =
        wakeable.iter().map(recipe::container_name).collect();
    let mut images = 0usize;
    for tag in docker::images_for("eren-preview-").await {
        // `eren-preview-<short>-<service>:tag` — the prefix up to the last
        // dash before the tag is the preview it belongs to.
        let named = tag.split(':').next().unwrap_or(&tag);
        let owner = match named.rsplit_once('-') {
            Some((owner, _service)) => owner.to_string(),
            None => named.to_string(),
        };
        if keep.contains(&owner) || keep.contains(named) {
            continue;
        }
        docker::remove_image(&tag).await;
        images += 1;
    }

    if settled > 0 || swept > 0 || images > 0 {
        tracing::info!(
            settled,
            swept,
            images,
            "previews reconciled with docker at boot"
        );
    }
    Ok((settled, swept))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_free_port_is_actually_bindable() {
        let port = free_port().unwrap();
        assert!(port > 0);
        // Released, not held: the whole point is that Docker can take it next.
        std::net::TcpListener::bind(("127.0.0.1", port)).expect("port was not released");
    }
}
