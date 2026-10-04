//! Cursor's CLI as an engine.
//!
//! `cursor-agent -p --output-format stream-json <task>` runs headless and
//! writes JSON Lines close to Claude Code's, with tool calls in a shape of
//! their own (see [`stream_parser`]). Since January 2026 the binary is also
//! installed as `agent`; `cursor-agent` is kept as the alias because `agent`
//! is a name too generic to trust on somebody's `PATH`.
//!
//! ## Written from the docs, not yet run against the binary
//!
//! From `cursor.com/docs/cli/*` on 2026-10-02. The docs disagree with each
//! other in one place that matters, and the adapter takes the cautious side:
//! the parameters page says `-p` "has access to all tools, including write
//! and shell", while the headless page says changes without `--force` are
//! only proposed. So without `--force` a run is assumed able to write, and a
//! read-only pass asks for `--mode ask` instead of trusting the default.
//!
//! ## What Cursor cannot do, and what that costs
//!
//! - **No pause-and-ask**, so `Reviewed` is refused at the click.
//! - **No Auto-edit.** There is no setting for "edit, but no commands" — the
//!   only lever is `--force`, which allows both. `vet` refuses Auto-edit
//!   rather than widening it, and Full Auto is the way to run Cursor.
//! - **No system-prompt flag**, so the persona is folded into the prompt.
//! - **No MCP config per run** — only `.cursor/mcp.json`, which would be
//!   written into the worktree. So `mcp_tools: false`.
//! - **No error event.** A failure exits non-zero with its reason on stderr,
//!   which is where the pump's tail and rate-limit watch come in.

pub mod stream_parser;

use crate::{pump, Capabilities, Engine, EngineInfo, EngineProcess, RunSpec};
use async_trait::async_trait;
use eren_shared::env_guard;
use eren_shared::PermissionMode;
use std::process::Stdio;

pub struct CursorEngine {
    binary: String,
}

impl Default for CursorEngine {
    fn default() -> Self {
        Self {
            binary: eren_shared::brand::var("CURSOR_BIN")
                .unwrap_or_else(|| "cursor-agent".to_string()),
        }
    }
}

/// The argument vector for one run. Pure, so it is testable without the
/// binary.
pub fn cursor_args(spec: &RunSpec) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
        // A worktree is a folder Cursor has never seen; headless, an
        // untrusted one would stop and ask.
        "--trust".into(),
    ];
    if let Some(model) = model_arg(&spec.model_id) {
        args.push("--model".into());
        args.push(model);
    }
    if crate::is_read_only(spec) {
        // A denial beats Full Auto: `ask` is Cursor's read-only mode, and
        // `--force` is never added beside it.
        args.push("--mode".into());
        args.push("ask".into());
    } else if spec.permission_mode == PermissionMode::FullAuto {
        args.push("--force".into());
    }
    if let Some(id) = spec.resume_session_id.as_deref().filter(|s| !s.is_empty()) {
        args.push("--resume".into());
        args.push(id.to_string());
    }
    for dir in &spec.extra_read_dirs {
        args.push("--add-dir".into());
        args.push(dir.display().to_string());
    }
    args.push(crate::positional(crate::prompt_with_persona(spec)));
    args
}

/// The `--model` value, or `None` to let Cursor pick (`auto`).
fn model_arg(id: &str) -> Option<String> {
    let id = id.trim();
    (!crate::foreign_model(id) && !id.contains('/')).then(|| id.to_string())
}

/// Does `cursor-agent status` say nobody is signed in?
fn signed_out(status: &str) -> bool {
    let s = status.to_ascii_lowercase();
    s.contains("not logged in") || s.contains("not authenticated") || s.contains("logged out")
}

#[async_trait]
impl Engine for CursorEngine {
    fn id(&self) -> &'static str {
        "cursor"
    }

    fn label(&self) -> &'static str {
        "Cursor CLI"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            interactive_permissions: false,
            structured_rate_limit: false,
            resume_sessions: true,
            append_system_prompt: false,
            // Which models an account can use is the account's business;
            // `auto` is the documented default and the only id every account
            // has.
            fixed_model_catalog: false,
            reports_cost: false,
            enforces_denied_tools: false,
            mcp_tools: false,
            auto_edit: false,
            // `--mode ask` reads and answers, and nothing in it can write.
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
        // Answered by running the binary, never by reading its config.
        let authenticated = env_guard::command(&self.binary)
            .arg("status")
            .stdin(Stdio::null())
            .output()
            .await
            .map(|o| {
                o.status.success()
                    && !signed_out(&String::from_utf8_lossy(&o.stdout))
                    && !signed_out(&String::from_utf8_lossy(&o.stderr))
            })
            .unwrap_or(false);
        Some(EngineInfo {
            version: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            authenticated,
            providers: vec![],
            models: vec![],
        })
    }

    fn start(&self, spec: RunSpec) -> anyhow::Result<EngineProcess> {
        // Without `--force` and outside `--mode ask`, Cursor's own docs say a
        // headless run can still write and run commands. So a writing run in
        // anything but Full Auto is refused here as well as by `vet`.
        if !crate::is_read_only(&spec) && spec.permission_mode != PermissionMode::FullAuto {
            anyhow::bail!(
                "Cursor CLI has no setting that allows edits without also allowing commands, \
so it only runs in Full Auto."
            );
        }
        let mut cmd = env_guard::command(&self.binary);
        cmd.current_dir(&spec.cwd).args(cursor_args(&spec));
        for (k, v) in &spec.extra_env {
            if eren_shared::is_auth_env(k) {
                anyhow::bail!("{}", eren_shared::auth_env_refusal(k));
            }
            cmd.env(k, v);
        }
        pump::spawn(
            cmd,
            Box::new(stream_parser::CursorStream::default()),
            "cursor",
        )
    }

    fn interactive_resume_argv(&self, session_id: &str) -> Option<Vec<String>> {
        Some(vec![
            "cursor-agent".into(),
            "--resume".into(),
            session_id.into(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eren_shared::ErenEvent;

    fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let i = args.iter().position(|a| a == flag)?;
        args.get(i + 1).map(String::as_str)
    }

    #[test]
    fn the_prompt_is_last_and_never_reads_as_a_flag() {
        let mut s = crate::test_spec();
        s.prompt = "--version please".into();
        let args = cursor_args(&s);
        assert_eq!(args.last().unwrap(), "\n--version please");
        assert_eq!(after(&args, "--output-format"), Some("stream-json"));
        assert_eq!(args[0], "-p");
    }

    #[test]
    fn full_auto_forces_and_a_denial_beats_it() {
        let mut s = crate::test_spec();
        s.permission_mode = PermissionMode::FullAuto;
        let args = cursor_args(&s);
        assert!(args.contains(&"--force".into()));
        assert_eq!(after(&args, "--mode"), None);

        s.denied_tools = vec!["Edit".into(), "Write".into(), "Bash".into()];
        let args = cursor_args(&s);
        assert!(!args.contains(&"--force".into()));
        assert_eq!(after(&args, "--mode"), Some("ask"));
    }

    #[test]
    fn model_resume_and_extra_dirs() {
        let mut s = crate::test_spec();
        s.model_id = "gpt-5".into();
        s.resume_session_id = Some("c-1".into());
        s.extra_read_dirs = vec!["/a".into(), "/b".into()];
        let args = cursor_args(&s);
        assert_eq!(after(&args, "--model"), Some("gpt-5"));
        assert_eq!(after(&args, "--resume"), Some("c-1"));
        assert_eq!(args.iter().filter(|a| *a == "--add-dir").count(), 2);
        s.model_id = "claude-opus-5".into();
        assert_eq!(after(&cursor_args(&s), "--model"), None);
    }

    #[test]
    fn auto_edit_is_refused_rather_than_widened_to_force() {
        let e = CursorEngine::default();
        let err = crate::vet(&e, PermissionMode::AutoEdit, false).unwrap_err();
        assert!(err.contains("Cursor CLI") && err.contains("Full Auto"));
        assert!(crate::vet(&e, PermissionMode::Reviewed, false).is_err());
        crate::vet(&e, PermissionMode::FullAuto, true).unwrap();
        assert!(!e.capabilities().mcp_tools);
    }

    #[tokio::test]
    async fn a_narrower_writing_mode_is_refused_but_a_read_only_pass_is_not() {
        let e = CursorEngine {
            binary: "true".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut s = crate::test_spec();
        s.cwd = dir.path().to_path_buf();
        s.permission_mode = PermissionMode::Reviewed;
        assert!(e.start(s.clone()).is_err());
        s.denied_tools = vec!["Edit".into(), "Write".into(), "Bash".into()];
        assert!(e.start(s).is_ok(), "read-only runs in ask mode");
    }

    #[test]
    fn status_text_that_says_signed_out_is_believed() {
        assert!(signed_out("Not logged in. Run `cursor-agent login`."));
        assert!(!signed_out("Logged in as someone@example.com"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stand_in_binary_runs_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = crate::test_spec();
        s.cwd = dir.path().to_path_buf();
        s.permission_mode = PermissionMode::FullAuto;
        let engine = CursorEngine {
            binary: crate::replaying(dir.path(), include_str!("fixtures/task.jsonl")),
        };
        let events = crate::drain(&engine, s.clone()).await;
        assert!(matches!(events.first(), Some(ErenEvent::RunStarted { .. })));
        assert!(matches!(
            events.last(),
            Some(ErenEvent::RunCompleted { .. })
        ));
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.lines().any(|l| l == "--force"));

        // No error event exists: a failure is a non-zero exit and stderr.
        let engine = CursorEngine {
            binary: crate::stand_in(dir.path(), "echo 'Error: model not available' >&2\nexit 1"),
        };
        match crate::drain(&engine, s.clone()).await.as_slice() {
            [ErenEvent::RunFailed { reason }] => assert_eq!(reason, "Error: model not available"),
            other => panic!("{other:?}"),
        }

        // A rate limit said only on stderr, by a run that then failed.
        let engine = CursorEngine {
            binary: crate::stand_in(
                dir.path(),
                "echo 'You have hit your usage limit' >&2\nexit 1",
            ),
        };
        assert!(matches!(
            crate::drain(&engine, s.clone()).await.as_slice(),
            [ErenEvent::RateLimited { .. }]
        ));

        // The same line from a run that exited cleanly did not stop it: the
        // run simply never said how it went.
        let engine = CursorEngine {
            binary: crate::stand_in(
                dir.path(),
                "echo 'You have hit your usage limit' >&2\nexit 0",
            ),
        };
        assert!(matches!(
            crate::drain(&engine, s).await.as_slice(),
            [ErenEvent::RunFailed { .. }]
        ));
    }
}
