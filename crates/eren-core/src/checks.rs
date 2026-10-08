//! Checks: the project's own test and lint commands, run in a card's worktree
//! once an agent has finished, so a person reviews work that is known to pass.
//!
//! Two rules decide everything else here.
//!
//! **Only a person sets the commands.** They live in `project_checks`, which has
//! one writer — `routes/checks.rs`, behind the write header — and a test that
//! fails if anything else writes it. See `attention.rs` for why a shell command
//! must never sit where an agent can write it.
//!
//! **Only Full Auto runs them unasked.** A check executes code the agent may
//! have just edited — a `package.json` script, a `build.rs`, the tests
//! themselves. An agent in Full Auto already has an unprompted shell, so
//! running its checks grants it nothing new. In any other mode a person clicks
//! "Run checks", and that click is the consent: running them automatically
//! after an Auto-edit run would hand an agent that was refused Bash a way to
//! execute anything, with no prompt.
//!
//! Every spawn goes through `env_guard::command_without_auth`, in its own
//! process group, so a timeout kills the whole tree — `cargo test` and the
//! test binaries it started, not just the `sh` in front of them — and so the
//! code a check runs sees none of the person's provider keys, tokens or
//! passwords: that code is the agent's, and an Auto-edit agent that cannot run
//! Bash can still write a test. A suite that needs a key reads it from a file
//! of its own.

use crate::db::Db;
use crate::worktrees::manager;
use eren_shared::env_guard;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use uuid::Uuid;

/// Bounds on what a person can configure, enforced by the one writer.
pub const MAX_COMMANDS: usize = 10;
pub const MAX_COMMAND_CHARS: usize = 2000;
pub const MAX_NAME_CHARS: usize = 60;
pub const TIMEOUT_RANGE: std::ops::RangeInclusive<i32> = 10..=3600;
pub const MAX_AUTO_FIX: i32 = 3;

/// How much of a command's output is kept: its end, which is where a test
/// runner puts the failures and the summary.
const TAIL_BYTES: usize = 32 * 1024;

/// One configured command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub command: String,
}

/// A project's checks, as configured. Only exists when there is at least one.
#[derive(Debug, Clone)]
pub struct Config {
    pub commands: Vec<Check>,
    pub timeout: Duration,
    pub auto_fix_attempts: i32,
}

/// The project's checks, or `None` when it has none configured.
pub async fn config(db: &Db, project_id: Uuid) -> anyhow::Result<Option<Config>> {
    let row = sqlx::query(
        "SELECT commands, timeout_secs, auto_fix_attempts FROM project_checks WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(&db.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let commands: Vec<Check> = serde_json::from_value(row.get("commands"))?;
    if commands.is_empty() {
        return Ok(None);
    }
    Ok(Some(Config {
        commands,
        timeout: Duration::from_secs(row.get::<i32, _>("timeout_secs").max(1) as u64),
        auto_fix_attempts: row.get("auto_fix_attempts"),
    }))
}

/// How one command went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub name: String,
    pub command: String,
    /// `None` when it never exited on its own: it could not start, or it was
    /// killed at the timeout.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub ms: u64,
    pub output_tail: String,
}

impl CheckResult {
    pub fn passed(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// Run one command in `dir`, killing its whole process tree at `timeout`.
pub async fn run_one(dir: &Path, check: &Check, timeout: Duration) -> CheckResult {
    let started = Instant::now();
    let finish = |exit_code, timed_out, output_tail| CheckResult {
        name: check.name.clone(),
        command: check.command.clone(),
        exit_code,
        timed_out,
        ms: started.elapsed().as_millis() as u64,
        output_tail,
    };

    let mut cmd = env_guard::command_without_auth("sh");
    // One stream, in the order it was written. Read from two pipes, a test
    // runner's "FAILED" on stderr lands before the stdout lines that led to
    // it, and the log reads backwards. `exec 2>&1` first makes the shell — and
    // everything it starts — write both to the one pipe.
    cmd.arg("-c")
        .arg(format!("exec 2>&1\n{}", check.command))
        .current_dir(dir)
        // Tools print plainer, non-interactive output under CI, and colour
        // codes would be noise in a log read in a browser.
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return finish(None, false, format!("could not start: {e}")),
    };
    let pid = child.id();

    let tail = Arc::new(Mutex::new(Tail::default()));
    let pumps = [
        child
            .stdout
            .take()
            .map(|r| tokio::spawn(pump(r, tail.clone()))),
        child
            .stderr
            .take()
            .map(|r| tokio::spawn(pump(r, tail.clone()))),
    ];

    let (exit_code, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(_)) => (None, false),
        Err(_) => (None, true),
    };
    // Whether it finished or not, nothing a check started outlives it: a
    // server a test suite forgot to stop would otherwise hold its port until
    // someone found it by hand.
    kill_group(pid);
    let _ = child.kill().await;

    // The pipes close when the group dies. A grandchild that escaped into its
    // own session could hold one open forever, so the wait for them is bounded.
    for pump in pumps.into_iter().flatten() {
        let _ = tokio::time::timeout(Duration::from_secs(2), pump).await;
    }
    let mut output = tail.lock().map(|t| t.text()).unwrap_or_default();
    if timed_out {
        output.push_str(&format!(
            "\n[eren: stopped after {}s — the check's time limit]",
            timeout.as_secs()
        ));
    }
    finish(exit_code, timed_out, output)
}

/// The last `TAIL_BYTES` of everything a command printed, both streams.
#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
    dropped: bool,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        // Trimmed in batches rather than per chunk, so a chatty command does
        // not pay a memmove for every line it prints.
        if self.bytes.len() > TAIL_BYTES * 2 {
            let cut = self.bytes.len() - TAIL_BYTES;
            self.bytes.drain(..cut);
            self.dropped = true;
        }
    }

    fn text(&self) -> String {
        let start = self.bytes.len().saturating_sub(TAIL_BYTES);
        let body = String::from_utf8_lossy(&self.bytes[start..]);
        if self.dropped || start > 0 {
            format!("…\n{body}")
        } else {
            body.into_owned()
        }
    }
}

async fn pump(mut reader: impl AsyncRead + Unpin, tail: Arc<Mutex<Tail>>) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Ok(mut t) = tail.lock() {
                    t.push(&chunk[..n]);
                }
            }
        }
    }
}

/// SIGKILL a whole process group. The group was created for this command, so
/// its id is the child's pid; once everyone in it has exited this is a no-op.
#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    if let Some(pid) = pid {
        // SAFETY: a plain syscall with integer arguments; a group that no
        // longer exists returns ESRCH, which is ignored.
        unsafe {
            kill(-(pid as i32), 9);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

/// How a check run ended, in a form a notification or a card can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub status: String,
    pub passed: usize,
    pub total: usize,
}

impl Summary {
    /// "checks passed (3/3)", "checks failed (2/3 passed)" …
    pub fn line(&self) -> String {
        match self.status.as_str() {
            "passed" => format!("checks passed ({}/{})", self.passed, self.total),
            "failed" => format!("checks failed ({}/{} passed)", self.passed, self.total),
            "canceled" => "checks were canceled".to_string(),
            _ => "checks could not run".to_string(),
        }
    }
}

/// One check run at a time on this machine. A test suite is the heaviest
/// thing Eren starts that is not an agent, and two at once — or ten, after
/// a manager pass lands a column of cards — would fight for the same cores.
static SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Record a new check run for a card, superseding any still pending.
///
/// Only the newest result can describe the worktree as it is now, so an older
/// run still waiting or going is canceled rather than left to report on code
/// that has since changed.
pub async fn begin(
    db: &Db,
    task_id: Uuid,
    run_id: Option<Uuid>,
    started_by: &str,
) -> anyhow::Result<Uuid> {
    let mut tx = db.pool.begin().await?;
    cancel_in(&mut tx, task_id).await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO check_runs (task_id, run_id, started_by) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(task_id)
    .bind(run_id)
    .bind(started_by)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Cancel a card's pending check runs — it merged, or newer work replaced them.
pub async fn cancel_for_task(db: &Db, task_id: Uuid) -> anyhow::Result<()> {
    let mut tx = db.pool.begin().await?;
    cancel_in(&mut tx, task_id).await?;
    tx.commit().await?;
    Ok(())
}

async fn cancel_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task_id: Uuid,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE check_runs SET status = 'canceled', finished_at = now()
          WHERE task_id = $1 AND status IN ('queued', 'running')",
    )
    .bind(task_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// On boot: a check run that was going when the server stopped never finished.
pub async fn recover_interrupted(db: &Db) -> anyhow::Result<u64> {
    let done = sqlx::query(
        "UPDATE check_runs SET status = 'error', finished_at = now(),
                error = 'the server stopped while these checks were running'
          WHERE status IN ('queued', 'running')",
    )
    .execute(&db.pool)
    .await?;
    Ok(done.rows_affected())
}

/// Run every configured command, in order, and record how each went.
///
/// All of them, even after one fails: "the tests fail" and "the tests fail and
/// so does the linter" are different amounts of work, and the person deciding
/// what to do next should see both.
pub async fn execute(
    db: &Db,
    check_run_id: Uuid,
    dir: &Path,
    config: &Config,
) -> anyhow::Result<Summary> {
    let total = config.commands.len();
    let ended = |status: &str, passed| Summary {
        status: status.to_string(),
        passed,
        total,
    };
    let _slot = SLOT.acquire().await?;

    let started =
        sqlx::query("UPDATE check_runs SET status = 'running' WHERE id = $1 AND status = 'queued'")
            .bind(check_run_id)
            .execute(&db.pool)
            .await?;
    if started.rows_affected() == 0 {
        // Superseded or canceled while it waited for the slot.
        return Ok(ended("canceled", 0));
    }
    if !dir.is_dir() {
        sqlx::query(
            "UPDATE check_runs SET status = 'error', finished_at = now(),
                    error = 'the worktree is gone — the card was merged or discarded'
              WHERE id = $1",
        )
        .bind(check_run_id)
        .execute(&db.pool)
        .await?;
        return Ok(ended("error", 0));
    }

    let before = manager::changed_paths(dir).await.unwrap_or_default();
    let mut results: Vec<CheckResult> = vec![];
    for check in &config.commands {
        if !still_running(db, check_run_id).await? {
            return Ok(ended(
                "canceled",
                results.iter().filter(|r| r.passed()).count(),
            ));
        }
        results.push(run_one(dir, check, config.timeout).await);
        // Written after every command, so the card shows progress through a
        // long suite rather than nothing until the end.
        sqlx::query("UPDATE check_runs SET results = $2 WHERE id = $1 AND status = 'running'")
            .bind(check_run_id)
            .bind(serde_json::to_value(&results)?)
            .execute(&db.pool)
            .await?;
    }
    // New paths only. A check that rewrites a file the agent had already
    // changed is invisible to `git status` — a known limit, stated rather than
    // papered over with a content hash of the whole tree.
    let after = manager::changed_paths(dir).await.unwrap_or_default();
    let dirtied: Vec<&String> = after.difference(&before).collect();

    let passed = results.iter().filter(|r| r.passed()).count();
    let status = if passed == total { "passed" } else { "failed" };
    let settled = sqlx::query(
        "UPDATE check_runs SET status = $2, dirtied = $3, finished_at = now()
          WHERE id = $1 AND status = 'running'",
    )
    .bind(check_run_id)
    .bind(status)
    .bind(serde_json::to_value(&dirtied)?)
    .execute(&db.pool)
    .await?;
    if settled.rows_affected() == 0 {
        return Ok(ended("canceled", passed));
    }
    Ok(ended(status, passed))
}

async fn still_running(db: &Db, check_run_id: Uuid) -> anyhow::Result<bool> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM check_runs WHERE id = $1")
        .bind(check_run_id)
        .fetch_optional(&db.pool)
        .await?;
    Ok(status.as_deref() == Some("running"))
}

/// Check what a person submitted, before it is stored as something this
/// machine will run. Returns the cleaned commands.
pub fn validate(
    commands: &[Check],
    timeout_secs: i32,
    auto_fix_attempts: i32,
) -> Result<Vec<Check>, String> {
    if commands.len() > MAX_COMMANDS {
        return Err(format!("at most {MAX_COMMANDS} checks"));
    }
    if !TIMEOUT_RANGE.contains(&timeout_secs) {
        return Err(format!(
            "the time limit must be between {} and {} seconds",
            TIMEOUT_RANGE.start(),
            TIMEOUT_RANGE.end()
        ));
    }
    if !(0..=MAX_AUTO_FIX).contains(&auto_fix_attempts) {
        return Err(format!(
            "automatic fixes must be between 0 and {MAX_AUTO_FIX}"
        ));
    }
    let mut cleaned = vec![];
    for (i, c) in commands.iter().enumerate() {
        let command = c.command.trim();
        if command.is_empty() {
            return Err(format!("check {} has no command", i + 1));
        }
        if command.chars().count() > MAX_COMMAND_CHARS {
            return Err(format!(
                "check {} is longer than {MAX_COMMAND_CHARS} characters",
                i + 1
            ));
        }
        let name = match c.name.trim() {
            "" => command
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" "),
            n => n.to_string(),
        };
        if name.chars().count() > MAX_NAME_CHARS {
            return Err(format!(
                "check {} has a name longer than {MAX_NAME_CHARS} characters",
                i + 1
            ));
        }
        cleaned.push(Check {
            name,
            command: command.to_string(),
        });
    }
    Ok(cleaned)
}

impl crate::runs::orchestrator::Orchestrator {
    /// Run a card's checks after its agent finished, then say so.
    ///
    /// Spawned, never awaited by the run: the run's concurrency slot is already
    /// released, so a ten-minute test suite holds back no other agent.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn settle_checks(
        self: std::sync::Arc<Self>,
        task_id: Uuid,
        run_id: Uuid,
        check_run_id: Uuid,
        dir: std::path::PathBuf,
        config: Config,
        title: String,
        chat_id: Option<Uuid>,
        summarize: bool,
    ) {
        let summary = match execute(&self.db, check_run_id, &dir, &config).await {
            Ok(summary) => summary,
            Err(e) => {
                tracing::warn!(%task_id, error = %e, "checks could not run");
                let _ = sqlx::query(
                    "UPDATE check_runs SET status = 'error', error = $2, finished_at = now()
                      WHERE id = $1 AND status IN ('queued', 'running')",
                )
                .bind(check_run_id)
                .bind(e.to_string())
                .execute(&self.db.pool)
                .await;
                Summary {
                    status: "error".into(),
                    passed: 0,
                    total: config.commands.len(),
                }
            }
        };
        // Superseded by newer work, which will announce itself.
        if summary.status == "canceled" {
            return;
        }
        // The card's thread says how the work it just reported fared.
        let line = summary.line();
        let mut said = line[..1].to_uppercase();
        said.push_str(&line[1..]);
        said.push('.');
        if let Err(e) =
            crate::runs::report::post_system(&self.db, task_id, Some(run_id), &said).await
        {
            tracing::warn!(%task_id, error = %e, "could not note the checks on the card");
        }
        self.announce_ready(run_id, &title, chat_id, false, Some(&summary))
            .await;

        let fixing = summary.status == "failed"
            && self
                .fix_failing_checks(task_id, check_run_id, &config)
                .await;
        // A fix run reports for itself; otherwise the silent run is asked what
        // it did — after the fix decision, since a queued summary would
        // otherwise be the live run that refuses the fix.
        if summarize && !fixing {
            self.ask_for_summary(task_id, run_id).await;
        }
        // The review a policy asks for comes after the checks. It waits for a
        // fix or a summary just queued, whose own completion asks again.
        self.settle_review(task_id, run_id, "checks_settled").await;
    }

    /// Start the bounded auto-fix for failing checks. True when a fix run was
    /// queued.
    async fn fix_failing_checks(&self, task_id: Uuid, check_run_id: Uuid, config: &Config) -> bool {
        if config.auto_fix_attempts == 0 {
            return false;
        }
        // Bounded: counted from the last run that was not itself a checks fix
        // or a summary pass, so a person's own run or review note resets it,
        // and an agent that cannot make the tests pass stops trying after the
        // number set. A summary or review pass is Eren asking, not a person,
        // and must not reset the count.
        let attempts: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM runs
              WHERE task_id = $1 AND trigger = 'checks'
                AND created_at > COALESCE(
                      (SELECT max(created_at) FROM runs
                        WHERE task_id = $1 AND trigger NOT IN ('checks', 'summary', 'peer_review')),
                      '-infinity')",
        )
        .bind(task_id)
        .fetch_one(&self.db.pool)
        .await
        .unwrap_or(i64::MAX);
        if attempts >= config.auto_fix_attempts as i64 {
            tracing::info!(%task_id, attempts, "checks still fail; leaving it for a person");
            crate::wake::raise(
                &self.db,
                task_id,
                None,
                crate::wake::Kind::ChecksExhausted,
                &format!("after {attempts} fix attempts"),
            )
            .await;
            return false;
        }
        match self
            .enqueue_follow_up(
                task_id,
                crate::runs::follow_up::FollowUp::FailingChecks { check_run_id },
            )
            .await
        {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(%task_id, error = %e, "could not start a fix for the failing checks");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(command: &str) -> Check {
        Check {
            name: "c".into(),
            command: command.into(),
        }
    }

    #[tokio::test]
    async fn a_passing_command_passes_and_keeps_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let r = run_one(
            dir.path(),
            &check("echo hello; echo oops >&2"),
            Duration::from_secs(10),
        )
        .await;
        assert!(r.passed(), "{r:?}");
        assert!(r.output_tail.contains("hello") && r.output_tail.contains("oops"));
    }

    /// stdout and stderr interleaved as written, not grouped by stream.
    #[tokio::test]
    async fn output_keeps_the_order_it_was_written_in() {
        let dir = tempfile::tempdir().unwrap();
        let r = run_one(
            dir.path(),
            &check("echo one; echo two >&2; echo three; echo four >&2"),
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(r.output_tail, "one\ntwo\nthree\nfour\n");
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let r = run_one(dir.path(), &check("exit 3"), Duration::from_secs(10)).await;
        assert!(!r.passed());
        assert_eq!(r.exit_code, Some(3));
        assert!(!r.timed_out);
    }

    #[tokio::test]
    async fn it_runs_in_the_worktree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "here").unwrap();
        let r = run_one(
            dir.path(),
            &check("cat marker.txt"),
            Duration::from_secs(10),
        )
        .await;
        assert!(r.passed());
        assert!(r.output_tail.contains("here"));
    }

    /// The bug this guards: killing only `sh` leaves `cargo test` — or here,
    /// the `sleep` it started — running long after the check "ended".
    #[cfg(unix)]
    #[tokio::test]
    async fn a_timeout_kills_the_whole_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        let cmd = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let started = Instant::now();
        let r = run_one(dir.path(), &check(&cmd), Duration::from_secs(1)).await;
        assert!(r.timed_out, "{r:?}");
        assert!(!r.passed());
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(r.output_tail.contains("time limit"));

        let pid = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .to_string();
        // Dead means gone, or a zombie: an orphan's new parent is whatever
        // reaps orphans here, and a container's init may never get to it.
        // Linux only, because it reads /proc.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit(')').next().map(|s| s.trim().to_string()))
            .is_some_and(|rest| !rest.starts_with('Z'));
        if cfg!(not(target_os = "linux")) {
            return;
        }
        assert!(!alive, "the grandchild {pid} outlived the check");
    }

    #[tokio::test]
    async fn only_the_end_of_a_long_output_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let r = run_one(
            dir.path(),
            &check("i=0; while [ $i -lt 20000 ]; do echo line-$i; i=$((i+1)); done; echo THE-END"),
            Duration::from_secs(30),
        )
        .await;
        assert!(r.output_tail.len() <= TAIL_BYTES + 8);
        assert!(r.output_tail.starts_with('…'));
        assert!(
            r.output_tail.contains("THE-END"),
            "the summary is at the end"
        );
        assert!(!r.output_tail.contains("line-0\n"));
    }

    /// A check is somebody's test suite, and must not be handed the object
    /// storage keys the server holds.
    #[tokio::test]
    async fn a_check_does_not_inherit_erens_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let printenv = eren_shared::own_secrets()
            .map(|k| format!("printf '%s=%s;' {k} \"${k}\""))
            .collect::<Vec<_>>()
            .join("; ");
        let r = run_one(dir.path(), &check(&printenv), Duration::from_secs(10)).await;
        for key in eren_shared::own_secrets() {
            assert!(
                r.output_tail.contains(&format!("{key}=;")),
                "{}",
                r.output_tail
            );
        }
    }

    /// A check runs code the agent wrote. The person's own keys are not for
    /// it — unlike an engine, which is given them on purpose.
    #[tokio::test]
    async fn a_check_does_not_inherit_a_provider_key() {
        let dir = tempfile::tempdir().unwrap();
        // A name no other test sets, so it cannot race one that unsets it.
        std::env::set_var("EREN_CHECK_TEST_FAKE_API_KEY", "would-leak");
        std::env::set_var("EREN_CHECK_TEST_PLAIN", "kept");
        let r = run_one(
            dir.path(),
            &check("printf 'key=%s;plain=%s;' \"$EREN_CHECK_TEST_FAKE_API_KEY\" \"$EREN_CHECK_TEST_PLAIN\""),
            Duration::from_secs(10),
        )
        .await;
        assert!(r.output_tail.contains("key=;plain=kept;"), "{}", r.output_tail);
    }

    #[test]
    fn a_summary_reads_as_a_sentence() {
        let s = |status: &str, passed, total| Summary {
            status: status.into(),
            passed,
            total,
        };
        assert_eq!(s("passed", 3, 3).line(), "checks passed (3/3)");
        assert_eq!(s("failed", 2, 3).line(), "checks failed (2/3 passed)");
    }

    #[test]
    fn validation_cleans_and_bounds_what_a_person_submits() {
        let ok = validate(
            &[Check {
                name: " ".into(),
                command: "  cargo test --workspace ".into(),
            }],
            600,
            1,
        )
        .unwrap();
        assert_eq!(ok[0].command, "cargo test --workspace");
        assert_eq!(
            ok[0].name, "cargo test",
            "a blank name is derived from the command"
        );

        assert!(validate(&[check("  ")], 600, 0).is_err());
        assert!(validate(&[check("true")], 5, 0).is_err());
        assert!(validate(&[check("true")], 600, 9).is_err());
        assert!(validate(&vec![check("true"); MAX_COMMANDS + 1], 600, 0).is_err());
        assert!(
            validate(&[], 600, 0).unwrap().is_empty(),
            "clearing all checks is allowed"
        );
    }
}
