//! Engine adapter layer.
//!
//! COMPLIANCE INVARIANTS (contribution rules — PRs violating these are rejected):
//! 1. Adapters spawn official agent binaries found on `PATH` and read their stdout.
//!    Nothing else.
//! 2. Never read, store, extract, or forward credentials. Never touch `~/.claude`
//!    or any engine's config/credential files.
//! 3. Never set authentication environment variables on spawned processes.
//!    (The person's own are passed through to an engine; a check or a git
//!    hook, which runs agent-written code, gets none of them —
//!    `env_guard::command_without_auth`.)
//! 4. Never proxy, intercept, or replay the engine's network traffic.

pub mod amp;
pub mod claude;
pub mod codex;
pub mod cursor;
pub mod gemini;
pub mod local;
pub mod mock;
pub mod opencode;
pub mod pump;
pub mod qwen;

use async_trait::async_trait;
use eren_shared::{ErenEvent, McpWiring, ModelTier, PermissionMode, ReasoningEffort};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct EngineInfo {
    pub version: String,
    pub authenticated: bool,
    /// Provider names and auth *type* only — never a credential. Empty for
    /// engines that don't expose this. Populated by running the CLI, never by
    /// reading its config or credential files.
    pub providers: Vec<ProviderInfo>,
    /// Model ids this install can actually reach right now, when the CLI can
    /// say. Empty means "we don't know" — never "none are available".
    ///
    /// This is what stops the tier defaults from being a guess: an engine
    /// fronting many providers has no fixed catalog, so the only honest
    /// source for "what can this machine run" is the machine.
    pub models: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderInfo {
    pub name: String,
    /// e.g. "api" or "oauth" — how the user authenticated, not the secret.
    pub auth: String,
}

/// What an engine can actually do.
///
/// Declared per adapter rather than discovered by `if engine == "..."` checks
/// scattered through the orchestrator. There is deliberately **no** `Default`
/// impl: a new adapter must state its own answers, because inheriting "yes I
/// can do everything" by omission is exactly how a descriptor like this rots
/// into a lie.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Capabilities {
    /// Can pause mid-run and ask a human to approve one tool call.
    /// `false` ⇒ `PermissionMode::Reviewed` cannot be honoured at all.
    pub interactive_permissions: bool,
    /// Emits a rate-limit signal carrying a reset time, so the queue can wait
    /// exactly as long as it needs to rather than guessing.
    pub structured_rate_limit: bool,
    /// Can resume a prior session by id.
    pub resume_sessions: bool,
    /// Can add to the system prompt without replacing the CLI's own.
    pub append_system_prompt: bool,
    /// Model ids come from a fixed catalog. `false` ⇒ free-text
    /// `provider/model`, which no catalog could keep up with.
    pub fixed_model_catalog: bool,
    /// Says what a run cost in dollars when it finishes. `false` ⇒ only
    /// tokens are known, so a dollar budget cannot see this engine's runs —
    /// they are counted against token caps instead, and the budget screen
    /// says so.
    pub reports_cost: bool,
    /// Refuses a denied tool on its own, so a read-only pass is read-only
    /// because the CLI enforces it — not because the model was asked nicely.
    /// `false` ⇒ an agent reviewer cannot run on it: a review that could
    /// edit the diff it is judging is not a review.
    pub enforces_denied_tools: bool,
    /// Can be handed Eren's MCP server for one run without writing a file
    /// into the run's folder (a flag, an env var, a config in Eren's own
    /// scratch dir). `false` ⇒ the features that live on Eren's tools — the
    /// chat assistant, a project manager, a team member — are refused at the
    /// click, and a card run simply goes without its toolbox. An adapter never
    /// writes a config into a worktree or the user's checkout: it would land
    /// in the diff, or overwrite their own.
    pub mcp_tools: bool,
    /// Can edit files without also being handed a shell. `false` ⇒ Auto-edit
    /// is refused by `vet` rather than quietly widened to Full Auto, for the
    /// same reason `Reviewed` is.
    pub auto_edit: bool,
    /// Can take a pass that must not change anything — a plan, a summary, a
    /// drafting call — in a mode where nothing can write, however the CLI
    /// spells that. `false` ⇒ such a pass is refused at the click (a plan-first
    /// card by `vet_card`, a drafting call by `utility_run`, both as a 409)
    /// rather than by the adapter once it starts. Not `enforces_denied_tools`:
    /// Cursor's ask mode cannot write, but it does not refuse one named tool.
    pub read_only_passes: bool,
}

#[derive(Debug, Clone)]
pub struct RunSpec {
    /// Working directory for the run — for board tasks this is an
    /// eren-managed git worktree, never the user's main checkout.
    pub cwd: PathBuf,
    pub prompt: String,
    pub model_tier: ModelTier,
    /// Concrete model ID resolved from the user's tier mapping.
    pub model_id: String,
    /// How hard to think. `None` leaves the CLI's own default alone.
    pub effort: Option<ReasoningEffort>,
    pub resume_session_id: Option<String>,
    pub permission_mode: PermissionMode,
    pub allowed_tools: Vec<String>,
    /// Tools this run must not be able to use, whatever else is granted.
    ///
    /// Not the inverse of `allowed_tools`, and not redundant with it: Claude
    /// Code's `--allowedTools` is an *auto-approval* list, so naming three
    /// read-only tools there does not stop it reaching for `Bash`. Only an
    /// explicit denial does. Adapters must apply this last, so it beats
    /// anything the allow-list or the permission mode would otherwise permit.
    pub denied_tools: Vec<String>,
    pub append_system_prompt: Option<String>,
    /// Stable id for per-run scratch files (the MCP config). The run id for
    /// task/chat runs, the step id for an org member — whatever the caller
    /// uses to keep concurrent runs from colliding.
    pub run_key: String,
    /// What this run should reach over MCP. Each adapter renders it into its
    /// own config dialect — the orchestrator no longer knows any of them.
    pub mcp: McpWiring,
    /// Directories outside `cwd` the run legitimately needs to read — today
    /// only the per-attachment dirs under `~/.eren/attachments`.
    ///
    /// Declaring them is belt-and-braces: current Claude Code versions let an
    /// allowed `Read` reach any absolute path, so attachments resolve without
    /// this. We still say so explicitly, because that default is the CLI's to
    /// change and a version that tightened it would break attachments silently.
    pub extra_read_dirs: Vec<PathBuf>,
    /// Route permission prompts through Eren's MCP approve tool. Task runs
    /// set true; chat/utility runs set false (they rely on an explicit
    /// allowed-tools list and headless auto-deny instead).
    pub permission_prompt_tool: bool,
    /// Non-auth environment (e.g. EREN_RUN_ID for hooks). Adapters must
    /// refuse auth-related keys via `eren_shared::is_auth_env`.
    pub extra_env: HashMap<String, String>,
}

/// A running engine process. Dropping it does not kill the child; call
/// `kill()` or let it run to completion.
pub struct EngineProcess {
    /// Normalized event stream. Closed channel = process finished.
    pub events: mpsc::Receiver<ErenEvent>,
    handle: Box<dyn ProcessHandle>,
}

impl EngineProcess {
    pub fn new(events: mpsc::Receiver<ErenEvent>, handle: Box<dyn ProcessHandle>) -> Self {
        Self { events, handle }
    }

    /// Graceful stop (SIGINT — lets the CLI checkpoint its session).
    pub async fn interrupt(&mut self) -> anyhow::Result<()> {
        self.handle.interrupt().await
    }

    /// Hard stop.
    pub fn kill(&mut self) {
        self.handle.kill();
    }
}

#[async_trait]
pub trait ProcessHandle: Send {
    async fn interrupt(&mut self) -> anyhow::Result<()>;
    fn kill(&mut self);
}

#[async_trait]
pub trait Engine: Send + Sync {
    /// Stable identifier, stored on cards and runs: "claude-code", "opencode",
    /// "codex", "gemini", "cursor", "qwen", "amp", "ollama", "lmstudio", "mock".
    fn id(&self) -> &'static str;

    /// Human-facing name, for pickers and error messages.
    fn label(&self) -> &'static str;

    /// What this engine can do. No default — see [`Capabilities`].
    fn capabilities(&self) -> Capabilities;

    /// Probe the CLI: is it installed and logged in? Implemented by running
    /// the binary, never by inspecting its config files.
    async fn detect(&self) -> Option<EngineInfo>;

    /// Re-read whatever `start` resolves a run against that can change while
    /// Eren is running — for a local runtime, the models it holds. Called
    /// right before every `start`, because `start` is synchronous and cannot
    /// go and ask. Nothing to re-read for a CLI whose catalog is its own, so
    /// the default does nothing.
    async fn refresh(&self) {}

    fn start(&self, spec: RunSpec) -> anyhow::Result<EngineProcess>;

    /// What a person types to pick this session back up in their own
    /// terminal, run from the directory the session ran in. `None` when this
    /// engine can't, or when nobody has checked the command against the
    /// binary — a wrong command shown with a Copy button is worse than none.
    ///
    /// No default, for the reason `Capabilities` has none: every adapter has
    /// to answer for itself.
    fn interactive_resume_argv(&self, session_id: &str) -> Option<Vec<String>>;
}

/// Tools whose denial means "this run must not change anything".
///
/// Eren says "read-only" as a denial list, because that is the vocabulary
/// Claude Code and OpenCode share. An engine with no per-tool vocabulary
/// translates it into whatever mode it has where nothing can write — and a
/// denial beats the permission mode, including `FullAuto`: a chat run is
/// dispatched `FullAuto` on purpose, *in the user's real checkout*, bounded
/// only by its denials (see `codex::config::sandbox_mode`).
pub(crate) const WRITE_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"];

/// Has the caller denied this run anything that could write?
pub(crate) fn is_read_only(spec: &RunSpec) -> bool {
    spec.denied_tools
        .iter()
        .any(|t| WRITE_TOOLS.iter().any(|w| w.eq_ignore_ascii_case(t)))
}

/// The prompt, with the persona folded in front for an engine that has no
/// way to add to its system prompt.
///
/// Nothing upstream does this — the orchestrator hands every adapter the
/// persona in `append_system_prompt` and trusts it to arrive. An adapter that
/// dropped it would run every agent as nobody in particular, and every
/// recalled memory would vanish with it.
pub(crate) fn prompt_with_persona(spec: &RunSpec) -> String {
    match spec.append_system_prompt.as_deref().map(str::trim) {
        Some(persona) if !persona.is_empty() => {
            format!("{persona}\n\n---\n\n{}", spec.prompt)
        }
        _ => spec.prompt.clone(),
    }
}

/// A prompt that can travel as a positional argument without being read as
/// anything else, for a CLI whose parser has not been checked for `--`.
///
/// Two ways a positional goes wrong: it starts with a dash and is taken for
/// an option, or it is one bare word that happens to be a subcommand — a chat
/// message reading just `login` or `mcp` would run that instead. A newline is
/// invisible to the model and fixes both.
pub(crate) fn positional(prompt: String) -> String {
    if prompt.starts_with('-') {
        format!("\n{prompt}")
    } else if !prompt.contains(char::is_whitespace) {
        format!("{prompt}\n")
    } else {
        prompt
    }
}

/// A model id from Claude Code's own catalog, or no id at all.
///
/// `TierMapping::model_for` falls back to Claude Code's `opus` for a tier it has
/// no entry for, so an engine whose mapping was never filled in is handed a
/// Claude Code id. Passing that on names a model the CLI has never heard of;
/// saying nothing lets it use the one it is configured for. Only the exact
/// catalog ids are dropped — an engine that fronts Anthropic's API under its
/// own naming keeps every id it actually uses. Claude Code's aliases are
/// in the catalog too, so `opus` is dropped as surely as `claude-opus-5-5`.
pub(crate) fn foreign_model(id: &str) -> bool {
    let id = id.trim();
    id.is_empty() || eren_shared::is_known_model(id)
}

/// Refuse a run the engine cannot honour, with a reason a person can act on.
///
/// The one place capability mismatches are decided, so the answer is the same
/// whether you hit it from the board, a chat, a team run or a bake-off.
///
/// Note what this deliberately does **not** do: downgrade. The orchestrator
/// downgrades `FullAuto` to `Reviewed` when the safety gate isn't satisfied,
/// which is a de-escalation and therefore safe. Quietly turning `Reviewed`
/// into `AutoEdit` because the engine can't ask would be the opposite — a
/// privilege escalation performed on the user's behalf, which is exactly the
/// kind of thing the compliance rules exist to prevent. So it errors.
pub fn vet(
    engine: &dyn Engine,
    permission_mode: PermissionMode,
    resuming: bool,
) -> Result<(), String> {
    let caps = engine.capabilities();
    if permission_mode == PermissionMode::Reviewed && !caps.interactive_permissions {
        return Err(format!(
            "{} can't run in Reviewed mode: it has no way to pause and ask you to \
approve a tool call mid-run — headless, it silently rejects every prompt \
instead. Choose Auto-edit (Eren pre-grants exactly the tools this run \
needs) or Don't-ask, or run this on Claude Code.",
            engine.label()
        ));
    }
    if permission_mode == PermissionMode::AutoEdit && !caps.auto_edit {
        return Err(format!(
            "{} can't run in Auto-edit: it has no setting that allows edits without \
also allowing commands. Choose Full Auto (in a worktree you can review) or run \
this on an engine that can.",
            engine.label()
        ));
    }
    if resuming && !caps.resume_sessions {
        return Err(format!(
            "{} can't resume a previous session, so this would silently start over \
without the earlier context.",
            engine.label()
        ));
    }
    Ok(())
}

/// A plain Auto-edit run in a worktree, for the adapters' argv tests.
#[cfg(test)]
pub(crate) fn test_spec() -> RunSpec {
    RunSpec {
        cwd: PathBuf::from("/tmp/wt"),
        prompt: "do the thing".into(),
        model_tier: ModelTier::Medium,
        model_id: String::new(),
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

/// Every event an engine sends for one run, to the end.
#[cfg(test)]
pub(crate) async fn drain(engine: &dyn Engine, spec: RunSpec) -> Vec<ErenEvent> {
    let mut proc = once_not_busy(|| engine.start(spec.clone()));
    let mut events = vec![];
    while let Some(e) = proc.events.recv().await {
        events.push(e);
    }
    events
}

/// A stand-in for an engine's binary: a shell script that records its argv
/// to `<dir>/argv` and then runs `body`. Lets an adapter be tested end to end
/// — argv, spawn, pump, parser — without the real CLI on the machine.
#[cfg(all(test, unix))]
pub(crate) fn stand_in(dir: &std::path::Path, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("engine");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}/argv'\n{body}\n",
            dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin.display().to_string()
}

/// Start a [`stand_in`], waiting out the moment its file is still "busy".
///
/// The script was just written, and if another test thread forked while it
/// was open for writing, that child holds the descriptor until its own exec —
/// running the script then fails with ETXTBSY. Nothing reopens the file for
/// writing, so the window closes for good within milliseconds; a retry is the
/// fix, not a mask. Anything else fails at once.
#[cfg(test)]
pub(crate) fn once_not_busy<T>(mut start: impl FnMut() -> anyhow::Result<T>) -> T {
    const TEXT_BUSY: i32 = 26; // ETXTBSY on Linux and macOS alike
    for _ in 0..100 {
        match start() {
            Err(e)
                if e.chain().any(|c| {
                    c.downcast_ref::<std::io::Error>()
                        .and_then(std::io::Error::raw_os_error)
                        == Some(TEXT_BUSY)
                }) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            other => return other.unwrap(),
        }
    }
    start().unwrap()
}

/// A stand-in that prints `fixture` on stdout and exits 0.
#[cfg(all(test, unix))]
pub(crate) fn replaying(dir: &std::path::Path, fixture: &str) -> String {
    let path = dir.join("fixture.jsonl");
    std::fs::write(&path, fixture).unwrap();
    stand_in(dir, &format!("cat '{}'", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Linux only: ETXTBSY is Linux's refusal. macOS execs a script that is
    // still open for writing, so there is nothing there to wait for.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_stand_in_still_open_for_writing_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stand_in(dir.path(), "exit 0");
        // What a forked sibling test does by accident: hold the script open
        // for writing. Running it then fails — shown, not assumed.
        let held = std::fs::OpenOptions::new().write(true).open(&bin).unwrap();
        let busy = eren_shared::env_guard::command(&bin).spawn().unwrap_err();
        assert_eq!(busy.raw_os_error(), Some(26), "{busy}");

        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            drop(held);
        });
        let mut child = once_not_busy(|| Ok(eren_shared::env_guard::command(&bin).spawn()?));
        assert!(child.wait().await.unwrap().success());
        release.join().unwrap();
    }

    struct Fake(Capabilities);

    #[async_trait]
    impl Engine for Fake {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn label(&self) -> &'static str {
            "Fake Engine"
        }
        fn capabilities(&self) -> Capabilities {
            self.0
        }
        async fn detect(&self) -> Option<EngineInfo> {
            None
        }
        fn start(&self, _spec: RunSpec) -> anyhow::Result<EngineProcess> {
            anyhow::bail!("not a real engine")
        }
        fn interactive_resume_argv(&self, _session_id: &str) -> Option<Vec<String>> {
            None
        }
    }

    fn caps(interactive: bool, resume: bool) -> Capabilities {
        Capabilities {
            interactive_permissions: interactive,
            structured_rate_limit: false,
            resume_sessions: resume,
            append_system_prompt: true,
            fixed_model_catalog: false,
            reports_cost: true,
            enforces_denied_tools: true,
            mcp_tools: true,
            auto_edit: true,
            read_only_passes: true,
        }
    }

    #[test]
    fn an_engine_without_auto_edit_refuses_it_rather_than_widening() {
        let engine = Fake(Capabilities {
            auto_edit: false,
            ..caps(false, true)
        });
        let err = vet(&engine, PermissionMode::AutoEdit, false).unwrap_err();
        assert!(err.contains("Fake Engine") && err.contains("Full Auto"));
        vet(&engine, PermissionMode::FullAuto, false).unwrap();
    }

    #[test]
    fn an_engine_that_cannot_ask_refuses_reviewed_rather_than_downgrading() {
        let engine = Fake(caps(false, true));
        let err = vet(&engine, PermissionMode::Reviewed, false).unwrap_err();
        assert!(
            err.contains("Fake Engine"),
            "the message must name the engine"
        );
        assert!(err.contains("Auto-edit"), "and offer a way forward");
    }

    #[test]
    fn the_same_engine_is_fine_for_modes_it_can_honour() {
        let engine = Fake(caps(false, true));
        vet(&engine, PermissionMode::AutoEdit, false).unwrap();
        vet(&engine, PermissionMode::FullAuto, false).unwrap();
    }

    #[test]
    fn resuming_is_refused_only_when_actually_resuming() {
        let engine = Fake(caps(true, false));
        vet(&engine, PermissionMode::AutoEdit, false).unwrap();
        assert!(vet(&engine, PermissionMode::AutoEdit, true).is_err());
    }

    #[test]
    fn a_fully_capable_engine_passes_everything() {
        let engine = Fake(caps(true, true));
        for mode in [
            PermissionMode::Reviewed,
            PermissionMode::AutoEdit,
            PermissionMode::FullAuto,
        ] {
            vet(&engine, mode, true).unwrap();
        }
    }
}
