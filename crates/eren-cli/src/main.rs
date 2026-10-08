use clap::{Parser, Subcommand};
use eren_core::runs::gate::{DbGate, DbWindow};
use eren_core::runs::permissions::PermissionBroker;
use eren_core::{Db, EventBus, Orchestrator, WorktreeManager};
use eren_engines::amp::AmpEngine;
use eren_engines::claude::ClaudeEngine;
use eren_engines::codex::CodexEngine;
use eren_engines::cursor::CursorEngine;
use eren_engines::gemini::GeminiEngine;
use eren_engines::local::LocalEngine;
use eren_engines::mock::MockEngine;
use eren_engines::opencode::OpenCodeEngine;
use eren_engines::qwen::QwenEngine;
use eren_engines::Engine;
use eren_shared::env_guard;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "eren",
    about = "Local-first multi-agent workflow platform — no API keys"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the Eren server and open the dashboard (default).
    Serve {
        #[arg(long, default_value_t = 4820)]
        port: u16,
        /// Don't open the browser.
        #[arg(long)]
        headless: bool,
    },
    /// Check that required tools (git, claude CLI) are installed and usable.
    Doctor,
    /// Manage accounts: make the admin, reset a password.
    #[command(subcommand)]
    Admin(AdminCmd),
}

#[derive(Subcommand)]
enum AdminCmd {
    /// Make the one admin, turning accounts on: from then on every browser
    /// signs in, and the admin owns every workspace made before.
    Create {
        #[arg(long)]
        username: String,
        /// Read the password from standard input instead of asking twice.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Set a new password for an account (the admin's included) and sign it
    /// out everywhere.
    ResetPassword {
        username: String,
        /// Read the password from standard input instead of asking twice.
        #[arg(long)]
        password_stdin: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Cmd::Serve {
        port: 4820,
        headless: false,
    }) {
        Cmd::Serve { port, headless } => serve(port, headless).await,
        Cmd::Doctor => doctor().await,
        Cmd::Admin(cmd) => admin(cmd).await,
    }
}

/// Reclaim attachment bytes: uploads the user abandoned by closing a composer,
/// and directories whose row was cascade-deleted with the task or chat.
///
/// Never fatal — a failure here costs disk space, not correctness, so it logs
/// and keeps going rather than taking the server down.
async fn sweep_attachments(db: eren_core::db::Db) {
    loop {
        match eren_core::runs::attachments::sweep_abandoned(&db).await {
            Ok(n) if n > 0 => tracing::info!(removed = n, "swept abandoned attachments"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "attachment sweep failed"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}

/// Where the local runtimes listen, when somebody has said.
///
/// `None` is not "the default" — see `eren_core::local_models::configured`.
#[derive(Default)]
struct LocalHosts {
    ollama: Option<String>,
    lmstudio: Option<String>,
}

/// The engines Eren knows how to drive, in the order they're offered.
///
/// One list, used by both `serve` and `doctor`, so adding another adapter
/// never means remembering to edit the doctor separately.
///
/// The last two are the same OpenCode binary pointed at a model on this
/// machine — see `eren_engines::local` for why that is an engine and not a
/// setting. `doctor` runs without a database, so it passes no addresses and
/// gets the stock ports; that costs nothing, because the only thing an
/// address changes is where the probe looks.
fn real_engines(local: LocalHosts) -> Vec<Arc<dyn Engine>> {
    vec![
        Arc::new(ClaudeEngine::default()) as Arc<dyn Engine>,
        Arc::new(OpenCodeEngine::default()) as Arc<dyn Engine>,
        Arc::new(CodexEngine::default()) as Arc<dyn Engine>,
        Arc::new(GeminiEngine::default()) as Arc<dyn Engine>,
        Arc::new(CursorEngine::default()) as Arc<dyn Engine>,
        Arc::new(QwenEngine::default()) as Arc<dyn Engine>,
        Arc::new(AmpEngine::default()) as Arc<dyn Engine>,
        Arc::new(LocalEngine::ollama(local.ollama)) as Arc<dyn Engine>,
        Arc::new(LocalEngine::lmstudio(local.lmstudio)) as Arc<dyn Engine>,
    ]
}

/// Where to get an engine this machine hasn't got.
///
/// Beside the "not installed" line rather than in a wall of links at the end,
/// because the person reading it has just been told about one specific thing.
fn install_hint(id: &str) -> Option<&'static str> {
    match id {
        "claude-code" => Some("https://code.claude.com"),
        "opencode" => Some("https://opencode.ai"),
        "codex" => Some("npm i -g @openai/codex — https://developers.openai.com/codex/cli"),
        "gemini" => {
            Some("npm i -g @google/gemini-cli — https://github.com/google-gemini/gemini-cli")
        }
        "cursor" => Some("curl https://cursor.com/install -fsS | bash — https://cursor.com/cli"),
        "qwen" => Some("npm i -g @qwen-code/qwen-code — https://github.com/QwenLM/qwen-code"),
        "amp" => {
            Some("npm i -g @sourcegraph/amp — https://ampcode.com (headless runs need AMP_API_KEY)")
        }
        "ollama" => Some("https://ollama.com — needs OpenCode too, to drive it"),
        "lmstudio" => Some("https://lmstudio.ai — needs OpenCode too, to drive it"),
        _ => None,
    }
}

/// Provider names and auth *type* — never a credential.
fn describe_providers(info: &eren_engines::EngineInfo) -> String {
    let s = info
        .providers
        .iter()
        .map(|p| format!("{} ({})", p.name, p.auth))
        .collect::<Vec<_>>()
        .join(", ");
    if s.is_empty() {
        "—".into()
    } else {
        s
    }
}

async fn serve(port: u16, headless: bool) -> anyhow::Result<()> {
    // Decided first, before a database is started or a port is claimed: a
    // refusal that arrives ten seconds in has already cost something, and the
    // thing it refuses is a configuration mistake somebody wants to hear about
    // immediately.
    // Loopback by default: this is a local-first app with no authentication of
    // any kind, and that is only affordable while the only possible caller is
    // this machine. Binding anywhere else spends that, so it has to be said out
    // loud — see `eren_server::exposure`, which also explains why refusing
    // non-loopback *callers* would not work.
    let bind: std::net::IpAddr = eren_shared::brand::var("BIND")
        .and_then(|b| b.parse().ok())
        .unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]));
    let acknowledged = eren_server::network_trusted();
    // Both read now, so a typo is refused before anything starts.
    let hosts = eren_server::access::parse_hosts(
        &eren_shared::brand::var("ALLOWED_HOSTS").unwrap_or_default(),
    )
    .map_err(|e| anyhow::anyhow!("EREN_ALLOWED_HOSTS: {e}"))?;
    let token_setting =
        eren_server::access::token_setting(eren_shared::brand::var("ACCESS_TOKEN").as_deref())
            .map_err(|e| anyhow::anyhow!("{}: {e}", eren_server::ACCESS_TOKEN))?;
    let wants_token = token_setting != eren_server::access::TokenSetting::Off;
    let token_in_file = token_setting == eren_server::access::TokenSetting::Generated;
    let exposure = eren_server::exposure(bind, acknowledged, wants_token);
    match exposure {
        eren_server::Exposure::Local | eren_server::Exposure::Protected => {}
        eren_server::Exposure::Network => {
            tracing::warn!(
                %bind,
                "eren is reachable from your network and has no authentication — \
                 anyone who can reach this port can read transcripts, browse files \
                 and start agents"
            );
        }
        eren_server::Exposure::Unacknowledged => {
            // Refused rather than warned: a warning in a log is not read by the
            // person who copied a line from somewhere and moved on.
            anyhow::bail!(eren_server::unacknowledged_message(bind));
        }
    }

    // Before the managed Postgres starts, while nothing in the folder is open.
    adopt_legacy_state()?;

    // After the home folder is where it belongs, because the token lives in it.
    let token = match (exposure, token_setting) {
        (eren_server::Exposure::Protected, eren_server::access::TokenSetting::Given(t)) => Some(t),
        (eren_server::Exposure::Protected, _) => Some(eren_server::access::load_or_create_token(
            &eren_shared::brand::home().join("access_token"),
        )?),
        _ => None,
    };
    let access = std::sync::Arc::new(eren_server::access::Access::new(hosts, token));
    let home = eren_shared::brand::home();
    tokio::fs::create_dir_all(&home).await?;

    // Database: the user's own DATABASE_URL wins; otherwise boot a private
    // embedded Postgres with data under ~/.eren/pgdata.
    let (database_url, _embedded) = match std::env::var("DATABASE_URL") {
        Ok(url) => (url, None),
        Err(_) => {
            // Retry: a previous instance's postgres may still be shutting
            // down for a few seconds after the old server exits.
            let mut attempt = 0;
            let pg = loop {
                match start_embedded_postgres(&home).await {
                    Ok(pg) => break pg,
                    Err(e) if attempt < 4 => {
                        attempt += 1;
                        tracing::warn!(error=%e, attempt, "embedded postgres not ready; retrying");
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                    Err(e) => return Err(e),
                }
            };
            (pg.settings().url(eren_shared::brand::DATABASE), Some(pg))
        }
    };

    let db = Db::connect(&database_url).await?;
    match eren_core::legacy::adopt_legacy_manifests(&db).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(apps = n, "renamed app manifests to their new name"),
        Err(e) => tracing::warn!(error = %e, "could not rename every app manifest"),
    }
    // Before anything serves a request: a half-migrated wiki answers searches
    // wrong for some pages and says nothing about why.
    if let Err(e) = eren_core::kb::backfill::run(&db).await {
        tracing::error!(error = %e, "knowledge-base migration failed; some pages may not be searchable");
    }

    let bus = EventBus::new();
    let worktrees = Arc::new(WorktreeManager::new(WorktreeManager::default_root()));
    let mcp_base = format!("http://127.0.0.1:{port}");

    let mut orchestrator = Orchestrator::new(
        db.clone(),
        bus.clone(),
        worktrees,
        max_concurrent(),
        Some(mcp_base),
    );
    // Read before the engines are built rather than threaded to each spawn
    // site: a local engine knows where its own runtime lives, which is the
    // only place that fact was ever needed.
    let (ollama, lmstudio) = eren_core::local_models::configured(&db).await;
    // Register only what is actually installed, so an engine that isn't
    // present is simply *not offered* rather than accepted and then failing
    // at spawn time.
    for engine in real_engines(LocalHosts { ollama, lmstudio }) {
        let id = engine.id();
        match orchestrator.register_if_available(engine).await {
            Some(info) => tracing::info!(
                id,
                version = %info.version,
                providers = %describe_providers(&info),
                "engine available"
            ),
            None => tracing::info!(id, "engine not found on PATH; it will not be offered"),
        }
    }
    orchestrator.register_engine(Arc::new(MockEngine::demo()) as Arc<dyn Engine>);
    let orchestrator = Arc::new(orchestrator);

    // Model routing is the user's choice, so it has to be in place before the
    // first run is claimed rather than applied on the one after.
    orchestrator.load_tier_mapping().await?;
    orchestrator.load_tier_efforts().await?;
    // Previews do not survive a restart, and the containers from the last one
    // are still holding their ports. Settled against Docker rather than the
    // table, so a container no row claims is swept rather than orphaned.
    if let Err(e) = eren_core::previews::reconcile(&db).await {
        tracing::warn!(error=%e, "could not reconcile previews with docker");
    }

    // Worktrees nothing can reach any more: a bake-off variant whose run
    // cascaded away with its card, a workflow fan-out directory whose id was
    // never written down, an uninstalled app's leftovers. Same reason previews
    // and attachments get a sweep — Postgres can drop a row but not a
    // directory, and these are the largest thing Eren puts on disk.
    match eren_core::worktrees::sweep::reconcile(&db, &orchestrator.worktrees).await {
        Ok(s) if s.worktrees > 0 || s.dead_projects > 0 => tracing::info!(
            worktrees = s.worktrees,
            mb = s.bytes / 1_000_000,
            dead_projects = s.dead_projects,
            "reclaimed worktrees nothing was using"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "could not sweep worktrees"),
    }

    // One small file per run and per preview, in three directories, none of
    // which anything had ever cleaned. Individually kilobytes; unbounded in
    // count, and every one keyed by an id the database can be asked about.
    match eren_core::leftovers::sweep(&db).await {
        Ok(s) if s.files > 0 => tracing::info!(
            files = s.files,
            kb = s.bytes / 1024,
            "removed per-run files whose run is long finished"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "could not sweep per-run files"),
    }

    let orphans = orchestrator.recover_orphans().await?;
    match eren_core::checks::recover_interrupted(&db).await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(n, "marked checks interrupted by the last shutdown"),
        Err(e) => tracing::warn!(error = %e, "could not settle interrupted checks"),
    }
    if orphans > 0 {
        tracing::warn!(
            orphans,
            "marked orphaned runs from previous session as failed"
        );
    }
    tokio::spawn(orchestrator.clone().run_loop());
    tokio::spawn(eren_core::Scheduler::new(db.clone(), orchestrator.clone()).run_loop());
    tokio::spawn(sweep_attachments(db.clone()));
    // Previews nobody is looking at stop themselves, keeping their images so
    // coming back costs seconds rather than a rebuild.
    tokio::spawn(eren_core::previews::idle_loop(db.clone()));

    // Object storage, when it's configured. The bucket is created on boot so
    // a fresh MinIO needs no manual setup step; a failure here is logged and
    // the feature simply stays off rather than taking the server down.
    let storage = match eren_core::storage::Storage::from_env() {
        Some(mut s) => match s.ensure_bucket().await {
            Ok(()) => {
                tracing::info!(bucket = s.bucket(), "object storage ready");
                Some(s)
            }
            Err(e) => {
                tracing::warn!(error = %e, "object storage unreachable; attachments disabled");
                None
            }
        },
        None => {
            tracing::info!("object storage not configured; knowledge-base attachments disabled");
            None
        }
    };

    // Built before the state literal, which moves `db` on its first line.
    // The broker needs a database of its own to park and unpark runs, and the
    // orchestrator's slots so a run waiting on a person stops occupying one.
    let permissions = {
        let cancel_orchestrator = orchestrator.clone();
        let seen_orchestrator = orchestrator.clone();
        PermissionBroker::new(
            bus.clone(),
            Arc::new(
                DbGate::new(db.clone(), move |run_id| {
                    cancel_orchestrator.cancel(run_id);
                })
                .on_unpark(move |run_id| seen_orchestrator.mark_seen(run_id)),
            ),
            orchestrator.slots(),
            // Asked per prompt, so it can never disagree with the engine
            // timeout the orchestrator derives from the same setting.
            Arc::new(DbWindow(db.clone())),
        )
    };

    let accounts = Arc::new(eren_server::auth::Accounts::load(&db).await?);
    if accounts.is_on(&db).await {
        tracing::info!("accounts are on: every browser signs in");
    } else if !bind.is_loopback() {
        tracing::info!(
            "accounts are off; to have everyone sign in with an account of their own, \
             run `eren admin create --username <name>`"
        );
    }

    let state = eren_server::AppState {
        db,
        bus: bus.clone(),
        orchestrator,
        permissions,
        storage,
        file_writes: Default::default(),
        access: access.clone(),
        accounts: accounts.clone(),
    };
    let app = eren_server::app(state);

    let addr = std::net::SocketAddr::new(bind, port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    // The address it is actually on, not an assumed loopback one.
    tracing::info!("eren dashboard: http://{}:{port}", displayable(bind));
    if !bind.is_loopback() {
        announce_network_access(bind, port, &access, token_in_file);
    }
    // The address it actually bound, not the hardcoded loopback the MCP base
    // uses — a spawned CLI is on this machine, but the person reading the push
    // on their phone is not, and a loopback link can never answer them. The
    // first allowed name is the one they reach it by, when there is one.
    let reachable = match (bind.is_loopback(), access.hosts().first()) {
        (false, Some(host)) => host.clone(),
        _ => displayable(bind),
    };
    eren_core::attention::set_dashboard_url(format!("http://{reachable}:{port}"));

    if !headless {
        let _ = env_guard::command("open")
            .arg(format!("http://127.0.0.1:{port}"))
            .spawn();
    }

    // With the peer's address, which is what decides whether a caller is this
    // machine — see `eren_server::access`.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Tell the person who bound wide how other devices get in — or why they
/// will not yet.
fn announce_network_access(
    bind: std::net::IpAddr,
    port: u16,
    access: &eren_server::access::Access,
    token_in_file: bool,
) {
    if access.hosts().is_empty() {
        // Without a name to answer to, every other device is refused by the
        // Host check, which looks like a broken server from the other side.
        let suggestion = if bind.is_unspecified() {
            eren_server::access::guess_lan_address()
        } else {
            Some(bind)
        };
        match suggestion {
            Some(ip) => tracing::warn!(
                "other devices will be refused until Eren is told the name they use: \
                 set EREN_ALLOWED_HOSTS={} (this machine's address on your network, \
                 as far as Eren can tell)",
                displayable(ip)
            ),
            None => tracing::warn!(
                "other devices will be refused until Eren is told the name they use: \
                 set EREN_ALLOWED_HOSTS to this machine's address on your network"
            ),
        }
    }
    if let Some(token) = access.token() {
        for host in access.hosts() {
            tracing::info!(
                "to use Eren from another device, open this link there once: \
                 http://{host}:{port}/?{}={token}",
                eren_server::access::QUERY
            );
        }
        if token_in_file {
            tracing::info!(
                "the access token is in {} — delete it and restart to sign every device out",
                eren_shared::brand::home().join("access_token").display()
            );
        }
    }
}

/// How to spell a bind address in a URL somebody can click.
///
/// `0.0.0.0` is not an address you connect to, so printing it as a link sends
/// people somewhere that does not answer; from this machine the answer is
/// loopback either way.
fn displayable(bind: std::net::IpAddr) -> String {
    if bind.is_unspecified() {
        "127.0.0.1".to_string()
    } else if bind.is_ipv6() {
        format!("[{bind}]")
    } else {
        bind.to_string()
    }
}

/// Bring state from before the rename across: the home folder once, and a
/// note for each setting still spelled the old way.
///
/// Never silent. Moving a folder in somebody's home directory is something
/// they should be able to find in the log, and a variable that still works
/// under its old name is one they will want to rename before it stops.
fn adopt_legacy_state() -> anyhow::Result<()> {
    use eren_shared::brand::{adopt_legacy_home, legacy_env_in_use, Adoption};
    match adopt_legacy_home() {
        Ok(Adoption::Nothing) => {}
        Ok(Adoption::Moved { from, to }) => tracing::info!(
            from = %from.display(),
            to = %to.display(),
            "moved the home folder to its new name and left a link at the old one"
        ),
        Ok(Adoption::Both { legacy }) => tracing::warn!(
            legacy = %legacy.display(),
            using = %eren_shared::brand::home().display(),
            "found the old home folder beside the new one; using the new one and \
             leaving the old one alone"
        ),
        Err(e) => anyhow::bail!(
            "could not move the old home folder to {}: {e}",
            eren_shared::brand::home().display()
        ),
    }
    for (old, new) in legacy_env_in_use() {
        tracing::warn!("{old} is still read, but has been renamed {new}");
    }
    Ok(())
}

/// `eren admin …`: account changes made from this machine's shell, which is
/// how the first admin comes to exist and how a lost admin password is
/// recovered. Talks to the same database `serve` does.
async fn admin(cmd: AdminCmd) -> anyhow::Result<()> {
    let (db, _embedded) = admin_database().await?;
    match cmd {
        AdminCmd::Create {
            username,
            password_stdin,
        } => {
            if eren_core::users::accounts_on(&db).await? {
                anyhow::bail!(
                    "an admin already exists; there is only ever one \
                     (`eren admin reset-password <name>` if its password is lost)"
                );
            }
            let password = read_password(password_stdin)?;
            let user = eren_core::users::create_admin(&db, &username, &password)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "✓ {} is the admin. Accounts are on: every browser now signs in, and \
                 the workspaces made so far belong to {}.\n  Sign-up is closed: open it \
                 under Users in the dashboard if other people should make their own \
                 accounts, and close it again once they have.",
                user.username, user.username
            );
        }
        AdminCmd::ResetPassword {
            username,
            password_stdin,
        } => {
            let Some(user) = eren_core::users::by_username(&db, &username).await? else {
                anyhow::bail!("there is no account called {username}");
            };
            let password = read_password(password_stdin)?;
            eren_core::users::set_password(&db, user.id, &password, false)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "✓ new password set for {}; it is signed out everywhere",
                user.username
            );
        }
    }
    Ok(())
}

/// The password, typed twice without echo — or one line from stdin, for a
/// script (`docker compose exec -T eren eren admin create … --password-stdin`).
fn read_password(from_stdin: bool) -> anyhow::Result<String> {
    if from_stdin {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let password = line.trim_end_matches(['\r', '\n']).to_string();
        eren_core::users::check_password(&password).map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok(password);
    }
    loop {
        let first = rpassword::prompt_password("Password: ")?;
        if let Err(e) = eren_core::users::check_password(&first) {
            eprintln!("{e}");
            continue;
        }
        let again = rpassword::prompt_password("Again: ")?;
        if first == again {
            return Ok(first);
        }
        eprintln!("those differ; once more");
    }
}

/// The database `serve` uses: `DATABASE_URL` when set (the container's
/// case); else the managed Postgres a running server left its port for; else
/// the managed Postgres started here, for a server that is not running.
async fn admin_database() -> anyhow::Result<(Db, Option<postgresql_embedded::PostgreSQL>)> {
    if let Ok(url) = std::env::var("DATABASE_URL") {
        return Ok((Db::connect(&url).await?, None));
    }
    adopt_legacy_state()?;
    let home = eren_shared::brand::home();
    let running = async {
        let port: u16 = tokio::fs::read_to_string(home.join("pg_port"))
            .await
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let password = tokio::fs::read_to_string(home.join("pg_password"))
            .await
            .ok()?;
        let settings = postgresql_embedded::Settings {
            port,
            password: password.trim().to_string(),
            ..postgresql_embedded::Settings::default()
        };
        Db::connect(&settings.url(eren_shared::brand::DATABASE))
            .await
            .ok()
    };
    if let Some(db) = running.await {
        return Ok((db, None));
    }
    let pg = start_embedded_postgres(&home).await?;
    let db = Db::connect(&pg.settings().url(eren_shared::brand::DATABASE)).await?;
    Ok((db, Some(pg)))
}

async fn start_embedded_postgres(
    home: &std::path::Path,
) -> anyhow::Result<postgresql_embedded::PostgreSQL> {
    use postgresql_embedded::{PostgreSQL, Settings};

    // The cluster is initialized with a password on first boot; every later
    // boot must present the same one, so persist it beside the data dir.
    let password_file = home.join("pg_password");
    let password = match tokio::fs::read_to_string(&password_file).await {
        Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => {
            use rand::distr::{Alphanumeric, SampleString};
            let p = Alphanumeric.sample_string(&mut rand::rng(), 32);
            tokio::fs::write(&password_file, &p).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &password_file,
                    std::fs::Permissions::from_mode(0o600),
                );
            }
            p
        }
    };

    let settings = Settings {
        data_dir: home.join("pgdata"),
        temporary: false,
        password,
        ..Settings::default()
    };
    let mut pg = PostgreSQL::new(settings);
    pg.setup().await?;
    pg.start().await?;
    let database = eren_shared::brand::DATABASE;
    if eren_core::legacy::adopt_database(
        &pg.settings().url("postgres"),
        eren_shared::brand::LEGACY_DATABASE,
        database,
    )
    .await?
    {
        tracing::info!(database, "renamed the managed database to its new name");
    }
    if !pg.database_exists(database).await? {
        pg.create_database(database).await?;
    }
    // For `eren admin` while this server runs: the port is chosen fresh each
    // boot and a second instance cannot open the same data directory, so the
    // CLI needs to be told where this one is. Not a secret — the password
    // beside it is, and stays 0600.
    let _ = tokio::fs::write(home.join("pg_port"), pg.settings().port.to_string()).await;
    tracing::info!(port = pg.settings().port, "embedded postgres up");
    Ok(pg)
}

fn max_concurrent() -> usize {
    eren_shared::brand::var("MAX_CONCURRENT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

async fn doctor() -> anyhow::Result<()> {
    // First, like `serve`: the person running a check after upgrading should
    // see the move and any renamed variables here, not on the next start.
    adopt_legacy_state()?;
    // Compliance invariant: we probe tools by RUNNING them, never by
    // inspecting their config or credential files. That is exactly what
    // `Engine::detect` does, which is why this loops the registry rather than
    // hand-coding a `--version` call per engine.
    let mut ok = true;

    match run_version("git", &["--version"]).await {
        Some(v) => println!("✓ git: {v}"),
        None => {
            println!("✗ git: not found on PATH");
            ok = false;
        }
    }

    // GitHub is optional — everything works without it — so a missing `gh` is
    // reported with a dot rather than a cross, the same way an uninstalled
    // engine is. An *expired* login is different: `gh` is right there and every
    // command it runs will fail, so it says which account and why.
    match eren_core::github::detect().await {
        None => println!("· gh: not installed — GitHub features won't be offered"),
        Some(info) if info.usable() => {
            let who = info
                .active()
                .map(|a| format!(" — {} on {}", a.login, a.host))
                .unwrap_or_default();
            println!("✓ gh: {}{who}", info.version);
        }
        Some(info) => {
            // `!` rather than `✗`, and `ok` is deliberately left alone: Eren
            // is completely usable without GitHub, so this must not fail a
            // setup check or contradict the "All good" at the end. It is a
            // thing to know, not a thing that is broken.
            println!("! gh: {} — not logged in", info.version);
            for account in &info.accounts {
                if let Some(problem) = &account.problem {
                    println!("    {} on {}: {problem}", account.login, account.host);
                }
            }
            println!("    GitHub features stay hidden until: gh auth login");
        }
    }

    let mut found = 0;
    for engine in real_engines(LocalHosts::default()) {
        match engine.detect().await {
            Some(info) => {
                found += 1;
                println!("✓ {}: {}", engine.label(), info.version);
                let providers = describe_providers(&info);
                if providers != "—" {
                    println!("  providers: {providers}");
                }
                let caps = engine.capabilities();
                if !caps.interactive_permissions {
                    if caps.auto_edit {
                        println!(
                            "  note: can't ask permission mid-run — use Auto-edit or Don't-ask"
                        );
                    } else {
                        println!("  note: can't ask permission mid-run and has no edit-only mode — use Full Auto");
                    }
                }
                if !caps.structured_rate_limit {
                    println!("  note: no rate-limit signal, so the queue can't back off for it");
                }
                if !caps.mcp_tools {
                    println!("  note: can't carry Eren's tools — not offered for chat, managers or teams");
                }
            }
            None => {
                print!("· {}: not installed — it won't be offered", engine.label());
                match install_hint(engine.id()) {
                    Some(where_from) => println!("\n    {where_from}"),
                    None => println!(),
                }
            }
        }
    }

    // A local runtime is the one case where "not installed" can be wrong: the
    // app is right there and its server is switched off, or OpenCode — which
    // is what actually drives it — is the piece that's missing. `detect` can
    // only say yes or no, so the difference is spelled out here.
    for hint in eren_engines::local::hints("opencode").await {
        println!("\n! {hint}");
    }

    if found == 0 {
        println!("\n✗ no agent CLI found. Install at least one:");
        println!("    claude   → https://code.claude.com");
        println!("    opencode → https://opencode.ai");
        ok = false;
    } else {
        println!("\n  (login is verified on first run; if runs fail immediately, start the");
        println!("   CLI interactively once and log in)");
    }

    if ok {
        println!("\nAll good. Start with: eren serve");
    } else {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_version(bin: &str, args: &[&str]) -> Option<String> {
    let out = env_guard::command(bin).args(args).output().await.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod docs_tests;

#[cfg(test)]
mod tests {
    use super::displayable;

    #[test]
    fn an_unspecified_bind_is_shown_as_somewhere_you_can_actually_click() {
        // `http://0.0.0.0:4820` is not a place; from this machine the answer is
        // loopback whatever it bound to.
        assert_eq!(displayable("0.0.0.0".parse().unwrap()), "127.0.0.1");
        assert_eq!(displayable("::".parse().unwrap()), "127.0.0.1");
        // A specific address is itself, and IPv6 needs its brackets in a URL.
        assert_eq!(displayable("192.168.1.5".parse().unwrap()), "192.168.1.5");
        assert_eq!(displayable("::1".parse().unwrap()), "[::1]");
    }
}
