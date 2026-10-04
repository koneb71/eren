//! Google's Gemini CLI as an engine.
//!
//! `gemini --prompt=<task> --output-format stream-json` runs headless and
//! writes JSON Lines in a schema of its own (see [`stream_parser`]). Spawned
//! from `PATH`, stdout read, nothing else.
//!
//! ## Written from the source, not yet run against the binary
//!
//! Every flag and every event shape here was read out of gemini-cli's own
//! repository (`packages/cli/src/config/config.ts`, `packages/core/src/output/
//! types.ts`) rather than observed. The Codex adapter is the cautionary tale —
//! almost none of its documentation-built first version survived the real
//! binary — so the fixtures are labelled synthetic and the things most likely
//! to be wrong are named where they are decided.
//!
//! ## What Gemini cannot do, and what that costs
//!
//! - **No pause-and-ask.** Headless, a tool that needs approval is denied, so
//!   `Reviewed` is refused at the click by `vet`.
//! - **No way to add to the system prompt.** `GEMINI_SYSTEM_MD` *replaces* it,
//!   which would throw away Gemini's own tool instructions, so the persona is
//!   folded into the prompt instead ([`crate::prompt_with_persona`]).
//! - **No MCP config per run.** Servers come only from settings files, and the
//!   one a run could use — `.gemini/settings.json` in its folder — would be
//!   written into the worktree and land in the diff. So `mcp_tools: false`:
//!   the assistant, a manager and a team member refuse Gemini at the click.
//! - **Plan mode is not read-only headless.** gemini-cli's own docs say a
//!   non-interactive plan-mode run approves its plan and then switches to
//!   YOLO to carry it out. A read-only pass therefore runs in `default` mode
//!   — and with an admin-tier policy denying the writing tools, because the
//!   headless deny `default` mode relies on sits in Gemini's lowest tier, and
//!   any allow rule the person or the repository has ("always allow git")
//!   outranks it. See [`read_only_policy`].
//!
//! ## The worktree is trusted for the run
//!
//! An untrusted folder makes a headless run exit 55, and every card's
//! worktree is a folder Gemini has never seen, so `--skip-trust` is always
//! passed. That lets the repository's own `.gemini/settings.json` apply, the
//! same way Claude Code honours a repository's `.claude/settings.json`: it is
//! the person's own code.

pub mod stream_parser;

use crate::{pump, Capabilities, Engine, EngineInfo, EngineProcess, RunSpec};
use async_trait::async_trait;
use eren_shared::env_guard;
use eren_shared::PermissionMode;
use std::path::{Path, PathBuf};
use std::process::Stdio;

pub struct GeminiEngine {
    binary: String,
}

impl Default for GeminiEngine {
    fn default() -> Self {
        Self {
            binary: eren_shared::brand::var("GEMINI_BIN").unwrap_or_else(|| "gemini".to_string()),
        }
    }
}

/// The `--approval-mode` for this run.
///
/// A denial beats the permission mode, for the reason [`crate::WRITE_TOOLS`]
/// gives; and `plan` is never used, because headless it ends in YOLO.
pub fn approval_mode(spec: &RunSpec) -> &'static str {
    if crate::is_read_only(spec) {
        return "default";
    }
    match spec.permission_mode {
        PermissionMode::FullAuto => "yolo",
        PermissionMode::AutoEdit => "auto_edit",
        // `vet` refuses Reviewed before it gets here. If it ever arrived, the
        // mode that denies is the safe reading of it.
        PermissionMode::Reviewed => "default",
    }
}

/// The tools a read-only pass denies, in Gemini's names: its own write.toml's
/// list, with `web_fetch` only when the run denies the web — a research pass
/// is read-only and reads the web.
fn denied_tools(spec: &RunSpec) -> Vec<&'static str> {
    let mut tools = vec![
        "replace",
        "write_file",
        "run_shell_command",
        "activate_skill",
    ];
    if spec.denied_tools.iter().any(|t| t == "WebFetch") {
        tools.push("web_fetch");
    }
    tools
}

/// An admin-tier policy holding a read-only pass to read-only.
///
/// Admin is the one tier above a person's own "always allow" choices and
/// their `tools.allowed`; nothing else Eren can pass outranks those. A
/// directory rather than a file, because that is what Gemini matches a tier
/// by — and in Eren's own folder, never the run's: a policy in the
/// worktree would land in the diff.
pub fn read_only_policy(spec: &RunSpec) -> String {
    let names: Vec<String> = denied_tools(spec)
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect();
    format!(
        "# Written by Eren for a pass that must not change anything.\n\
         [[rule]]\ntoolName = [{}]\ndecision = \"deny\"\npriority = 999\n",
        names.join(", ")
    )
}

fn write_policy(spec: &RunSpec) -> anyhow::Result<PathBuf> {
    let dir = eren_shared::brand::home().join("gemini-policy").join(
        if denied_tools(spec).contains(&"web_fetch") {
            "read-only-offline"
        } else {
            "read-only"
        },
    );
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("read-only.toml"), read_only_policy(spec))?;
    Ok(dir)
}

/// The argument vector for one run. Pure, so it is testable without the
/// binary: the policy for a read-only pass is written by `start`.
///
/// Every value travels as `--flag=value`: yargs then cannot read a prompt
/// that begins with a dash as an option of its own.
pub fn gemini_args(spec: &RunSpec, policy: Option<&Path>) -> Vec<String> {
    let mut args = vec![
        format!("--prompt={}", crate::prompt_with_persona(spec)),
        "--output-format=stream-json".to_string(),
        format!("--approval-mode={}", approval_mode(spec)),
        "--skip-trust".to_string(),
    ];
    if let Some(model) = model_arg(&spec.model_id) {
        args.push(format!("--model={model}"));
    }
    if let Some(id) = spec.resume_session_id.as_deref().filter(|s| !s.is_empty()) {
        args.push(format!("--resume={id}"));
    }
    for dir in &spec.extra_read_dirs {
        args.push(format!("--include-directories={}", dir.display()));
    }
    if let Some(dir) = policy {
        args.push(format!("--admin-policy={}", dir.display()));
    }
    args
}

/// The `--model` value, or `None` to let Gemini use its own.
///
/// `provider/model` is OpenCode's shape and never a Gemini id.
fn model_arg(id: &str) -> Option<String> {
    let id = id.trim();
    (!crate::foreign_model(id) && !id.contains('/')).then(|| id.to_string())
}

#[async_trait]
impl Engine for GeminiEngine {
    fn id(&self) -> &'static str {
        "gemini"
    }

    fn label(&self) -> &'static str {
        "Gemini CLI"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            interactive_permissions: false,
            // Quota errors arrive as a `result` naming the error class, with
            // no reset time the queue could wait on.
            structured_rate_limit: false,
            resume_sessions: true,
            append_system_prompt: false,
            // `pro`, `flash`, `flash-lite` and `auto` are documented aliases
            // that follow Google's releases, so the tier defaults name those.
            fixed_model_catalog: true,
            reports_cost: false,
            // Read-only is `default` mode, where approval-needing tools are
            // denied — a policy, not a per-tool refusal Eren has watched.
            enforces_denied_tools: false,
            mcp_tools: false,
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
        Some(EngineInfo {
            version: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            // Gemini has no status command; the only way to ask is a run,
            // which costs quota. A run that is not signed in exits 41, and
            // the parser turns that into a sentence a person can act on.
            authenticated: true,
            providers: vec![],
            // The documented aliases, which follow Google's releases. Offered
            // as the catalog; a full id like `gemini-2.5-pro` is still valid.
            models: ["auto", "pro", "flash", "flash-lite"]
                .map(String::from)
                .to_vec(),
        })
    }

    fn start(&self, spec: RunSpec) -> anyhow::Result<EngineProcess> {
        // Refused before anything is written.
        if let Some(k) = spec.extra_env.keys().find(|k| eren_shared::is_auth_env(k)) {
            anyhow::bail!("{}", eren_shared::auth_env_refusal(k));
        }
        let policy = if crate::is_read_only(&spec) {
            Some(write_policy(&spec)?)
        } else {
            None
        };
        let mut cmd = env_guard::command(&self.binary);
        cmd.current_dir(&spec.cwd)
            .args(gemini_args(&spec, policy.as_deref()))
            .envs(&spec.extra_env);
        pump::spawn(
            cmd,
            Box::new(stream_parser::GeminiStream::default()),
            "gemini",
        )
    }

    fn interactive_resume_argv(&self, session_id: &str) -> Option<Vec<String>> {
        // `--resume <uuid>` without `--prompt` opens the session in the TUI.
        Some(vec!["gemini".into(), "--resume".into(), session_id.into()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eren_shared::ErenEvent;

    fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
        args.iter()
            .find_map(|a| a.strip_prefix(&format!("--{name}=")))
    }

    #[test]
    fn a_prompt_that_starts_with_a_dash_stays_the_prompt() {
        let mut s = crate::test_spec();
        s.prompt = "--version please".into();
        let args = gemini_args(&s, None);
        assert_eq!(flag(&args, "prompt"), Some("--version please"));
        assert_eq!(flag(&args, "output-format"), Some("stream-json"));
        assert!(args.contains(&"--skip-trust".to_string()));
    }

    #[test]
    fn a_denial_beats_full_auto_and_plan_mode_is_never_used() {
        let mut s = crate::test_spec();
        s.permission_mode = PermissionMode::FullAuto;
        assert_eq!(approval_mode(&s), "yolo");
        s.denied_tools = vec!["Edit".into(), "Write".into(), "Bash".into()];
        assert_eq!(approval_mode(&s), "default");
        s.permission_mode = PermissionMode::AutoEdit;
        assert_eq!(approval_mode(&s), "default");
        s.denied_tools.clear();
        assert_eq!(approval_mode(&s), "auto_edit");
        s.permission_mode = PermissionMode::Reviewed;
        assert_eq!(approval_mode(&s), "default");
    }

    #[test]
    fn a_read_only_pass_carries_a_deny_that_outranks_any_allow() {
        let mut s = crate::test_spec();
        s.denied_tools = ["Edit", "Write", "Bash", "WebFetch"]
            .map(String::from)
            .to_vec();
        let policy = read_only_policy(&s);
        for tool in [
            "replace",
            "write_file",
            "run_shell_command",
            "activate_skill",
            "web_fetch",
        ] {
            assert!(policy.contains(&format!("\"{tool}\"")), "{tool}");
        }
        assert!(policy.contains("decision = \"deny\""));
        // A research pass reads the web, so the web stays open to it.
        s.denied_tools = ["Edit", "Write", "Bash"].map(String::from).to_vec();
        assert!(!read_only_policy(&s).contains("web_fetch"));

        let args = gemini_args(
            &s,
            Some(Path::new("/home/me/.eren/gemini-policy/read-only")),
        );
        assert_eq!(
            flag(&args, "admin-policy"),
            Some("/home/me/.eren/gemini-policy/read-only")
        );
        assert_eq!(flag(&gemini_args(&s, None), "admin-policy"), None);
    }

    #[test]
    fn the_persona_rides_in_front_of_the_prompt() {
        let mut s = crate::test_spec();
        s.append_system_prompt = Some("You are Ada, a careful reviewer.".into());
        let prompt = flag(&gemini_args(&s, None), "prompt").unwrap().to_string();
        assert!(prompt.starts_with("You are Ada"));
        assert!(prompt.ends_with("do the thing"));
    }

    #[test]
    fn model_resume_and_extra_dirs() {
        let mut s = crate::test_spec();
        s.model_id = "flash".into();
        s.resume_session_id = Some("5a1d".into());
        s.extra_read_dirs = vec!["/home/me/.eren/attachments/x".into()];
        let args = gemini_args(&s, None);
        assert_eq!(flag(&args, "model"), Some("flash"));
        assert_eq!(flag(&args, "resume"), Some("5a1d"));
        assert_eq!(
            flag(&args, "include-directories"),
            Some("/home/me/.eren/attachments/x")
        );
        // A blank session is no session, and another engine's model is none.
        s.resume_session_id = Some(String::new());
        s.model_id = "claude-opus-5".into();
        let args = gemini_args(&s, None);
        assert_eq!(flag(&args, "resume"), None);
        assert_eq!(flag(&args, "model"), None);
        s.model_id = "anthropic/claude-sonnet-4-5".into();
        assert_eq!(flag(&gemini_args(&s, None), "model"), None);
    }

    #[test]
    fn it_claims_only_what_it_can_do() {
        let e = GeminiEngine::default();
        let caps = e.capabilities();
        assert!(!caps.mcp_tools && !caps.append_system_prompt && !caps.reports_cost);
        assert!(crate::vet(&e, PermissionMode::Reviewed, false).is_err());
        crate::vet(&e, PermissionMode::AutoEdit, true).unwrap();
    }

    #[test]
    fn an_auth_variable_is_refused_before_anything_starts() {
        let mut s = crate::test_spec();
        s.extra_env.insert("GEMINI_API_KEY".into(), "x".into());
        let err = GeminiEngine {
            binary: "true".into(),
        }
        .start(s)
        .err()
        .unwrap();
        assert!(err.to_string().contains("GEMINI_API_KEY"));
    }

    /// The whole adapter against a stand-in binary: the argv reaches it, the
    /// pump reads its stdout through the parser, and a run that exits 41
    /// without a result is explained rather than "exited 41".
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stand_in_binary_runs_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = crate::test_spec();
        s.cwd = dir.path().to_path_buf();
        let engine = GeminiEngine {
            binary: crate::replaying(dir.path(), include_str!("fixtures/task.jsonl")),
        };
        let events = crate::drain(&engine, s.clone()).await;
        assert!(matches!(events.first(), Some(ErenEvent::RunStarted { .. })));
        assert!(matches!(
            events.last(),
            Some(ErenEvent::RunCompleted { .. })
        ));
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.lines().any(|l| l == "--output-format=stream-json"));

        let engine = GeminiEngine {
            binary: crate::stand_in(dir.path(), "echo 'Please set an Auth method' >&2\nexit 41"),
        };
        match crate::drain(&engine, s).await.as_slice() {
            [ErenEvent::RunFailed { reason }] => {
                assert!(reason.contains("signed in"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// Gemini announces a quota fallback on stderr and keeps working. That
    /// line must not hold the run — only the stream's own ending counts —
    /// and the run must end exactly once.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_quota_warning_on_stderr_does_not_hold_a_run_that_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("task.jsonl");
        std::fs::write(&fixture, include_str!("fixtures/task.jsonl")).unwrap();
        let engine = GeminiEngine {
            binary: crate::stand_in(
                dir.path(),
                &format!(
                    "echo 'Possible quota limitations in place. Switching to the flash model.' >&2\ncat '{}'\nexit 1",
                    fixture.display()
                ),
            ),
        };
        let mut s = crate::test_spec();
        s.cwd = dir.path().to_path_buf();
        let events = crate::drain(&engine, s).await;
        let endings: Vec<_> = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    ErenEvent::RunCompleted { .. }
                        | ErenEvent::RunFailed { .. }
                        | ErenEvent::RateLimited { .. }
                )
            })
            .collect();
        assert!(
            matches!(endings[..], [ErenEvent::RunCompleted { .. }]),
            "{endings:?}"
        );

        // The same line from a run that died without saying how is the
        // reason it died.
        let engine = GeminiEngine {
            binary: crate::stand_in(
                dir.path(),
                "echo '[API Error: 429 quota exceeded]' >&2\nexit 1",
            ),
        };
        let mut s = crate::test_spec();
        s.cwd = dir.path().to_path_buf();
        assert!(matches!(
            crate::drain(&engine, s).await.as_slice(),
            [ErenEvent::RateLimited { .. }]
        ));
    }
}
