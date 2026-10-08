//! OpenCode adapter: spawns the official `opencode` binary from PATH in
//! headless mode and normalizes its NDJSON output.
//!
//! Same compliance posture as the Claude adapter — spawn the binary the user
//! installed, read its stdout, never touch its credentials. Whatever provider
//! OpenCode is logged into is the user's business; `detect()` reports the
//! provider *name and auth type* by running `opencode providers list`, which
//! prints no secrets, rather than by reading `auth.json`.

pub mod config;
pub mod stream_parser;
pub mod tools;

use crate::{
    Capabilities, Engine, EngineInfo, EngineProcess, ProcessHandle, ProviderInfo, RunSpec,
};
use async_trait::async_trait;
use eren_shared::env_guard;
use eren_shared::{ErenEvent, PermissionMode, ReasoningEffort};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use stream_parser::StreamState;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

/// Claude's effort vocabulary → OpenCode's `--variant`.
///
/// `None` omits the flag entirely, keeping whatever the model's default is.
/// Medium maps to nothing on purpose: it *is* the default, and naming it
/// would override a provider that has its own idea of normal.
fn variant(effort: ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::Low => Some("minimal"),
        ReasoningEffort::Medium => None,
        ReasoningEffort::High => Some("high"),
        ReasoningEffort::XHigh | ReasoningEffort::Max => Some("max"),
    }
}

/// Build the argv, minus the binary. Split out so the flag logic is testable
/// without spawning anything.
fn opencode_args(spec: &RunSpec) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "run".into(),
        // One argv entry: `message` is variadic, so a split prompt would be
        // silently rejoined with different whitespace. Through `positional`,
        // so a brief opening with "- " is not read as a flag — Ollama and LM
        // Studio runs are built here too.
        crate::positional(spec.prompt.clone()).into(),
        "--format".into(),
        "json".into(),
        // The agent our generated config defines — carries the model,
        // permissions, and (via `instructions`) the persona.
        "--agent".into(),
        config::AGENT_NAME.into(),
        // Belt and braces with `current_dir` and `PWD`; see `start`.
        "--dir".into(),
        spec.cwd.clone().into(),
    ];

    if !spec.model_id.is_empty() {
        args.push("-m".into());
        args.push(spec.model_id.clone().into());
    }
    if let Some(v) = spec.effort.and_then(variant) {
        args.push("--variant".into());
        args.push(v.into());
    }
    if let Some(session) = &spec.resume_session_id {
        // Never `--fork`: Eren's semantics are "continue this session".
        args.push("-s".into());
        args.push(session.clone().into());
    }
    // `--auto` approves anything not explicitly denied. Only FullAuto, which
    // the orchestrator already gates on an eren-managed worktree and a
    // per-project opt-in. Reviewed never reaches here — `vet` refuses it,
    // because OpenCode cannot ask mid-run.
    if spec.permission_mode == PermissionMode::FullAuto {
        args.push("--auto".into());
    }
    args
}

pub struct OpenCodeEngine {
    /// Binary name resolved from PATH. Tests may point at a stub script.
    pub binary: String,
}

impl Default for OpenCodeEngine {
    fn default() -> Self {
        Self {
            binary: "opencode".to_string(),
        }
    }
}

/// Write the persona to a file, because `instructions` takes paths.
fn write_instructions(run_key: &str, body: &str) -> anyhow::Result<PathBuf> {
    let dir = eren_shared::brand::home().join("prompts");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{run_key}.md"));
    std::fs::write(&path, body)?;
    Ok(path)
}

#[async_trait]
impl Engine for OpenCodeEngine {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn label(&self) -> &'static str {
        "OpenCode"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Headless, it can only approve everything (`--auto`) or reject
            // everything. There is no way to answer one prompt mid-run, so
            // Reviewed mode is refused rather than silently widened.
            interactive_permissions: false,
            // Only a provider error string; no reset time to wait for.
            structured_rate_limit: false,
            resume_sessions: true,
            // Via `instructions`, which appends. `agent.prompt` would replace.
            append_system_prompt: true,
            // 75+ providers plus local models — no catalog could keep up.
            fixed_model_catalog: false,
            // Each step's `cost`, summed into the final event.
            reports_cost: true,
            // Permission rules are config the CLI applies, but not verified against
            // every tool a review would deny.
            enforces_denied_tools: false,
            // The config travels in `OPENCODE_CONFIG`, a file in Eren's
            // scratch directory — never the run's folder.
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

        // Names and auth *types* only. This runs the binary rather than
        // reading `auth.json`, which is the whole point.
        let providers = env_guard::command(&self.binary)
            .args(["providers", "list"])
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| parse_providers(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default();

        // `opencode models` lists only what the user's configured providers
        // expose, which is the difference between offering a real catalog and
        // offering a list they'd have to discover was wrong.
        let models = env_guard::command(&self.binary)
            .arg("models")
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| parse_models(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default();

        Some(EngineInfo {
            version,
            authenticated: !providers.is_empty(),
            providers,
            models,
        })
    }

    fn start(&self, spec: RunSpec) -> anyhow::Result<EngineProcess> {
        let instructions = match spec
            .append_system_prompt
            .as_deref()
            .filter(|p| !p.is_empty())
        {
            Some(body) => Some(write_instructions(&spec.run_key, body)?),
            None => None,
        };
        let cfg = config::build(&spec, instructions.as_deref());

        let mut cmd = env_guard::command(&self.binary);
        cmd.current_dir(&spec.cwd).args(opencode_args(&spec));

        // OpenCode resolves its working directory as `PWD ?? cwd()`, and
        // `current_dir` does **not** update `PWD` — the child would inherit
        // Eren's. Left unset, runs silently operate on Eren's own tree
        // while appearing to succeed. Verified against 1.18.9.
        cmd.env("PWD", &spec.cwd);
        // The binary self-updates; pin behaviour for the length of a run.
        cmd.env("OPENCODE_DISABLE_AUTOUPDATE", "1");

        for (k, v) in &spec.extra_env {
            if eren_shared::is_auth_env(k) {
                anyhow::bail!("{}", eren_shared::auth_env_refusal(k));
            }
            cmd.env(k, v);
        }
        // Set after the loop above: this value is generated by Eren, and it
        // must not be overridable by a caller. It carries no credential of
        // Eren's own — but it does carry whatever headers and `env` the
        // person configured on an MCP server they handed this agent, and
        // every process OpenCode's shell starts inherits it. The variable
        // rather than a file because OpenCode applies it *after* a
        // repository's own `opencode.json`, which is what keeps a repository
        // from widening what Eren granted.
        cmd.env("OPENCODE_CONFIG_CONTENT", serde_json::to_string(&cfg)?);

        cmd.stdin(Stdio::null()) // a run that needs input hangs forever otherwise
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (tx, rx) = mpsc::channel::<ErenEvent>(256);

        let tx_err = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut tail: Vec<String> = vec![];
            while let Ok(Some(line)) = lines.next_line().await {
                if eren_shared::rate_limit_signal(&line) {
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
            tracing::debug!(stderr_tail = ?tail, "opencode stderr closed");
        });

        // Grab the pid before the pump takes ownership: cancelling a run
        // needs to signal the process, and the `Child` is moved into the task
        // because the terminal event depends on its exit status.
        let pid = child.id();
        let model = (!spec.model_id.is_empty()).then(|| spec.model_id.clone());
        // The pump owns the child, because the terminal event depends on its
        // exit status — unlike Claude, where a `result` line says how it went.
        tokio::spawn(async move {
            let mut state = StreamState::new(model);
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                for event in stream_parser::parse_line(&line, &mut state) {
                    if tx.send(event).await.is_err() {
                        return; // receiver dropped — run canceled
                    }
                }
            }
            let exit_ok = child.wait().await.map(|s| s.success()).unwrap_or(false);
            let _ = tx.send(state.finish(exit_ok)).await;
        });

        Ok(EngineProcess::new(rx, Box::new(OpenCodeHandle { pid })))
    }

    /// Not offered until the TUI's session flag is checked against an
    /// `opencode` binary.
    fn interactive_resume_argv(&self, _session_id: &str) -> Option<Vec<String>> {
        None
    }
}

/// `Google  api` → `ProviderInfo { name: "Google", auth: "api" }`.
///
/// The CLI wraps this in box-drawing characters and colour codes, so anything
/// that isn't two plain words is skipped rather than guessed at.
fn parse_providers(stdout: &str) -> Vec<ProviderInfo> {
    stdout
        .lines()
        .filter_map(|line| {
            let cleaned: String = strip_ansi(line);
            let text = cleaned.trim_matches(|c: char| !c.is_alphanumeric()).trim();
            let mut parts = text.split_whitespace();
            let (name, auth) = (parts.next()?, parts.next()?);
            // The header and footer lines ("Credentials", "1 credentials")
            // would otherwise pass.
            if name.eq_ignore_ascii_case("credentials") || name.chars().all(|c| c.is_numeric()) {
                return None;
            }
            Some(ProviderInfo {
                name: name.to_string(),
                auth: auth.to_string(),
            })
        })
        .collect()
}

/// `opencode models` prints one `provider/model` per line.
///
/// Anything that isn't that shape is dropped rather than guessed at: a future
/// version adding a header would otherwise put "Models" in the picker.
fn parse_models(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(strip_ansi)
        .map(|l| l.trim().to_string())
        .filter(|l| eren_shared::is_provider_model_shape(l))
        .collect()
}

pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

struct OpenCodeHandle {
    pid: Option<u32>,
}

#[async_trait]
impl ProcessHandle for OpenCodeHandle {
    async fn interrupt(&mut self) -> anyhow::Result<()> {
        // SIGINT so the CLI can checkpoint its session before exiting; the
        // pump owns the `Child`, so this goes through the pid.
        if let Some(pid) = self.pid {
            unsafe {
                libc_kill(pid as i32, 2);
            }
        }
        Ok(())
    }

    fn kill(&mut self) {
        if let Some(pid) = self.pid {
            unsafe {
                libc_kill(pid as i32, 9);
            }
        }
    }
}

pub(crate) unsafe fn libc_kill(pid: i32, sig: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(pid, sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eren_shared::{McpWiring, ModelTier};

    fn spec() -> RunSpec {
        RunSpec {
            cwd: PathBuf::from("/tmp/worktree"),
            prompt: "add a health endpoint".into(),
            model_tier: ModelTier::Medium,
            model_id: "anthropic/claude-sonnet-4-5".into(),
            effort: None,
            resume_session_id: None,
            permission_mode: PermissionMode::AutoEdit,
            allowed_tools: vec![],
            denied_tools: vec![],
            append_system_prompt: None,
            mcp: McpWiring::default(),
            run_key: "run-1".into(),
            extra_read_dirs: vec![],
            permission_prompt_tool: false,
            extra_env: Default::default(),
        }
    }

    fn args_of(spec: &RunSpec) -> Vec<String> {
        opencode_args(spec)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_prompt_that_starts_with_a_dash_is_not_a_flag() {
        let mut s = spec();
        s.prompt = "- fix the login\n- add a test".into();
        let a = args_of(&s);
        assert_eq!(a[0], "run");
        assert!(!a[1].starts_with('-'), "{:?}", a[1]);
        assert_eq!(a[1].trim_start(), s.prompt);
    }

    #[test]
    fn the_prompt_is_one_argv_entry() {
        // `message` is variadic; splitting it would rejoin with different
        // whitespace and quietly change the prompt.
        let a = args_of(&spec());
        assert_eq!(a[1], "add a health endpoint");
    }

    #[test]
    fn the_working_directory_is_passed_explicitly() {
        // Belt and braces against the PWD footgun — see `start`.
        let a = args_of(&spec());
        let i = a
            .iter()
            .position(|x| x == "--dir")
            .expect("--dir must be passed");
        assert_eq!(a[i + 1], "/tmp/worktree");
    }

    #[test]
    fn only_full_auto_gets_the_dangerous_flag() {
        let mut s = spec();
        s.permission_mode = PermissionMode::AutoEdit;
        assert!(!args_of(&s).contains(&"--auto".to_string()));

        s.permission_mode = PermissionMode::FullAuto;
        assert!(args_of(&s).contains(&"--auto".to_string()));
    }

    #[test]
    fn reviewed_never_produces_auto_even_though_vet_should_have_stopped_it() {
        // Defence in depth: if a Reviewed run somehow reached the adapter, it
        // must not be silently upgraded to "approve everything".
        let mut s = spec();
        s.permission_mode = PermissionMode::Reviewed;
        assert!(!args_of(&s).contains(&"--auto".to_string()));
    }

    #[test]
    fn effort_maps_to_opencodes_own_vocabulary() {
        let mut s = spec();
        s.effort = Some(ReasoningEffort::Low);
        assert!(args_of(&s).contains(&"minimal".to_string()));

        s.effort = Some(ReasoningEffort::Max);
        assert!(args_of(&s).contains(&"max".to_string()));

        // Medium is the default; naming it would override a provider that
        // disagrees about what normal means.
        s.effort = Some(ReasoningEffort::Medium);
        assert!(!args_of(&s).contains(&"--variant".to_string()));

        s.effort = None;
        assert!(!args_of(&s).contains(&"--variant".to_string()));
    }

    #[test]
    fn resuming_continues_rather_than_forking() {
        let mut s = spec();
        s.resume_session_id = Some("ses_abc".into());
        let a = args_of(&s);
        let i = a.iter().position(|x| x == "-s").unwrap();
        assert_eq!(a[i + 1], "ses_abc");
        assert!(
            !a.contains(&"--fork".to_string()),
            "forking is not continuing"
        );
    }

    #[test]
    fn pure_is_not_passed() {
        // It hangs on at least one real install, and it disables the plugins
        // some users authenticate through. Documented as a decision.
        assert!(!args_of(&spec()).contains(&"--pure".to_string()));
    }

    #[test]
    fn providers_are_parsed_from_the_cli_not_from_auth_json() {
        let out = "\u{1b}[0m\n┌  Credentials ~/.local/share/opencode/auth.json\n│\n●  Google \u{1b}[90mapi\n│\n└  1 credentials\n";
        let providers = parse_providers(out);
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "Google");
        assert_eq!(providers[0].auth, "api");
    }

    #[test]
    fn an_unauthenticated_install_reports_no_providers() {
        assert!(parse_providers("┌  Credentials\n└  0 credentials\n").is_empty());
    }
}
