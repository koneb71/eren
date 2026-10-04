//! Claude Code adapter: spawns the official `claude` binary from PATH in
//! headless mode and normalizes its stream-json output.

pub mod compat;
pub mod mcp;
pub mod stream_parser;

use crate::{Capabilities, Engine, EngineInfo, EngineProcess, ProcessHandle, RunSpec};
use async_trait::async_trait;
use eren_shared::env_guard;
use eren_shared::{ErenEvent, PermissionMode};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc;

/// Build the argv for a run, minus the binary itself.
///
/// Split out from `start` so the flag logic is testable without spawning
/// anything — the conditionals here (empty allowed-tools, the three-way
/// permission-prompt guard) are the parts that have historically been easy to
/// get subtly wrong.
fn claude_args(spec: &RunSpec, mcp_config: Option<PathBuf>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-p".into(),
        // Through `positional`: a brief that opens with a markdown list item
        // ("- fix the login") was read as a flag, and a one-word chat message
        // can name a subcommand.
        crate::positional(spec.prompt.clone()).into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--model".into(),
        spec.model_id.clone().into(),
    ];

    // Omitted when unset so the CLI keeps its own default. An unknown value
    // is only a warning there, so this is safe on an older CLI too.
    if let Some(effort) = spec.effort {
        args.push("--effort".into());
        args.push(effort.as_str().into());
    }

    match spec.permission_mode {
        PermissionMode::Reviewed => {}
        PermissionMode::AutoEdit => {
            args.push("--permission-mode".into());
            args.push("acceptEdits".into());
        }
        PermissionMode::FullAuto => {
            // The orchestrator only ever sets FullAuto for runs whose cwd
            // is an eren-managed worktree; enforced again there.
            args.push("--dangerously-skip-permissions".into());
        }
    }
    if let Some(session) = &spec.resume_session_id {
        args.push("--resume".into());
        args.push(session.clone().into());
    }
    // Empty means "omit the flag", which leaves the CLI on its default tool
    // set. Passing an empty list would instead forbid every tool.
    if !spec.allowed_tools.is_empty() {
        args.push("--allowedTools".into());
        args.push(spec.allowed_tools.join(",").into());
    }
    // `--allowedTools` pre-approves; it does not forbid. Anything that must
    // not run has to be named here, or the CLI will happily reach for it.
    if !spec.denied_tools.is_empty() {
        args.push("--disallowedTools".into());
        args.push(spec.denied_tools.join(",").into());
    }
    if let Some(sys) = &spec.append_system_prompt {
        args.push("--append-system-prompt".into());
        args.push(sys.clone().into());
    }
    let has_mcp = mcp_config.is_some();
    if let Some(mcp) = mcp_config {
        args.push("--mcp-config".into());
        args.push(mcp.into());
    }
    // One flag per directory: `--add-dir` is variadic, so a single flag
    // carrying several values would swallow whatever argument follows.
    // Verified against CLI 2.1.205 that repeating it accumulates.
    for dir in &spec.extra_read_dirs {
        args.push("--add-dir".into());
        args.push(dir.clone().into());
    }
    if spec.permission_prompt_tool && has_mcp && spec.permission_mode != PermissionMode::FullAuto {
        args.push("--permission-prompt-tool".into());
        args.push("mcp__eren__approve".into());
    }
    args
}

pub struct ClaudeEngine {
    /// Binary name resolved from PATH. Always "claude" in production; tests
    /// may point at a stub script.
    pub binary: String,
}

impl Default for ClaudeEngine {
    fn default() -> Self {
        Self {
            binary: "claude".to_string(),
        }
    }
}

#[async_trait]
impl Engine for ClaudeEngine {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn label(&self) -> &'static str {
        "Claude Code"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // The permission proxy: the CLI parks on `mcp__eren__approve`
            // until the user answers in the dashboard.
            interactive_permissions: true,
            // `rate_limit_event` carries `resetsAt`.
            structured_rate_limit: true,
            resume_sessions: true,
            // `--append-system-prompt` appends rather than replaces.
            append_system_prompt: true,
            fixed_model_catalog: true,
            // `total_cost_usd` on the final result line.
            reports_cost: true,
            // `--disallowedTools` is enforced by the CLI.
            enforces_denied_tools: true,
            // `--mcp-config` takes a file Eren writes in its own scratch
            // directory, per run.
            mcp_tools: true,
            auto_edit: true,
            read_only_passes: true,
        }
    }

    async fn detect(&self) -> Option<EngineInfo> {
        let version = env_guard::command(&self.binary)
            .arg("--version")
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;
        // Auth is probed by running the CLI, never by reading its config.
        // A tiny -p invocation fails fast with a login error when logged out.
        Some(EngineInfo {
            version,
            authenticated: true,
            providers: vec![],
            // Claude Code's catalog is fixed and lives in `MODEL_CHOICES`.
            models: vec![],
        })
    }

    fn start(&self, spec: RunSpec) -> anyhow::Result<EngineProcess> {
        // The adapter renders its own dialect now; the orchestrator only says
        // what should be reachable.
        let mcp_config = if spec.mcp.is_empty() {
            None
        } else {
            Some(mcp::write(&mcp::config_dir(), &spec.run_key, &spec.mcp)?)
        };

        let mut cmd = env_guard::command(&self.binary);
        cmd.current_dir(&spec.cwd)
            .args(claude_args(&spec, mcp_config));

        for (k, v) in &spec.extra_env {
            if eren_shared::is_auth_env(k) {
                anyhow::bail!("{}", eren_shared::auth_env_refusal(k));
            }
            cmd.env(k, v);
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (tx, rx) = mpsc::channel::<ErenEvent>(256);

        // stderr watcher: surfaces fatal errors / rate-limit notices that
        // never make it into stream-json.
        let tx_err = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut tail: Vec<String> = vec![];
            while let Ok(Some(line)) = lines.next_line().await {
                if stream_parser::rate_limit_signal(&line) {
                    let _ = tx_err
                        .send(ErenEvent::RateLimited {
                            reset_at: None,
                            message: line.clone(),
                        })
                        .await;
                }
                tail.push(line);
                if tail.len() > 20 {
                    tail.remove(0);
                }
            }
            tracing::debug!(stderr_tail = ?tail, "claude stderr closed");
        });

        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut saw_terminal = false;
            while let Ok(Some(line)) = lines.next_line().await {
                for event in stream_parser::parse_line(&line) {
                    saw_terminal |= matches!(
                        event,
                        ErenEvent::RunCompleted { .. }
                            | ErenEvent::RunFailed { .. }
                            | ErenEvent::RateLimited { .. }
                    );
                    if tx.send(event).await.is_err() {
                        return; // receiver dropped — run canceled
                    }
                }
            }
            if !saw_terminal {
                let _ = tx
                    .send(ErenEvent::RunFailed {
                        reason: "engine process exited without a result message".to_string(),
                    })
                    .await;
            }
        });

        Ok(EngineProcess::new(rx, Box::new(ClaudeHandle { child })))
    }

    /// `claude --resume <id>`, checked against `claude --help` (2.1.x). The
    /// session is found per working directory, which is why the dashboard
    /// prints it after a `cd` into the run's worktree.
    fn interactive_resume_argv(&self, session_id: &str) -> Option<Vec<String>> {
        Some(vec![
            "claude".to_string(),
            "--resume".to_string(),
            session_id.to_string(),
        ])
    }
}

struct ClaudeHandle {
    child: Child,
}

#[async_trait]
impl ProcessHandle for ClaudeHandle {
    async fn interrupt(&mut self) -> anyhow::Result<()> {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // SIGINT lets the CLI checkpoint its session before exiting.
            unsafe { libc_kill(pid as i32, 2) };
            return Ok(());
        }
        self.kill();
        Ok(())
    }

    fn kill(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, sig);
}

#[cfg(test)]
mod tests {
    use super::claude_args;
    use crate::RunSpec;
    use eren_shared::{ModelTier, PermissionMode, ReasoningEffort};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn spec() -> RunSpec {
        RunSpec {
            cwd: std::env::temp_dir(),
            prompt: "do the thing".into(),
            model_tier: ModelTier::Medium,
            model_id: "claude-opus-5".into(),
            effort: None,
            resume_session_id: None,
            permission_mode: PermissionMode::Reviewed,
            allowed_tools: vec![],
            denied_tools: vec![],
            append_system_prompt: None,
            mcp: Default::default(),
            run_key: "test".to_string(),
            extra_read_dirs: vec![],
            permission_prompt_tool: false,
            extra_env: HashMap::new(),
        }
    }

    /// Flatten to lossy strings; assertions read better than OsStr comparisons.
    /// Mirrors what `start` does: a spec with wiring gets a config path, one
    /// without gets none. Passing a bare `None` here would quietly make every
    /// MCP-dependent assertion below vacuous.
    fn args_of(spec: &RunSpec) -> Vec<String> {
        let config = (!spec.mcp.is_empty()).then(|| PathBuf::from("/tmp/eren-test-mcp.json"));
        claude_args(spec, config)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_prompt_that_starts_with_a_dash_is_not_a_flag() {
        let mut s = spec();
        s.prompt = "- fix the login\n- add a test".into();
        let args = args_of(&s);
        assert_eq!(args[0], "-p");
        assert!(!args[1].starts_with('-'), "{:?}", args[1]);
        assert_eq!(args[1].trim_start(), s.prompt);
        // An ordinary prompt travels exactly as written.
        assert_eq!(args_of(&spec())[1], "do the thing");
    }

    #[test]
    fn effort_is_omitted_unless_set() {
        assert!(!args_of(&spec()).contains(&"--effort".to_string()));

        let mut s = spec();
        s.effort = Some(ReasoningEffort::XHigh);
        let args = args_of(&s);
        let i = args
            .iter()
            .position(|a| a == "--effort")
            .expect("flag present");
        assert_eq!(args[i + 1], "xhigh");
    }

    #[test]
    fn extra_read_dirs_become_one_add_dir_flag_each() {
        let mut s = spec();
        s.extra_read_dirs = vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")];
        let args = args_of(&s);

        assert_eq!(args.iter().filter(|a| *a == "--add-dir").count(), 2);
        // Each flag must be immediately followed by its own directory —
        // a single variadic flag would swallow the next argument.
        let first = args.iter().position(|a| a == "--add-dir").unwrap();
        assert_eq!(args[first + 1], "/tmp/a");
        assert_eq!(args[first + 3], "/tmp/b");
    }

    #[test]
    fn no_add_dir_flag_when_there_are_no_extra_dirs() {
        assert!(!args_of(&spec()).iter().any(|a| a == "--add-dir"));
    }

    #[test]
    fn empty_allowed_tools_omits_the_flag_entirely() {
        // Passing an empty --allowedTools would forbid every tool rather than
        // fall back to the CLI default set.
        assert!(!args_of(&spec()).iter().any(|a| a == "--allowedTools"));

        let mut s = spec();
        s.allowed_tools = vec!["Read".into(), "Grep".into()];
        let args = args_of(&s);
        let i = args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(args[i + 1], "Read,Grep");
    }

    #[test]
    fn permission_prompt_tool_requires_an_mcp_config_and_not_full_auto() {
        let mut s = spec();
        s.permission_prompt_tool = true;
        // No MCP config ⇒ no broker to talk to, so the flag is pointless.
        assert!(!args_of(&s).iter().any(|a| a == "--permission-prompt-tool"));

        s.mcp.eren_url = Some("http://127.0.0.1:4820/mcp/run/x".into());
        assert!(args_of(&s).iter().any(|a| a == "--permission-prompt-tool"));

        // FullAuto deliberately bypasses the broker entirely.
        s.permission_mode = PermissionMode::FullAuto;
        let args = args_of(&s);
        assert!(!args.iter().any(|a| a == "--permission-prompt-tool"));
        assert!(args.iter().any(|a| a == "--dangerously-skip-permissions"));
    }

    #[test]
    fn prompt_is_passed_as_a_single_argv_entry() {
        let mut s = spec();
        s.prompt = "line one\nline two --not-a-flag".into();
        let args = args_of(&s);
        let i = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[i + 1], "line one\nline two --not-a-flag");
    }
}
