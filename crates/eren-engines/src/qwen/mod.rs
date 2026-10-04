//! Alibaba's Qwen Code as an engine.
//!
//! `qwen --output-format=stream-json <task>` runs headless and writes Claude
//! Code's stream format, which [`crate::claude::compat`] reads. Spawned from
//! `PATH`, stdout read, nothing else.
//!
//! ## Written from the source, not yet run against the binary
//!
//! Flags from `packages/cli/src/config/top-level-options.ts` and stream types
//! from `packages/cli/src/nonInteractive/types.ts` in qwen-code 0.24.7.
//!
//! ## The closest of the four to Claude Code
//!
//! It takes `--append-system-prompt`, and `--mcp-config` with a file path, so
//! a persona keeps Qwen's own instructions and a run gets Eren's tools
//! without anything being written into its folder. Eren's own server is
//! marked `trust: true` — its tools are the ones that pass `approve` without
//! a person anyway, and nothing on them may merge, start a run or write
//! settings — while a server the person added keeps Qwen's default, so in
//! Auto-edit its tools are refused rather than quietly allowed.
//!
//! ## What it cannot do
//!
//! - **No pause-and-ask**, so `Reviewed` is refused at the click.
//! - **No dollar cost** in the stream; tokens only.
//! - **No sign-in to check.** Qwen OAuth's free tier ended on 2026-04-15 and
//!   `qwen auth` went with it; Qwen now authenticates with a provider key the
//!   person configured for it, which Eren never sees or sets.
//! - **Plan mode is not used for read-only passes.** Headless it fails closed
//!   when the model tries to leave it, which ends the pass early; `default`
//!   mode denies anything that would need approval, and the denied tools are
//!   also excluded outright.

use crate::claude::compat::ClaudeCompat;
use crate::{pump, Capabilities, Engine, EngineInfo, EngineProcess, RunSpec};
use async_trait::async_trait;
use eren_shared::{env_guard, McpServerSpec, McpTransport, McpWiring, PermissionMode};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::process::Stdio;

pub struct QwenEngine {
    binary: String,
}

impl Default for QwenEngine {
    fn default() -> Self {
        Self {
            binary: eren_shared::brand::var("QWEN_BIN").unwrap_or_else(|| "qwen".to_string()),
        }
    }
}

/// The `--approval-mode` for this run; a denial beats the permission mode.
pub fn approval_mode(spec: &RunSpec) -> &'static str {
    if crate::is_read_only(spec) {
        return "default";
    }
    match spec.permission_mode {
        PermissionMode::FullAuto => "yolo",
        PermissionMode::AutoEdit => "auto-edit",
        PermissionMode::Reviewed => "default",
    }
}

/// Qwen's names for a tool Eren denies, by Claude Code's name.
fn qwen_tools(denied: &str) -> &'static [&'static str] {
    match denied {
        "Bash" => &["run_shell_command", "exec"],
        "Edit" | "MultiEdit" => &["edit"],
        "NotebookEdit" => &["notebook_edit"],
        "Write" => &["write_file"],
        "WebFetch" => &["web_fetch"],
        "WebSearch" => &["web_search"],
        _ => &[],
    }
}

/// The argument vector for one run. Pure: the MCP file, if any, has already
/// been written by `start`.
pub fn qwen_args(spec: &RunSpec, mcp_file: Option<&Path>) -> Vec<String> {
    let mut args = vec![format!("--approval-mode={}", approval_mode(spec))];
    let mut excluded: Vec<&str> = vec![];
    // Qwen has writers of its own that Claude Code's vocabulary cannot name,
    // and that headless `default` mode allows: `enter_worktree` runs `git
    // worktree add` inside the checkout, the memory tools write to the
    // person's ~/.qwen. A pass that must not change anything excludes them.
    let own_writers: &[&str] = if crate::is_read_only(spec) {
        &[
            "enter_worktree",
            "exit_worktree",
            "notebook_edit",
            "save_memory",
            "manage_memory",
        ]
    } else {
        &[]
    };
    for tool in spec
        .denied_tools
        .iter()
        .flat_map(|t| qwen_tools(t))
        .chain(own_writers)
    {
        if !excluded.contains(tool) {
            excluded.push(tool);
        }
    }
    args.extend(excluded.iter().map(|t| format!("--exclude-tools={t}")));
    if let Some(persona) = spec
        .append_system_prompt
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    {
        args.push(format!("--append-system-prompt={persona}"));
    }
    if let Some(file) = mcp_file {
        args.push(format!("--mcp-config={}", file.display()));
    }
    if let Some(model) = Some(spec.model_id.trim()).filter(|m| !crate::foreign_model(m)) {
        args.push(format!("--model={model}"));
    }
    if let Some(id) = spec.resume_session_id.as_deref().filter(|s| !s.is_empty()) {
        args.push(format!("--resume={id}"));
    }
    for dir in &spec.extra_read_dirs {
        args.push(format!("--include-directories={}", dir.display()));
    }
    // `--exclude-tools` and `--include-directories` are yargs arrays, which go
    // on taking every word that follows them, `=` form or not: the prompt
    // became a directory to include. A scalar flag ends an array, so one
    // stands between them and the prompt. Not `--`: Qwen reads the prompt
    // only from its `query` positional, which words after `--` never reach.
    args.push("--output-format=stream-json".into());
    // Positional and last: `--prompt` is deprecated in favour of it.
    args.push(crate::positional(spec.prompt.clone()));
    args
}

/// The `mcpServers` object Qwen reads from `--mcp-config`.
///
/// `httpUrl` is Qwen's streamable-HTTP transport (`url` would be SSE).
/// Eren's entry is written first and cannot be displaced, as in Claude
/// Code's config, because it carries the run's own toolbox.
pub fn mcp_config(wiring: &McpWiring) -> Value {
    let mut servers = Map::new();
    if let Some(url) = &wiring.eren_url {
        servers.insert("eren".into(), json!({ "httpUrl": url, "trust": true }));
    }
    for server in &wiring.servers {
        if !servers.contains_key(&server.name) {
            servers.insert(server.name.clone(), entry(server));
        }
    }
    json!({ "mcpServers": servers })
}

fn entry(server: &McpServerSpec) -> Value {
    let pairs = |kv: &[(String, String)]| {
        Value::Object(kv.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
    };
    match &server.transport {
        McpTransport::Http { url, headers } => {
            let mut e = json!({ "httpUrl": url });
            if !headers.is_empty() {
                e["headers"] = pairs(headers);
            }
            e
        }
        McpTransport::Stdio { command, args, env } => {
            let mut e = json!({ "command": command, "args": args });
            if !env.is_empty() {
                e["env"] = pairs(env);
            }
            e
        }
    }
}

#[async_trait]
impl Engine for QwenEngine {
    fn id(&self) -> &'static str {
        "qwen"
    }

    fn label(&self) -> &'static str {
        "Qwen Code"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            interactive_permissions: false,
            structured_rate_limit: false,
            resume_sessions: true,
            append_system_prompt: true,
            // Ids depend on which provider the person pointed Qwen at.
            fixed_model_catalog: false,
            reports_cost: false,
            // `--exclude-tools` should make a denial a refusal, but nobody
            // has watched it refuse one.
            enforces_denied_tools: false,
            mcp_tools: true,
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
            // No status command exists to ask; a run without a usable key
            // ends in an error `result` that says so.
            authenticated: true,
            providers: vec![],
            models: vec![],
        })
    }

    fn start(&self, spec: RunSpec) -> anyhow::Result<EngineProcess> {
        // Refused before anything is written.
        for k in spec.extra_env.keys() {
            if eren_shared::is_auth_env(k) {
                anyhow::bail!("{}", eren_shared::auth_env_refusal(k));
            }
        }
        let mcp_file = if spec.mcp.is_empty() {
            None
        } else {
            Some(crate::claude::mcp::write_config(
                &crate::claude::mcp::config_dir(),
                &spec.run_key,
                &mcp_config(&spec.mcp),
            )?)
        };
        let mut cmd = env_guard::command(&self.binary);
        cmd.current_dir(&spec.cwd)
            .args(qwen_args(&spec, mcp_file.as_deref()))
            .envs(&spec.extra_env);
        pump::spawn(cmd, Box::new(ClaudeCompat { label: "Qwen Code" }), "qwen")
    }

    fn interactive_resume_argv(&self, session_id: &str) -> Option<Vec<String>> {
        Some(vec!["qwen".into(), "--resume".into(), session_id.into()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eren_shared::ErenEvent;

    fn flags<'a>(args: &'a [String], name: &str) -> Vec<&'a str> {
        let prefix = format!("--{name}=");
        args.iter()
            .filter_map(|a| a.strip_prefix(prefix.as_str()))
            .collect()
    }

    #[test]
    fn a_chat_run_cannot_write_to_the_users_checkout() {
        // Chat is dispatched FullAuto, bounded only by its denials, in the
        // person's real checkout — so the denials have to win.
        let mut s = crate::test_spec();
        s.permission_mode = PermissionMode::FullAuto;
        s.denied_tools = ["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"]
            .map(String::from)
            .to_vec();
        let args = qwen_args(&s, None);
        assert_eq!(flags(&args, "approval-mode"), ["default"]);
        assert_eq!(
            flags(&args, "exclude-tools"),
            [
                "edit",
                "write_file",
                "notebook_edit",
                "run_shell_command",
                "exec",
                "enter_worktree",
                "exit_worktree",
                "save_memory",
                "manage_memory"
            ]
        );
        assert!(!args.iter().any(|a| a.contains("yolo")));
    }

    #[test]
    fn a_scalar_flag_stands_between_the_arrays_and_the_prompt() {
        // The arrays before it would otherwise swallow it: an attachment
        // made the task a directory to include, and a read-only pass made it
        // a tool to exclude.
        let mut s = crate::test_spec();
        s.extra_read_dirs = vec!["/att".into()];
        s.denied_tools = vec!["Edit".into()];
        let args = qwen_args(&s, None);
        // Checked against yargs 17.7.2 with Qwen's option shapes: after a
        // scalar flag the prompt is the query; after an array flag, or after
        // `--`, it is not.
        assert_eq!(args[args.len() - 2], "--output-format=stream-json");
        assert!(!args.contains(&"--".to_string()));
        // Read-only or not, the writers Qwen has of its own are only shut
        // off when the pass must not write.
        s.denied_tools.clear();
        assert!(!qwen_args(&s, None)
            .iter()
            .any(|a| a.contains("enter_worktree")));
    }

    #[test]
    fn modes_map_and_the_prompt_is_last() {
        let mut s = crate::test_spec();
        assert_eq!(approval_mode(&s), "auto-edit");
        s.permission_mode = PermissionMode::FullAuto;
        assert_eq!(approval_mode(&s), "yolo");
        s.prompt = "-n is a flag".into();
        assert_eq!(qwen_args(&s, None).last().unwrap(), "\n-n is a flag");
        s.prompt = "mcp".into();
        assert_eq!(qwen_args(&s, None).last().unwrap(), "mcp\n");
    }

    #[test]
    fn persona_mcp_model_resume_and_dirs() {
        let mut s = crate::test_spec();
        s.append_system_prompt = Some("You are Ada.".into());
        s.model_id = "qwen3-coder-plus".into();
        s.resume_session_id = Some("sess-1".into());
        s.extra_read_dirs = vec!["/att".into()];
        let args = qwen_args(&s, Some(Path::new("/home/me/.eren/mcp/run-1.json")));
        assert_eq!(flags(&args, "append-system-prompt"), ["You are Ada."]);
        assert_eq!(
            flags(&args, "mcp-config"),
            ["/home/me/.eren/mcp/run-1.json"]
        );
        assert_eq!(flags(&args, "model"), ["qwen3-coder-plus"]);
        assert_eq!(flags(&args, "resume"), ["sess-1"]);
        assert_eq!(flags(&args, "include-directories"), ["/att"]);
        // The persona is not also folded into the prompt.
        assert_eq!(args.last().unwrap(), "do the thing");
        // A provider-qualified id is Qwen's to use; Claude Code's fallback is not.
        s.model_id = "qwen/qwen3-coder".into();
        assert_eq!(flags(&qwen_args(&s, None), "model"), ["qwen/qwen3-coder"]);
        s.model_id = "claude-opus-5".into();
        assert!(flags(&qwen_args(&s, None), "model").is_empty());
    }

    #[test]
    fn eren_is_trusted_and_cannot_be_displaced_and_nobody_else_is_trusted() {
        let c = mcp_config(&McpWiring {
            eren_url: Some("http://127.0.0.1:4820/mcp/run/r1".into()),
            servers: vec![
                McpServerSpec {
                    name: "eren".into(),
                    transport: McpTransport::Http {
                        url: "http://evil".into(),
                        headers: vec![],
                    },
                },
                McpServerSpec {
                    name: "docs".into(),
                    transport: McpTransport::Http {
                        url: "https://docs.example/mcp".into(),
                        headers: vec![("X-Team".into(), "core".into())],
                    },
                },
                McpServerSpec {
                    name: "pw".into(),
                    transport: McpTransport::Stdio {
                        command: "npx".into(),
                        args: vec!["@playwright/mcp".into()],
                        env: vec![],
                    },
                },
            ],
        });
        let s = &c["mcpServers"];
        assert_eq!(
            s["eren"],
            json!({ "httpUrl": "http://127.0.0.1:4820/mcp/run/r1", "trust": true })
        );
        assert_eq!(s["docs"]["httpUrl"], "https://docs.example/mcp");
        assert_eq!(s["docs"]["headers"]["X-Team"], "core");
        assert!(s["docs"].get("trust").is_none() && s["pw"].get("trust").is_none());
        assert_eq!(s["pw"]["command"], "npx");
    }

    #[test]
    fn it_can_carry_erens_tools_and_a_persona() {
        let e = QwenEngine::default();
        let caps = e.capabilities();
        assert!(caps.mcp_tools && caps.append_system_prompt && caps.auto_edit);
        assert!(!caps.enforces_denied_tools && !caps.reports_cost);
        assert!(crate::vet(&e, PermissionMode::Reviewed, false).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stand_in_binary_runs_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = crate::test_spec();
        s.cwd = dir.path().to_path_buf();
        let engine = QwenEngine {
            binary: crate::replaying(dir.path(), include_str!("fixtures/task.jsonl")),
        };
        let events = crate::drain(&engine, s.clone()).await;
        assert!(
            matches!(events.first(), Some(ErenEvent::RunStarted { session_id: Some(id), .. }) if id == "5c0b6f0e-qwen")
        );
        assert!(events.iter().any(
            |e| matches!(e, ErenEvent::ToolCall { tool_name, .. } if tool_name == "write_file")
        ));
        match events.last().unwrap() {
            ErenEvent::RunCompleted {
                session_id,
                cost_usd,
                usage,
                ..
            } => {
                assert_eq!(session_id, "5c0b6f0e-qwen");
                assert_eq!(*cost_usd, None);
                assert_eq!(usage.output_tokens, 96);
            }
            other => panic!("{other:?}"),
        }

        let engine = QwenEngine {
            binary: crate::replaying(dir.path(), include_str!("fixtures/auth_error.jsonl")),
        };
        match crate::drain(&engine, s.clone()).await.as_slice() {
            [ErenEvent::RunFailed { reason }] => assert!(reason.contains("auth"), "{reason}"),
            other => panic!("{other:?}"),
        }

        s.extra_env.insert("DASHSCOPE_API_KEY".into(), "x".into());
        let err = engine.start(s).err().unwrap();
        assert!(err.to_string().contains("DASHSCOPE_API_KEY"));
    }
}
