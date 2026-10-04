//! OpenAI's Codex CLI as an engine.
//!
//! `codex exec "<task>" --json` runs headless and emits JSON Lines, which is
//! the same shape the other two adapters consume. Spawned from `PATH`, stdout
//! read, nothing else — the first compliance invariant, unchanged.
//!
//! ## Built from documentation once, then corrected against the binary
//!
//! The first version of this file was written without `codex` installed, from
//! OpenAI's non-interactive documentation. Almost none of it survived contact
//! with `codex-cli 0.147.0`, and the corrections are worth knowing about
//! because they were not small:
//!
//! - **`--ask-for-approval` does not exist on `codex exec`.** It is a
//!   top-level flag for the interactive TUI. Passing it made every run die at
//!   argument parsing before a single token was spent, and the docs still
//!   present it as an `exec` flag — so the binary, not the prose, is the
//!   authority here. Approvals are configured instead, and `exec` already
//!   defaults to never asking.
//! - **`codex exec resume` takes a narrower flag set than `codex exec`**: no
//!   `--sandbox`, no `--cd`, no `--add-dir`. Every exec-level flag has to come
//!   *before* the `resume` keyword. The old argv put them after, so the resume
//!   path had a second, independent parse failure.
//! - **`gpt-5-codex` is not a model this CLI knows.** It answers with
//!   "Model metadata … not found. Defaulting to fallback metadata; this can
//!   degrade performance" — the same warning a wholly invented id gets. So
//!   `detect` now reports what `codex doctor` says this install actually runs,
//!   and the tier defaults are derived from that rather than hardcoded.
//! - **openai/codex#15451, the hazard the old header warned about, was the
//!   wrong issue.** It concerns `--output-schema`, which Eren never passes,
//!   not `--json`, which it does; and it was closed as model behaviour rather
//!   than fixed. The warning described a configuration Eren cannot enter, so
//!   it is gone. What *is* worth watching once MCP is exercised in anger is
//!   openai/codex#24536 — `codex exec` completing empty when MCP tools are
//!   deferred behind tool search.
//!
//! What was verified live, on this machine, against a real transcript:
//! `--json` emits JSONL; `thread.started` carries the `thread_id` a later run
//! resumes with; `developer_instructions` reaches the model *and* leaves
//! Codex's own instructions intact; `model_reasoning_effort="xhigh"` measurably
//! changes reasoning-token spend. The fixtures in [`stream_parser`] are those
//! runs.
//!
//! ## Permissions are expressed as a sandbox, and denials win
//!
//! Codex has no per-tool permission vocabulary, so `denied_tools` is
//! translated into the sandbox mode by [`config::sandbox_mode`], where a
//! denial beats `FullAuto` rather than the other way round. That ordering is
//! load-bearing rather than tidy, and the reason is written where the code is.
//!
//! `interactive_permissions: false` still holds: `codex exec` has no channel
//! to ask a person mid-run, so `vet` refuses `Reviewed` at the click rather
//! than silently widening it.

pub mod config;

pub mod stream_parser;

use crate::{Capabilities, Engine, EngineInfo, EngineProcess, ProcessHandle, RunSpec};
use async_trait::async_trait;
use eren_shared::env_guard;
use eren_shared::ErenEvent;
use std::process::Stdio;
use stream_parser::StreamState;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

pub struct CodexEngine {
    binary: String,
}

impl Default for CodexEngine {
    fn default() -> Self {
        Self {
            binary: eren_shared::brand::var("CODEX_BIN").unwrap_or_else(|| "codex".to_string()),
        }
    }
}

/// The argument vector for one run. Pure, so the flags are testable without
/// the binary — which, for an adapter written from documentation, is the only
/// part that can be tested at all.
pub fn codex_args(spec: &RunSpec) -> anyhow::Result<Vec<String>> {
    let mut args: Vec<String> = vec!["exec".into()];

    args.push("--json".into());
    // No console exists here. A prompt is not a slow answer, it is a process
    // that never returns — the hazard `connect.rs` documents for `gh`.
    args.push("--skip-git-repo-check".into());

    if let Some(model) = model_arg(&spec.model_id) {
        args.push("--model".into());
        args.push(model);
    }

    // Sandbox, approvals, persona and MCP all travel as config overrides —
    // see `config`. Doing it that way is what lets the resume path below use
    // the identical set: `codex exec resume` accepts `-c` but rejects
    // `--sandbox`.
    for o in config::overrides(spec)? {
        args.push("-c".into());
        args.push(o);
    }

    // Resume keeps a thread going rather than starting a new one, which is
    // what makes a follow-up turn cheaper than a fresh run.
    //
    // It goes *after* every exec-level flag and before the prompt, because
    // `resume` is a subcommand: clap stops accepting the parent's options once
    // it has been seen. Putting it first — which is what this used to do —
    // makes `--sandbox` an "unexpected argument" and the run never starts.
    if let Some(id) = spec.resume_session_id.as_deref().filter(|s| !s.is_empty()) {
        args.push("resume".into());
        args.push(id.to_string());
    }

    // The prompt last and positional, so a prompt beginning with a dash is
    // read as the task rather than as a flag.
    args.push("--".into());
    args.push(spec.prompt.clone());
    Ok(args)
}

/// The `--model` value, or `None` to let Codex use its own.
///
/// A model id that plainly belongs to another engine is dropped rather than
/// passed on. It happens for one boring reason: `TierMapping::model_for` falls
/// back to Claude Code's `opus` for a tier it has no entry for, so an install whose
/// Codex mapping was never derived would hand this a Claude model. Codex does
/// not reject that — it warns that the model metadata is unknown and carries
/// on with fallback metadata, which is a quietly degraded run. Saying nothing
/// and letting Codex use the model it is configured for is strictly better
/// than naming one it has never heard of.
fn model_arg(model_id: &str) -> Option<String> {
    let id = model_id.trim();
    // `provider/model` is OpenCode's and the local runtimes' shape; `claude-*`
    // and Claude Code's aliases (`opus`, `sonnet`, …) are Claude Code's.
    // None is ever a Codex model id.
    if crate::foreign_model(id) || id.contains('/') || id.starts_with("claude-") {
        return None;
    }
    Some(id.to_string())
}

/// The model `codex doctor` says this install actually runs.
///
/// Reported as the engine's catalog so the tier defaults are derived from the
/// machine rather than hardcoded — the same reasoning as OpenCode's
/// `opencode models`, and the fix for a default that named a model this CLI
/// does not know. One entry, because `doctor` reports the configured model
/// rather than a catalog; a person who wants another one types it, which is
/// what `fixed_model_catalog: false` means.
fn doctor_model(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        // The real line is `      model                    gpt-5.5 · openai`.
        let rest = line.trim().strip_prefix("model")?;
        // The next character must be a space, or `model_reasoning_effort`
        // would match on its own name.
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let id = rest.split_whitespace().next()?;
        // A model id carries a version: `gpt-5.5` does, the word `provider`
        // does not. Anything that isn't the shape is dropped rather than
        // guessed at, which is the same rule `opencode::parse_models` follows.
        (id.chars().any(|c| c.is_ascii_digit())
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c)))
        .then(|| id.to_string())
    })
}

#[async_trait]
impl Engine for CodexEngine {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn label(&self) -> &'static str {
        "Codex"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // `codex exec` is headless: approvals are decided by the flags at
            // spawn time, and there is no channel to ask a person mid-run.
            // So `Reviewed` is refused at the click by `vet`, exactly as it
            // is for OpenCode, rather than being silently downgraded — that
            // would be a privilege escalation performed on the user's behalf.
            interactive_permissions: false,
            // No documented machine-readable rate-limit signal with a reset
            // time. The shared text matcher on stderr still fires; what is
            // absent is the *structured* one the queue can wait precisely on.
            structured_rate_limit: false,
            resume_sessions: true,
            // Via `developer_instructions`, and this used to say `false`.
            // Verified additive rather than replacing: with it set, a run
            // still knew its own sandbox mode and still drove its shell tool,
            // so Codex's own instructions survive alongside Eren's. That is
            // the distinction that matters — `base_instructions` would replace
            // them, which is the trap `opencode::config` documents for
            // `agent.prompt`. While this said false, every persona and every
            // recalled memory silently vanished on a Codex run, because
            // nothing downstream folds the text in for an engine that cannot
            // append.
            append_system_prompt: true,
            // Free-text model ids: Codex takes whatever `--model` is given and
            // the catalog moves faster than any list here could.
            fixed_model_catalog: false,
            // `codex exec` reports tokens, never money (`stream_parser`
            // leaves `cost_usd` empty), so its runs are counted by tokens.
            reports_cost: false,
            // A denied write tool maps to a read-only sandbox, but a denied Bash or
            // WebFetch has no separate switch — not verified as a full refusal.
            enforces_denied_tools: false,
            // `-c mcp_servers.…` overrides, per invocation.
            mcp_tools: true,
            // `workspace-write`: edits, with commands held to the sandbox.
            auto_edit: true,
            read_only_passes: true,
        }
    }

    async fn detect(&self) -> Option<EngineInfo> {
        let out = env_guard::command(&self.binary)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .await
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();

        // "Is it logged in?" is answered by running the binary, never by
        // reading its config — the second invariant. `codex login status`
        // exits non-zero when there is no usable session.
        let authenticated = env_guard::command(&self.binary)
            .args(["login", "status"])
            .stdin(Stdio::null())
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

        // What this install is actually configured to run, from the CLI's own
        // diagnosis. Reported so the tier defaults are derived from the
        // machine rather than hardcoded — the old default named `gpt-5-codex`,
        // which this version answers with "model metadata not found".
        let models = env_guard::command(&self.binary)
            .arg("doctor")
            .stdin(Stdio::null())
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| doctor_model(&String::from_utf8_lossy(&o.stdout)))
            .map(|m| vec![m])
            .unwrap_or_default();

        Some(EngineInfo {
            version,
            authenticated,
            providers: vec![],
            models,
        })
    }

    fn start(&self, spec: RunSpec) -> anyhow::Result<EngineProcess> {
        let mut cmd = env_guard::command(&self.binary);
        cmd.current_dir(&spec.cwd).args(codex_args(&spec)?);

        for (k, v) in &spec.extra_env {
            if eren_shared::is_auth_env(k) {
                anyhow::bail!("{}", eren_shared::auth_env_refusal(k));
            }
            cmd.env(k, v);
        }

        cmd.stdin(Stdio::null()) // a run that needs input hangs forever otherwise
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = mpsc::channel::<ErenEvent>(256);

        // Codex documents progress on stderr and JSON on stdout, so stderr is
        // read for the rate-limit signal and kept as a tail for diagnosis.
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
            tracing::debug!(stderr_tail = ?tail, "codex stderr closed");
        });

        let pid = child.id();
        let model = (!spec.model_id.is_empty()).then(|| spec.model_id.clone());
        // The pump owns the child: like OpenCode and unlike Claude Code, no
        // line says how the run went, so the exit status is the verdict.
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

        Ok(EngineProcess::new(rx, Box::new(CodexHandle { pid })))
    }

    /// Not offered until the interactive resume command is checked against a
    /// `codex` binary — the `exec` flags this adapter uses taught that lesson.
    fn interactive_resume_argv(&self, _session_id: &str) -> Option<Vec<String>> {
        None
    }
}

struct CodexHandle {
    pid: Option<u32>,
}

#[async_trait]
impl ProcessHandle for CodexHandle {
    async fn interrupt(&mut self) -> anyhow::Result<()> {
        // SIGINT so the CLI can checkpoint its thread before exiting; the
        // pump owns the `Child`, so this goes through the pid.
        if let Some(pid) = self.pid {
            unsafe { crate::opencode::libc_kill(pid as i32, 2) };
        }
        Ok(())
    }

    fn kill(&mut self) {
        if let Some(pid) = self.pid {
            unsafe { crate::opencode::libc_kill(pid as i32, 9) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eren_shared::{ModelTier, PermissionMode};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn spec() -> RunSpec {
        RunSpec {
            cwd: PathBuf::from("/tmp/wt"),
            prompt: "do the thing".into(),
            model_tier: ModelTier::Medium,
            model_id: "gpt-5.5".into(),
            effort: None,
            resume_session_id: None,
            permission_mode: PermissionMode::AutoEdit,
            allowed_tools: vec![],
            denied_tools: vec![],
            append_system_prompt: None,
            run_key: "run-1".into(),
            extra_read_dirs: vec![],
            permission_prompt_tool: false,
            extra_env: HashMap::new(),
            mcp: Default::default(),
        }
    }

    fn args_of(s: &RunSpec) -> Vec<String> {
        codex_args(s).unwrap()
    }

    #[test]
    fn the_prompt_is_positional_and_last() {
        // A prompt beginning with a dash must be read as the task, not as a
        // flag — which is what the `--` separator is for.
        let mut s = spec();
        s.prompt = "--version please".into();
        let args = args_of(&s);
        assert_eq!(args.last().unwrap(), "--version please");
        assert_eq!(args[args.len() - 2], "--");
    }

    #[test]
    fn it_asks_for_json_and_never_passes_a_flag_exec_does_not_have() {
        // `--ask-for-approval` is a top-level flag for the interactive TUI.
        // `codex exec` answers it with "unexpected argument", so every run
        // this adapter started used to die at clap before spending a token.
        // The stance is set as configuration instead.
        let args = args_of(&spec());
        assert!(args.contains(&"--json".to_string()));
        assert!(!args.contains(&"--ask-for-approval".to_string()));
        assert!(!args.contains(&"--sandbox".to_string()));
        assert!(args.contains(&r#"approval_policy="never""#.to_string()));
    }

    #[test]
    fn full_auto_is_the_only_mode_that_leaves_the_sandbox() {
        let sandbox_of = |mode| {
            let mut s = spec();
            s.permission_mode = mode;
            config::sandbox_mode(&s)
        };
        assert_eq!(sandbox_of(PermissionMode::FullAuto), "danger-full-access");
        assert_eq!(sandbox_of(PermissionMode::AutoEdit), "workspace-write");
        // Reviewed is refused before it gets here by `vet`, but if it ever
        // arrived it must not be the permissive one.
        assert_eq!(sandbox_of(PermissionMode::Reviewed), "workspace-write");
    }

    #[test]
    fn every_exec_level_flag_comes_before_the_resume_keyword() {
        // `resume` is a subcommand: once clap has seen it, the parent's
        // options stop being accepted. The old argv put it first, so a
        // resumed run died on `--sandbox` even after the approval flag went.
        let mut s = spec();
        s.resume_session_id = Some("th_9".into());
        let args = args_of(&s);
        let resume = args.iter().position(|a| a == "resume").unwrap();
        assert_eq!(args[resume + 1], "th_9");
        assert!(args.iter().position(|a| a == "--json").unwrap() < resume);
        assert!(args.iter().position(|a| a == "--model").unwrap() < resume);
        assert!(args.iter().rposition(|a| a == "-c").unwrap() < resume);
        // And the prompt still trails the whole thing.
        assert_eq!(args[resume + 2], "--");
    }

    #[test]
    fn an_empty_session_id_is_not_a_resume() {
        // A blank column is "no session", not a thread called "".
        let mut s = spec();
        s.resume_session_id = Some(String::new());
        assert!(!args_of(&s).contains(&"resume".to_string()));
    }

    #[test]
    fn a_model_belonging_to_another_engine_is_dropped_rather_than_passed_on() {
        // `TierMapping::model_for` falls back to Claude Code's `opus` for a
        // tier it has no entry for. Codex does not reject that — it warns that the
        // metadata is unknown and runs degraded, which is worse than silence.
        assert_eq!(model_arg("gpt-5.5"), Some("gpt-5.5".to_string()));
        assert_eq!(model_arg("claude-opus-5"), None);
        assert_eq!(model_arg("opus"), None);
        assert_eq!(model_arg("anthropic/claude-sonnet-4-5"), None);
        assert_eq!(model_arg("ollama/deepseek-r1:latest"), None);
        assert_eq!(model_arg("  "), None);

        let mut s = spec();
        s.model_id = "claude-opus-5".into();
        assert!(!args_of(&s).contains(&"--model".to_string()));
    }

    #[test]
    fn the_install_default_is_read_out_of_doctor() {
        // Verbatim from `codex doctor` on 2026-08-17.
        let out = "  \u{2713} codex        codex-cli 0.147.0\n                         version                  0.147.0\n                         default model provider   openai\n                         rollout DB model providers openai=35\n                         model                    gpt-5.5 \u{b7} openai\n";
        assert_eq!(doctor_model(out).as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn a_line_that_merely_starts_with_the_word_model_is_not_a_model() {
        // Getting this wrong would report "provider" as the engine's catalog,
        // and `pick_defaults` would route every tier to it.
        assert_eq!(
            doctor_model("      default model provider   openai\n"),
            None
        );
        assert_eq!(doctor_model("      model_reasoning_effort   high\n"), None);
        assert_eq!(
            doctor_model("      model                    provider\n"),
            None
        );
        assert_eq!(doctor_model(""), None);
    }

    #[test]
    fn it_refuses_to_claim_a_capability_it_does_not_have() {
        // The pairing `vet` exists to refuse: a headless engine cannot honour
        // Reviewed, and saying it could would produce a run that rejects
        // every tool call instead of a refusal at the click.
        let caps = CodexEngine::default().capabilities();
        assert!(!caps.interactive_permissions);
        assert!(crate::vet(&CodexEngine::default(), PermissionMode::Reviewed, false).is_err());
        assert!(crate::vet(&CodexEngine::default(), PermissionMode::AutoEdit, false).is_ok());
        // And one it does have, now that it is wired: a persona reaches the
        // model through `developer_instructions`.
        assert!(caps.append_system_prompt);
    }

    #[test]
    fn a_chat_run_on_codex_cannot_write_to_the_users_checkout() {
        // The regression this whole file exists to prevent. Chat is dispatched
        // FullAuto because its denials bound it, and it runs in the real
        // checkout — so the denials have to survive the translation.
        let mut s = spec();
        s.permission_mode = PermissionMode::FullAuto;
        s.denied_tools = vec!["Edit".into(), "Write".into(), "Bash".into()];
        let args = args_of(&s);
        assert!(args.contains(&r#"sandbox_mode="read-only""#.to_string()));
        assert!(!args.iter().any(|a| a.contains("danger-full-access")));
    }
}
