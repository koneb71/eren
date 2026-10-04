//! Sourcegraph's Amp as an engine.
//!
//! `amp -x <task> --stream-json` runs headless and writes Claude Code's
//! stream format, which [`crate::claude::compat`] reads. Spawned from `PATH`,
//! stdout read, nothing else.
//!
//! ## Written from the docs, not yet run against the binary
//!
//! From `ampcode.com/docs/markdown/*` and the command line Amp's own npm SDK
//! builds (`@ampcode/sdk`), on 2026-10-02.
//!
//! ## The most different of the four
//!
//! - **There is no model flag.** Amp chooses the model by *mode* — `low`,
//!   `medium`, `high`, `ultra` — and which model sits behind each one is
//!   Amp's call. So a tier maps to a mode, and the catalog is those four.
//! - **It never asks.** By default Amp runs every tool without approval and
//!   documents no read-only or plan mode. So Auto-edit is refused by `vet`,
//!   Reviewed with it, and a pass that must not change anything — a plan, a
//!   summary, a review — is refused when it starts rather than run with a
//!   shell it was told it did not have.
//! - **No system-prompt flag**, so the persona is folded into the prompt.
//! - **MCP per run exists** (`--mcp-config`), but its format for a remote
//!   server is not documented, and a guess that is wrong is a run with no
//!   tools that thinks it has some. `mcp_tools: false` until someone checks.
//! - **Headless runs authenticate with `AMP_API_KEY`**, which the person sets
//!   in their own environment; Eren passes the environment through and
//!   never sets it (`env_guard` refuses `AMP_` keys on a run).

use crate::claude::compat::ClaudeCompat;
use crate::{pump, Capabilities, Engine, EngineInfo, EngineProcess, RunSpec};
use async_trait::async_trait;
use eren_shared::{env_guard, PermissionMode};
use std::process::Stdio;

/// Amp's modes, in the order a tier climbs them.
pub const MODES: &[&str] = &["low", "medium", "high", "ultra"];

pub struct AmpEngine {
    binary: String,
}

impl Default for AmpEngine {
    fn default() -> Self {
        Self {
            binary: eren_shared::brand::var("AMP_BIN").unwrap_or_else(|| "amp".to_string()),
        }
    }
}

/// Why a read-only pass cannot run on Amp.
pub const NO_READ_ONLY: &str =
    "Amp has no read-only mode — it runs every tool without asking — so \
it can't take a pass that must not change anything (a plan, a summary or a review). Run this on \
an engine that has one.";

/// The argument vector for one run. Pure, so it is testable without the
/// binary.
pub fn amp_args(spec: &RunSpec) -> Vec<String> {
    let mut args: Vec<String> = vec![];
    // `threads continue` is a subcommand that then takes the same execute
    // flags, so it goes first.
    if let Some(id) = spec.resume_session_id.as_deref().filter(|s| !s.is_empty()) {
        args.extend(["threads".into(), "continue".into(), id.to_string()]);
    }
    args.push("-x".into());
    args.push(crate::positional(crate::prompt_with_persona(spec)));
    args.push("--stream-json".into());
    if let Some(mode) = MODES.iter().find(|m| **m == spec.model_id.trim()) {
        args.push("--mode".into());
        args.push(mode.to_string());
    }
    if let Some(effort) = spec.effort {
        args.push("--effort".into());
        args.push(effort.as_str().to_string());
    }
    if spec.permission_mode == PermissionMode::FullAuto {
        args.push("--dangerously-allow-all".into());
    }
    args
}

#[async_trait]
impl Engine for AmpEngine {
    fn id(&self) -> &'static str {
        "amp"
    }

    fn label(&self) -> &'static str {
        "Amp"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            interactive_permissions: false,
            structured_rate_limit: false,
            resume_sessions: true,
            append_system_prompt: false,
            fixed_model_catalog: true,
            // Billed in credits, shown only in Amp's own UI.
            reports_cost: false,
            enforces_denied_tools: false,
            mcp_tools: false,
            auto_edit: false,
            // Every tool or none: there is no mode in which it only reads,
            // so a plan, a summary or a drafting call is refused at the click
            // (`NO_READ_ONLY`) rather than when it starts.
            read_only_passes: false,
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
        Some(EngineInfo {
            version: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            // No status command is documented; a run without `AMP_API_KEY`
            // fails and says so.
            authenticated: true,
            providers: vec![],
            models: MODES.iter().map(|m| m.to_string()).collect(),
        })
    }

    fn start(&self, spec: RunSpec) -> anyhow::Result<EngineProcess> {
        if crate::is_read_only(&spec) {
            anyhow::bail!("{NO_READ_ONLY}");
        }
        // `vet` refuses the narrower modes; this is the adapter refusing them
        // too, so a caller that skipped the vet cannot get an unrestricted
        // run by asking for a restricted one.
        if spec.permission_mode != PermissionMode::FullAuto {
            anyhow::bail!("Amp runs every tool without asking, so it only runs in Full Auto.");
        }
        let mut cmd = env_guard::command(&self.binary);
        cmd.current_dir(&spec.cwd).args(amp_args(&spec));
        for (k, v) in &spec.extra_env {
            if eren_shared::is_auth_env(k) {
                anyhow::bail!("{}", eren_shared::auth_env_refusal(k));
            }
            cmd.env(k, v);
        }
        pump::spawn(cmd, Box::new(ClaudeCompat { label: "Amp" }), "amp")
    }

    fn interactive_resume_argv(&self, session_id: &str) -> Option<Vec<String>> {
        Some(vec![
            "amp".into(),
            "threads".into(),
            "continue".into(),
            session_id.into(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eren_shared::{ErenEvent, ReasoningEffort};

    fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let i = args.iter().position(|a| a == flag)?;
        args.get(i + 1).map(String::as_str)
    }

    #[test]
    fn a_tier_is_a_mode_and_nothing_else_is() {
        let mut s = crate::test_spec();
        s.model_id = "high".into();
        assert_eq!(after(&amp_args(&s), "--mode"), Some("high"));
        // Amp has no model flag: a model id is not passed anywhere.
        s.model_id = "claude-opus-5".into();
        let args = amp_args(&s);
        assert_eq!(after(&args, "--mode"), None);
        assert!(!args.iter().any(|a| a.contains("claude-opus-5")));
    }

    #[test]
    fn resume_comes_first_and_the_prompt_follows_x() {
        let mut s = crate::test_spec();
        s.resume_session_id = Some("T-abc".into());
        s.append_system_prompt = Some("You are Ada.".into());
        s.effort = Some(ReasoningEffort::XHigh);
        let args = amp_args(&s);
        assert_eq!(&args[..3], ["threads", "continue", "T-abc"]);
        let prompt = after(&args, "-x").unwrap();
        assert!(prompt.starts_with("You are Ada.") && prompt.ends_with("do the thing"));
        assert_eq!(after(&args, "--effort"), Some("xhigh"));
        assert!(args.contains(&"--stream-json".into()));
    }

    #[test]
    fn only_full_auto_says_allow_all_and_auto_edit_is_refused() {
        let mut s = crate::test_spec();
        s.permission_mode = PermissionMode::FullAuto;
        assert!(amp_args(&s).contains(&"--dangerously-allow-all".into()));
        s.permission_mode = PermissionMode::AutoEdit;
        assert!(!amp_args(&s).contains(&"--dangerously-allow-all".into()));
        let e = AmpEngine::default();
        assert!(crate::vet(&e, PermissionMode::AutoEdit, false).is_err());
        assert!(crate::vet(&e, PermissionMode::Reviewed, false).is_err());
        crate::vet(&e, PermissionMode::FullAuto, true).unwrap();
    }

    #[test]
    fn a_narrower_mode_is_refused_rather_than_run_unrestricted() {
        let mut s = crate::test_spec();
        s.permission_mode = PermissionMode::Reviewed;
        let err = AmpEngine {
            binary: "true".into(),
        }
        .start(s)
        .err()
        .unwrap();
        assert!(err.to_string().contains("only runs in Full Auto"));
    }

    #[test]
    fn a_read_only_pass_is_refused_before_anything_starts() {
        let mut s = crate::test_spec();
        s.permission_mode = PermissionMode::FullAuto;
        s.denied_tools = vec!["Edit".into(), "Write".into(), "Bash".into()];
        // A binary that would succeed, to prove nothing was spawned.
        let err = AmpEngine {
            binary: "true".into(),
        }
        .start(s)
        .err()
        .unwrap();
        assert!(err.to_string().contains("no read-only mode"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stand_in_binary_runs_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = crate::test_spec();
        s.cwd = dir.path().to_path_buf();
        s.permission_mode = PermissionMode::FullAuto;
        let engine = AmpEngine {
            binary: crate::replaying(dir.path(), include_str!("fixtures/task.jsonl")),
        };
        let events = crate::drain(&engine, s.clone()).await;
        assert!(
            matches!(events.first(), Some(ErenEvent::RunStarted { session_id: Some(t), .. }) if t.starts_with("T-"))
        );
        assert!(events.iter().any(
            |e| matches!(e, ErenEvent::ToolResult { tool_use_id, .. } if tool_use_id == "toolu_01")
        ));
        assert!(
            matches!(events.last(), Some(ErenEvent::RunCompleted { result_text, .. }) if result_text == "8")
        );

        let engine = AmpEngine {
            binary: crate::replaying(dir.path(), include_str!("fixtures/error.jsonl")),
        };
        assert_eq!(
            crate::drain(&engine, s).await.last(),
            Some(&ErenEvent::RunFailed {
                reason: "Tool execution failed: read: ENOENT".into()
            })
        );
    }
}
