//! Run orchestrator: owns the run state machine, the concurrency semaphore,
//! and the queue loop. All status transitions happen here and every event is
//! persisted before it is broadcast, so the DB is the source of truth.
//!
//! Two kinds of runs share the same machinery:
//! - task runs (board cards): isolated worktree, permission proxy
//! - chat runs (project assistant turns): real checkout cwd, but locked to a
//!   read-only + eren-tools allowed list — never Bash/Edit/Write there.

use chrono::{DateTime, Utc};
use eren_engines::{Engine, RunSpec};
use eren_shared::workflow::{SessionMode, StepOutputs, Workflow};
use eren_shared::{
    EngineTierEffort, EngineTierMapping, ErenEvent, EventEnvelope, McpWiring, ModelTier,
    PermissionMode, ReasoningEffort, RunStatus, TierChoice, TierMapping,
};
use futures::StreamExt;
use sqlx::Row;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::apps;
use crate::bus::EventBus;
use crate::db::Db;
use crate::queue::rate_limit_backoff;
use crate::runs::attachments;
use crate::runs::memory;
use crate::runs::mentions;
use crate::runs::task_plan;
use crate::runs::usage_tally::{UsageDelta, UsageTally};
use crate::runs::{follow_up, report};
use crate::worktrees::manager::WorktreeManager;

/// Tools a chat run may use: read-only inspection of the checkout plus
/// Eren's own workspace tools. Compliance-adjacent invariant: never add
/// Bash/Edit/Write here — chat runs execute in the user's real checkout.
pub const CHAT_ALLOWED_TOOLS: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "mcp__eren__create_task",
    "mcp__eren__start_task",
    "mcp__eren__list_tasks",
    "mcp__eren__get_task_status",
    "mcp__eren__list_agents",
    "mcp__eren__cancel_task",
    "mcp__eren__get_diff",
    "mcp__eren__get_spend",
    "mcp__eren__list_skills",
    "mcp__eren__move_task",
    "mcp__eren__search_code",
    // Asking instead of guessing. Survives plan mode deliberately — it is the
    // most plan-appropriate thing an assistant can do.
    "mcp__eren__ask_user",
];

/// Named explicitly rather than left to the allow-list.
///
/// `--allowedTools` pre-*approves*; it does not forbid. Anything the chat
/// assistant must never reach has to be denied by name or the CLI will happily
/// pick it up — and the assistant runs in the user's real checkout, not a
/// worktree, so an edit here would land straight on their files.
pub const CHAT_DENIED_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"];

/// The weakest permission mode this engine can actually honour for a read-only
/// tool set.
///
/// The chat assistant cannot edit anything: its whole surface is Read/Grep/Glob
/// plus Eren's own task tools, with the mutating tools denied above. So there
/// is nothing here for a human to review, and asking for review is not caution
/// — on an engine that cannot pause and ask, it is silence.
///
/// This used to be a flat `Reviewed`, which is why chat looked broken on
/// OpenCode: with no way to answer a prompt mid-run it rejects every tool call,
/// so the assistant could not read the repository or reach its own tools and
/// fell back to asking the user what they were working on. `vet` exists to
/// refuse exactly that pairing, and this was the one caller that never ran it.
/// What a run that was refused Full Auto runs as instead: the narrowest mode
/// this engine can honour, or `None` when it has none — Amp and Cursor allow
/// every tool or nothing, so for them the refusal has to be a refusal.
///
/// Reviewed where the engine can ask; otherwise Auto-edit, which is no more
/// than a person could pick for the card directly. Never a mode `vet` would
/// refuse: on OpenCode, Reviewed silently rejects every tool call.
pub(crate) fn short_of_full_auto(caps: &eren_engines::Capabilities) -> Option<PermissionMode> {
    if caps.interactive_permissions {
        Some(PermissionMode::Reviewed)
    } else if caps.auto_edit {
        Some(PermissionMode::AutoEdit)
    } else {
        None
    }
}

fn chat_permission_mode(engine: &dyn eren_engines::Engine) -> PermissionMode {
    if engine.capabilities().interactive_permissions {
        // Claude: the allow-list has already pre-approved the read tools, so
        // nothing prompts. AutoEdit rather than FullAuto keeps the dangerous
        // flag off a run that has no business editing anything.
        PermissionMode::AutoEdit
    } else {
        // OpenCode: approve-everything is the only setting it has, and
        // "everything" here is bounded by the denials above.
        PermissionMode::FullAuto
    }
}

const CHAT_SYSTEM_PROMPT: &str = "You are the Eren project assistant embedded in a workspace \
dashboard. You can inspect this repository (Read/Grep/Glob) but you cannot edit it directly. \
To do coding work, create a task with mcp__eren__create_task (set start=true to launch it \
immediately); each task runs a coding agent in an isolated git worktree and its result appears \
on the user's board for review. Use mcp__eren__list_tasks / get_task_status to report \
progress, and mcp__eren__list_agents to pick a specialized agent for a task. When the user \
writes @Name in their message they are naming an agent from their library — assign that work to \
them by passing agent_name, spelled exactly. Keep replies short and conversational; the user \
sees them in a chat panel. \
**Before you create a task from an ambiguous request, ask.** If two readings of what the user \
said would lead to different cards, or to a different scope, call mcp__eren__ask_user with \
the readings as options and stop there — a card started on the wrong reading costs a real run \
and a diff nobody wanted. Do not ask about something you can settle by reading the code, and \
do not ask for permission to proceed; ask when the answer changes the work. \
You can also stop a task, summarise what it changed, file it in a column, and say what it has \
cost — but you cannot merge anything into the user's checkout, and there is no tool for it. \
When a card looks ready, say so and what it changed, and let them press Merge on the card, \
where they can read the diff first.";

/// The system prompt for a *general* chat: no project, no board, no repo.
///
/// Its own constant rather than the project prompt with caveats — a prompt
/// that describes ten tools the assistant does not have teaches it to promise
/// things every reply will then fail to deliver.
const GENERAL_CHAT_SYSTEM_PROMPT: &str = "You are the Eren assistant. This conversation is \
not attached to any project: there is no repository to inspect, no board to create tasks on, \
and no code you can see. You can search the web (WebSearch) and read pages (WebFetch) to \
answer questions, and you can reason and write. If the user asks for coding work on one of \
their projects, tell them to switch this chat to that project — the picker is above the \
conversation list. Keep replies short and conversational; the user sees them in a chat panel.";

/// Tools for a chat scoped to a *space* — a folder of documents, not a repo.
///
/// The read tools point at the documents (which is the whole point of a
/// space: drop files in, ask about them), the web tools cover what the
/// documents reference. No board tools — see the MCP wiring decision — and
/// the same denials as every chat, because the run stands in the user's real
/// folder.
pub const SPACE_CHAT_ALLOWED_TOOLS: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "WebSearch",
    "WebFetch",
    "mcp__eren__search_documents",
    "mcp__eren__list_documents",
    "mcp__eren__ask_user",
];

/// The system prompt for a space chat.
const SPACE_CHAT_SYSTEM_PROMPT: &str = "You are the Eren assistant. This conversation is attached to a document space: the folder you are in holds documents the user has collected, and your job is to answer questions about and around them. Relevant passages from the documents are attached to each message automatically, with file names — cite them when you draw on them. Use mcp__eren__search_documents to search again with a different phrasing, mcp__eren__list_documents to see what is here, and Read to open a whole file; WebSearch and WebFetch cover what the documents reference. Ground answers about the documents in what they actually say — quote or name the file — and say plainly when the answer is not in them. There is no repository here and no task board; if the user asks for coding work, tell them to switch this chat to one of their projects. When a question could be read two ways and the readings lead to different answers, call mcp__eren__ask_user with the readings as options and stop, rather than answering both or picking one. Keep replies short and conversational; the user sees them in a chat panel.";

/// One attempt in a bake-off: an agent, a tier, or both.
///
/// Either field may be absent — "the same agent at three effort levels" and
/// "three different agents at their own tiers" are both things you'd want to
/// compare, and the label is what the user reads either way.
#[derive(Debug, Clone)]
pub struct Variant {
    pub label: String,
    pub agent_id: Option<Uuid>,
    pub tier: Option<String>,
    /// Which CLI runs this attempt. `None` means the card's. This is what
    /// makes "Claude vs OpenCode on the same brief" a thing you can ask for.
    pub engine: Option<String>,
}

/// Why the queue is, or isn't, dispatching.
#[derive(Debug, Clone, PartialEq)]
pub enum QueueGate {
    Open,
    /// Someone pressed pause. Cleared by pressing resume.
    Paused,
    /// A machine-wide budget is spent. Clears itself when its window turns —
    /// there is no resume for this one, which is why it can't just be a bool.
    OverBudget(crate::budgets::OverBudget),
}

pub struct Orchestrator {
    pub db: Db,
    pub bus: EventBus,
    engines: HashMap<&'static str, Arc<dyn Engine>>,
    /// What `detect()` said at boot, kept so the engines endpoint can answer
    /// without spawning three CLIs on every page load.
    detected: HashMap<&'static str, eren_engines::EngineInfo>,
    /// Tier → model routing, live rather than baked at boot: changing it in
    /// settings must affect the next run, not require a restart.
    tiers: Arc<std::sync::RwLock<EngineTierMapping>>,
    /// What each engine *would* route to if the user set nothing, derived at
    /// boot from the models that install actually reported.
    ///
    /// Kept apart from `tiers` because it answers a different question: that
    /// one is "what runs now", this one is "what does Reset go back to". The
    /// built-in constant cannot answer the second for an engine fronting many
    /// providers — it names Anthropic ids to a Google-only install, and none
    /// at all for a local runtime, whose models are a fact about a disk.
    derived: Arc<std::sync::RwLock<EngineTierMapping>>,
    tier_efforts: Arc<std::sync::RwLock<EngineTierEffort>>,
    pub worktrees: Arc<WorktreeManager>,
    pub(crate) slots: Arc<crate::runs::slots::Slots>,
    /// Whether the over-budget hook has already fired for the current spell.
    /// The gate is read every 750ms, so this is what turns a state into an
    /// event.
    /// Base URL of Eren's MCP endpoints, e.g. "http://127.0.0.1:4820".
    /// None disables MCP wiring (mock engine / tests).
    pub(crate) mcp_base_url: Option<String>,
    /// Keyed by run, never by step — see `CancelState`.
    cancels: Mutex<HashMap<Uuid, CancelState>>,
    /// Runs whose `execute` has not returned yet, with when each last showed
    /// a sign of life. In memory on purpose: it answers "is this process
    /// still working on it", which no row can — a row says `running` just as
    /// confidently after the task holding it died. Entered and left only by
    /// [`Alive`], so an `execute` that panics or errors still leaves it.
    liveness: Arc<Mutex<HashMap<Uuid, Liveness>>>,
}

/// One executing run's signs of life.
#[derive(Clone, Copy)]
struct Liveness {
    /// The last event it produced (or when it started).
    seen: std::time::Instant,
    /// When `runs.last_event_at` was last written for it, so a chatty run
    /// writes the row every fifteen seconds rather than every event.
    written: Option<std::time::Instant>,
}

/// A run's place in [`Orchestrator::liveness`] for as long as it is held.
pub(crate) struct Alive {
    map: Arc<Mutex<HashMap<Uuid, Liveness>>>,
    run_id: Uuid,
}

impl Drop for Alive {
    fn drop(&mut self) {
        if let Ok(mut map) = self.map.lock() {
            map.remove(&self.run_id);
        }
    }
}

pub(crate) struct StreamOutcome {
    pub status: RunStatus,
    pub reason: Option<String>,
    /// Final assistant/result text, used to feed later pipeline steps.
    pub output: String,
    pub session_id: Option<String>,
}

/// Cancellation for a run that may be several steps long.
///
/// `requested` is the part that matters: interrupting the process running
/// right now is not enough, because a workflow or organization would simply
/// start the next assignment. The flag is checked between steps, so asking
/// to cancel stops the whole run rather than one step of it.
#[derive(Default)]
pub(crate) struct CancelState {
    requested: bool,
    /// Live steps to interrupt. Several at once during a fan-out.
    steps: HashMap<Uuid, oneshot::Sender<()>>,
}

/// An agent from the library, resolved for a step or task.
pub(crate) struct BoundAgent {
    pub id: Uuid,
    pub system_prompt: String,
    pub tier: ModelTier,
    pub effort: Option<ReasoningEffort>,
    pub allowed_tools: Vec<String>,
    /// `None` means "inherit" — nullable since migration 0020. Decoding this
    /// as a plain `String` panics on every agent that inherits, which is now
    /// all of them by default.
    pub permission_preset: Option<String>,
    /// `None` means "whatever the workflow or card says".
    pub engine: Option<String>,
}

/// A step's permission mode, and whether it got there by being cut down.
///
/// The distinction matters because the two produce the same mode for very
/// different reasons, and only one of them is something the author wrote.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StepPermission {
    pub mode: PermissionMode,
    /// True when FullAuto was refused and Reviewed substituted.
    pub downgraded: bool,
}

/// Apply the FullAuto safety gate.
///
/// Pure so it can be tested without a database or a worktree on disk — the
/// gate is a safety property and deserves to be pinned down directly rather
/// than inferred from an integration run.
pub(crate) fn resolve_step_permission(
    asked: PermissionMode,
    gate_satisfied: bool,
) -> StepPermission {
    if asked == PermissionMode::FullAuto && !gate_satisfied {
        // Down, never up: refusing FullAuto is de-escalation and safe.
        StepPermission {
            mode: PermissionMode::Reviewed,
            downgraded: true,
        }
    } else {
        StepPermission {
            mode: asked,
            downgraded: false,
        }
    }
}

/// Which of the six things that stream an engine this dispatch is.
///
/// It replaced a bare `finalize: bool`, because the two questions it answers
/// were being answered independently and one of them was never asked at all.
/// A caller has to say what it *is*, and the answers follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallerKind {
    /// A card doing its work.
    TaskWork,
    /// A card writing the plan it will be asked to approve.
    TaskPlanning,
    /// One step of a workflow.
    WorkflowStep,
    /// One teammate's turn inside an organization run.
    OrgMember,
    /// A chat turn in the assistant panel.
    Chat,
    /// A one-shot reply to a comment.
    CommentReply,
    /// Generating a knowledge-base article.
    KbGeneration,
    /// Investigating a question about a project: read-only plus the web,
    /// producing a report.
    Research,
}

/// What to do when the engine reports a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnRateLimit {
    /// Mark the run held and put it back on the queue behind a backoff.
    Hold,
    /// Give up and let the caller finish the run. Re-dispatching would charge
    /// again for work that is already paid for.
    Fail,
}

impl CallerKind {
    /// Does `stream_run` own this run's ending?
    ///
    /// False where a *step* ending is not a *run* ending: a plan that
    /// completed is not a completed run, and marking it terminal would send
    /// the card to review with nothing done. Workflows and organizations
    /// decide their own outcome once every step is in.
    pub(crate) fn finalizes(self) -> bool {
        !matches!(
            self,
            Self::TaskPlanning | Self::WorkflowStep | Self::OrgMember
        )
    }

    /// Can this run be handed back to `execute` later, unchanged?
    ///
    /// This is the whole question, and it was never asked: `stream_run` wrote
    /// a queue row for *every* caller, so a workflow step that hit a limit put
    /// its run back on the queue, the workflow then failed the run, `finish`
    /// left the row behind, and five minutes later `claim_next` popped a run
    /// marked `failed` and re-ran the entire pipeline — paying again for every
    /// step that had already succeeded.
    ///
    /// Exhaustive on purpose. A seventh caller is a compile error here rather
    /// than a silent leak, which is how the first six got one.
    pub(crate) fn on_rate_limit(self) -> OnRateLimit {
        match self {
            // `execute_task_run` reuses the card's worktree and rebuilds its
            // spec from the row, so a second dispatch continues rather than
            // duplicates. Everything after `stream_run` is guarded on
            // `Completed`, so a held run falls straight through it.
            Self::TaskWork => OnRateLimit::Hold,
            // Same, plus one early return: `park_for_approval` treats "not
            // completed" as "no plan to approve" and fails the run, which
            // would undo the hold a line later.
            Self::TaskPlanning => OnRateLimit::Hold,
            // Nothing is written until the article completes, and the run row
            // still carries the brief.
            Self::KbGeneration => OnRateLimit::Hold,
            // Same shape: the report is written only on completion, and the
            // run row carries `research_id`, so a re-dispatch rebuilds the
            // identical spec.
            Self::Research => OnRateLimit::Hold,
            // Both post exactly once, on completion, so a second dispatch
            // produces one reply rather than two.
            Self::Chat | Self::CommentReply => OnRateLimit::Hold,
            // `create_step_row` and `outputs` are written per dispatch, so a
            // re-run of a half-finished pipeline is charged for in full and
            // duplicates its own step rows. Failing honestly beats that; a
            // workflow that resumes from its `steps` rows is its own feature.
            Self::WorkflowStep => OnRateLimit::Fail,
            // The one that reads like it should hold and cannot. A member is
            // a *step* of an org run: the batch loop is still driving other
            // assignments in parallel and will finish the run itself, so a
            // hold written here is overwritten by a terminal status moments
            // later — leaving exactly the orphan queue row this is meant to
            // stop. Stopping a batch cleanly mid-flight is a feature, not a
            // guard, so the assignment fails and says why.
            Self::OrgMember => OnRateLimit::Fail,
        }
    }
}

/// A card was asked to start while a run of it is still live.
///
/// Its own type so a route can answer 409 — the person double-clicked, or the
/// board is stale — rather than a 500 wrapping the same sentence.
#[derive(Debug, thiserror::Error)]
#[error("this card is already running — cancel it before starting it again")]
pub struct AlreadyRunning;

/// The work needs Eren's tools — the chat assistant's board tools, a team
/// member's hand-off tools — and this engine cannot be given them for one
/// run without a config file in the run's folder. Refused at the click, the
/// way `vet` refuses a permission mode.
#[derive(Debug, thiserror::Error)]
#[error("{label} can't be handed Eren's tools for a single run, and {what} works through them — pick an engine that can ({can})")]
pub struct NoTools {
    pub label: String,
    pub what: &'static str,
    /// The installed engines that can, by name — computed, so the advice is
    /// never an engine this machine does not have.
    pub can: String,
}

/// An engine that cannot honour the mode some work needs — `vet`'s answer,
/// as a refusal a door turns into a 409 rather than a run that fails later.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CantHonour(pub String);

/// Race-free `events.seq` allocation. A workflow run has several steps
/// writing concurrently, and `(run_id, seq)` is unique.
#[derive(Clone)]
pub(crate) struct SeqAlloc(Arc<std::sync::atomic::AtomicI64>);

impl SeqAlloc {
    pub(crate) fn starting_at(seq: i64) -> Self {
        Self(Arc::new(std::sync::atomic::AtomicI64::new(seq)))
    }
    pub(crate) fn next(&self) -> i64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

impl Orchestrator {
    pub fn new(
        db: Db,
        bus: EventBus,
        worktrees: Arc<WorktreeManager>,
        max_concurrent: usize,
        mcp_base_url: Option<String>,
    ) -> Self {
        Self {
            db,
            bus,
            engines: HashMap::new(),
            detected: HashMap::new(),
            tiers: Arc::new(std::sync::RwLock::new(EngineTierMapping::default())),
            derived: Arc::new(std::sync::RwLock::new(
                EngineTierMapping(Default::default()),
            )),
            tier_efforts: Arc::new(std::sync::RwLock::new(EngineTierEffort::default())),
            worktrees,
            slots: Arc::new(crate::runs::slots::Slots::new(max_concurrent)),
            mcp_base_url,
            cancels: Mutex::new(HashMap::new()),
            liveness: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn alive(&self, run_id: Uuid) -> Alive {
        self.liveness.lock().unwrap().insert(
            run_id,
            Liveness {
                seen: std::time::Instant::now(),
                written: None,
            },
        );
        Alive {
            map: self.liveness.clone(),
            run_id,
        }
    }

    /// Is this process still inside the run's `execute` — including the
    /// post-work after the engine exits (the report, checks, the review)?
    /// A run's status turns terminal before that work is done, so "not live
    /// in the database" alone does not mean "safe to start another run in
    /// its worktree".
    pub fn is_executing(&self, run_id: Uuid) -> bool {
        self.liveness.lock().unwrap().contains_key(&run_id)
    }

    /// How long since an executing run last showed a sign of life. `None`
    /// when this process is not executing it.
    pub fn since_last_sign(&self, run_id: Uuid) -> Option<std::time::Duration> {
        self.liveness
            .lock()
            .unwrap()
            .get(&run_id)
            .map(|l| l.seen.elapsed())
    }

    /// A sign of life that is not an event — a parked run allowed to go on.
    /// Memory only.
    pub fn mark_seen(&self, run_id: Uuid) {
        if let Some(l) = self.liveness.lock().unwrap().get_mut(&run_id) {
            l.seen = std::time::Instant::now();
        }
    }

    /// Pretend an executing run last spoke `ago` ago. For the reaper's tests.
    #[cfg(test)]
    pub(crate) fn backdate(&self, run_id: Uuid, ago: std::time::Duration) {
        if let Some(l) = self.liveness.lock().unwrap().get_mut(&run_id) {
            l.seen = std::time::Instant::now() - ago;
        }
    }

    /// A run said something. Cheap: memory every time, the row at most every
    /// fifteen seconds, and never an error for the run.
    async fn touch(&self, run_id: Uuid) {
        let now = std::time::Instant::now();
        let write = {
            let mut map = self.liveness.lock().unwrap();
            match map.get_mut(&run_id) {
                Some(l) => {
                    l.seen = now;
                    let due = l.written.is_none_or(|w| {
                        now.duration_since(w) >= std::time::Duration::from_secs(15)
                    });
                    if due {
                        l.written = Some(now);
                    }
                    due
                }
                None => false,
            }
        };
        if write {
            let _ = sqlx::query("UPDATE runs SET last_event_at = now() WHERE id = $1")
                .bind(run_id)
                .execute(&self.db.pool)
                .await;
        }
    }

    /// What to set `MCP_TOOL_TIMEOUT` to, in milliseconds.
    ///
    /// Derived from the attention window rather than written out, because the
    /// two numbers have to agree and used to agree only by comment. The CLI
    /// abandoning the tool call before the broker's window closes is the same
    /// silent lie the window was widened to remove — the engine would get a
    /// timeout error nobody chose, at a moment when the person was still
    /// perfectly able to answer.
    ///
    /// "Wait forever" has no expressible value here, so it becomes the ceiling
    /// the setting itself clamps to.
    ///
    /// The margin is load-bearing, not padding. Set the two to the same number
    /// and they race — and the CLI won, so the engine got
    /// `MCP server "eren" tool "approve" timed out after 60s` instead of
    /// Eren's own sentence, then carried on trying other ways to do the same
    /// edit. The broker has to be the one that decides how a wait ends,
    /// because it is the only one of the two that knows the difference between
    /// a refusal and an empty room.
    pub(crate) async fn mcp_tool_timeout_ms(&self) -> String {
        crate::attention::cli_timeout_ms(crate::attention::load(&self.db).await.window())
    }

    /// The queue's concurrency budget, so the permission broker can lend a
    /// parked run's slot back while it waits.
    pub fn slots(&self) -> Arc<crate::runs::slots::Slots> {
        self.slots.clone()
    }

    pub fn register_engine(&mut self, engine: Arc<dyn Engine>) {
        self.engines.insert(engine.id(), engine);
    }

    /// Register an engine only if its CLI is actually installed.
    ///
    /// An engine that isn't here is simply *not offered* — which is a far
    /// better failure than accepting the choice and then dying at spawn time,
    /// minutes later and nowhere near where the user made it.
    ///
    /// Returns what `detect()` found, so the caller can log it.
    pub async fn register_if_available(
        &mut self,
        engine: Arc<dyn Engine>,
    ) -> Option<eren_engines::EngineInfo> {
        let info = engine.detect().await?;
        self.detected.insert(engine.id(), info.clone());
        self.engines.insert(engine.id(), engine);
        Some(info)
    }

    /// What `detect()` reported at boot. `None` for engines registered
    /// without a probe (the mock).
    pub fn engine_info(&self, id: &str) -> Option<&eren_engines::EngineInfo> {
        self.detected.get(id)
    }

    pub fn engine(&self, id: &str) -> Option<Arc<dyn Engine>> {
        self.engines.get(id).cloned()
    }

    /// Every engine that was actually found at boot. Sorted by id so the UI
    /// column order doesn't shuffle between restarts (a `HashMap` iteration
    /// order would).
    pub fn engines(&self) -> Vec<Arc<dyn Engine>> {
        let mut v: Vec<_> = self.engines.values().cloned().collect();
        v.sort_by_key(|e| e.id());
        v
    }

    /// What to run when nothing upstream stated a preference.
    ///
    /// Claude Code when it's installed — it's the one engine that can do
    /// everything Eren offers, including stopping to ask permission. Only
    /// when it's absent does something else become the default, and then the
    /// alternative to picking one is offering the user nothing at all.
    pub fn default_engine(&self) -> String {
        if self.engines.contains_key("claude-code") {
            return "claude-code".into();
        }
        // Otherwise the most capable engine installed, by what it can do: one
        // that can carry Eren's tools and edit without a shell first, since
        // the assistant, a manager, a team and an ordinary card all fall back
        // to this. Alphabetical order alone made Amp the default beside Codex
        // — an engine every one of those would refuse.
        let installed: Vec<_> = self
            .engines()
            .into_iter()
            .filter(|e| e.id() != "mock")
            .collect();
        let rank = |e: &Arc<dyn Engine>| {
            let c = e.capabilities();
            match (c.mcp_tools, c.auto_edit) {
                (true, true) => 0,
                (true, false) => 1,
                (false, true) => 2,
                (false, false) => 3,
            }
        };
        installed
            .iter()
            .min_by_key(|e| rank(e))
            .map(|e| e.id().to_string())
            .unwrap_or_else(|| "claude-code".into())
    }

    /// Create a run for a board task and put it on the queue. A task handed
    /// to a team runs as that team instead of a single agent.
    pub async fn enqueue_task(&self, task_id: Uuid) -> anyhow::Result<Uuid> {
        // One start of a card at a time. The checks below and the insert that
        // acts on them used to be separate statements, so a double click — or
        // a drag racing the Start button — passed both checks twice and put
        // two runs, two agents, in the card's one worktree.
        //
        // `FOR NO KEY UPDATE`, not `FOR UPDATE`: the run row inserted below
        // references this task, and its foreign-key check takes `KEY SHARE`,
        // which this lock admits and `FOR UPDATE` would not — the team path
        // inserts on its own connection and would wait on us forever.
        self.supersede_summary(task_id).await?;
        let mut guard = self.db.pool.begin().await?;
        sqlx::query("SELECT 1 FROM tasks WHERE id = $1 FOR NO KEY UPDATE")
            .bind(task_id)
            .fetch_one(&mut *guard)
            .await?;
        let running: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM runs WHERE task_id = $1
                               AND status NOT IN ('completed','failed','canceled'))",
        )
        .bind(task_id)
        .fetch_one(&mut *guard)
        .await?;
        if running {
            return Err(AlreadyRunning.into());
        }

        // The last line of defence against two agents in one checkout.
        //
        // A sub-ticket's work is already running under a step of its epic's run,
        // and starting it again would put a second agent in the same worktree
        // with a second writer for the card's column. The routes refuse this
        // with a 409, but they are not the only door: dropping a card into
        // "In Progress", Retry, and the chat MCP's `start_task` all arrive here
        // directly.
        let live: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM steps s JOIN runs r ON r.id = s.run_id
                  WHERE s.task_id = $1
                    AND s.status IN ('queued','starting','running','waiting_permission','rate_limited')
                    AND r.status NOT IN ('completed','failed','canceled'))",
        )
        .bind(task_id)
        .fetch_one(&self.db.pool)
        .await?;
        if live {
            anyhow::bail!("a teammate is already working on this sub-task as part of its epic");
        }

        // Dependencies hold until the blocker's work has LANDED — 'done', not
        // 'review'. A blocker in review has a diff nobody merged; a dependent
        // run started then would branch from main without the work it builds
        // on. Checked here, the one door every start comes through (routes,
        // drag, retry, the chat MCP's start_task), not only in the UI.
        let blockers: Vec<String> = sqlx::query_scalar(
            "SELECT b.title FROM task_deps d JOIN tasks b ON b.id = d.blocked_by
             WHERE d.task_id = $1 AND b.board_column <> 'done' ORDER BY b.title",
        )
        .bind(task_id)
        .fetch_all(&self.db.pool)
        .await?;
        if !blockers.is_empty() {
            anyhow::bail!(
                "blocked by {} — land {} first",
                blockers.join(", "),
                if blockers.len() == 1 {
                    "that card"
                } else {
                    "those cards"
                }
            );
        }

        let assigned = sqlx::query("SELECT agent_id, team_id FROM tasks WHERE id = $1")
            .bind(task_id)
            .fetch_one(&self.db.pool)
            .await?;
        let assigned_team: Option<Uuid> = assigned.get("team_id");
        // A paused or retired assignee starts nothing, on any door. A team's
        // own check is in `enqueue_task_for_team`.
        if assigned_team.is_none() {
            let agent: Option<Uuid> = assigned.get("agent_id");
            crate::agents::assert_can_run(&self.db, agent.as_slice()).await?;
        }
        // A spent budget refuses here, at the click, with the policy's name —
        // not by queueing the card to sit until the window turns.
        crate::budgets::check(
            &self.db,
            &crate::budgets::scope_of_task(&self.db, task_id).await?,
            true,
        )
        .await?;
        if let Some(team_id) = assigned_team {
            let run_id = self.enqueue_task_for_team(task_id, team_id).await?;
            guard.commit().await?;
            return Ok(run_id);
        }

        // The bound agent's engine wins over the card's: an agent that names
        // one has been deliberately configured for it, while a card's is the
        // machine default nobody chose.
        //
        // A new attempt is a new answer to whatever stopped the last one.
        sqlx::query(
            "UPDATE tasks SET blocked_note = NULL WHERE id = $1 AND blocked_note IS NOT NULL",
        )
        .bind(task_id)
        .execute(&mut *guard)
        .await?;
        // The run and its queue row commit together. Apart, a crash between
        // them left a run reading `queued` that nothing would ever dispatch.
        let row = sqlx::query(
            "INSERT INTO runs (task_id, status, trigger, engine, plan_approval)
             SELECT t.id, 'queued', 'manual', COALESCE(a.engine, t.engine), t.plan_first
             FROM tasks t LEFT JOIN agents a ON a.id = t.agent_id
             WHERE t.id = $1
             RETURNING id",
        )
        .bind(task_id)
        .fetch_one(&mut *guard)
        .await?;
        let run_id: Uuid = row.get("id");
        sqlx::query("INSERT INTO queue (run_id, priority) VALUES ($1, 10)")
            .bind(run_id)
            .execute(&mut *guard)
            .await?;
        guard.commit().await?;
        Ok(run_id)
    }

    /// Pick a dead run back up: a new run row carrying the old one's session.
    ///
    /// Everything that decides *whether* this is allowed lives in
    /// `runs::resume` and has already run by the time this is called — the
    /// route answers the person, this writes the row. See that module for why
    /// resuming creates a row rather than re-queuing the old one.
    pub async fn resume_run(&self, prior_run_id: Uuid, session_id: &str) -> anyhow::Result<Uuid> {
        let prior = sqlx::query(
            "SELECT r.task_id, r.engine, r.agent_id, r.tier_override, r.variant_label,
                    r.worktree_path, r.error_reason,
                    COALESCE(r.prompt_override, t.prompt) AS prompt,
                    COALESCE(r.agent_id, t.agent_id) AS runs_as
             FROM runs r JOIN tasks t ON t.id = r.task_id
             WHERE r.id = $1",
        )
        .bind(prior_run_id)
        .fetch_one(&self.db.pool)
        .await?;
        let runs_as: Option<Uuid> = prior.get("runs_as");
        crate::agents::assert_can_run(&self.db, runs_as.as_slice()).await?;
        crate::budgets::check(
            &self.db,
            &crate::budgets::scope_of_run(&self.db, prior_run_id).await?,
            true,
        )
        .await?;

        let prompt = crate::runs::resume::continuation_prompt(
            &prior.get::<String, _>("prompt"),
            prior.get::<Option<String>, _>("error_reason").as_deref(),
        );

        let row = sqlx::query(
            // `plan_approval` is deliberately FALSE and not copied. A
            // plan-first run whose plan was never stored plans again — see
            // `a_plan_first_run_with_no_stored_plan_plans_again` — so carrying
            // it forward would hand a session that has been writing code a
            // planning pass with the mutating tools denied, and it would
            // produce a second plan instead of finishing the work.
            //
            // `comment_id` is likewise omitted rather than copied: with it set
            // `execute` routes to `execute_comment_run`, which builds its own
            // prompt and posts another reply. Only a card run gets here at all
            // (`Refusal::NotATaskRun`), so there is nothing to carry.
            "INSERT INTO runs (task_id, status, trigger, engine, plan_approval,
                               agent_id, tier_override, variant_label, worktree_path,
                               session_id, session_engine, prompt_override, resumed_from)
             VALUES ($1, 'queued', 'resume', $2, FALSE, $3, $4, $5, $6, $7, $2, $8, $9)
             RETURNING id",
        )
        .bind(prior.get::<Uuid, _>("task_id"))
        .bind(prior.get::<String, _>("engine"))
        .bind(prior.get::<Option<Uuid>, _>("agent_id"))
        .bind(prior.get::<Option<String>, _>("tier_override"))
        .bind(prior.get::<Option<String>, _>("variant_label"))
        .bind(prior.get::<Option<String>, _>("worktree_path"))
        .bind(session_id)
        .bind(&prompt)
        .bind(prior_run_id)
        .fetch_one(&self.db.pool)
        .await?;
        let run_id: Uuid = row.get("id");
        // Same priority a manual start gets: this *is* a manual start, of work
        // that is further along than a fresh one.
        sqlx::query("INSERT INTO queue (run_id, priority) VALUES ($1, 10)")
            .bind(run_id)
            .execute(&self.db.pool)
            .await?;
        Ok(run_id)
    }

    /// The worktree a run should work in. Team runs launched from a board
    /// task reuse that task's worktree, so the existing review-and-merge
    /// flow keeps working no matter who did the work.
    pub(crate) async fn worktree_for_run(
        &self,
        run_id: Uuid,
        task_id: Option<Uuid>,
        project_path: &std::path::Path,
        default_branch: &str,
        slug: &str,
    ) -> anyhow::Result<crate::worktrees::manager::Worktree> {
        // Looked up here rather than passed in: every caller would otherwise
        // have to remember, and forgetting means `git worktree add` against a
        // folder with no repository.
        if self.is_in_place(project_path).await? {
            return Ok(crate::worktrees::manager::Worktree {
                path: project_path.to_path_buf(),
                // Empty signals "no branch to merge"; nothing is persisted to
                // the task, so diff/merge stay unavailable.
                branch: String::new(),
            });
        }
        if let Some(task_id) = task_id {
            let row = sqlx::query("SELECT worktree_path, branch FROM tasks WHERE id = $1")
                .bind(task_id)
                .fetch_one(&self.db.pool)
                .await?;
            if let (Some(path), Some(branch)) = (
                row.get::<Option<String>, _>("worktree_path"),
                row.get::<Option<String>, _>("branch"),
            ) {
                return Ok(crate::worktrees::manager::Worktree {
                    path: PathBuf::from(path),
                    branch,
                });
            }
            let worktree = self
                .worktrees
                .create(project_path, default_branch, task_id, slug)
                .await?;
            sqlx::query("UPDATE tasks SET worktree_path=$1, branch=$2 WHERE id=$3")
                .bind(worktree.path.to_string_lossy().as_ref())
                .bind(&worktree.branch)
                .bind(task_id)
                .execute(&self.db.pool)
                .await?;
            return Ok(worktree);
        }
        self.worktrees
            .create(project_path, default_branch, run_id, slug)
            .await
    }

    /// True when a project has no version control, so its runs happen in the
    /// folder itself instead of an isolated worktree.
    pub(crate) async fn is_in_place(&self, project_path: &std::path::Path) -> anyhow::Result<bool> {
        let vcs: Option<(String,)> = sqlx::query_as("SELECT vcs FROM projects WHERE path = $1")
            .bind(project_path.to_string_lossy().as_ref())
            .fetch_optional(&self.db.pool)
            .await?;
        Ok(matches!(vcs, Some((v,)) if v != "git"))
    }

    /// Move a team-run task onto its landing column when the run finishes:
    /// review when there is a diff to look at, done when there isn't.
    pub(crate) async fn settle_task_for_run(
        &self,
        task_id: Option<Uuid>,
        status: RunStatus,
    ) -> anyhow::Result<()> {
        let Some(task_id) = task_id else {
            return Ok(());
        };
        if status == RunStatus::Completed {
            sqlx::query(
                "UPDATE tasks t SET board_column = CASE WHEN p.vcs = 'git' THEN 'review' ELSE 'done' END
                 FROM projects p WHERE p.id = t.project_id AND t.id = $1",
            )
            .bind(task_id)
            .execute(&self.db.pool)
            .await?;
            // A no-op unless that was 'done'.
            self.landed(task_id).await;
        }
        Ok(())
    }

    /// Refuse an engine that cannot be handed Eren's MCP tools per run, for
    /// work that lives on them. An unknown engine passes: dispatch reports
    /// that with a better message.
    pub fn needs_tools(&self, engine: &str, what: &'static str) -> Result<(), NoTools> {
        match self.engine(engine) {
            Some(e) if !e.capabilities().mcp_tools => {
                let mut can: Vec<&str> = self
                    .engines
                    .values()
                    .filter(|e| e.id() != "mock" && e.capabilities().mcp_tools)
                    .map(|e| e.label())
                    .collect();
                can.sort_unstable();
                Err(NoTools {
                    label: e.label().to_string(),
                    what,
                    can: match can.len() {
                        0 => "none is installed".to_string(),
                        _ => can.join(", "),
                    },
                })
            }
            _ => Ok(()),
        }
    }

    /// Every refusal a chat turn can meet, without starting it — so a caller
    /// that writes something first (an answer, an approval) can ask before
    /// it does, and a refusal leaves nothing half-done.
    pub async fn vet_chat_turn(&self, chat_id: Uuid, engine: &str) -> anyhow::Result<()> {
        // Only a chat on a project is handed Eren's tools (`execute_chat_run`
        // wires them by project); a general chat or a watch routine reads the
        // web and nothing else, and any engine can do that.
        let row: Option<(Option<Uuid>, Option<Uuid>)> =
            sqlx::query_as("SELECT project_id, agent_id FROM chats WHERE id = $1")
                .bind(chat_id)
                .fetch_optional(&self.db.pool)
                .await?;
        let (project, agent) = row.unwrap_or_default();
        if project.is_some() {
            self.needs_tools(engine, "the assistant")?;
        }
        // A chat with an agent is that agent at work: paused or retired, it
        // does not answer.
        crate::agents::assert_can_run(&self.db, &agent.into_iter().collect::<Vec<_>>()).await?;
        // The assistant spends like anything else: a spent workspace or
        // project says so on the message, not by leaving the turn queued.
        crate::budgets::check(
            &self.db,
            &crate::budgets::scope_of_chat(&self.db, chat_id).await?,
            true,
        )
        .await?;
        Ok(())
    }

    /// The engine a chat's next turn runs on when nobody picked one: the one
    /// its last turn ran on, so an answer or an approval carries on where the
    /// conversation was — a manager thread on Qwen stays on Qwen. A chat with
    /// an agent starts on the agent's engine.
    pub async fn chat_engine(&self, chat_id: Uuid) -> anyhow::Result<String> {
        let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT (SELECT engine FROM runs WHERE chat_id = c.id ORDER BY created_at DESC LIMIT 1),
                    a.engine
               FROM chats c LEFT JOIN agents a ON a.id = c.agent_id
              WHERE c.id = $1",
        )
        .bind(chat_id)
        .fetch_optional(&self.db.pool)
        .await?;
        let (last, agent) = row.unwrap_or_default();
        Ok(last
            .into_iter()
            .chain(agent)
            .find(|e| self.engine(e).is_some())
            .unwrap_or_else(|| self.default_engine()))
    }

    /// Create a run for a chat turn. Chat runs outrank task runs in the
    /// queue (priority 20 vs 10) so the assistant feels responsive.
    ///
    /// A chat with an agent runs *as* that agent: the gate is asked here, and
    /// the run carries `agent_id`, so its limits, budgets and dispatch check
    /// apply as they do to its cards. The assistant (no agent) needs no gate;
    /// work it hands an agent starts through `enqueue_task`, and a manager
    /// pass checks its agent in `routines::dispatch`.
    pub async fn enqueue_chat_turn(&self, chat_id: Uuid, engine: &str) -> anyhow::Result<Uuid> {
        self.vet_chat_turn(chat_id, engine).await?;
        let agent: Option<Uuid> = sqlx::query_scalar("SELECT agent_id FROM chats WHERE id = $1")
            .bind(chat_id)
            .fetch_optional(&self.db.pool)
            .await?
            .flatten();
        crate::agents::assert_can_run(&self.db, &agent.into_iter().collect::<Vec<_>>()).await?;
        let row = sqlx::query(
            "INSERT INTO runs (chat_id, status, trigger, engine, agent_id)
             VALUES ($1, 'queued', 'chat', $2, $3) RETURNING id",
        )
        .bind(chat_id)
        .bind(engine)
        .bind(agent)
        .fetch_one(&self.db.pool)
        .await?;
        let run_id: Uuid = row.get("id");
        sqlx::query("INSERT INTO queue (run_id, priority) VALUES ($1, 20)")
            .bind(run_id)
            .execute(&self.db.pool)
            .await?;
        Ok(run_id)
    }

    /// Queue an agent's reply to an @-mention in a task comment. Priority 15:
    /// a mentioned agent should feel responsive, but the user's own chat
    /// turns (20) still come first.
    pub async fn enqueue_comment_reply(
        &self,
        comment_id: Uuid,
        agent_id: Uuid,
        engine: &str,
    ) -> anyhow::Result<Uuid> {
        crate::agents::assert_can_run(&self.db, &[agent_id]).await?;
        let task: Option<Uuid> =
            sqlx::query_scalar("SELECT task_id FROM task_comments WHERE id = $1")
                .bind(comment_id)
                .fetch_optional(&self.db.pool)
                .await?;
        let mut scope = match task {
            Some(task) => crate::budgets::scope_of_task(&self.db, task).await?,
            None => crate::budgets::Scope::default(),
        };
        scope.agent = Some(agent_id);
        crate::budgets::check(&self.db, &scope, true).await?;
        let row = sqlx::query(
            "INSERT INTO runs (comment_id, agent_id, status, trigger, engine)
             VALUES ($1, $2, 'queued', 'comment', $3) RETURNING id",
        )
        .bind(comment_id)
        .bind(agent_id)
        .bind(engine)
        .fetch_one(&self.db.pool)
        .await?;
        let run_id: Uuid = row.get("id");
        sqlx::query("INSERT INTO queue (run_id, priority) VALUES ($1, 15)")
            .bind(run_id)
            .execute(&self.db.pool)
            .await?;
        Ok(run_id)
    }

    /// One brief, several attempts, each in its own checkout.
    ///
    /// Deliberately does *not* touch the task's own worktree: until a winner
    /// is picked the card is unchanged, so a bake-off you abandon costs
    /// nothing but the tokens.
    pub async fn enqueue_bakeoff(
        &self,
        task_id: Uuid,
        variants: &[Variant],
    ) -> anyhow::Result<Vec<Uuid>> {
        if variants.len() < 2 {
            anyhow::bail!("a bake-off needs at least two variants to compare");
        }
        let card = sqlx::query("SELECT engine, agent_id FROM tasks WHERE id = $1")
            .bind(task_id)
            .fetch_one(&self.db.pool)
            .await?;
        let engine: String = card.get("engine");
        // A variant without its own agent runs as the card's.
        let card_agent: Option<Uuid> = card.get("agent_id");
        let agents: Vec<Uuid> = variants
            .iter()
            .filter_map(|v| v.agent_id.or(card_agent))
            .collect();
        crate::agents::assert_can_run(&self.db, &agents).await?;
        crate::budgets::check(
            &self.db,
            &crate::budgets::scope_of_task(&self.db, task_id).await?,
            true,
        )
        .await?;

        let mut ids = vec![];
        for variant in variants {
            let row = sqlx::query(
                "INSERT INTO runs (task_id, agent_id, tier_override, variant_label,
                                   status, trigger, engine)
                 VALUES ($1,$2,$3,$4,'queued','bakeoff',$5) RETURNING id",
            )
            .bind(task_id)
            .bind(variant.agent_id)
            .bind(variant.tier.as_deref())
            .bind(&variant.label)
            .bind(variant.engine.as_ref().unwrap_or(&engine))
            .fetch_one(&self.db.pool)
            .await?;
            let run_id: Uuid = row.get("id");
            // Below interactive work: a bake-off is exploratory, and it is
            // several runs at once against one rate limit.
            sqlx::query("INSERT INTO queue (run_id, priority) VALUES ($1, 8)")
                .bind(run_id)
                .execute(&self.db.pool)
                .await?;
            ids.push(run_id);
        }
        Ok(ids)
    }

    /// Adopt one variant's work as the task's, and throw the rest away.
    ///
    /// The winner's worktree *becomes* the card's, rather than being copied
    /// or merged: it already holds the branch, the commits, and the diff the
    /// user just read and chose.
    pub async fn keep_variant(&self, run_id: Uuid) -> anyhow::Result<()> {
        let row = sqlx::query(
            "SELECT r.task_id, r.worktree_path, r.variant_label, p.path AS project_path
             FROM runs r
             JOIN tasks t ON t.id = r.task_id
             JOIN projects p ON p.id = t.project_id
             WHERE r.id = $1 AND r.variant_label IS NOT NULL",
        )
        .bind(run_id)
        .fetch_optional(&self.db.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("that run is not a bake-off variant"))?;

        let task_id: Uuid = row.get("task_id");
        let project_path: String = row.get("project_path");
        let worktree: String = row
            .get::<Option<String>, _>("worktree_path")
            .ok_or_else(|| anyhow::anyhow!("that variant never produced a checkout"))?;

        let branch = crate::worktrees::manager::current_branch(std::path::Path::new(&worktree))
            .await
            .unwrap_or_default();

        sqlx::query(
            "UPDATE tasks SET worktree_path = $1, branch = $2, board_column = 'review'
             WHERE id = $3",
        )
        .bind(&worktree)
        .bind(&branch)
        .bind(task_id)
        .execute(&self.db.pool)
        .await?;

        // The losers' checkouts are removed, but their runs stay: what the
        // other attempts cost and produced is the record of why this one won.
        let losers: Vec<(Uuid, Option<String>)> = sqlx::query_as(
            "SELECT id, worktree_path FROM runs
             WHERE task_id = $1 AND variant_label IS NOT NULL AND id <> $2",
        )
        .bind(task_id)
        .bind(run_id)
        .fetch_all(&self.db.pool)
        .await?;
        for (loser, path) in losers {
            if let Some(path) = path {
                let repo = std::path::Path::new(&project_path);
                if let Err(e) = self
                    .worktrees
                    .discard(repo, std::path::Path::new(&path))
                    .await
                {
                    // Disk left behind is untidy, not broken.
                    tracing::warn!(%loser, error=%e, "could not remove losing variant's worktree");
                }
            }
            // Forget the path whether or not the removal worked, and this is
            // load-bearing twice over. The row pointed at a directory that is
            // gone; and `worktrees::sweep` treats every path any row names as
            // *claimed*, so a stale one made that directory permanently
            // unsweepable — the sweep would skip it forever while nothing else
            // could ever name it either.
            let _ = sqlx::query("UPDATE runs SET worktree_path = NULL WHERE id = $1")
                .bind(loser)
                .execute(&self.db.pool)
                .await;
        }
        Ok(())
    }

    /// Queue a workflow execution. `trigger` distinguishes manual runs from
    /// scheduled ones for the activity view.
    pub async fn enqueue_workflow(&self, workflow_id: Uuid, trigger: &str) -> anyhow::Result<Uuid> {
        // The workflow's own `defaults.engine` is authoritative — dispatch
        // reads it too. Recording it here means the activity list shows the
        // truth for the window between queued and running, rather than
        // whatever literal happened to be in this INSERT.
        let found = sqlx::query(
            "SELECT w.source_yaml, p.workspace_id, p.id AS project_id FROM workflows w
               JOIN projects p ON p.id = w.project_id WHERE w.id = $1",
        )
        .bind(workflow_id)
        .fetch_optional(&self.db.pool)
        .await?;
        let workflow = found
            .as_ref()
            .and_then(|r| Workflow::from_yaml(&r.get::<String, _>("source_yaml")).ok());
        let engine = workflow
            .as_ref()
            .map(|w| w.defaults.engine.clone())
            .unwrap_or_else(|| self.default_engine());
        // Every step's agent, asked now rather than at its step: a pipeline
        // that would stop at stage three is better refused at the click. Each
        // step asks again when it starts (`load_agent`), for a pause between.
        if let (Some(row), Some(workflow)) = (&found, &workflow) {
            let names: Vec<String> = workflow
                .steps
                .iter()
                .filter_map(|s| s.agent.clone())
                .collect();
            crate::agents::assert_steps_can_run(&self.db, row.get("workspace_id"), &names).await?;
        }
        let scope = crate::budgets::Scope {
            workspace: found.as_ref().map(|r| r.get("workspace_id")),
            project: found.as_ref().map(|r| r.get("project_id")),
            ..Default::default()
        };
        crate::budgets::check(&self.db, &scope, true).await?;

        let row = sqlx::query(
            "INSERT INTO runs (workflow_id, status, trigger, engine)
             VALUES ($1, 'queued', $2, $3) RETURNING id",
        )
        .bind(workflow_id)
        .bind(trigger)
        .bind(&engine)
        .fetch_one(&self.db.pool)
        .await?;
        let run_id: Uuid = row.get("id");
        // Scheduled work yields to anything a human is waiting on.
        let priority = if trigger == "schedule" { 1 } else { 10 };
        sqlx::query("INSERT INTO queue (run_id, priority) VALUES ($1, $2)")
            .bind(run_id)
            .bind(priority)
            .execute(&self.db.pool)
            .await?;
        Ok(run_id)
    }

    /// Ask a run to stop. Returns true if it was executing — a queued or
    /// parked run has no process to interrupt and is handled by the caller.
    pub fn cancel(&self, run_id: Uuid) -> bool {
        let mut cancels = self.cancels.lock().unwrap();
        let state = cancels.entry(run_id).or_default();
        state.requested = true;
        let live = std::mem::take(&mut state.steps);
        let was_running = !live.is_empty();
        for (_, tx) in live {
            let _ = tx.send(());
        }
        was_running
    }

    /// Has someone asked this run to stop? Checked between steps.
    pub(crate) fn cancel_requested(&self, run_id: Uuid) -> bool {
        self.cancels
            .lock()
            .unwrap()
            .get(&run_id)
            .is_some_and(|c| c.requested)
    }

    /// Drop cancellation bookkeeping once a run reaches a terminal state, so
    /// the map doesn't grow for the life of the process.
    pub(crate) fn forget_cancel(&self, run_id: Uuid) {
        self.cancels.lock().unwrap().remove(&run_id);
    }

    /// On boot: anything left in starting/running died with the previous
    /// process. Mark failed; the UI offers one-click resume via session_id.
    pub async fn recover_orphans(&self) -> anyhow::Result<u64> {
        let mut tx = self.db.pool.begin().await?;
        // A run that was waiting on a person already recorded what it was
        // waiting for, and "orphaned by server restart" would throw that away.
        // The question itself is in `permission_requests`; it is marked
        // expired below, which is what the inbox offers to resume from.
        let orphans: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE runs SET status='failed',
                    error_reason = CASE WHEN status='waiting_permission'
                        THEN 'the server restarted while this run was '
                             || COALESCE(error_reason, 'waiting for you')
                        ELSE 'orphaned by server restart' END,
                    finished_at=now()
             WHERE status IN ('starting','running','waiting_permission')
             RETURNING id",
        )
        .fetch_all(&mut *tx)
        .await?;
        // The steps died with the run. Leaving them at 'running' is what makes
        // a failed team run still animate a teammate as "working…".
        settle_steps(&mut tx, &orphans, RunStatus::Failed).await?;
        // The questions those runs were holding will never be answered now.
        sqlx::query(
            "UPDATE permission_requests SET resolved_at = now(), decision = 'expired'
              WHERE resolved_at IS NULL",
        )
        .execute(&mut *tx)
        .await?;
        // A proposal claimed for approval whose effect never reported back —
        // the server went down between the two — goes back to the inbox
        // rather than reading as approved with nothing done.
        sqlx::query(
            "UPDATE decisions SET status = 'open', decided_at = NULL
              WHERE status = 'approved' AND outcome IS NULL",
        )
        .execute(&mut *tx)
        .await?;
        // Nothing terminal keeps a place in the queue. This is the sweep for
        // rows written before `finish` learned to delete them — without it,
        // every failure this repository has already recorded stays claimable
        // and gets re-dispatched on the next boot.
        sqlx::query(
            "DELETE FROM queue q USING runs r
              WHERE r.id = q.run_id AND r.status IN ('completed','failed','canceled')",
        )
        .execute(&mut *tx)
        .await?;
        // And the mirror image: a run that says it is held has to actually be
        // waiting for something. A crash between `hold_rate_limited`'s two
        // statements, or a queue row deleted by hand, leaves `rate_limited`
        // with nothing to bring it back — a run that is neither running nor
        // finished and never will be. Behind the same backoff it would have
        // had, so a restart loop cannot become a retry storm.
        sqlx::query(
            "INSERT INTO queue (run_id, priority, not_before)
             SELECT r.id, 5, now() + interval '5 minutes' FROM runs r
              WHERE r.status = 'rate_limited'
                AND NOT EXISTS (SELECT 1 FROM queue q WHERE q.run_id = r.id)
             ON CONFLICT (run_id) DO NOTHING",
        )
        .execute(&mut *tx)
        .await?;
        // The same promise for runs that never started: `queued` means
        // something will dispatch it. Every path that writes `queued` also
        // writes the queue row, but not always in one transaction — a plan
        // approval, a team start — and a crash between the two left a card
        // that read "queued" forever.
        sqlx::query(
            "INSERT INTO queue (run_id, priority)
             SELECT r.id, 10 FROM runs r
              WHERE r.status = 'queued'
                AND NOT EXISTS (SELECT 1 FROM queue q WHERE q.run_id = r.id)
             ON CONFLICT (run_id) DO NOTHING",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        // And so do the cards those steps stand for — otherwise a restart leaves
        // a column of sub-tickets stuck at "In Progress" with nothing behind
        // them, which is the same lie `settle_steps` exists to prevent.
        for run_id in &orphans {
            if let Err(e) = crate::runs::org::epic::mirror_run(&self.db, *run_id).await {
                tracing::warn!(%run_id, error=%e, "could not update this run's sub-task cards");
            }
        }
        Ok(orphans.len() as u64)
    }

    /// Queue loop: claim ready runs under the concurrency semaphore.
    pub async fn run_loop(self: Arc<Self>) {
        loop {
            let permit = self.slots.sem().acquire_owned().await.expect("semaphore");
            // A run that parked on a permission prompt lent this slot to the
            // queue and has since taken it back. Pay that debt here rather than
            // where the person clicked Allow: their engine is holding an HTTP
            // request open, and making the click wait on someone else's
            // twenty-minute run is how the call they just approved times out.
            if self.slots.take_debt() {
                permit.forget();
                continue;
            }
            match self.claim_next().await {
                Ok(Some(run_id)) => {
                    let this = self.clone();
                    tokio::spawn(async move {
                        let _alive = this.alive(run_id);
                        if let Err(e) = this.execute(run_id).await {
                            tracing::error!(%run_id, error=%e, "run execution error");
                            let _ = this
                                .finish(run_id, RunStatus::Failed, Some(e.to_string()))
                                .await;
                        }
                        drop(permit);
                    });
                }
                Ok(None) => {
                    drop(permit);
                    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                }
                Err(e) => {
                    drop(permit);
                    tracing::error!(error=%e, "queue claim failed");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// Would this card's engine refuse the mode the card would run in? The
    /// card's own answer to [`Self::vet_engine`]: the bound agent's engine and
    /// preset win over the card's, and a card with no mode takes the
    /// machine's default — exactly what the run would get.
    pub async fn vet_card(&self, task_id: Uuid) -> anyhow::Result<Option<String>> {
        let row = sqlx::query(
            "SELECT COALESCE(a.engine, t.engine) AS engine,
                    COALESCE(a.permission_preset, t.permission_mode) AS mode,
                    p.full_auto_opt_in, p.vcs
             FROM tasks t JOIN projects p ON p.id = t.project_id
             LEFT JOIN agents a ON a.id = t.agent_id WHERE t.id = $1",
        )
        .bind(task_id)
        .fetch_one(&self.db.pool)
        .await?;
        let mut mode = match row.get::<Option<String>, _>("mode") {
            Some(m) => serde_json::from_value(serde_json::Value::String(m)).unwrap_or_default(),
            None => self.default_permission_mode().await,
        };
        let engine_id: String = row.get("engine");
        // The Full Auto gate dispatch will apply, said at the click: where it
        // will not hold, the card runs as the narrowest mode its engine has —
        // or, for an engine with none, cannot start at all.
        if mode == PermissionMode::FullAuto {
            // In place (no git) means no worktree, which Full Auto needs.
            let gated =
                !row.get::<bool, _>("full_auto_opt_in") || row.get::<String, _>("vcs") != "git";
            if let (true, Some(engine)) = (gated, self.engine(&engine_id)) {
                match short_of_full_auto(&engine.capabilities()) {
                    Some(narrower) => mode = narrower,
                    None => {
                        return Ok(Some(format!(
                            "{} can only run with every tool allowed, and Full Auto is off for \
this project (it needs the project's opt-in and a worktree to work in). Turn it on for the \
project, or run this card on an engine with a narrower mode.",
                            engine.label()
                        )))
                    }
                }
            }
        }
        Ok(self.vet_engine(&engine_id, mode))
    }

    /// Start a card the way the Start button does — vetted, queued, moved to
    /// In Progress — for a start nobody clicked.
    pub async fn start_card(&self, task_id: Uuid) -> anyhow::Result<Uuid> {
        if let Some(reason) = self.vet_card(task_id).await? {
            anyhow::bail!(reason);
        }
        let run_id = self.enqueue_task(task_id).await?;
        sqlx::query("UPDATE tasks SET board_column='running' WHERE id=$1")
            .bind(task_id)
            .execute(&self.db.pool)
            .await?;
        Ok(run_id)
    }

    /// Would this engine refuse this mode? Checked before a run is queued so
    /// the answer lands where the user is standing, not forty minutes later.
    ///
    /// Returns the reason, or `None` when the pairing is fine. An unknown
    /// engine is *not* an error here — dispatch reports that with a better
    /// message than "capability check failed".
    pub fn vet_engine(&self, engine_id: &str, mode: PermissionMode) -> Option<String> {
        let engine = self.engine(engine_id)?;
        eren_engines::vet(engine.as_ref(), mode, false).err()
    }

    /// Which model this engine runs at this tier right now.
    ///
    /// Takes the engine because "medium" cannot mean one model globally —
    /// `claude-opus-5` is not something OpenCode can be asked for.
    pub fn model_for(&self, engine: &str, tier: ModelTier) -> String {
        self.tiers.read().unwrap().model_for(engine, tier)
    }

    /// A snapshot, for callers that need the whole mapping. Cloned rather
    /// than lent, so no lock guard is ever held across an await.
    pub fn tier_mapping(&self) -> EngineTierMapping {
        self.tiers.read().unwrap().clone()
    }

    /// Load the user's routing from settings. Called once at boot; anything
    /// missing or unparseable falls back to the built-in mapping rather than
    /// leaving runs with no model at all.
    pub async fn load_tier_mapping(&self) -> anyhow::Result<()> {
        let stored: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'tier_models'")
                .fetch_optional(&self.db.pool)
                .await?;
        // Which engines the user has actually configured, read off the raw
        // JSON rather than the parsed struct: parsing fills in defaults for
        // every engine, which would make "they chose this" indistinguishable
        // from "we invented it".
        let explicit: std::collections::BTreeSet<String> = stored
            .as_ref()
            .and_then(|v| v.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let mut mapping = stored
            .and_then(|v| serde_json::from_value::<EngineTierMapping>(v).ok())
            .unwrap_or_default();

        // For an engine nobody has configured, the built-in default is a
        // guess — and for a multi-provider engine it's usually a wrong one.
        // The install itself knows better, so ask it.
        for engine in self.engines() {
            // An engine with a catalog of its own and defaults named from it
            // (Gemini's aliases, Amp's modes) already has the right answer:
            // those names match none of the keywords below, and deriving
            // from them put every tier on one model.
            if engine.capabilities().fixed_model_catalog
                && !EngineTierMapping::defaults_for(engine.id()).0.is_empty()
            {
                continue;
            }
            let Some(info) = self.detected.get(engine.id()) else {
                continue;
            };
            let Some(picked) = eren_shared::pick_defaults(&info.models) else {
                continue;
            };
            tracing::info!(
                engine = engine.id(),
                medium = %picked.model_for(ModelTier::Medium),
                "tier defaults derived from the models this install can reach"
            );
            // Recorded for every engine, applied only to the ones nobody has
            // configured. Deriving it unconditionally is what lets the
            // settings page say what Reset goes back to without undoing the
            // user's own choice to get there.
            self.derived
                .write()
                .unwrap()
                .0
                .insert(engine.id().to_string(), picked.clone());
            if !explicit.contains(engine.id()) {
                mapping.0.insert(engine.id().to_string(), picked);
            }
        }
        *self.tiers.write().unwrap() = mapping;
        Ok(())
    }

    /// What this engine routes to when the user has set nothing.
    ///
    /// `None` for an engine whose install said nothing about its models, in
    /// which case [`EngineTierMapping::defaults_for`] is the only answer
    /// there is.
    pub fn derived_defaults(&self, engine: &str) -> Option<TierMapping> {
        self.derived.read().unwrap().0.get(engine).cloned()
    }

    /// How hard each tier thinks, per engine. A snapshot, for the same reason
    /// `tier_mapping` returns one.
    pub fn tier_efforts(&self) -> EngineTierEffort {
        self.tier_efforts.read().unwrap().clone()
    }

    /// Unlike the model mapping there is nothing to invent when this is
    /// missing: no entry means inherit, which is a real answer.
    pub async fn load_tier_efforts(&self) -> anyhow::Result<()> {
        let stored: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'tier_efforts'")
                .fetch_optional(&self.db.pool)
                .await?;
        let map = stored
            .and_then(|v| serde_json::from_value::<EngineTierEffort>(v).ok())
            .unwrap_or_default();
        *self.tier_efforts.write().unwrap() = map;
        Ok(())
    }

    pub async fn set_tier_efforts(&self, efforts: EngineTierEffort) -> anyhow::Result<()> {
        self.put_setting("tier_efforts", serde_json::to_value(&efforts)?)
            .await?;
        *self.tier_efforts.write().unwrap() = efforts;
        Ok(())
    }

    /// The budget a run should actually think with.
    ///
    /// Four places can have an opinion and the most specific wins: whatever was
    /// pinned on the bound agent or the card, then what this engine's tier
    /// says, then the machine default, then the CLI's own. Every step is
    /// resolved when the run starts rather than frozen at create time, so
    /// changing any of them reaches work already sitting in the backlog.
    ///
    /// One function rather than three copies, because the three call sites —
    /// a card, a chat turn, a teammate's reply — drifted apart the last time
    /// they were written out separately.
    pub async fn resolve_effort(
        &self,
        agent: Option<ReasoningEffort>,
        card: Option<ReasoningEffort>,
        engine: &str,
        tier: ModelTier,
    ) -> Option<ReasoningEffort> {
        // Bound out of the expression so no lock guard is alive across the
        // await below.
        let from_tier = self.tier_efforts.read().unwrap().effort_for(engine, tier);
        eren_shared::resolve_effort(agent, card, from_tier, self.default_effort().await).0
    }

    /// Persist and apply a new routing.
    pub async fn set_tier_mapping(&self, mapping: EngineTierMapping) -> anyhow::Result<()> {
        for (engine, tiers) in &mapping.0 {
            for (tier, model) in &tiers.0 {
                if !eren_shared::is_known_model_for(engine, model) {
                    anyhow::bail!("{model} is not a model {engine} can run (tier {tier:?})");
                }
            }
        }
        self.put_setting("tier_models", serde_json::to_value(&mapping)?)
            .await?;
        *self.tiers.write().unwrap() = mapping;
        Ok(())
    }

    /// Is the queue paused? Read per tick rather than cached, so the pause
    /// takes effect on the next claim instead of whenever a process happens
    /// to restart.
    pub async fn queue_paused(&self) -> bool {
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT value FROM settings WHERE key = 'queue_paused'",
        )
        .fetch_optional(&self.db.pool)
        .await
        // Never fail closed on a read error — a database hiccup must not
        // silently stop every run on the machine.
        .unwrap_or_else(|e| {
            tracing::error!(error=%e, "queue pause read failed; dispatching anyway");
            None
        })
        .and_then(|v| serde_json::from_value::<bool>(v).ok())
        .unwrap_or(false)
    }

    /// Why the queue is or isn't handing out work.
    ///
    /// One answer for both reasons it can stop, because the UI has to tell
    /// them apart: a pause you chose is resumed with a click, while a spent
    /// budget clears on its own when its window turns and a resume button
    /// would be a lie. Only machine-wide budgets stop the whole queue; a
    /// narrower one holds just the runs it covers (`claim_next`).
    pub async fn queue_gate(&self) -> QueueGate {
        if self.queue_paused().await {
            return QueueGate::Paused;
        }
        match crate::budgets::machine_gate(&self.db).await {
            Some(over) => QueueGate::OverBudget(over),
            None => QueueGate::Open,
        }
    }

    /// Stop or resume dispatching. Runs already executing are left alone —
    /// pausing is about not spending more, not about throwing away work in
    /// progress.
    pub async fn set_queue_paused(&self, paused: bool) -> anyhow::Result<()> {
        self.put_setting("queue_paused", serde_json::json!(paused))
            .await
    }

    /// How much freedom a new task gets when the caller doesn't say.
    ///
    /// Stored rather than hard-coded because "ask me before every command"
    /// and "just get on with it" are both legitimate, and which one you want
    /// is a property of how much you trust the isolation — not of the code.
    pub async fn default_permission_mode(&self) -> PermissionMode {
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT value FROM settings WHERE key = 'default_permission_mode'",
        )
        .fetch_optional(&self.db.pool)
        .await
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value::<PermissionMode>(v).ok())
        .unwrap_or_default()
    }

    /// How hard the model thinks, when nothing more specific says.
    ///
    /// `None` is a real answer and the default one: it leaves each CLI on its
    /// own built-in behaviour rather than Eren picking a number on the user's
    /// behalf. A stored value is an explicit choice to override that.
    pub async fn default_effort(&self) -> Option<ReasoningEffort> {
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT value FROM settings WHERE key = 'default_effort'",
        )
        .fetch_optional(&self.db.pool)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().and_then(ReasoningEffort::parse))
    }

    /// `None` clears it, returning every run to its CLI's own default — the
    /// same shape as removing the budget cap rather than setting it to zero.
    pub async fn set_default_effort(&self, effort: Option<ReasoningEffort>) -> anyhow::Result<()> {
        match effort {
            Some(e) => {
                self.put_setting("default_effort", serde_json::json!(e.as_str()))
                    .await
            }
            None => {
                sqlx::query("DELETE FROM settings WHERE key = 'default_effort'")
                    .execute(&self.db.pool)
                    .await?;
                Ok(())
            }
        }
    }

    pub async fn set_default_permission_mode(&self, mode: PermissionMode) -> anyhow::Result<()> {
        self.put_setting("default_permission_mode", serde_json::to_value(mode)?)
            .await
    }

    /// The machine's daily dollar cap — the one budget there used to be, now
    /// the "Daily budget" policy. Kept for the cap control on the activity
    /// page and anything else that still asks the old question.
    pub async fn daily_budget(&self) -> Option<f64> {
        crate::budgets::daily_cap(&self.db).await
    }

    /// `None` removes the cap. A zero or negative cap would mean "never run
    /// anything", which is what the pause is for, so it is stored as no cap.
    pub async fn set_daily_budget(&self, cap: Option<f64>) -> anyhow::Result<()> {
        crate::budgets::set_daily_cap(&self.db, cap.filter(|c| *c > 0.0)).await
    }

    async fn put_setting(&self, key: &str, value: serde_json::Value) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.db.pool)
        .await?;
        Ok(())
    }

    async fn claim_next(&self) -> anyhow::Result<Option<Uuid>> {
        if self.queue_paused().await {
            return Ok(None);
        }
        // No budgets and no agent limits: claiming is exactly what it was
        // before either existed.
        if !crate::budgets::any_enabled(&self.db).await
            && !crate::agents::any_limits(&self.db).await
        {
            return self.claim_head().await;
        }
        // A spent budget holds the runs it covers until its window turns —
        // and the next run in line is asked instead, so one spent project
        // does not stop every other. A machine-wide one covers every run, so
        // it holds them all, as the daily cap always did; it is asked per
        // run rather than once up front because a run coming back from a
        // rate limit was counted when it first started and must not be held
        // by its own count. Bounded, so a queue full of held runs costs a
        // few reads per tick, not one per row.
        for _ in 0..8 {
            let mut tx = self.db.pool.begin().await?;
            let Some(candidate) = sqlx::query(
                "SELECT q.run_id, r.started_at IS NOT NULL AS counted
                   FROM queue q JOIN runs r ON r.id = q.run_id
                  WHERE q.not_before IS NULL OR q.not_before <= now()
                  ORDER BY q.priority DESC, q.enqueued_at ASC
                  FOR UPDATE OF q SKIP LOCKED LIMIT 1",
            )
            .fetch_optional(&mut *tx)
            .await?
            else {
                return Ok(None);
            };
            let run_id: Uuid = candidate.get("run_id");
            // Started once already — a rate-limit hold coming back — and so
            // already counted against any run cap.
            let counted: bool = candidate.get("counted");
            let scope = crate::budgets::scope_of_run(&self.db, run_id)
                .await
                .unwrap_or_default();
            // An agent's own limits: how many at once, how many a day, how
            // long to rest between. Over one, the run waits its turn.
            let limited = crate::agents::hold_for_limits(&self.db, run_id)
                .await
                .unwrap_or_else(|e| {
                    tracing::error!(%run_id, error = %e, "agent limit read failed; letting the run through");
                    None
                });
            if let Some((until, why)) = limited {
                sqlx::query(
                    "UPDATE queue SET not_before = $2, hold_reason = $3, held_by = NULL
                      WHERE run_id = $1",
                )
                .bind(run_id)
                .bind(until)
                .bind(&why)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                continue;
            }
            match crate::budgets::check_claim(&self.db, &scope, true, counted).await {
                Ok(()) => {
                    sqlx::query("DELETE FROM queue WHERE run_id = $1")
                        .bind(run_id)
                        .execute(&mut *tx)
                        .await?;
                    // Counted from this moment, not from when its process
                    // starts a few seconds on: the next claim, a moment away,
                    // must already see it against a run cap or a daily limit.
                    sqlx::query(
                        "UPDATE runs SET started_at = COALESCE(started_at, now())
                          WHERE id = $1 AND status = 'queued'",
                    )
                    .bind(run_id)
                    .execute(&mut *tx)
                    .await?;
                    tx.commit().await?;
                    return Ok(Some(run_id));
                }
                Err(over) => {
                    sqlx::query(
                        "UPDATE queue SET not_before = $2, hold_reason = $3, held_by = $4
                          WHERE run_id = $1",
                    )
                    .bind(run_id)
                    .bind(over.resets_at)
                    .bind(over.to_string())
                    .bind(over.policy_id)
                    .execute(&mut *tx)
                    .await?;
                    tx.commit().await?;
                    tracing::info!(%run_id, policy = %over.policy, "held by a budget");
                }
            }
        }
        Ok(None)
    }

    /// Between the steps of a run that is already going: may it start more?
    pub(crate) async fn budget_allows_more(
        &self,
        run_id: Uuid,
    ) -> Result<(), crate::budgets::OverBudget> {
        let scope = crate::budgets::scope_of_run(&self.db, run_id)
            .await
            .unwrap_or_default();
        // The run asking was counted when it was claimed; a run cap it fills
        // exactly must not stop it halfway through its own work.
        crate::budgets::check_claim(&self.db, &scope, true, true).await
    }

    /// The next run in line, with nothing to vet. Stamped started as it is
    /// claimed, like a vetted claim: a team run never stamps itself, and a
    /// budget made later in the window must still see it. Only a run still
    /// queued — a row left behind by a canceled one must not count.
    async fn claim_head(&self) -> anyhow::Result<Option<Uuid>> {
        let row = sqlx::query(
            "WITH claimed AS (
                 DELETE FROM queue WHERE run_id = (
                     SELECT run_id FROM queue
                     WHERE not_before IS NULL OR not_before <= now()
                     ORDER BY priority DESC, enqueued_at ASC
                     FOR UPDATE SKIP LOCKED LIMIT 1
                 ) RETURNING run_id
             )
             , stamped AS (
                 UPDATE runs SET started_at = COALESCE(started_at, now())
                  WHERE id IN (SELECT run_id FROM claimed) AND status = 'queued'
             )
             SELECT run_id FROM claimed",
        )
        .fetch_optional(&self.db.pool)
        .await?;
        Ok(row.map(|r| r.get("run_id")))
    }

    /// Dispatcher: task runs and chat runs share the streaming machinery but
    /// build their RunSpec differently.
    async fn execute(self: &Arc<Self>, run_id: Uuid) -> anyhow::Result<()> {
        // The guard this never had, and then the race in the guard. `execute`
        // reads only the columns that decide *which kind* of run it is, so a
        // queue row that outlived its run — cancelled, already finished, or
        // claimed twice — dispatched a second engine against it and was
        // charged for. A plain read fixed that, but left a window: a cancel
        // landing between the read and the first status write was written,
        // then overwritten with `starting` — and the CLI was spawned only for
        // the in-memory cancel flag to kill it.
        //
        // So the check *is* the write: claimed in one statement, and a run no
        // longer waiting to start is dropped on the floor here, which is the
        // correct response to a row that should not exist.
        let Some(row) = sqlx::query(
            "UPDATE runs SET status='starting'
              WHERE id=$1 AND status IN ('queued','rate_limited')
              RETURNING task_id, chat_id, workflow_id, team_id, comment_id, kb_brief, research_id",
        )
        .bind(run_id)
        .fetch_optional(&self.db.pool)
        .await?
        else {
            tracing::warn!(%run_id, "dropped a queued run that is no longer waiting");
            return Ok(());
        };
        // Asked again here, not only when the run was queued: a run can wait
        // a long time — behind the concurrency cap, a rate limit, a plan
        // nobody had approved yet — and an agent paused meanwhile has said
        // "start nothing". It ends with the reason, where a person will see
        // it, rather than quietly running under a paused agent.
        if let Err(e) = crate::agents::assert_may_dispatch(&self.db, run_id).await {
            if e.is::<crate::agents::Unavailable>() {
                return self
                    .finish(run_id, RunStatus::Failed, Some(e.to_string()))
                    .await;
            }
            return Err(e);
        }
        match (
            row.get::<Option<Uuid>, _>("chat_id"),
            row.get::<Option<Uuid>, _>("workflow_id"),
            row.get::<Option<Uuid>, _>("team_id"),
            row.get::<Option<Uuid>, _>("comment_id"),
        ) {
            (Some(chat_id), _, _, _) => self.execute_chat_run(run_id, chat_id).await,
            (_, Some(workflow_id), _, _) => self.execute_workflow_run(run_id, workflow_id).await,
            (_, _, Some(team_id), _) => self.execute_org_run(run_id, team_id).await,
            // A comment *reply* is the only run with a comment and no card of
            // its own. A run that has a card is work on that card — rows written
            // before 0071 carried both, and this arm used to claim them and fail.
            (_, _, _, Some(comment_id)) if row.get::<Option<Uuid>, _>("task_id").is_none() => {
                self.execute_comment_run(run_id, comment_id).await
            }
            // Before the kb arm: the two columns are disjoint today, but if
            // any future path ever stamps KB columns onto a research run, the
            // more specific kind must win rather than lean on an invariant
            // nothing enforces.
            _ if row.get::<Option<Uuid>, _>("research_id").is_some() => {
                self.execute_research_run(run_id).await
            }
            _ if row.get::<Option<String>, _>("kb_brief").is_some() => {
                self.execute_kb_run(run_id).await
            }
            _ => self.execute_task_run(run_id).await,
        }
    }

    async fn execute_task_run(self: &Arc<Self>, run_id: Uuid) -> anyhow::Result<()> {
        let run = sqlx::query(
            // A bake-off variant overrides the card: its own agent, its own
            // tier, its own worktree. COALESCE keeps every ordinary run on
            // exactly the path it was on before.
            "SELECT r.id, r.engine, r.session_id, r.session_engine, t.id AS task_id,
                    COALESCE(r.prompt_override, t.prompt) AS prompt,
                    t.model_tier, r.tier_override,
                    COALESCE(r.agent_id, t.agent_id) AS agent_id,
                    r.variant_label, r.worktree_path AS run_worktree, r.review_comment_id, r.trigger,
                    t.permission_mode, t.effort AS task_effort,
                    t.title, t.worktree_path, t.branch, t.chat_id AS task_chat_id,
                    r.plan_approval, r.plan_approved_at,
                    p.id AS project_id, p.path AS project_path, p.default_branch, p.full_auto_opt_in,
                    p.vcs, p.kind AS project_kind,
                    a.system_prompt AS agent_prompt, a.model_tier AS agent_tier,
                    a.effort AS agent_effort,
                    a.allowed_tools AS agent_tools, a.permission_preset AS agent_preset,
                    a.name AS agent_name, t.skill_id
             FROM runs r
             JOIN tasks t ON t.id = r.task_id
             JOIN projects p ON p.id = t.project_id
             LEFT JOIN agents a ON a.id = COALESCE(r.agent_id, t.agent_id)
             WHERE r.id = $1",
        )
        .bind(run_id)
        .fetch_one(&self.db.pool)
        .await?;

        self.set_status(run_id, RunStatus::Starting).await?;

        let engine_id: String = run.get("engine");
        // A session id is only meaningful to the engine that minted it.
        // Handing an OpenCode `ses_…` to Claude doesn't error — it starts
        // over with none of the context, which is the quiet kind of wrong.
        let resume_session_id: Option<String> =
            run.get::<Option<String>, _>("session_id").filter(|_| {
                run.get::<Option<String>, _>("session_engine").as_deref() == Some(&engine_id)
            });
        let engine = self
            .engine(&engine_id)
            .ok_or_else(|| anyhow::anyhow!("unknown engine {engine_id}"))?;

        // Worktree: reuse the task's if it exists, else create one.
        let task_id: Uuid = run.get("task_id");
        let project_path = PathBuf::from(run.get::<String, _>("project_path"));
        let default_branch: String = run.get("default_branch");
        // A project without version control has nowhere to branch from, so the
        // run happens in the folder itself. That trades away isolation and the
        // review step; `settle` below sends the task straight to done because
        // there is no diff to look at.
        let in_place = run.get::<String, _>("vcs") != "git";
        // A bake-off variant gets its own checkout, keyed by run rather than
        // task — the whole point is that the attempts don't see each other,
        // and sharing the task's worktree would have them overwrite one
        // another's answer.
        let variant: Option<String> = run.get("variant_label");
        let existing = match &variant {
            Some(_) => run.get::<Option<String>, _>("run_worktree"),
            None => run.get::<Option<String>, _>("worktree_path"),
        };
        let cwd = match existing {
            Some(p) => PathBuf::from(p),
            None if in_place => project_path.clone(),
            None => {
                let title: String = run.get("title");
                let (key, slug) = match &variant {
                    Some(label) => (run_id, format!("{}-{}", slugify(&title), slugify(label))),
                    None => (task_id, slugify(&title)),
                };
                let wt = self
                    .worktrees
                    .create(&project_path, &default_branch, key, &slug)
                    .await?;
                if variant.is_some() {
                    sqlx::query("UPDATE runs SET worktree_path=$1 WHERE id=$2")
                        .bind(wt.path.to_string_lossy().as_ref())
                        .bind(run_id)
                        .execute(&self.db.pool)
                        .await?;
                } else {
                    sqlx::query("UPDATE tasks SET worktree_path=$1, branch=$2 WHERE id=$3")
                        .bind(wt.path.to_string_lossy().as_ref())
                        .bind(&wt.branch)
                        .bind(task_id)
                        .execute(&self.db.pool)
                        .await?;
                }
                wt.path
            }
        };

        // Agent binding: a bound agent overrides tier, system prompt,
        // allowed tools, and permission preset.
        let agent_prompt: Option<String> = run.get("agent_prompt");
        let agent_tier: Option<String> = run.get("agent_tier");
        // Resolved below, once the tier is known — see `resolve_effort` for
        // the order these two sit in and what they fall through to.
        let agent_effort = run
            .get::<Option<String>, _>("agent_effort")
            .and_then(|e| ReasoningEffort::parse(&e));
        let card_effort = run
            .get::<Option<String>, _>("task_effort")
            .and_then(|e| ReasoningEffort::parse(&e));
        let agent_tools: Option<Vec<String>> = run.get("agent_tools");
        let agent_preset: Option<String> = run.get("agent_preset");

        // Normally a bound agent's tier wins over the card's. A bake-off
        // variant inverts that — comparing tiers means the variant's tier has
        // to beat the agent's, or every variant would run the same model.
        //
        // Resolved further down rather than here, because `auto` needs to know
        // which pass this is: writing a plan and carrying out an approved one
        // are different jobs and should not draw the same model.
        let (tier_str, tier_source) = match run.get::<Option<String>, _>("tier_override") {
            Some(explicit) => (explicit, "variant"),
            None => match agent_tier {
                Some(t) => (t, "agent"),
                None => (run.get("model_tier"), "card"),
            },
        };
        // Most specific wins: the agent's own preset, else the card's, else
        // the workspace default. Both of the first two are nullable and mean
        // "inherit" when unset — resolved here rather than frozen at create
        // time, so changing the default reaches work already in the backlog.
        let permission_mode = match agent_preset
            .or_else(|| run.get::<Option<String>, _>("permission_mode"))
            .and_then(|m| {
                serde_json::from_value::<PermissionMode>(serde_json::Value::String(m)).ok()
            }) {
            Some(explicit) => explicit,
            None => self.default_permission_mode().await,
        };

        // Plan-first: write the plan, park, and let a person approve or
        // rewrite it before anything changes. Both halves are ordinary runs of
        // this same function — the phase decides what gets asked for.
        let task_prompt: String = run.get("prompt");
        let stored_plan = self.stored_plan(run_id).await?;
        let phase = task_plan::decide(
            run.get("plan_approval"),
            match stored_plan.as_deref() {
                Some(text) => task_plan::PlanStep::Written(text),
                None => task_plan::PlanStep::Missing,
            },
            run.get::<Option<chrono::DateTime<chrono::Utc>>, _>("plan_approved_at")
                .is_some(),
        );
        let planning = phase == task_plan::Phase::Plan;

        // Now the phase is known, settle the tier. `auto` means Eren picks;
        // anything else is a person's choice and is left exactly alone.
        //
        // The decision is recorded on the run below, before the process
        // starts. A router that changed which model ran your work without
        // saying so would be the same silent downgrade this codebase refuses
        // elsewhere — the reason has to survive to the card.
        let (tier, tier_source, tier_rule, tier_reason) = match TierChoice::parse(&tier_str)
            .unwrap_or(TierChoice::Medium)
        {
            TierChoice::Auto => {
                let signals = self.tier_signals(run_id, task_id, &run).await;
                let phase = match (&phase, planning) {
                    (_, true) => eren_shared::TierPhase::Plan,
                    (task_plan::Phase::Work { plan: Some(_) }, _) => eren_shared::TierPhase::Work,
                    _ => eren_shared::TierPhase::Single,
                };
                let d = eren_shared::classify_tier(&signals, phase);
                tracing::info!(%run_id, tier = ?d.tier, rule = d.rule, "auto tier");
                (d.tier, "auto", Some(d.rule.to_string()), Some(d.because))
            }
            fixed => (
                // `unwrap_or_default` is safe here only because `Auto` is
                // handled above: `fixed()` is `None` for that case alone.
                fixed.fixed().unwrap_or_default(),
                tier_source,
                None,
                None,
            ),
        };
        let effort = self
            .resolve_effort(agent_effort, card_effort, &engine_id, tier)
            .await;

        // FullAuto is refused outside eren-managed worktrees and outside
        // opted-in projects — the structural safety gate. The run steps down
        // to the narrowest mode its engine can actually honour; one with
        // nothing narrower (Amp, Cursor) keeps FullAuto here only so that
        // the vet below refuses it with the reason.
        let full_auto_opt_in: bool = run.get("full_auto_opt_in");
        let permission_mode = if permission_mode == PermissionMode::FullAuto
            && !(full_auto_opt_in && self.worktrees.manages(&cwd))
        {
            match short_of_full_auto(&engine.capabilities()) {
                Some(mode) => {
                    tracing::warn!(%run_id, ?mode, "stepping FullAuto down (gate not satisfied)");
                    mode
                }
                None => {
                    let reason = format!(
                        "{} can only run with every tool allowed, and this project does not allow \
that here — Full Auto needs the project's opt-in and a worktree to work in. Turn it on for the \
project, or run this card on an engine with a narrower mode.",
                        engine.label()
                    );
                    let seq = next_seq(&self.db, run_id).await?;
                    self.persist_and_publish(
                        run_id,
                        None,
                        seq,
                        &ErenEvent::RunFailed {
                            reason: reason.clone(),
                        },
                    )
                    .await?;
                    self.finish(run_id, RunStatus::Failed, Some(reason)).await?;
                    return Ok(());
                }
            }
        } else {
            permission_mode
        };

        // Capability gate. Checked here as well as at enqueue time because a
        // card's mode can be edited after it was queued — and a run that
        // can't honour its mode must fail loudly rather than quietly running
        // with more freedom than was asked for. On the mode the run will
        // actually have, after the gate above: Amp cannot be held to
        // Reviewed and runs every tool regardless, so a Full Auto it was
        // refused has to stop it, not hand it Reviewed and let it run.
        if let Err(reason) = eren_engines::vet(
            engine.as_ref(),
            permission_mode,
            resume_session_id.is_some(),
        ) {
            // Persist the reason as an event, not just a status, so it shows
            // up in the run's transcript where the user is already looking.
            let seq = next_seq(&self.db, run_id).await?;
            self.persist_and_publish(
                run_id,
                None,
                seq,
                &ErenEvent::RunFailed {
                    reason: reason.clone(),
                },
            )
            .await?;
            self.finish(run_id, RunStatus::Failed, Some(reason)).await?;
            return Ok(());
        }

        // Servers this agent opted into. Loaded before the config is written
        // because they go into the same file as Eren's own endpoint.
        let bound_agent: Option<Uuid> = run.get("agent_id");
        let user_servers = crate::mcp_servers::for_agent(&self.db, bound_agent)
            .await
            .unwrap_or_else(|e| {
                // A broken server row must not take the whole run down; the
                // agent simply runs without the extra capability.
                tracing::warn!(%run_id, error=%e, "could not load agent MCP servers");
                vec![]
            });

        // No engine-id check: every engine that can do MCP now gets the same
        // wiring, and each adapter renders it. The old `"claude-code"` match
        // silently handed any other engine an empty config.
        let mcp = McpWiring {
            eren_url: self
                .mcp_base_url
                .as_ref()
                .map(|b| format!("{b}/mcp/run/{run_id}")),
            servers: user_servers.iter().map(|s| s.to_spec()).collect(),
        };

        // An empty allow-list means "no --allowedTools flag", which allows
        // everything — so only extend a list that already exists. Appending
        // to an empty one would silently narrow the run to MCP tools alone.
        let mut allowed_tools = agent_tools.unwrap_or_default();
        if !allowed_tools.is_empty() {
            allowed_tools.extend(user_servers.iter().map(|s| s.tool_prefix()));
            // Eren's own run toolbox — comment, report_blocker, look-ups.
            allowed_tools.push("mcp__eren".to_string());
        }

        let model_id = self.model_for(&engine_id, tier);
        // The tier lands on the row in the same write as the model, so the
        // choice is durable *before* the process starts rather than inferred
        // afterwards. `model` alone could not answer it: two tiers can map to
        // one model and the mapping is editable, so a model id read back next
        // week cannot say which tier asked for it, or who decided.
        sqlx::query(
            "UPDATE runs SET model=$1, started_at=now(),
                             tier_resolved=$3, tier_source=$4, tier_rule=$5, tier_reason=$6
             WHERE id=$2",
        )
        .bind(&model_id)
        .bind(run_id)
        .bind(TierChoice::from(tier).as_str())
        .bind(tier_source)
        .bind(&tier_rule)
        .bind(&tier_reason)
        .execute(&self.db.pool)
        .await?;

        // Attachments are folded in here rather than at the route, so that
        // re-running a task re-attaches and `tasks.prompt` keeps holding
        // exactly what the user typed.
        let atts = attachments::for_task(&self.db, task_id)
            .await
            .unwrap_or_default();
        // What the run is actually asked for depends on the phase: draft a
        // plan, or carry out the one that was approved. Attachments are folded
        // into either, since a spec you were given is context for planning too.
        let asked = match &phase {
            task_plan::Phase::Plan => match self.plan_revision_note(run_id).await? {
                Some(note) => task_plan::revise_prompt(
                    &task_prompt,
                    stored_plan.as_deref().unwrap_or(""),
                    &note,
                ),
                None => task_plan::plan_prompt(&task_prompt),
            },
            task_plan::Phase::Work { plan: Some(plan) } => task_plan::work_prompt(
                &task_prompt,
                plan,
                // Whether a human rewrote it, not whether it merely differs.
                self.plan_was_edited(run_id).await?,
            ),
            task_plan::Phase::Work { plan: None } => task_prompt.clone(),
        };
        let (prompt, extra_read_dirs) = attachments::augment_prompt(&asked, &atts);
        // Knowledge-base articles tagged onto the card. This is what makes
        // tagging one worth doing: the agent is handed the runbook rather than
        // left to infer it from the code.
        // A follow-up acting on a note also reads what the note was sent with.
        let articles = crate::kb::for_run(
            &self.db,
            Some(task_id),
            run.get::<Option<Uuid>, _>("review_comment_id"),
        )
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(%run_id, error = %e, "could not load tagged articles");
            vec![]
        });
        let prompt = crate::kb::augment_prompt(&prompt, &articles);

        // A bound agent carries its memory into the run: what it did on this
        // project before is context, and what it does now becomes memory below.
        let project_id: Uuid = run.get("project_id");

        // What the person who owns this project wants every run to know, and
        // the skill this card was created with — background then method. Last,
        // so the request and its attachments are read first and these are what
        // they sit against; and in the *prompt* rather than the system prompt,
        // because standing context that outranked the request is the failure
        // both are fenced against.
        //
        // This was two calls spelled out here and nowhere else, which is how
        // a $75 team card ran with none of it. `Standing::apply` is those two
        // calls in that order and is pinned byte-identical to them.
        let standing = crate::runs::context::Standing::load(
            &self.db,
            Some(project_id),
            run.get::<Option<Uuid>, _>("skill_id"),
        )
        .await;
        let prompt = standing.apply(&prompt);
        // What the card's work is for: the goal chain and the epic, fenced,
        // after everything else — background, never the brief.
        let prompt = match crate::goals::why_this_matters(&self.db, task_id).await {
            Ok(Some(why)) => format!("{prompt}\n\n{why}"),
            Ok(None) => prompt,
            Err(e) => {
                tracing::warn!(%run_id, error = %e, "could not load the card's goals");
                prompt
            }
        };
        let memory_block = match bound_agent {
            Some(agent_id) => memory::recall(&self.db, agent_id, Some(project_id))
                .await
                .ok()
                .as_deref()
                .and_then(memory::render),
            None => None,
        };

        // The work pass picks up the planning session, so the agent keeps
        // everything it learned reading the code — but only from an engine
        // that minted it and only if that engine can resume at all.
        let resume_session_id = match (&phase, engine.capabilities().resume_sessions) {
            (task_plan::Phase::Work { plan: Some(_) }, true) => self
                .plan_session(run_id)
                .await?
                .filter(|(_, minted_by)| minted_by == &engine_id)
                .map(|(sid, _)| sid)
                .or(resume_session_id),
            _ => resume_session_id,
        };

        let tool_timeout_ms = self.mcp_tool_timeout_ms().await;
        // Kept for the checks that may run in it once the work is done.
        let work_dir = cwd.clone();
        // A summary pass explains work already done, and a review pass judges
        // it; either may read the worktree but not change it, exactly like a
        // planning pass. A review that could edit the diff it judges is not a
        // review — which is why `review::start` refuses an engine that does
        // not enforce `denied_tools`.
        let read_only = planning
            || matches!(
                run.get::<String, _>("trigger").as_str(),
                "summary" | crate::review::PEER_REVIEW
            );
        let spec = RunSpec {
            cwd,
            prompt,
            model_tier: tier,
            model_id,
            effort,
            resume_session_id,
            // Planning is read-only whatever the card says. A plan you are
            // going to be asked to approve is worthless if the work already
            // happened while it was being written — and with nothing to
            // approve, nothing can prompt either. A summary pass is the same.
            permission_mode: if read_only {
                PermissionMode::AutoEdit
            } else {
                permission_mode
            },
            allowed_tools: if read_only {
                // With no prompt tool to ask through, anything not listed is
                // refused — including Eren's own look-ups, which the
                // toolbox offers a read-only pass and nothing else.
                task_plan::PLANNING_TOOLS
                    .iter()
                    .map(|t| t.to_string())
                    .chain(std::iter::once("mcp__eren".to_string()))
                    .collect()
            } else {
                allowed_tools
            },
            denied_tools: if read_only {
                task_plan::PLANNING_DENIED
                    .iter()
                    .map(|t| t.to_string())
                    .collect()
            } else {
                vec![]
            },
            append_system_prompt: match (agent_prompt.filter(|p| !p.is_empty()), memory_block) {
                (Some(p), Some(m)) => Some(format!("{p}{m}")),
                (Some(p), None) => Some(p),
                // Memory is useful even when the agent has no system prompt.
                (None, Some(m)) => Some(m),
                (None, None) => None,
            },
            mcp,
            run_key: run_id.to_string(),
            extra_read_dirs,
            // Nothing to approve during planning, so nothing to ask about.
            permission_prompt_tool: !read_only,
            extra_env: eren_shared::brand::with_legacy_env(HashMap::from([
                ("EREN_RUN_ID".to_string(), run_id.to_string()),
                // Permission prompts block the MCP tools/call until the user
                // answers in the dashboard, so the CLI has to be willing to
                // wait exactly as long as the broker is. See
                // `mcp_tool_timeout_ms` — one value, three call sites.
                ("MCP_TOOL_TIMEOUT".to_string(), tool_timeout_ms.clone()),
                // Server startup. 60s was ample when the only server was
                // Eren's own local HTTP endpoint; a user-connected server
                // may cold-start `npx -y …` and fetch a package first, and a
                // server that misses this window fails every tool call after
                // it with an unhelpful "operation timed out".
                ("MCP_TIMEOUT".to_string(), "180000".to_string()),
            ])),
        };

        let seq = SeqAlloc::starting_at(next_seq(&self.db, run_id).await?);
        // Planning doesn't finalize: a completed *plan* is not a completed
        // run, and letting `stream_run` mark it terminal would send the card
        // to review with nothing done.
        let outcome = self
            .stream_run(
                run_id,
                None,
                &seq,
                engine,
                spec,
                if planning {
                    CallerKind::TaskPlanning
                } else {
                    CallerKind::TaskWork
                },
            )
            .await?;

        // A held run is coming back; it has not ended. `park_for_approval`
        // reads "not completed" as "no plan to approve" and fails the run,
        // which would undo the hold `hold_rate_limited` just wrote — status,
        // queue row and all — one line after writing it.
        if outcome.status == RunStatus::RateLimited {
            return Ok(());
        }

        if planning {
            return self.park_for_approval(run_id, &outcome).await;
        }

        // Only the completed case here. A run that ended badly has already had
        // its card moved off In Progress by `finish`, which is the one place
        // every ending goes through — including the ones that never reach this
        // function at all.
        if outcome.status == RunStatus::Completed {
            let trigger: String = run.get("trigger");
            // A summary pass only explains work that was already announced
            // and changed nothing, so the card stays where it is and nothing
            // is checked or announced again.
            let summarizing = trigger == "summary";
            // A review pass is the same: it judged the work, it did none, and
            // its verdict is posted by `settle_review` below.
            let reviewing = trigger == crate::review::PEER_REVIEW;
            let passive = summarizing || reviewing;
            if !passive {
                // Review exists to gate a diff onto the base branch. An
                // in-place run already wrote to the user's folder and produced
                // no diff, so parking it in review would offer a review that
                // cannot happen.
                //
                // Unless the agent said it is waiting on another card
                // (`report_blocker` with a card): then the work is not ready
                // to review, it is waiting, and the backlog is where a card
                // waits — which is also where a landing looks for the cards
                // it unblocks, so this one is woken when its blocker lands.
                let waiting_on: Vec<String> = sqlx::query_scalar(
                    "SELECT b.title FROM tasks t
                       JOIN task_deps d ON d.task_id = t.id
                       JOIN tasks b ON b.id = d.blocked_by
                      WHERE t.id = $1 AND t.blocked_note IS NOT NULL AND b.board_column <> 'done'
                      ORDER BY b.title",
                )
                .bind(task_id)
                .fetch_all(&self.db.pool)
                .await?;
                let column = match (waiting_on.is_empty(), in_place) {
                    (false, _) => "backlog",
                    (true, true) => "done",
                    (true, false) => "review",
                };
                sqlx::query("UPDATE tasks SET board_column=$2 WHERE id=$1")
                    .bind(task_id)
                    .bind(column)
                    .execute(&self.db.pool)
                    .await?;
                if !waiting_on.is_empty() {
                    let note = format!(
                        "Waiting for {} to land — back in the backlog until then.",
                        waiting_on
                            .iter()
                            .map(|t| format!("\u{201c}{t}\u{201d}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    if let Err(e) =
                        report::post_system(&self.db, task_id, Some(run_id), &note).await
                    {
                        tracing::warn!(%run_id, error = %e, "could not note a waiting card");
                    }
                }
            }

            // What the run did goes on the card, where a person reading it
            // looks — not only into the agent's memory. Best-effort: a report
            // that failed to write must not fail a completed run. A review's
            // report is its verdict, which `settle_review` posts.
            let posted = if reviewing {
                Ok(())
            } else {
                report::post(
                    &self.db,
                    task_id,
                    run_id,
                    bound_agent,
                    &trigger,
                    variant.as_deref(),
                    &outcome.output,
                )
                .await
            };
            if let Err(e) = posted {
                tracing::warn!(%run_id, error = %e, "could not post the run's report");
            }
            // A run that said nothing gets one read-only pass to explain
            // itself — one, because a summary pass never asks for another.
            // Not for a bake-off variant (the card's worktree is not the one
            // it worked in) or an in-place card (there is no worktree).
            let summarize = outcome.output.trim().is_empty()
                && !passive
                && !in_place
                && variant.is_none()
                // An app's change lands by itself just below; a summary
                // queued now would run on a card that is already done.
                && run.get::<String, _>("project_kind") != "app";

            // A conflict the agent was asked to resolve is concluded here, so
            // the card's branch carries the merge before anything checks it.
            // Markers left behind keep it open — and Merge refuses — which is
            // the point: the resolution is not done.
            if trigger == "conflict" {
                if let Some(branch) = run.get::<Option<String>, _>("branch") {
                    let wt = crate::worktrees::manager::Worktree {
                        path: work_dir.clone(),
                        branch,
                    };
                    let message = format!(
                        "eren: resolve conflicts in {}",
                        run.get::<String, _>("title")
                    );
                    if let Err(e) = self.worktrees.conclude_merge(&wt, &message).await {
                        tracing::warn!(%run_id, error = %e, "the conflict is not resolved yet");
                    }
                }
            }

            // The project's checks run before anyone is told the card is
            // ready, so the news can say whether the work passes. Unasked only
            // after a Full Auto run — see `checks` for why — and never for an
            // in-place project (no worktree), an app (it lands by itself) or a
            // bake-off variant (its own worktree, compared by a person).
            //
            // Or after any run, where the project's review policy carries a
            // person's standing consent to that (`run_checks_after_every_run`).
            let title: String = run.get("title");
            let chat_id: Option<Uuid> = run.get("task_chat_id");
            let reviewable = !in_place
                && run.get::<Option<String>, _>("variant_label").is_none()
                && run.get::<String, _>("project_kind") == "repo";
            let consented = permission_mode == PermissionMode::FullAuto
                || crate::review::policy(&self.db, project_id)
                    .await
                    .is_ok_and(|p| p.run_checks_after_every_run);
            let auto_checks = if !passive && reviewable && consented {
                crate::checks::config(&self.db, project_id)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(%run_id, error = %e, "could not read this project's checks");
                        None
                    })
            } else {
                None
            };
            let checking = auto_checks.is_some();
            match auto_checks {
                Some(config) => {
                    let check_run_id =
                        crate::checks::begin(&self.db, task_id, Some(run_id), "auto").await?;
                    let this = self.clone();
                    tokio::spawn(async move {
                        this.settle_checks(
                            task_id,
                            run_id,
                            check_run_id,
                            work_dir,
                            config,
                            title,
                            chat_id,
                            summarize,
                        )
                        .await
                    });
                }
                None if passive => {}
                None => {
                    self.announce_ready(run_id, &title, chat_id, in_place, None)
                        .await;
                    if summarize {
                        self.ask_for_summary(task_id, run_id).await;
                    }
                }
            }
            // With checks running, `settle_checks` asks once they are done.
            if reviewable && !checking {
                self.settle_review(task_id, run_id, &trigger).await;
            }

            // The work joins the agent's memory. Best-effort: a failed memory
            // write must not fail a completed run. A reviewer did no work.
            if let Some(agent_id) = bound_agent.filter(|_| !reviewing) {
                let title: String = run.get("title");
                let note = format!(
                    "Completed task \"{title}\": {}",
                    if outcome.output.is_empty() {
                        "(no summary)"
                    } else {
                        &outcome.output
                    }
                );
                if let Err(e) = memory::remember(
                    &self.db,
                    agent_id,
                    Some(project_id),
                    Some(task_id),
                    "task_result",
                    &note,
                )
                .await
                {
                    tracing::warn!(%run_id, error = %e, "agent memory write failed");
                }
            }
        }

        // An app's own change lands by itself. A no-op for every other card,
        // and best-effort like the memory write above: a build that failed to
        // merge must not turn a completed run into a failed one — the build row
        // records what happened and the card keeps its diff.
        if let Err(e) = apps::build::settle(
            &self.db,
            &self.worktrees,
            task_id,
            outcome.status,
            outcome.reason.as_deref(),
        )
        .await
        {
            tracing::warn!(%run_id, error = %e, "an app's change did not land");
        }
        // Either of the two above may just have landed the card: an in-place
        // run settles straight to done, and an app build squash-merges. A
        // no-op for every other card.
        self.landed(task_id).await;

        // A task spawned from chat reports back into that chat.
        if let Some(chat_id) = run.get::<Option<Uuid>, _>("task_chat_id") {
            if outcome.status.is_terminal() {
                let title: String = run.get("title");
                let note = match outcome.status {
                    // Said by `announce_ready`, after any checks have run.
                    RunStatus::Completed => return Ok(()),
                    RunStatus::Canceled => format!("Task \"{title}\" was canceled."),
                    _ => format!(
                        "Task \"{title}\" failed: {}",
                        outcome.reason.clone().unwrap_or_default()
                    ),
                };
                let _ = sqlx::query(
                    "INSERT INTO chat_messages (chat_id, role, content, run_id)
                     VALUES ($1, 'system', $2, $3)",
                )
                .bind(chat_id)
                .bind(note)
                .bind(run_id)
                .execute(&self.db.pool)
                .await;
            }
        }
        Ok(())
    }

    /// Queue the one read-only pass that asks a silent run what it did.
    /// Best-effort: the card already says "(no summary)", and a refusal —
    /// a person started another run first, say — leaves it at that.
    pub(crate) async fn ask_for_summary(&self, task_id: Uuid, run_id: Uuid) {
        if let Err(e) = self
            .enqueue_follow_up(task_id, follow_up::FollowUp::Summarize { run_id })
            .await
        {
            tracing::info!(%run_id, error = %e, "no summary pass for a silent run");
        }
    }

    /// Tell whoever is listening that a card's work is ready: the attention
    /// hook's "a run finished", and the chat the card came from.
    ///
    /// One place, called either straight away or once checks have settled, so
    /// the message can carry their result — "ready for review" about a diff
    /// whose tests fail is not the same news.
    pub(crate) async fn announce_ready(
        &self,
        run_id: Uuid,
        title: &str,
        chat_id: Option<Uuid>,
        in_place: bool,
        checks: Option<&crate::checks::Summary>,
    ) {
        let checked = checks
            .map(|c| format!(" — {}", c.line()))
            .unwrap_or_default();
        // "A run finished" has been offered in the attention settings all
        // along and never sent: nothing fired it. Off by default there, so
        // this reaches only the people who turned it on.
        let ctx = crate::attention::Ctx {
            title: format!(
                "eren: \"{title}\" {}{checked}",
                if in_place {
                    "is done"
                } else {
                    "is ready for review"
                }
            ),
            ..crate::attention::ctx_for_run(&self.db, run_id, None).await
        };
        crate::attention::fire(&self.db, crate::attention::Event::Finished, ctx).await;

        // A task spawned from chat reports back into that chat.
        if let Some(chat_id) = chat_id {
            let note =
                format!("Task \"{title}\" completed — ready for review on the board{checked}.");
            let _ = sqlx::query(
                "INSERT INTO chat_messages (chat_id, role, content, run_id)
                 VALUES ($1, 'system', $2, $3)",
            )
            .bind(chat_id)
            .bind(note)
            .bind(run_id)
            .execute(&self.db.pool)
            .await;
        }
    }

    /// Queue a run that writes documentation instead of code.
    ///
    /// `article_id` present means "revise this one"; absent means a new draft.
    pub async fn enqueue_kb_article(
        &self,
        workspace_id: Uuid,
        project_id: Uuid,
        brief: &str,
        engine: Option<&str>,
        article_id: Option<Uuid>,
        parent_id: Option<Uuid>,
    ) -> anyhow::Result<Uuid> {
        let engine = engine
            .map(str::to_string)
            .unwrap_or_else(|| self.default_engine());
        if self.engine(&engine).is_none() {
            anyhow::bail!("{engine} isn't installed on this machine");
        }
        crate::budgets::check(
            &self.db,
            &crate::budgets::Scope {
                workspace: Some(workspace_id),
                project: Some(project_id),
                ..Default::default()
            },
            true,
        )
        .await?;
        // A new article's row is created up front, so the editor has something
        // to open the moment the run is queued rather than only once it lands.
        let article_id = match article_id {
            Some(id) => id,
            None => {
                sqlx::query_scalar(
                    "INSERT INTO kb_articles
                    (workspace_id, project_id, parent_id, title, status, origin, position)
                 VALUES ($1, $2, $3, $4, 'draft', 'agent',
                         COALESCE((SELECT max(position) + 1000 FROM kb_articles
                                    WHERE workspace_id = $1
                                      AND parent_id IS NOT DISTINCT FROM $3), 1000))
                 RETURNING id",
                )
                .bind(workspace_id)
                .bind(project_id)
                .bind(parent_id)
                .bind(crate::kb::sanitize::summarize(brief, 80))
                .fetch_one(&self.db.pool)
                .await?
            }
        };

        let run_id: Uuid = sqlx::query_scalar(
            "INSERT INTO runs (status, trigger, engine, kb_article_id, kb_brief, kb_project_id)
             VALUES ('queued', 'kb', $1, $2, $3, $4) RETURNING id",
        )
        .bind(&engine)
        .bind(article_id)
        .bind(brief)
        .bind(project_id)
        .fetch_one(&self.db.pool)
        .await?;
        sqlx::query("UPDATE kb_articles SET source_run_id=$1 WHERE id=$2")
            .bind(run_id)
            .bind(article_id)
            .execute(&self.db.pool)
            .await?;
        self.queue(run_id, 9).await?;
        Ok(run_id)
    }

    /// Write the article, then store it.
    ///
    /// Read-only over the project itself — no worktree, no branch. Nothing is
    /// being changed, so there is nothing to isolate or review, and creating a
    /// worktree for a run that only reads would leave litter behind.
    async fn execute_kb_run(self: &Arc<Self>, run_id: Uuid) -> anyhow::Result<()> {
        let row = sqlx::query(
            "SELECT r.engine, r.kb_brief, r.kb_article_id, p.path AS project_path,
                    a.title, a.content_html, a.current_seq, p.id AS project_id
             FROM runs r
             JOIN projects p ON p.id = r.kb_project_id
             LEFT JOIN kb_articles a ON a.id = r.kb_article_id
             WHERE r.id = $1",
        )
        .bind(run_id)
        .fetch_one(&self.db.pool)
        .await?;

        self.set_status(run_id, RunStatus::Starting).await?;
        let engine_id: String = row.get("engine");
        let engine = self
            .engine(&engine_id)
            .ok_or_else(|| anyhow::anyhow!("unknown engine {engine_id}"))?;

        let brief: String = row.get::<Option<String>, _>("kb_brief").unwrap_or_default();
        let existing: String = row
            .get::<Option<String>, _>("content_html")
            .unwrap_or_default();
        // The revision the agent is working from, captured before it starts.
        // If a person edits the page while it runs, the review UI compares
        // against *this* and can say the page moved on underneath it — rather
        // than silently presenting a stale rewrite as an up-to-date one.
        let base_seq: i32 = row.get::<Option<i32>, _>("current_seq").unwrap_or(0);
        // Brain, and no skill. The job is "describe this repository
        // accurately", and the Brain is a hand-written list of exactly the
        // facts a generated page gets wrong. This is also the one prompt in
        // the codebase whose *output* becomes input to other prompts —
        // `kb::for_run` feeds published pages back into task runs as reference
        // — so a page written without the facts is wrong repeatedly, not once.
        //
        // No skill because nobody named one: a generated article was never
        // asked to be written a particular way, and inferring one is the thing
        // Skills exist not to do.
        let standing =
            crate::runs::context::Standing::brain_only(&self.db, Some(row.get("project_id"))).await;
        let context = standing.block();
        let prompt = if existing.trim().is_empty() {
            crate::kb::write::prompt(&brief, &context)
        } else {
            crate::kb::write::rewrite_prompt(
                &brief,
                &row.get::<Option<String>, _>("title").unwrap_or_default(),
                &existing,
                &context,
            )
        };

        let tier = ModelTier::Medium;
        let model_id = self.model_for(&engine_id, tier);
        sqlx::query("UPDATE runs SET model=$1, started_at=now() WHERE id=$2")
            .bind(&model_id)
            .bind(run_id)
            .execute(&self.db.pool)
            .await?;

        let spec = RunSpec {
            cwd: PathBuf::from(row.get::<String, _>("project_path")),
            prompt,
            model_tier: tier,
            model_id,
            effort: None,
            resume_session_id: None,
            permission_mode: PermissionMode::AutoEdit,
            allowed_tools: crate::kb::write::TOOLS
                .iter()
                .map(|t| t.to_string())
                .collect(),
            denied_tools: crate::kb::write::DENIED
                .iter()
                .map(|t| t.to_string())
                .collect(),
            append_system_prompt: None,
            mcp: McpWiring::default(),
            run_key: run_id.to_string(),
            extra_read_dirs: vec![],
            permission_prompt_tool: false,
            extra_env: eren_shared::brand::with_legacy_env(HashMap::from([(
                "EREN_RUN_ID".to_string(),
                run_id.to_string(),
            )])),
        };

        let seq = SeqAlloc::starting_at(next_seq(&self.db, run_id).await?);
        let outcome = self
            .stream_run(run_id, None, &seq, engine, spec, CallerKind::KbGeneration)
            .await?;
        if outcome.status != RunStatus::Completed {
            return Ok(());
        }

        let article_id: Uuid = row.get("kb_article_id");
        let prepared = crate::kb::render::prepare(&crate::kb::write::extract_html(&outcome.output));
        if prepared.html.trim().is_empty() {
            // A brand-new page with nothing in it is litter, not a draft:
            // remove the placeholder rather than leaving an empty row in the
            // tree that nobody can tell apart from a page someone meant to
            // start. An existing page is left exactly as it was.
            //
            // `base_seq == 0` alone is not enough to call a page empty — a page
            // whose body exists but whose revision log doesn't yet (a backfill
            // that failed) also reads as 0, and deleting that would destroy
            // somebody's writing. The body is the authority.
            if base_seq == 0 && existing.trim().is_empty() {
                sqlx::query("DELETE FROM kb_articles WHERE id=$1")
                    .bind(article_id)
                    .execute(&self.db.pool)
                    .await?;
            }
            self.finish(
                run_id,
                RunStatus::Failed,
                Some("the agent produced no article".into()),
            )
            .await?;
            return Ok(());
        }

        let title = crate::kb::write::title_from(&prepared.html, &brief);
        let rev = crate::kb::revisions::NewRevision {
            title: &title,
            html: &prepared.html,
            text: &prepared.text,
            author: crate::kb::revisions::Author::Agent,
            kind: "agent",
            base_seq: Some(base_seq),
            run_id: Some(run_id),
            note: "",
        };

        if base_seq == 0 && existing.trim().is_empty() {
            // Nothing to overwrite and nobody to ask: a page that was created
            // by asking an agent to write it *is* the agent's page, and making
            // someone approve a proposal against a blank page is ceremony
            // with no decision in it. It stays a draft either way.
            //
            // Guarded on the body as well as the pointer, so a page that has
            // content but no revision row is treated as somebody's work rather
            // than as an empty placeholder to write over.
            //
            // `Some(0)` rather than `None`, because `existing` was read long
            // before this line — a whole agent run ago. If somebody started
            // typing on the placeholder while the run was in flight the page is
            // no longer blank, and this refuses instead of erasing them.
            crate::kb::revisions::save_edit(&self.db, article_id, rev, Some(0)).await?;
        } else {
            // Everything else is a proposal. This is the rule the whole
            // revision log exists for: an agent must never replace a body a
            // person may have written, with no copy and no diff.
            crate::kb::revisions::propose(&self.db, article_id, rev).await?;
        }
        Ok(())
    }

    /// Queue a deep-research run for a question about a project.
    ///
    /// Returns `(research_id, run_id)`. A re-run of an existing research goes
    /// through `enqueue_research_run` instead, which reuses the row.
    pub async fn enqueue_research(
        &self,
        // A project, or a workspace for a *general* research — one with no
        // repository behind it, answered from the web alone. Exactly one; the
        // table CHECKs the same rule.
        project_id: Option<Uuid>,
        workspace_id: Option<Uuid>,
        question: &str,
        engine: Option<&str>,
        // NULL means the research defaults: Complex, with the operator's
        // effort for that tier. Stored on the research so a re-run asks the
        // same way.
        model_tier: Option<ModelTier>,
        effort: Option<ReasoningEffort>,
    ) -> anyhow::Result<(Uuid, Uuid)> {
        if project_id.is_none() && workspace_id.is_none() {
            anyhow::bail!("a research belongs to a project or to a workspace");
        }
        let engine = engine
            .map(str::to_string)
            .unwrap_or_else(|| self.default_engine());
        if self.engine(&engine).is_none() {
            anyhow::bail!("{engine} isn't installed on this machine");
        }
        // Before the research exists, so a refusal leaves nothing behind.
        crate::budgets::check(
            &self.db,
            &crate::budgets::Scope {
                workspace: workspace_id,
                project: project_id,
                ..Default::default()
            },
            true,
        )
        .await?;
        let research_id: Uuid = sqlx::query_scalar(
            "INSERT INTO researches (project_id, workspace_id, question, model_tier, effort)
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(project_id)
        .bind(workspace_id)
        .bind(question)
        .bind(model_tier.map(|t| TierChoice::from(t).as_str().to_string()))
        .bind(effort.map(|e| e.as_str().to_string()))
        .fetch_one(&self.db.pool)
        .await?;
        let run_id = self.enqueue_research_run(research_id, &engine).await?;
        Ok((research_id, run_id))
    }

    /// A (re-)run against an existing research. Its own function because
    /// re-running is a fresh runs row against the same question — the report
    /// is replaced wholesale on completion, never merged.
    pub async fn enqueue_research_run(
        &self,
        research_id: Uuid,
        engine: &str,
    ) -> anyhow::Result<Uuid> {
        if self.engine(engine).is_none() {
            anyhow::bail!("{engine} isn't installed on this machine");
        }
        crate::budgets::check(
            &self.db,
            &crate::budgets::scope_of_research(&self.db, research_id).await?,
            true,
        )
        .await?;
        let run_id: Uuid = sqlx::query_scalar(
            "INSERT INTO runs (status, trigger, engine, research_id)
             VALUES ('queued', 'research', $1, $2) RETURNING id",
        )
        .bind(engine)
        .bind(research_id)
        .fetch_one(&self.db.pool)
        .await?;
        // Above task runs (10), below comment replies (15): a person is
        // sitting on the Research page watching, but the wait is measured in
        // minutes either way.
        self.queue(run_id, 12).await?;
        Ok(run_id)
    }

    /// Investigate a question: read-only over the project's real checkout,
    /// plus the CLI's own web search. Same shape as `execute_kb_run` — no
    /// worktree, nothing to isolate — with two deliberate differences, both
    /// commented at the site.
    async fn execute_research_run(self: &Arc<Self>, run_id: Uuid) -> anyhow::Result<()> {
        // LEFT JOIN: a *general* research has no project — it is answered
        // from the web alone, out of a scratch directory.
        let row = sqlx::query(
            "SELECT r.engine, rs.id AS research_id, rs.question,
                    rs.model_tier AS research_tier, rs.effort AS research_effort,
                    p.path AS project_path, p.id AS project_id
             FROM runs r
             JOIN researches rs ON rs.id = r.research_id
             LEFT JOIN projects p ON p.id = rs.project_id
             WHERE r.id = $1",
        )
        .bind(run_id)
        .fetch_one(&self.db.pool)
        .await?;

        self.set_status(run_id, RunStatus::Starting).await?;
        let engine_id: String = row.get("engine");
        let engine = self
            .engine(&engine_id)
            .ok_or_else(|| anyhow::anyhow!("unknown engine {engine_id}"))?;

        let research_id: Uuid = row.get("research_id");
        let question: String = row.get("question");
        let project_id: Option<Uuid> = row.get("project_id");
        // The two shapes differ in everything the project used to supply:
        // where to stand, what to read, and which facts come along.
        //
        // Brain (project case only), no skill — the same reasoning as KB
        // generation: the Brain is a hand-written list of exactly the facts
        // an investigation gets wrong, and nobody named a skill.
        let (cwd, prompt, tools) = match (project_id, row.get::<Option<String>, _>("project_path"))
        {
            (Some(pid), Some(path)) => {
                let standing =
                    crate::runs::context::Standing::brain_only(&self.db, Some(pid)).await;
                (
                    PathBuf::from(path),
                    crate::runs::research::prompt(&question, &standing.block()),
                    crate::runs::research::TOOLS,
                )
            }
            _ => {
                // A scratch directory, the utility-run precedent — the agent
                // has to stand somewhere, and it must not be anywhere with
                // files worth reading.
                let scratch = eren_shared::brand::home().join("tmp");
                tokio::fs::create_dir_all(&scratch).await?;
                (
                    scratch,
                    crate::runs::research::web_prompt(&question),
                    crate::runs::research::WEB_TOOLS,
                )
            }
        };

        // Difference one from KB: the person's choice, with Complex as the
        // default — research is the thinking-heavy kind of run — and the
        // effort actually resolved rather than hardcoded, so the operator's
        // per-tier setting applies when nothing was picked.
        let tier = row
            .get::<Option<String>, _>("research_tier")
            .and_then(|t| TierChoice::parse(&t))
            .and_then(TierChoice::fixed)
            .unwrap_or(ModelTier::Complex);
        let effort = self
            .resolve_effort(
                None,
                row.get::<Option<String>, _>("research_effort")
                    .and_then(|e| ReasoningEffort::parse(&e)),
                &engine_id,
                tier,
            )
            .await;
        let model_id = self.model_for(&engine_id, tier);
        sqlx::query("UPDATE runs SET model=$1, started_at=now() WHERE id=$2")
            .bind(&model_id)
            .bind(run_id)
            .execute(&self.db.pool)
            .await?;

        let spec = RunSpec {
            cwd,
            prompt,
            model_tier: tier,
            model_id,
            effort,
            resume_session_id: None,
            // Difference two: not KB's hardcoded AutoEdit. Research grants
            // web tools, and an engine that cannot pause to ask (OpenCode)
            // answers Reviewed by rejecting every call — the same reason chat
            // resolves this per capability. The denials still bind either way.
            permission_mode: chat_permission_mode(engine.as_ref()),
            allowed_tools: tools.iter().map(|t| t.to_string()).collect(),
            denied_tools: crate::runs::research::DENIED
                .iter()
                .map(|t| t.to_string())
                .collect(),
            append_system_prompt: None,
            mcp: McpWiring::default(),
            run_key: run_id.to_string(),
            extra_read_dirs: vec![],
            permission_prompt_tool: false,
            extra_env: eren_shared::brand::with_legacy_env(HashMap::from([(
                "EREN_RUN_ID".to_string(),
                run_id.to_string(),
            )])),
        };

        let seq = SeqAlloc::starting_at(next_seq(&self.db, run_id).await?);
        let outcome = self
            .stream_run(run_id, None, &seq, engine, spec, CallerKind::Research)
            .await?;
        if outcome.status != RunStatus::Completed {
            return Ok(());
        }

        let report = crate::runs::research::extract_report(&outcome.output);
        if report.trim().is_empty() {
            // Unlike KB there is no placeholder to clean up: the researches
            // row keeps the question, and the page offers a re-run.
            self.finish(
                run_id,
                RunStatus::Failed,
                Some("the agent produced no report".into()),
            )
            .await?;
            return Ok(());
        }
        // Scrubbed at write time, not at save-to-KB: the report quotes web
        // content — third-party text — and later becomes a KB page quoted
        // into future prompts. The same reason `rewrite_prompt` scrubs the
        // article it is handed.
        let report = crate::fence::scrub_foreign(&report, &[]);
        let title = crate::runs::research::title_from(&report, &question);
        sqlx::query("UPDATE researches SET report_md=$1, title=$2, updated_at=now() WHERE id=$3")
            .bind(&report)
            .bind(&title)
            .bind(research_id)
            .execute(&self.db.pool)
            .await?;
        Ok(())
    }

    async fn execute_chat_run(self: &Arc<Self>, run_id: Uuid, chat_id: Uuid) -> anyhow::Result<()> {
        let row = sqlx::query(
            // Lateral join rather than a scalar subquery: the turn needs the
            // message's id as well as its text, to look up its attachments.
            "SELECT r.engine, c.session_id, c.session_engine, c.model_tier AS chat_tier,
                    c.effort AS chat_effort, c.plan_mode, c.model_id AS chat_model,
                    p.path AS project_path,
                    p.id AS project_id, p.kind AS project_kind,
                    m.id AS user_message_id, m.content AS user_message,
                    -- The manager's persona, joined here rather than fetched
                    -- separately: this runs on every chat turn in the
                    -- application, and a manager thread is a handful of them.
                    -- NULL for all the rest, which is the common case.
                    mg.system_prompt AS manager_persona,
                    -- The agent this conversation is with, if any.
                    ca.id AS agent_id, ca.name AS agent_name,
                    ca.system_prompt AS agent_prompt, ca.model_tier AS agent_tier,
                    ca.effort AS agent_effort
             FROM runs r JOIN chats c ON c.id = r.chat_id
             LEFT JOIN projects p ON p.id = c.project_id
             LEFT JOIN agents ca ON ca.id = c.agent_id
             LEFT JOIN (
                 routines rt JOIN agents mg ON mg.id = rt.agent_id
             ) ON rt.kind = 'manage' AND rt.chat_id = c.id
             LEFT JOIN LATERAL (
                 SELECT id, content FROM chat_messages
                 WHERE chat_id = c.id AND role = 'user'
                 ORDER BY created_at DESC LIMIT 1
             ) m ON TRUE
             WHERE r.id = $1",
        )
        .bind(run_id)
        .fetch_one(&self.db.pool)
        .await?;

        self.set_status(run_id, RunStatus::Starting).await?;

        let engine_id: String = row.get("engine");
        let engine = self
            .engine(&engine_id)
            .ok_or_else(|| anyhow::anyhow!("unknown engine {engine_id}"))?;

        // A session is only resumable by the engine that created it — a
        // mock-produced session id would make the real CLI fail instantly.
        let session_id: Option<String> = row.get::<Option<String>, _>("session_id").filter(|_| {
            row.get::<Option<String>, _>("session_engine").as_deref() == Some(&engine_id)
        });
        let user_message: String = row
            .get::<Option<String>, _>("user_message")
            .unwrap_or_else(|| "Introduce yourself briefly.".to_string());
        // The retrieval query, captured before any augment: what the person
        // typed, not the attachment framing or mention instructions that get
        // wrapped around it below.
        let raw_user_text = user_message.clone();
        // NULL for a *general* chat — a conversation with no project behind
        // it. Everything project-shaped below is gated on this: attachments
        // and mentions are project machinery, the brain belongs to a project,
        // and the MCP task tools resolve through one.
        let project_id: Option<Uuid> = row.get("project_id");
        let project_kind: Option<String> = row.get("project_kind");
        // Plan mode: propose, do not act. Only meaningful where there is
        // something to act *on* — a space chat and a general chat have no
        // board tools to take away, so the flag is inert there rather than
        // producing a plan nobody can approve into anything.
        let planning: bool = row.get::<bool, _>("plan_mode")
            && project_id.is_some()
            && project_kind.as_deref() != Some("space");

        // An attachment-only turn stores empty content, so the prompt may be
        // nothing but the attachment block.
        let atts = match row.get::<Option<Uuid>, _>("user_message_id") {
            Some(message_id) => attachments::for_message(&self.db, message_id)
                .await
                .unwrap_or_default(),
            None => vec![],
        };
        let (user_message, extra_read_dirs) = attachments::augment_prompt(&user_message, &atts);

        // Who the user named with `@`. Resolved when the message was sent, so
        // this is a lookup rather than a second parse — and `create_task` reads
        // the same rows, which is what makes the binding hold even if the model
        // forgets to pass `agent_name`.
        let mentioned: Vec<String> = mentions::latest_for_chat(&self.db, chat_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        let user_message = mentions::augment_prompt(&user_message, &mentioned);

        // And which skills. Same lookup, same reason — but a different message:
        // a mentioned skill needs no action from the assistant, only for it not
        // to stop and ask about a name it cannot find in the agent library.
        let named_skills: Vec<String> = mentions::latest_skills_for_chat(&self.db, chat_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        let user_message = mentions::augment_skills_prompt(&user_message, &named_skills);

        // And which knowledge-base pages the person attached to this turn.
        //
        // Per-message like the attachments above, and *not* like the brain
        // below: a page is chosen for a question. It therefore runs on a
        // resumed session too, which is correct — the session carries the
        // earlier turns, but it cannot carry a page that had not been picked
        // yet. The same fenced, capped block a board run gets, because these
        // bodies are written by people *and by other agents* and must read as
        // reference material rather than as instructions.
        let pages = crate::kb::for_chat(&self.db, chat_id)
            .await
            .unwrap_or_default();
        let user_message = crate::kb::augment_prompt(&user_message, &pages);

        // Plan mode, appended last of the per-message blocks so it is the
        // nearest thing to the request it modifies. Per-turn and not
        // resume-gated: which turn is a planning turn is a fact about this
        // message, and a resumed session cannot know it.
        let user_message = if planning {
            format!("{user_message}{}", crate::runs::chat_plan::instruction())
        } else {
            user_message
        };

        // A space chat retrieves before it answers: the top passages from the
        // documents, semantically matched against what the person just typed.
        // Per-message context, exactly like attachments — which passages
        // matter depends on this question, so it runs every turn, resumed
        // session or not (unlike the brain below, which the session already
        // carries). Any failure — empty index, embedder still downloading,
        // offline — leaves the prompt untouched: the chat must work without
        // it, it just answers from Read/Grep instead.
        let user_message = match (project_id, project_kind.as_deref()) {
            (Some(pid), Some("space")) => {
                match crate::rag::retrieve::top_k(
                    &self.db,
                    pid,
                    &raw_user_text,
                    crate::rag::retrieve::DEFAULT_K,
                )
                .await
                {
                    Ok(passages) => crate::rag::retrieve::augment_prompt(&user_message, &passages),
                    Err(e) => {
                        tracing::warn!(%run_id, error=%e, "space retrieval skipped");
                        user_message
                    }
                }
            }
            _ => user_message,
        };

        // The same standing context a board run gets. A chat that has to be
        // told where the code lives every time is the reason this feature
        // exists — and the chat is where a person asks the questions the brain
        // is written to answer.
        //
        // Only on the first turn. A chat *resumes* its session (see
        // `resume_session_id` below), so every later message is the same
        // conversation and already has this. Appending each time paid for the
        // block again per turn and stacked N copies of "read this as
        // background" into one context, which is precisely how a framing stops
        // being read as a framing — the failure the fence is there to prevent.
        //
        // Brain and *not* skill, decided rather than defaulted: chat resolves
        // `@skill` mentions by name a few lines above, and deliberately sends
        // only the name — the body travels with the card the chat creates, so
        // pasting it here would spend it twice and put the method in front of
        // a conversation that has not agreed on the job yet.
        let user_message = match (&session_id, project_id) {
            (None, Some(pid)) => crate::runs::context::Standing::brain_only(&self.db, Some(pid))
                .await
                .apply(&user_message),
            _ => user_message,
        };

        // The Eren tools all resolve through the chat's project, so a
        // general chat gets none — tools that answer every call with an
        // error about a missing project are worse than an empty toolbox.
        // Any project-attached chat gets the endpoint; *which* tools it
        // lists is decided server-side by kind, so a space sees the document
        // tools and never the board tools (whose cards would edit the
        // document folder in place).
        let is_repo =
            project_kind.as_deref() == Some("repo") || project_kind.as_deref() == Some("app");
        let mcp = match project_id {
            Some(_) => McpWiring {
                eren_url: self
                    .mcp_base_url
                    .as_ref()
                    // The run in the URL, so a CLI that outlives its turn
                    // cannot keep calling the board tools on the chat's behalf.
                    .map(|b| format!("{b}/mcp/chat/{chat_id}/{run_id}")),
                servers: vec![],
            },
            None => McpWiring::default(),
        };

        // Both were hardcoded — Medium, and no effort at all — which is why the
        // chat composer had a picker for the engine and nothing else. NULL still
        // means inherit, resolved now rather than frozen when the chat opened.
        //
        // `auto` is not offered for chat yet, and lands on Medium — stated
        // here rather than left to `unwrap_or_default`, because that is the
        // shape of accident this whole feature is guarding against: an
        // unrecognised tier quietly becoming the dearest ordinary model.
        //
        // With an agent, its own tier and effort answer when the chat has not
        // been given one — what was set on the chat still wins, as it would
        // for the assistant.
        let agent_id: Option<Uuid> = row.get("agent_id");
        let tier: ModelTier = row
            .get::<Option<String>, _>("chat_tier")
            .or_else(|| row.get::<Option<String>, _>("agent_tier"))
            .and_then(|t| TierChoice::parse(&t))
            .and_then(TierChoice::fixed)
            .unwrap_or(ModelTier::Medium);
        let chat_effort = self
            .resolve_effort(
                None,
                row.get::<Option<String>, _>("chat_effort")
                    .or_else(|| row.get::<Option<String>, _>("agent_effort"))
                    .and_then(|e| ReasoningEffort::parse(&e)),
                &engine_id,
                tier,
            )
            .await;
        // A model named on the chat wins over the tier mapping.
        //
        // The mapping answers "what does Medium mean for this engine" and is
        // shared by every card, workflow step and conversation. This answers
        // "what does *this* conversation run on", which used to be sayable
        // only by redefining the former for everyone. NULL still means
        // resolve from the tier, so a chat nobody has touched behaves exactly
        // as it did.
        let model_id = row
            .get::<Option<String>, _>("chat_model")
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| self.model_for(&engine_id, tier));
        sqlx::query("UPDATE runs SET model=$1, started_at=now() WHERE id=$2")
            .bind(&model_id)
            .bind(run_id)
            .execute(&self.db.pool)
            .await?;

        // Where the assistant stands and what it may reach for. A repo chat
        // works read-only in the real checkout with the board tools; a space
        // chat stands in its document folder with the read tools and the web
        // (documents plus what they reference); a general chat stands in a
        // scratch directory with the web alone.
        let (cwd, allowed, system_prompt) = match row.get::<Option<String>, _>("project_path") {
            Some(path) if is_repo => (
                PathBuf::from(path),
                CHAT_ALLOWED_TOOLS
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
                CHAT_SYSTEM_PROMPT,
            ),
            Some(path) => (
                PathBuf::from(path),
                SPACE_CHAT_ALLOWED_TOOLS
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
                SPACE_CHAT_SYSTEM_PROMPT,
            ),
            None => {
                let scratch = eren_shared::brand::home().join("tmp");
                tokio::fs::create_dir_all(&scratch).await?;
                (
                    scratch,
                    crate::runs::research::WEB_TOOLS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    GENERAL_CHAT_SYSTEM_PROMPT,
                )
            }
        };
        // Who is answering. A chat with an agent is that agent, with what it
        // remembers (global memories in a general chat, the project's too in
        // a project chat); a manager thread is its manager; anything else is
        // the assistant alone.
        let persona = match agent_id {
            Some(agent) => {
                let name: String = row.get("agent_name");
                let prompt: String = row
                    .get::<Option<String>, _>("agent_prompt")
                    .unwrap_or_default();
                let mut persona = format!(
                    "You are {name}, one of the agents in this workspace, and the \
                     person is talking to you directly. Answer as {name}."
                );
                if !prompt.trim().is_empty() {
                    persona.push_str(&format!("\n\n{prompt}"));
                }
                let memories = memory::recall(&self.db, agent, project_id)
                    .await
                    .unwrap_or_default();
                if let Some(block) = memory::render(&memories) {
                    persona.push_str(&block);
                }
                Some(persona)
            }
            None => row
                .get::<Option<String>, _>("manager_persona")
                .filter(|p| !p.trim().is_empty()),
        };
        let spec = RunSpec {
            cwd,
            prompt: user_message,
            model_tier: tier,
            model_id,
            effort: chat_effort,
            resume_session_id: session_id.clone(),
            permission_mode: chat_permission_mode(engine.as_ref()),
            // Both halves in plan mode. Dropping the four from `allowed` keeps
            // the assistant from reaching for them; adding them to `denied` is
            // what actually stops the call, because `allowed_tools` is an
            // auto-approval list and not a restriction.
            allowed_tools: if planning {
                crate::runs::chat_plan::without_acting(&allowed)
            } else {
                allowed
            },
            // The agent's (or the manager's) persona, after the assistant's
            // brief and never instead of it. An agent says who it is and how
            // it wants the job done; it does not get to redefine what the
            // tools are or that the mutating ones are denied — a chat stands
            // in the real checkout whoever is talking.
            append_system_prompt: Some(match persona {
                Some(persona) => format!("{system_prompt}\n\n{persona}"),
                None => system_prompt.to_string(),
            }),
            mcp,
            denied_tools: {
                let base: Vec<String> = CHAT_DENIED_TOOLS.iter().map(|s| s.to_string()).collect();
                if planning {
                    crate::runs::chat_plan::with_acting_denied(&base)
                } else {
                    base
                }
            },
            run_key: run_id.to_string(),
            extra_read_dirs,
            permission_prompt_tool: false,
            extra_env: eren_shared::brand::with_legacy_env(HashMap::from([(
                "EREN_CHAT_ID".to_string(),
                chat_id.to_string(),
            )])),
        };

        let seq = SeqAlloc::starting_at(next_seq(&self.db, run_id).await?);
        let outcome = self
            .stream_run(run_id, None, &seq, engine, spec, CallerKind::Chat)
            .await?;

        // The session id is kept whatever the ending, and that is the
        // difference between Stop meaning "that's enough of this answer" and
        // Stop meaning "throw the conversation away". The engine reports it on
        // `RunStarted`, long before the turn finishes, so a cancelled turn has
        // one — and without saving it the next message would open a fresh CLI
        // session with no memory of anything said so far.
        let stopped = outcome.status == RunStatus::Canceled;
        if matches!(outcome.status, RunStatus::Completed | RunStatus::Canceled) {
            if let Some(sid) = &outcome.session_id {
                sqlx::query(
                    "UPDATE chats SET session_id=$1, session_engine=$2, updated_at=now() WHERE id=$3",
                )
                .bind(sid)
                .bind(&engine_id)
                .bind(chat_id)
                .execute(&self.db.pool)
                .await?;
            }
        }

        if outcome.status == RunStatus::Completed || stopped {
            // Did this turn end by asking rather than by answering?
            //
            // In plan mode the assistant is told to call `ask_user` when a
            // choice would change the plan, and that tool ends the turn on
            // purpose. What it leaves behind is a question, not a plan — so
            // marking the reply `is_plan` would put an Approve button under
            // it, and approving sends "carry out the plan exactly as you
            // wrote it" when there is no plan to carry out. On a chat that
            // can create and start cards, that is the expensive kind of
            // wrong.
            let asked: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM chat_questions
                                 WHERE run_id = $1 AND answered_at IS NULL)",
            )
            .bind(run_id)
            .fetch_one(&self.db.pool)
            .await
            .unwrap_or(false);
            let planning = planning && !asked;

            // An older open plan stops being open the moment a newer one
            // lands: two Approve buttons in one thread is an invitation to
            // carry out the same work twice. A stopped turn wrote no plan
            // worth approving, so it supersedes nothing — and neither does a
            // turn that only asked a question, which leaves the previous plan
            // exactly as applicable as it was.
            if planning && !stopped {
                sqlx::query(
                    "UPDATE chat_messages SET plan_outcome = 'superseded'
                     WHERE chat_id = $1 AND is_plan AND plan_outcome IS NULL",
                )
                .bind(chat_id)
                .execute(&self.db.pool)
                .await?;
            }
            sqlx::query(
                "INSERT INTO chat_messages (chat_id, role, content, run_id, is_plan, stopped)
                 VALUES ($1, 'assistant', $2, $3, $4, $5)",
            )
            .bind(chat_id)
            .bind(if !outcome.output.is_empty() {
                outcome.output.clone()
            } else if stopped {
                // Stopped before it said anything. Still a row: the thread
                // has to show that a turn happened and ended, and the settle
                // in the browser waits for a row carrying this run's id.
                "(stopped before the assistant replied)".to_string()
            } else {
                "(no reply)".to_string()
            })
            .bind(run_id)
            // Recorded from the run, which knew: "is this a plan?" cannot be
            // read back out of prose, and the button that carries it out must
            // not appear under a message that merely contains a list.
            .bind(planning)
            // A stopped reply is not a short one, and a reader cannot tell the
            // difference from the text. The assistant's session still holds
            // what it was part-way through saying; the thread has to be honest
            // that what is shown is not all of it.
            .bind(stopped)
            .execute(&self.db.pool)
            .await?;
        } else if outcome.status == RunStatus::Failed {
            let reason = outcome.reason.clone().unwrap_or_default();
            // The CLI no longer has the session this chat resumes — deleted, or
            // gone with a container that kept no ~/.claude. Every later turn
            // would fail the same way, so the chat lets go of it; and says so,
            // because the next turn starts a conversation that has not heard
            // the earlier ones, and quietly forgetting is the thing not to do.
            let lost = session_id.is_some()
                && eren_engines::claude::stream_parser::session_not_found(&reason);
            if lost {
                sqlx::query(
                    "UPDATE chats SET session_id = NULL, session_engine = NULL, updated_at = now()
                     WHERE id = $1 AND session_id = $2",
                )
                .bind(chat_id)
                .bind(&session_id)
                .execute(&self.db.pool)
                .await?;
            }
            sqlx::query(
                "INSERT INTO chat_messages (chat_id, role, content, run_id)
                 VALUES ($1, 'system', $2, $3)",
            )
            .bind(chat_id)
            .bind(if lost {
                format!(
                    "Assistant turn failed: {engine_id} no longer has this conversation \
                     ({reason}). Send your message again to start a new one — it will \
                     not remember what was said above."
                )
            } else {
                format!("Assistant turn failed: {reason}")
            })
            .bind(run_id)
            .execute(&self.db.pool)
            .await?;
        }
        Ok(())
    }

    /// An agent answering an @-mention on a task card.
    ///
    /// Structurally a chat-shaped run: cwd is the real checkout with read-only
    /// tools, so the agent can ground its answer in the code but can never
    /// edit anything from a comment. Thread context and the agent's memories
    /// travel in the prompt — replies are one-shot, not resumed sessions,
    /// because the thread itself is the durable state.
    async fn execute_comment_run(
        self: &Arc<Self>,
        run_id: Uuid,
        comment_id: Uuid,
    ) -> anyhow::Result<()> {
        let row = sqlx::query(
            "SELECT r.engine, r.agent_id, c.task_id, c.content AS mention,
                    t.title, t.prompt AS task_prompt, t.board_column,
                    p.id AS project_id, p.path AS project_path,
                    a.name AS agent_name, a.system_prompt, a.model_tier AS agent_tier,
                    a.effort AS agent_effort
             FROM runs r
             JOIN task_comments c ON c.id = r.comment_id
             JOIN tasks t ON t.id = c.task_id
             JOIN projects p ON p.id = t.project_id
             JOIN agents a ON a.id = r.agent_id
             WHERE r.id = $1",
        )
        .bind(run_id)
        .fetch_one(&self.db.pool)
        .await?;

        self.set_status(run_id, RunStatus::Starting).await?;
        let engine_id: String = row.get("engine");
        let engine = self
            .engine(&engine_id)
            .ok_or_else(|| anyhow::anyhow!("unknown engine {engine_id}"))?;

        let agent_id: Uuid = row.get("agent_id");
        let task_id: Uuid = row.get("task_id");
        let project_id: Uuid = row.get("project_id");
        let agent_name: String = row.get("agent_name");
        let title: String = row.get("title");

        // The thread so far, oldest first, excluding the mention itself —
        // that goes last, as the thing being answered.
        let thread: Vec<(String, Option<String>, String)> = sqlx::query_as(
            "SELECT c.author, a.name, c.content FROM task_comments c
             LEFT JOIN agents a ON a.id = c.agent_id
             WHERE c.task_id = $1 AND c.id <> $2
             ORDER BY c.created_at DESC LIMIT 15",
        )
        .bind(task_id)
        .bind(comment_id)
        .fetch_all(&self.db.pool)
        .await?;

        let memories = memory::recall(&self.db, agent_id, Some(project_id))
            .await
            .unwrap_or_default();

        let mut prompt = format!(
            "You were mentioned in a comment on the task card \"{title}\" \
             (column: {}).\n\nThe task brief:\n{}\n",
            row.get::<String, _>("board_column"),
            row.get::<String, _>("task_prompt"),
        );
        if !thread.is_empty() {
            prompt.push_str("\nThe discussion so far, oldest first:\n");
            for (author, name, content) in thread.iter().rev() {
                let who = match author.as_str() {
                    "agent" => name.clone().unwrap_or_else(|| "agent".into()),
                    "system" => "eren".into(),
                    _ => "user".into(),
                };
                prompt.push_str(&format!("[{who}] {content}\n"));
            }
        }
        if let Some(block) = memory::render(&memories) {
            prompt.push_str(&block);
        }
        // Articles referenced by the card or by this comment specifically —
        // "@agent, see #runbook" has to reach the reply, or the reference is
        // decoration.
        let articles = crate::kb::for_run(&self.db, Some(task_id), Some(comment_id))
            .await
            .unwrap_or_default();
        prompt = crate::kb::augment_prompt(&prompt, &articles);
        // The project's standing context reaches a reply too: an agent
        // answering "where does this live?" on a card should know the same
        // things as one doing the work.
        //
        // Brain, not skill: a reply answers a question, it does not do the
        // job, and this run holds only Read, Grep and Glob. A skill describes
        // how the person wants work done, which is not what is being asked
        // for here.
        prompt = crate::runs::context::Standing::brain_only(
            &self.db,
            Some(row.get::<Uuid, _>("project_id")),
        )
        .await
        .apply(&prompt);
        prompt.push_str(&format!(
            "\nThe comment mentioning you:\n{}\n\n\
             Reply as a comment on this card: concise, concrete, grounded in this \
             repository (use Read/Grep/Glob to check before claiming). You cannot \
             edit files from here — if work is needed, describe it so the user can \
             run the task. Your entire output is posted verbatim as your comment: \
             no preamble, and never remark on tools, task lists, or how the \
             comment gets delivered.",
            row.get::<String, _>("mention"),
        ));

        let tier: ModelTier = serde_json::from_value(serde_json::Value::String(
            row.get::<String, _>("agent_tier"),
        ))
        .unwrap_or_default();
        let model_id = self.model_for(&engine_id, tier);
        sqlx::query("UPDATE runs SET model=$1, started_at=now() WHERE id=$2")
            .bind(&model_id)
            .bind(run_id)
            .execute(&self.db.pool)
            .await?;

        let system_prompt: String = row.get("system_prompt");
        // A teammate replying used to be the one path that ignored everything
        // but its own agent's budget — so a tier set to think hard did nothing
        // in the surface where several agents talk at once.
        let reply_effort = self
            .resolve_effort(
                row.get::<Option<String>, _>("agent_effort")
                    .and_then(|e| ReasoningEffort::parse(&e)),
                None,
                &engine_id,
                tier,
            )
            .await;
        let persona = format!(
            "You are {agent_name}, an agent on this project's kanban board.{}",
            if system_prompt.is_empty() {
                String::new()
            } else {
                format!("\n\n{system_prompt}")
            }
        );

        let spec = RunSpec {
            cwd: PathBuf::from(row.get::<String, _>("project_path")),
            prompt,
            model_tier: tier,
            model_id,
            effort: reply_effort,
            resume_session_id: None,
            permission_mode: PermissionMode::Reviewed,
            // Read-only, and no MCP: a comment reply answers, it doesn't act.
            allowed_tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
            append_system_prompt: Some(persona),
            mcp: McpWiring::default(),
            denied_tools: vec![],
            run_key: run_id.to_string(),
            extra_read_dirs: vec![],
            permission_prompt_tool: false,
            extra_env: eren_shared::brand::with_legacy_env(HashMap::from([(
                "EREN_RUN_ID".to_string(),
                run_id.to_string(),
            )])),
        };

        let seq = SeqAlloc::starting_at(next_seq(&self.db, run_id).await?);
        let outcome = self
            .stream_run(run_id, None, &seq, engine, spec, CallerKind::CommentReply)
            .await?;

        if outcome.status == RunStatus::Completed {
            let reply = if outcome.output.trim().is_empty() {
                "(no reply)".to_string()
            } else {
                outcome.output.clone()
            };
            sqlx::query(
                "INSERT INTO task_comments (task_id, author, agent_id, content, run_id)
                 VALUES ($1, 'agent', $2, $3, $4)",
            )
            .bind(task_id)
            .bind(agent_id)
            .bind(&reply)
            .bind(run_id)
            .execute(&self.db.pool)
            .await?;
            // The exchange becomes memory, so the agent's next run knows it
            // happened.
            let note = format!(
                "On card \"{title}\": asked \"{}\" — I replied: {}",
                row.get::<String, _>("mention"),
                reply
            );
            memory::remember(
                &self.db,
                agent_id,
                Some(project_id),
                Some(task_id),
                "comment_reply",
                &note,
            )
            .await?;
        }
        Ok(())
    }

    /// Execute a workflow: steps run in dependency order, a step's outputs
    /// feed later prompts, and `strategy.parallel` fans a step out into
    /// independent attempts. The whole workflow is one run, so the existing
    /// streaming, replay, and cost tracking apply unchanged.
    async fn execute_workflow_run(
        self: &Arc<Self>,
        run_id: Uuid,
        workflow_id: Uuid,
    ) -> anyhow::Result<()> {
        let row = sqlx::query(
            "SELECT w.source_yaml, w.name, r.engine, r.trigger, r.task_id, p.id AS project_id,
                    p.workspace_id, p.path AS project_path, p.default_branch,
                    p.full_auto_opt_in
             FROM runs r JOIN workflows w ON w.id = r.workflow_id
             JOIN projects p ON p.id = w.project_id
             WHERE r.id = $1",
        )
        .bind(run_id)
        .fetch_one(&self.db.pool)
        .await?;

        self.set_status(run_id, RunStatus::Starting).await?;

        let workflow = Workflow::from_yaml(&row.get::<String, _>("source_yaml"))?;
        // The YAML's `defaults.engine` wins: a scheduled run is queued long
        // before we know which engine the workflow asked for.
        let engine_id = workflow.defaults.engine.clone();
        let trigger: String = row.get("trigger");
        // Resolved here purely to fail fast: a workflow naming an engine this
        // machine doesn't have should die before it creates a worktree, not
        // three steps in. Each step resolves its own below.
        self.engine(&engine_id)
            .ok_or_else(|| anyhow::anyhow!("unknown engine {engine_id}"))?;
        sqlx::query("UPDATE runs SET engine=$1 WHERE id=$2")
            .bind(&engine_id)
            .bind(run_id)
            .execute(&self.db.pool)
            .await?;
        let project_path = PathBuf::from(row.get::<String, _>("project_path"));
        let default_branch: String = row.get("default_branch");
        let workspace_id: Uuid = row.get("workspace_id");
        let full_auto_opt_in: bool = row.get("full_auto_opt_in");

        // Shared worktree: sequential stages build on each other's work.
        // Fan-out steps that ask for isolation get their own.
        let task_id: Option<Uuid> = row.get("task_id");
        let shared = self
            .worktree_for_run(
                run_id,
                task_id,
                &project_path,
                &default_branch,
                &slugify(&workflow.name),
            )
            .await?;

        // Read once for the whole pipeline, not per step. A workflow is one
        // decision, and a Brain edited while a six-step run is in flight
        // should not leave the second half of it working from different
        // facts than the first.
        let standing = crate::runs::context::Standing::brain_only(
            &self.db,
            Some(row.get::<Uuid, _>("project_id")),
        )
        .await;

        let seq = SeqAlloc::starting_at(next_seq(&self.db, run_id).await?);
        let mut outputs = StepOutputs::new();
        // Session ids, tagged with the engine that minted them: a `continue`
        // step whose dependency ran elsewhere must start fresh rather than
        // hand an `ses_…` to a CLI that has never seen it.
        let mut sessions: HashMap<String, (String, String)> = HashMap::new();
        let mut failure: Option<String> = None;

        'layers: for layer in workflow.layers()? {
            for step_id in layer {
                // Same reason as the org executor: a cancel has to stop the
                // pipeline, not just whichever step was mid-flight.
                if self.cancel_requested(run_id) {
                    failure = Some("canceled".to_string());
                    break 'layers;
                }
                // A budget spent since the pipeline started stops it before
                // the next step spends more.
                if let Err(over) = self.budget_allows_more(run_id).await {
                    failure = Some(over.to_string());
                    break 'layers;
                }
                let step = workflow
                    .step(&step_id)
                    .ok_or_else(|| anyhow::anyhow!("missing step {step_id}"))?;

                let agent = self.load_agent(workspace_id, step.agent.as_deref()).await?;
                // The step's agent has a budget of its own.
                if let Some(a) = &agent {
                    let own = crate::budgets::Scope {
                        agent: Some(a.id),
                        ..Default::default()
                    };
                    if let Err(over) = crate::budgets::check(&self.db, &own, false).await {
                        failure = Some(over.to_string());
                        break 'layers;
                    }
                }
                // A step may name its own engine, and so may the agent bound
                // to it; the workflow's default is what they fall back to.
                let step_engine_id = step
                    .engine
                    .clone()
                    .or_else(|| agent.as_ref().and_then(|a| a.engine.clone()))
                    .unwrap_or_else(|| engine_id.clone());
                let step_engine = self
                    .engine(&step_engine_id)
                    .ok_or_else(|| anyhow::anyhow!("unknown engine {step_engine_id}"))?;
                let model_id = workflow.resolve_model(
                    step,
                    agent.as_ref().map(|a| a.tier),
                    &self.tier_mapping().for_engine(&step_engine_id),
                );
                let prompt = eren_shared::interpolate(&step.prompt, &outputs);

                let resolved = self.workflow_permission_mode(
                    &workflow,
                    agent.as_ref(),
                    full_auto_opt_in,
                    &shared.path,
                );
                let permission_mode = resolved.mode;

                // Nobody is at the keyboard at 3am. Parking no longer freezes
                // the queue — a parked run lends its slot back — but a
                // scheduled step that stops to ask would still burn tokens
                // getting to the question, wait out the whole attention
                // window, and then be cancelled unanswered. Refusing at
                // dispatch is the same outcome for free, and it says so.
                //
                // Manual runs still park, and that is now a real offer rather
                // than an assumption: someone chose to start them, the card
                // says plainly that it is waiting, and the hook can go and
                // tell them.
                if trigger == "schedule" && permission_mode == PermissionMode::Reviewed {
                    anyhow::bail!(
                        "{}",
                        if resolved.downgraded {
                            format!(
                                "step {step_id} asks for Don't-ask, which this project hasn't \
opted into, so it falls back to asking permission — and a scheduled run has nobody to ask. \
Turn on \"Don't ask\" for the project, or give the step Auto-edit."
                            )
                        } else {
                            format!(
                                "step {step_id} runs in Reviewed mode, which needs someone to \
approve each tool call, and a scheduled run has nobody to ask. Give it Auto-edit, or run \
this workflow manually."
                            )
                        }
                    );
                }

                // Resume the session of the step we depend on, so a
                // "continue" stage keeps the prior stage's context.
                let resume = if step.session == SessionMode::Continue {
                    step.needs
                        .first()
                        .and_then(|n| sessions.get(n))
                        .filter(|(engine, _)| engine == &step_engine_id)
                        .map(|(_, sid)| sid.clone())
                } else {
                    None
                };

                // Standing context, and both halves of *when* are load bearing.
                //
                // Only on a step that starts fresh. A `continue` step is the
                // same conversation as the one it follows and already has it;
                // appending again pays twice and puts a second "read this as
                // background" fence in one context, which is how a framing
                // stops being read as a framing.
                //
                // And strictly after `interpolate`, which splices the previous
                // steps' *model-generated* output into every `{{ … }}` in the
                // whole string. Augment first and that output can land inside
                // the brain's fence, where nothing has neutralised it — the
                // fence is scrubbed against the body as it stood when the
                // block was built, not against text spliced in afterwards.
                //
                // Brain only: `workflow::Step` has no `skill` field, and a
                // Skill is named, never inferred.
                let prompt = match resume {
                    None => standing.apply(&prompt),
                    Some(_) => prompt,
                };

                // Refuse a step this engine can't honour, before it runs.
                if let Err(reason) =
                    eren_engines::vet(step_engine.as_ref(), permission_mode, resume.is_some())
                {
                    anyhow::bail!("step {step_id}: {reason}");
                }

                // Steps used to get no MCP wiring at all while still asking
                // for `permission_prompt_tool` — so a step that did stop to
                // ask pointed the CLI at a server that wasn't there, and the
                // agent's own connections were silently unavailable.
                let user_servers = crate::mcp_servers::for_agent(
                    &self.db,
                    agent.as_ref().map(|a| a.id),
                )
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(%run_id, %step_id, error=%e, "could not load step MCP servers");
                    vec![]
                });
                let mcp = McpWiring {
                    eren_url: self
                        .mcp_base_url
                        .as_ref()
                        .map(|b| format!("{b}/mcp/run/{run_id}")),
                    servers: user_servers.iter().map(|s| s.to_spec()).collect(),
                };
                // Same rule as task runs: an empty list means "no flag",
                // which allows everything, so only extend one that exists.
                let mut step_tools = agent
                    .as_ref()
                    .map(|a| a.allowed_tools.clone())
                    .unwrap_or_default();
                if !step_tools.is_empty() {
                    step_tools.extend(user_servers.iter().map(|s| s.tool_prefix()));
                    step_tools.push("mcp__eren".to_string());
                }

                let attempts = step.parallelism();
                let mut plans = Vec::with_capacity(attempts);
                for index in 0..attempts {
                    let step_key = if attempts > 1 {
                        format!("{step_id}#{}", index + 1)
                    } else {
                        step_id.clone()
                    };
                    let cwd = if step.isolated_worktrees() && attempts > 1 {
                        self.worktrees
                            .create(
                                &project_path,
                                &default_branch,
                                Uuid::new_v4(),
                                &format!("{}-{}", slugify(&step_key), index + 1),
                            )
                            .await?
                            .path
                    } else {
                        shared.path.clone()
                    };
                    let db_step_id = self.create_step_row(run_id, &step_key).await?;
                    plans.push((db_step_id, step_key, cwd));
                }

                // Opportunistic parallelism: this run already holds one queue
                // permit; extra ones are taken only if immediately free, so a
                // fan-out can never deadlock against another workflow.
                let extra: Vec<_> = (1..plans.len())
                    .filter_map(|_| self.slots.sem().try_acquire_owned().ok())
                    .collect();
                let concurrency = 1 + extra.len();
                // Read once for the fan-out rather than per step: it is the
                // same setting for all of them, and the closure below is not
                // async.
                let tool_timeout_ms = self.mcp_tool_timeout_ms().await;

                let results =
                    futures::stream::iter(plans.into_iter().map(|(db_step_id, step_key, cwd)| {
                        let this = self.clone();
                        let engine = step_engine.clone();
                        let seq = seq.clone();
                        let tool_timeout_ms = tool_timeout_ms.clone();
                        let spec = RunSpec {
                            cwd,
                            prompt: prompt.clone(),
                            model_tier: agent.as_ref().map(|a| a.tier).unwrap_or_default(),
                            model_id: model_id.clone(),
                            effort: agent.as_ref().and_then(|a| a.effort),
                            resume_session_id: resume.clone(),
                            permission_mode,
                            allowed_tools: step_tools.clone(),
                            append_system_prompt: agent.as_ref().and_then(|a| {
                                (!a.system_prompt.is_empty()).then(|| a.system_prompt.clone())
                            }),
                            denied_tools: vec![],
                            mcp: mcp.clone(),
                            // Per step, not per run: concurrent steps would
                            // otherwise write the same scratch config path.
                            run_key: db_step_id.to_string(),
                            extra_read_dirs: vec![],
                            permission_prompt_tool: true,
                            extra_env: eren_shared::brand::with_legacy_env(HashMap::from([
                                ("EREN_RUN_ID".to_string(), run_id.to_string()),
                                ("EREN_STEP".to_string(), step_key.clone()),
                                ("MCP_TOOL_TIMEOUT".to_string(), tool_timeout_ms.clone()),
                            ])),
                        };
                        async move {
                            let outcome = this
                                .stream_run(
                                    run_id,
                                    Some(db_step_id),
                                    &seq,
                                    engine,
                                    spec,
                                    CallerKind::WorkflowStep,
                                )
                                .await;
                            (db_step_id, step_key, outcome)
                        }
                    }))
                    .buffer_unordered(concurrency)
                    .collect::<Vec<_>>()
                    .await;
                drop(extra);

                let mut step_outputs = Vec::with_capacity(results.len());
                for (db_step_id, step_key, outcome) in results {
                    let outcome = outcome?;
                    self.finish_step_row(db_step_id, &engine_id, &outcome)
                        .await?;
                    if outcome.status != RunStatus::Completed {
                        failure = Some(format!(
                            "step '{step_key}' {}: {}",
                            outcome.status.as_str(),
                            outcome.reason.clone().unwrap_or_default()
                        ));
                        break 'layers;
                    }
                    if let Some(sid) = outcome.session_id.clone() {
                        sessions
                            .entry(step_id.clone())
                            .or_insert((step_engine_id.clone(), sid));
                    }
                    step_outputs.push(outcome.output);
                }
                outputs.insert(step_id.clone(), step_outputs);
            }
        }

        if failure.as_deref() == Some("canceled") {
            self.finish(run_id, RunStatus::Canceled, None).await?;
            self.settle_task_for_run(task_id, RunStatus::Canceled)
                .await?;
            return Ok(());
        }
        let status = match failure {
            None => {
                self.finish(run_id, RunStatus::Completed, None).await?;
                sqlx::query("UPDATE workflows SET last_run_at = now() WHERE id = $1")
                    .bind(workflow_id)
                    .execute(&self.db.pool)
                    .await?;
                RunStatus::Completed
            }
            Some(reason) => {
                self.finish(run_id, RunStatus::Failed, Some(reason)).await?;
                RunStatus::Failed
            }
        };
        // A pipeline launched from a board task lands on review like any
        // other task run.
        self.settle_task_for_run(task_id, status).await?;
        Ok(())
    }

    /// Store the plan and stop, releasing the queue slot.
    ///
    /// Returning is the point: a run that sat here waiting would hold its
    /// concurrency permit for however long a person takes to read, and with a
    /// small default that is most of the queue. Approval re-queues it, and the
    /// phase decision then sends it to work.
    async fn park_for_approval(
        self: &Arc<Self>,
        run_id: Uuid,
        outcome: &StreamOutcome,
    ) -> anyhow::Result<()> {
        // Stopped by a person is not a planning failure, and must not read as
        // "produced no plan".
        if outcome.status == RunStatus::Canceled {
            self.finish(run_id, RunStatus::Canceled, None).await?;
            return Ok(());
        }
        // A plan that never arrived is a failed run, not an empty approval
        // prompt — there is nothing for anyone to say yes to.
        if outcome.status != RunStatus::Completed || outcome.output.trim().is_empty() {
            let reason = outcome.reason.clone().unwrap_or_else(|| {
                "the planning pass produced no plan, so there is nothing to approve".to_string()
            });
            self.finish(run_id, RunStatus::Failed, Some(reason)).await?;
            return Ok(());
        }

        // Replace rather than accumulate: a revised plan supersedes the one
        // that was sent back, and two 'plan' rows would make `stored_plan`
        // return whichever the database felt like.
        sqlx::query("DELETE FROM steps WHERE run_id = $1 AND step_key = 'plan'")
            .bind(run_id)
            .execute(&self.db.pool)
            .await?;
        sqlx::query(
            "INSERT INTO steps (run_id, step_key, status, session_id, session_engine,
                                output_text, started_at, finished_at)
             VALUES ($1, 'plan', 'completed', $2, $3, $4, now(), now())",
        )
        .bind(run_id)
        .bind(&outcome.session_id)
        .bind(
            sqlx::query_scalar::<_, String>("SELECT engine FROM runs WHERE id=$1")
                .bind(run_id)
                .fetch_one(&self.db.pool)
                .await?,
        )
        .bind(&outcome.output)
        .execute(&self.db.pool)
        .await?;

        // The card stays in "running": it is not in review — there is no diff
        // — and certainly not done. The activity view already sorts
        // awaiting_approval to the top and lists it as a blocker.
        self.set_status(run_id, RunStatus::AwaitingApproval).await?;
        let ctx = crate::attention::Ctx {
            title: "eren: a plan needs your review".to_string(),
            ..crate::attention::ctx_for_run(&self.db, run_id, None).await
        };
        crate::attention::fire(&self.db, crate::attention::Event::Plan, ctx).await;
        Ok(())
    }

    /// The plan this run has on file, if any.
    ///
    /// A `steps` row rather than a column on `runs`: it is a thing the agent
    /// produced, with a session behind it, and organizations already store
    /// their plans exactly this way.
    async fn stored_plan(&self, run_id: Uuid) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, Option<String>>(
            "SELECT output_text FROM steps
             WHERE run_id = $1 AND step_key = 'plan' AND status = 'completed'",
        )
        .bind(run_id)
        .fetch_optional(&self.db.pool)
        .await?
        .flatten()
        .filter(|t| !t.trim().is_empty()))
    }

    /// Outstanding feedback on a rejected plan, consumed as it is read.
    ///
    /// Cleared here rather than by the route, so a revise request survives a
    /// crash between asking and dispatching but can never be replayed into a
    /// second pass it wasn't meant for.
    async fn plan_revision_note(&self, run_id: Uuid) -> anyhow::Result<Option<String>> {
        // A CTE, because `RETURNING` hands back the *new* row — reading the
        // column it was just cleared to would always be null.
        Ok(sqlx::query_scalar::<_, Option<String>>(
            "WITH prev AS (SELECT plan_note FROM runs WHERE id = $1)
             UPDATE runs SET plan_note = NULL WHERE id = $1
             RETURNING (SELECT plan_note FROM prev)",
        )
        .bind(run_id)
        .fetch_optional(&self.db.pool)
        .await?
        .flatten()
        .filter(|n| !n.trim().is_empty()))
    }

    /// Did a person rewrite the plan, rather than approve what was proposed?
    async fn plan_was_edited(&self, run_id: Uuid) -> anyhow::Result<bool> {
        Ok(
            sqlx::query_scalar::<_, bool>("SELECT plan_edited FROM runs WHERE id = $1")
                .bind(run_id)
                .fetch_optional(&self.db.pool)
                .await?
                .unwrap_or(false),
        )
    }

    /// The session the planning pass ran in, so the work pass can pick up the
    /// context it built while reading the code rather than re-reading it all.
    async fn plan_session(&self, run_id: Uuid) -> anyhow::Result<Option<(String, String)>> {
        let row = sqlx::query(
            "SELECT session_id, session_engine FROM steps
             WHERE run_id = $1 AND step_key = 'plan'",
        )
        .bind(run_id)
        .fetch_optional(&self.db.pool)
        .await?;
        Ok(row.and_then(|r| {
            match (
                r.get::<Option<String>, _>("session_id"),
                r.get::<Option<String>, _>("session_engine"),
            ) {
                (Some(sid), Some(engine)) => Some((sid, engine)),
                _ => None,
            }
        }))
    }

    async fn create_step_row(&self, run_id: Uuid, step_key: &str) -> anyhow::Result<Uuid> {
        let row = sqlx::query(
            "INSERT INTO steps (run_id, step_key, status, started_at)
             VALUES ($1, $2, 'running', now()) RETURNING id",
        )
        .bind(run_id)
        .bind(step_key)
        .fetch_one(&self.db.pool)
        .await?;
        Ok(row.get("id"))
    }

    async fn finish_step_row(
        &self,
        step_id: Uuid,
        engine_id: &str,
        outcome: &StreamOutcome,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE steps SET status=$1, session_id=$2, session_engine=$5, output_text=$3,
             finished_at=now() WHERE id=$4",
        )
        .bind(outcome.status.as_str())
        .bind(&outcome.session_id)
        .bind(&outcome.output)
        .bind(step_id)
        .bind(engine_id)
        .execute(&self.db.pool)
        .await?;
        Ok(())
    }

    /// What is known about a card before its run starts, for the auto tier.
    ///
    /// One round trip, and every signal is structural — how much was attached,
    /// how much was written, what happened last time. Deliberately *not* the
    /// project's historical cost: that measures the model previously used, so
    /// routing on it would close a loop where cheap runs keep justifying the
    /// cheap tier. It belongs on the spend page as context, not in here.
    ///
    /// Best-effort: a signal query that fails must not fail the run, so a
    /// failure yields empty signals, which classify to Medium — the same tier
    /// the card would have had before any of this existed.
    async fn tier_signals(
        &self,
        run_id: Uuid,
        task_id: Uuid,
        run: &sqlx::postgres::PgRow,
    ) -> eren_shared::TierSignals {
        let title: String = run.get("title");
        let prompt: String = run.get("prompt");
        let mut signals = eren_shared::TierSignals {
            brief_chars: title.chars().count() + prompt.chars().count(),
            replans: run.try_get("replans").unwrap_or(0),
            ..Default::default()
        };

        let row = sqlx::query(
            "SELECT (SELECT count(*) FROM attachments   WHERE task_id = $1) AS attachments,
                    (SELECT count(*) FROM task_articles WHERE task_id = $1) AS articles,
                    (SELECT status        FROM runs WHERE task_id = $1 AND id <> $2
                      ORDER BY created_at DESC LIMIT 1) AS prior_status,
                    (SELECT tier_resolved FROM runs WHERE task_id = $1 AND id <> $2
                       AND tier_resolved IS NOT NULL
                      ORDER BY created_at DESC LIMIT 1) AS prior_tier",
        )
        .bind(task_id)
        .bind(run_id)
        .fetch_optional(&self.db.pool)
        .await;

        match row {
            Ok(Some(r)) => {
                signals.attachments = r.get::<i64, _>("attachments").max(0) as usize;
                signals.kb_articles = r.get::<i64, _>("articles").max(0) as usize;
                signals.prior_failed = matches!(
                    r.get::<Option<String>, _>("prior_status").as_deref(),
                    Some("failed")
                );
                signals.prior_tier = r
                    .get::<Option<String>, _>("prior_tier")
                    .and_then(|t| TierChoice::parse(&t))
                    .and_then(TierChoice::fixed);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(%run_id, error = %e, "auto tier signals unavailable"),
        }
        signals
    }

    /// Apply one step's token delta to the run, and to the step row when there
    /// is one.
    ///
    /// Additive rather than absolute so a fan-out's parallel steps can each
    /// write their own share of the same run without reading first — and
    /// therefore without clobbering each other.
    ///
    /// `provisional` marks figures no final engine message ever reconciled. It
    /// is OR-ed in, never cleared: one estimated step makes the run's total an
    /// estimate, and a later exact step does not make the earlier guess true.
    async fn flush_usage(
        &self,
        run_id: Uuid,
        step_id: Option<Uuid>,
        delta: UsageDelta,
        provisional: bool,
    ) -> anyhow::Result<()> {
        if delta.is_zero() && !provisional {
            return Ok(());
        }
        sqlx::query(
            "UPDATE runs SET input_tokens          = input_tokens          + $2,
                             output_tokens         = output_tokens         + $3,
                             cache_read_tokens     = cache_read_tokens     + $4,
                             cache_creation_tokens = cache_creation_tokens + $5,
                             tokens_provisional    = tokens_provisional OR $6
             WHERE id=$1",
        )
        .bind(run_id)
        .bind(delta.input)
        .bind(delta.output)
        .bind(delta.cache_read)
        .bind(delta.cache_creation)
        .bind(provisional)
        .execute(&self.db.pool)
        .await?;

        if let Some(step_id) = step_id {
            sqlx::query(
                "UPDATE steps SET input_tokens          = input_tokens          + $2,
                                  output_tokens         = output_tokens         + $3,
                                  cache_read_tokens     = cache_read_tokens     + $4,
                                  cache_creation_tokens = cache_creation_tokens + $5
                 WHERE id=$1",
            )
            .bind(step_id)
            .bind(delta.input)
            .bind(delta.output)
            .bind(delta.cache_read)
            .bind(delta.cache_creation)
            .execute(&self.db.pool)
            .await?;
        }
        Ok(())
    }

    pub(crate) async fn load_agent(
        &self,
        workspace_id: Uuid,
        name: Option<&str>,
    ) -> anyhow::Result<Option<BoundAgent>> {
        let Some(name) = name else { return Ok(None) };
        let row = sqlx::query(
            "SELECT id, system_prompt, model_tier, effort, allowed_tools, permission_preset,
                    engine
             FROM agents WHERE workspace_id=$1 AND name=$2",
        )
        .bind(workspace_id)
        .bind(name)
        .fetch_optional(&self.db.pool)
        .await?;
        // A workflow outlives the click that started it; an agent paused
        // since then does not take its next step.
        if let Some(r) = &row {
            crate::agents::assert_can_run(&self.db, &[r.get("id")]).await?;
        }
        Ok(row.map(|r| BoundAgent {
            id: r.get("id"),
            system_prompt: r.get("system_prompt"),
            // An agent cannot be set to `auto` from the UI yet; if one ever is,
            // it resolves to Medium here rather than falling through an
            // unwrap_or_default that would read as a deliberate choice.
            tier: TierChoice::parse(&r.get::<String, _>("model_tier"))
                .and_then(TierChoice::fixed)
                .unwrap_or_default(),
            effort: r
                .get::<Option<String>, _>("effort")
                .and_then(|e| ReasoningEffort::parse(&e)),
            allowed_tools: {
                let mut tools: Vec<String> = r.get("allowed_tools");
                eren_shared::brand::rename_tools(&mut tools);
                tools
            },
            permission_preset: r.get("permission_preset"),
            engine: r.get("engine"),
        }))
    }

    /// Workflow steps default to auto-edit (they run unattended in a
    /// worktree); FullAuto still requires the project opt-in and a managed
    /// worktree, same gate as task runs.
    fn workflow_permission_mode(
        &self,
        workflow: &Workflow,
        agent: Option<&BoundAgent>,
        full_auto_opt_in: bool,
        cwd: &std::path::Path,
    ) -> StepPermission {
        let spec = agent
            .and_then(|a| a.permission_preset.clone())
            .or_else(|| workflow.defaults.permission_mode.clone());
        let asked = spec
            .and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok())
            .unwrap_or(PermissionMode::AutoEdit);
        resolve_step_permission(asked, full_auto_opt_in && self.worktrees.manages(cwd))
    }

    /// Shared streaming loop: spawn the engine, persist+publish every event,
    /// handle cancel / rate-limit / terminal transitions.
    ///
    /// `step_id` tags events for pipeline steps. `caller` answers both of the
    /// questions the ending depends on — who owns the run's final status, and
    /// whether a rate limit can be waited out or has to be reported — which
    /// used to be one `bool` answering only the first.
    pub(crate) async fn stream_run(
        self: &Arc<Self>,
        run_id: Uuid,
        step_id: Option<Uuid>,
        seq: &SeqAlloc,
        engine: Arc<dyn Engine>,
        spec: RunSpec,
        caller: CallerKind,
    ) -> anyhow::Result<StreamOutcome> {
        let finalize = caller.finalizes();
        // The status write is the gate the spawn passes through, not a note
        // made after it. A run that ended while this step was being prepared
        // — cancelled before any process existed to interrupt — starts
        // nothing, rather than spawning a CLI only to kill it.
        if !self.set_status(run_id, RunStatus::Running).await? {
            return Ok(StreamOutcome {
                status: RunStatus::Canceled,
                reason: None,
                output: String::new(),
                session_id: None,
            });
        }
        let mut proc = engine.start(spec)?;

        // Registered under the run, with a per-step slot so a fan-out's
        // steps don't clobber each other. A cancel that arrived while this
        // step was starting is honoured immediately rather than lost.
        let step_key = step_id.unwrap_or(run_id);
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        {
            let mut cancels = self.cancels.lock().unwrap();
            let state = cancels.entry(run_id).or_default();
            let already = state.requested;
            state.steps.insert(step_key, cancel_tx);
            if already {
                // Asked to stop while this step was starting: fire it now
                // rather than letting the request fall through the gap.
                if let Some(tx) = state.steps.remove(&step_key) {
                    let _ = tx.send(());
                }
            }
        }

        let mut outcome: Option<(RunStatus, Option<String>)> = None;
        let mut text_parts: Vec<String> = vec![];
        let mut result_text = String::new();
        let mut session_id: Option<String> = None;
        // Both adapters report usage twice — per message as they go, and
        // authoritatively at the end. The tally reconciles the two so the row
        // is neither double-counted nor left at zero when no final message
        // arrives; see `usage_tally` for why that is not just a sum.
        let mut tally = UsageTally::default();
        // How far a `stop` budget lets this run go, in output tokens. Read
        // once: it moves only as other runs finish, and a run cannot see
        // those mid-stream anyway. Dollars have no equivalent — no engine
        // says what a run cost until it ends.
        let token_limit = crate::budgets::token_headroom(&self.db, run_id, step_id).await;
        loop {
            tokio::select! {
                _ = &mut cancel_rx => {
                    let _ = proc.interrupt().await;
                    outcome = Some((RunStatus::Canceled, None));
                    break;
                }
                event = proc.events.recv() => {
                    let Some(event) = event else { break };
                    self.persist_and_publish(run_id, step_id, seq.next(), &event).await?;
                    self.touch(run_id).await;
                    match &event {
                        ErenEvent::AssistantText { text } => text_parts.push(text.clone()),
                        ErenEvent::RunStarted { session_id: sid, .. } => {
                            if let Some(sid) = sid {
                                session_id = Some(sid.clone());
                                sqlx::query(
                                    "UPDATE runs SET session_id=$1, session_engine=$2 WHERE id=$3",
                                )
                                    .bind(sid).bind(engine.id()).bind(run_id)
                                    .execute(&self.db.pool).await?;
                            }
                        }
                        ErenEvent::RunCompleted { session_id: sid, cost_usd, usage, result_text: rt } => {
                            session_id = Some(sid.clone());
                            result_text = rt.clone();
                            // The engine's own figures replace whatever the
                            // mid-run telemetry estimated. Tokens are written
                            // once after the loop, so this only adopts them.
                            tally.adopt(usage);
                            // Costs accumulate: a workflow run has many steps.
                            sqlx::query(
                                "UPDATE runs SET session_id=$1, session_engine=$4,
                                 cost_usd = CASE WHEN $2::float8 IS NULL THEN cost_usd
                                                 ELSE COALESCE(cost_usd, 0) + $2::float8 END
                                 WHERE id=$3")
                                .bind(sid)
                                .bind(cost_usd)
                                .bind(run_id)
                                .bind(engine.id())
                                .execute(&self.db.pool).await?;
                            if let Some(step_id) = step_id {
                                sqlx::query(
                                    // No price is no price: an engine that never
                                    // reports one leaves the step unpriced rather
                                    // than free, so a dollar budget is not told
                                    // it cost nothing.
                                    "UPDATE steps SET cost_usd = CASE WHEN $2::float8 IS NULL THEN cost_usd
                                                                      ELSE COALESCE(cost_usd, 0) + $2::float8 END
                                      WHERE id=$1")
                                    .bind(step_id)
                                    .bind(cost_usd)
                                    .execute(&self.db.pool).await?;
                            }
                            outcome = Some((RunStatus::Completed, None));
                        }
                        ErenEvent::UsageUpdated { usage } => {
                            // Only an estimate until a final message arrives —
                            // but the only figures a cancelled run will ever
                            // have, which is why they are kept at all.
                            tally.observe(usage);
                            if let Some((limit, policy)) = &token_limit {
                                if tally.output_tokens() > *limit {
                                    let _ = proc.interrupt().await;
                                    outcome = Some((
                                        RunStatus::Failed,
                                        Some(format!(
                                            "stopped by budget \u{201c}{policy}\u{201d}: it allows {limit} more output tokens and this run went past them"
                                        )),
                                    ));
                                    break;
                                }
                            }
                        }
                        ErenEvent::RunFailed { reason } => {
                            outcome = Some((RunStatus::Failed, Some(reason.clone())));
                        }
                        ErenEvent::UsageStatus {
                            limit_type,
                            status,
                            resets_at,
                            using_overage,
                        } => {
                            // Telemetry, never an outcome — it must not touch
                            // `outcome`, or a healthy ping would end the run.
                            if let Err(e) = crate::usage::record(
                                &self.db,
                                engine.id(),
                                limit_type,
                                status,
                                *resets_at,
                                *using_overage,
                            )
                            .await
                            {
                                tracing::warn!(error=%e, "could not record plan usage");
                            }
                        }
                        ErenEvent::RateLimited { reset_at, message } => {
                            // Held or failed, never both, and never a queue
                            // row without the status that explains it — see
                            // `CallerKind::on_rate_limit`.
                            outcome = Some(match caller.on_rate_limit() {
                                OnRateLimit::Hold => {
                                    self.hold_rate_limited(run_id, *reset_at).await?;
                                    (RunStatus::RateLimited, Some(message.clone()))
                                }
                                OnRateLimit::Fail => (
                                    RunStatus::Failed,
                                    Some(format!("{message} (this run can't be held and resumed, so it stopped here)")),
                                ),
                            });
                            let ctx = crate::attention::Ctx {
                                title: "eren: rate limited".to_string(),
                                ..crate::attention::ctx_for_run(&self.db, run_id, None).await
                            };
                            crate::attention::fire(
                                &self.db,
                                crate::attention::Event::RateLimited,
                                ctx,
                            )
                            .await;
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(state) = self.cancels.lock().unwrap().get_mut(&run_id) {
            state.steps.remove(&step_key);
        }

        // The step's tokens, written exactly once, whichever way it ended. A
        // run that was cancelled or whose engine died never sent a final
        // message, and used to record zero — so an interrupted session showed
        // as free, and the daily budget under-counted it to match.
        let provisional = tally.is_provisional();
        let delta = tally.take_delta();
        if let Err(e) = self.flush_usage(run_id, step_id, delta, provisional).await {
            // Never fail a run over its own bookkeeping.
            tracing::warn!(%run_id, error = %e, "could not record token usage");
        }

        let (status, reason) = outcome.unwrap_or((
            RunStatus::Failed,
            Some("event stream ended unexpectedly".into()),
        ));
        // A held run is already `rate_limited` with a queue row behind it, put
        // there together by `hold_rate_limited`; finishing it here would strand
        // that row under a terminal status, which is the leak this slice is
        // about. Every other ending is the caller's to record, if it owns one.
        if finalize && status != RunStatus::RateLimited {
            self.finish(run_id, status, reason.clone()).await?;
        }
        let output = if result_text.is_empty() {
            text_parts.join("\n")
        } else {
            result_text
        };
        Ok(StreamOutcome {
            status,
            reason,
            output,
            session_id,
        })
    }

    /// Put a rate-limited run back on the queue behind a backoff, and mark it
    /// held — the two halves written together, in the one place either is
    /// written.
    ///
    /// They used to be separate: the queue row went in here, unconditionally,
    /// while the status was set by `stream_run` only when it was finalizing.
    /// Every caller that did not finalize therefore left a queue row under a
    /// run that never said it was waiting, and `finish` did not clean up after
    /// it. `claim_next` popped it five minutes later and re-ran the whole
    /// pipeline, charged in full.
    ///
    /// The counter climbs first so the *first* hold reads attempt 0. It lives
    /// on `runs` because `claim_next` is a `DELETE … RETURNING run_id` — a
    /// column on `queue` would have to be threaded through five signatures to
    /// reach this one number — and because a resumed or retried run is a new
    /// row, so it resets with no reset logic.
    async fn hold_rate_limited(
        &self,
        run_id: Uuid,
        reset_at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        let holds: i32 = sqlx::query_scalar(
            "UPDATE runs SET status='rate_limited', rate_limit_attempts = rate_limit_attempts + 1
             WHERE id=$1 RETURNING rate_limit_attempts",
        )
        .bind(run_id)
        .fetch_one(&self.db.pool)
        .await?;
        let not_before = rate_limit_backoff(crate::queue::attempt_index(holds), reset_at);
        sqlx::query(
            "INSERT INTO queue (run_id, priority, not_before) VALUES ($1, 5, $2)
             ON CONFLICT (run_id) DO UPDATE SET not_before = EXCLUDED.not_before",
        )
        .bind(run_id)
        .bind(not_before)
        .execute(&self.db.pool)
        .await?;
        Ok(())
    }

    /// Move a live run to `status`. Returns false, writing nothing, when the
    /// run has already ended.
    ///
    /// Ended is final. The cancel route closes out a run that has no process
    /// yet, and the executor preparing it writes `starting` and `running` a
    /// moment later; unguarded, those writes flipped a run the person had just
    /// stopped back to live, until the cancel flag caught up with it.
    pub(crate) async fn set_status(&self, run_id: Uuid, status: RunStatus) -> anyhow::Result<bool> {
        let moved = sqlx::query(
            "UPDATE runs SET status=$1
              WHERE id=$2 AND status NOT IN ('completed','failed','canceled')",
        )
        .bind(status.as_str())
        .bind(run_id)
        .execute(&self.db.pool)
        .await?;
        Ok(moved.rows_affected() > 0)
    }

    /// Close out a run that has no process to interrupt — the other half of
    /// [`Self::cancel`], through the same door every other ending uses.
    pub async fn cancel_idle(&self, run_id: Uuid) -> anyhow::Result<()> {
        self.finish(run_id, RunStatus::Canceled, None).await
    }

    pub(crate) async fn finish(
        &self,
        run_id: Uuid,
        status: RunStatus,
        reason: Option<String>,
    ) -> anyhow::Result<()> {
        // `finish` means ended. `rate_limited` is a *held* run — it is coming
        // back — and writing it through here would set `finished_at` on a run
        // that has not finished and delete the queue row that brings it back.
        // Debug-assert rather than return an error, because the callers that
        // could get this wrong are inside this file and this is a programming
        // mistake, not a runtime condition.
        debug_assert_ne!(
            status,
            RunStatus::RateLimited,
            "a held run has not finished"
        );
        self.forget_cancel(run_id);
        let mut tx = self.db.pool.begin().await?;
        // `COALESCE`, because a cancel carries `reason: None` and the broker
        // may already have written the true one — "nobody answered the request
        // to allow Bash". Safe only because `unpark` clears the column, so a
        // run that parked and then finished cleanly reports nothing.
        //
        // And only a live run: the first ending wins. A cancel that closed the
        // run out is not rewritten as `failed` by the executor tripping over
        // the run it was preparing — and whatever follows settles steps and
        // announces by the status the run actually has.
        sqlx::query(
            "UPDATE runs SET status=$1, error_reason=COALESCE($2, error_reason),
             finished_at=now() WHERE id=$3 AND status NOT IN ('completed','failed','canceled')",
        )
        .bind(status.as_str())
        .bind(reason)
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
        let status = sqlx::query_scalar::<_, String>("SELECT status FROM runs WHERE id=$1")
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?
            .as_deref()
            .and_then(RunStatus::parse)
            .unwrap_or(status);
        // Nothing waiting to be dispatched can outlive the run it belongs to.
        // A run reaches here from a cancel, a crash in `execute`, a failed
        // dispatch or a plain ending, and any of those can happen while a
        // queue row exists — a held run someone cancelled, most obviously.
        // Left behind, that row is re-claimed later and `execute` runs the
        // whole thing again on a row that reads `failed`.
        sqlx::query("DELETE FROM queue WHERE run_id=$1")
            .bind(run_id)
            .execute(&mut *tx)
            .await?;
        // A cancel mid-step, or a failure that skipped the per-step bookkeeping,
        // leaves step rows non-terminal under a terminal run. Normally a no-op.
        settle_steps(&mut tx, &[run_id], status).await?;

        // A run that ended badly takes its card off In Progress with it.
        // Without this the card sat under In Progress for good — nothing
        // working on it, no pulse, and no way out but to drag it back by hand.
        // It lives here rather than at the end of `run_task` because the
        // endings that need it most never reach `run_task`: a dispatch that
        // bailed on an unknown engine failed before the function was entered.
        //
        // Review, because that already means "a person needs to look at this"
        // and is where the badge and the Retry button are — the same rule
        // `org::epic::COLUMN_FOR_STEP` states for a step that ended badly.
        //
        // Two guards, both load bearing. Only a card actually *on* In Progress
        // moves, so a failure never drags one out of Backlog or back from Done.
        // And only when nothing else is still working on it: a bake-off runs
        // several runs against one card, and the first variant to fail must not
        // send it to review while the others are still going.
        if status != RunStatus::Completed {
            sqlx::query(
                "UPDATE tasks SET board_column = 'review'
                  WHERE id = (SELECT task_id FROM runs WHERE id = $1)
                    AND board_column = 'running'
                    AND NOT EXISTS (
                          SELECT 1 FROM runs other
                           WHERE other.task_id = tasks.id
                             AND other.status NOT IN ('completed', 'failed', 'canceled'))",
            )
            .bind(run_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        // Every ending comes through here — completion, cancellation, a crash in
        // `execute`, a planning failure — which is why the epic mirror hangs off
        // `finish` rather than off the end of `work_phase`. A no-op unless the
        // run has steps with cards.
        if let Err(e) = crate::runs::org::epic::mirror_run(&self.db, run_id).await {
            tracing::warn!(%run_id, error=%e, "could not update this run's sub-task cards");
        }
        // Same reasoning for routines: `finish` is the one door every ending
        // leaves through, and a routine's whole point is running while nobody
        // watches. A no-op unless the run was a routine firing.
        crate::routines::announce_finished(&self.db, run_id, status).await;
        // A review pass that did not complete is still a review that ended:
        // a verdict it gave before dying is acted on, and one that gave none
        // is recorded fail-closed and put to a person. A person's own cancel
        // with no verdict is left alone — they stopped it on purpose.
        if matches!(status, RunStatus::Failed | RunStatus::Canceled) {
            if let Ok(Some((task_id, decided, reaped))) = sqlx::query_as::<_, (Uuid, bool, bool)>(
                "SELECT r.task_id,
                            EXISTS (SELECT 1 FROM review_decisions d WHERE d.run_id = r.id),
                            r.reaped IS NOT NULL
                       FROM runs r WHERE r.id = $1 AND r.task_id IS NOT NULL AND r.trigger = $2",
            )
            .bind(run_id)
            .bind(crate::review::PEER_REVIEW)
            .fetch_optional(&self.db.pool)
            .await
            {
                if status == RunStatus::Failed || decided || reaped {
                    // Boxed: settling can start a fix through the follow-up
                    // door, which can end a summary through `finish` — a
                    // cycle an async fn cannot size without the indirection.
                    Box::pin(self.settle_review(task_id, run_id, crate::review::PEER_REVIEW)).await;
                }
            }
        }
        // A card's work that failed is news to its project's manager. Not a
        // cancel (a person did that), and not Eren's own passes.
        if status == RunStatus::Failed {
            if let Ok(Some((task_id, reason))) = sqlx::query_as::<_, (Uuid, Option<String>)>(
                "SELECT task_id, error_reason FROM runs WHERE id = $1 AND task_id IS NOT NULL
                    AND trigger NOT IN ('summary', 'peer_review')",
            )
            .bind(run_id)
            .fetch_optional(&self.db.pool)
            .await
            {
                crate::wake::raise(
                    &self.db,
                    task_id,
                    Some(run_id),
                    crate::wake::Kind::Failed,
                    reason.as_deref().unwrap_or(""),
                )
                .await;
            }
        }
        Ok(())
    }

    async fn persist_and_publish(
        &self,
        run_id: Uuid,
        step_id: Option<Uuid>,
        seq: i64,
        event: &ErenEvent,
    ) -> anyhow::Result<()> {
        let payload = serde_json::to_value(event)?;
        let type_name = payload
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let ts: DateTime<Utc> = Utc::now();
        sqlx::query(
            "INSERT INTO events (run_id, step_id, seq, type, payload, ts)
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(run_id)
        .bind(step_id)
        .bind(seq)
        .bind(&type_name)
        .bind(&payload)
        .bind(ts)
        .execute(&self.db.pool)
        .await?;
        self.bus.publish(EventEnvelope {
            run_id,
            step_id,
            seq,
            ts,
            event: event.clone(),
        });
        Ok(())
    }
}

/// Drag step rows to a terminal state along with the run that owned them.
///
/// The UI derives "who is working right now" from step status, so a step left
/// at 'running' under a failed run reads as a live teammate forever. Two
/// buckets, because they are not the same fact: a step that was mid-flight
/// really did fail (or was canceled with the run), while a step still queued
/// was never opened — calling that one 'failed' would paint its assignee as
/// blocked on work they never started.
pub(crate) async fn settle_steps(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_ids: &[Uuid],
    run_status: RunStatus,
) -> anyhow::Result<()> {
    if run_ids.is_empty() {
        return Ok(());
    }
    let interrupted = if run_status == RunStatus::Canceled {
        RunStatus::Canceled
    } else {
        RunStatus::Failed
    };
    sqlx::query(
        "UPDATE steps SET status=$1, finished_at=now()
         WHERE run_id = ANY($2)
           AND status IN ('starting','running','waiting_permission','rate_limited')",
    )
    .bind(interrupted.as_str())
    .bind(run_ids)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE steps SET status='skipped', finished_at=now()
         WHERE run_id = ANY($1) AND status='queued'",
    )
    .bind(run_ids)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn next_seq(db: &Db, run_id: Uuid) -> anyhow::Result<i64> {
    let row = sqlx::query("SELECT COALESCE(MAX(seq), -1) + 1 AS next FROM events WHERE run_id=$1")
        .bind(run_id)
        .fetch_one(&db.pool)
        .await?;
    Ok(row.get("next"))
}

/// Truncate on a character boundary, marking that something was dropped.
pub(crate) fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "\n…"
}

pub(crate) fn slugify(s: &str) -> String {
    let slug: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    slug.chars().take(40).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every caller, so a seventh cannot be added without answering.
    const CALLERS: [CallerKind; 8] = [
        CallerKind::TaskWork,
        CallerKind::TaskPlanning,
        CallerKind::WorkflowStep,
        CallerKind::OrgMember,
        CallerKind::Chat,
        CallerKind::CommentReply,
        CallerKind::KbGeneration,
        CallerKind::Research,
    ];

    /// The invariant the leak violated: a run only goes back on the queue if
    /// something will pick it up again unchanged.
    ///
    /// Holding is the interesting half. The two that are refused are refused
    /// because re-dispatching them costs money twice — a workflow re-runs
    /// every step it already paid for, and an org run's batch loop has already
    /// moved on and will write a terminal status over the hold.
    #[test]
    fn only_a_run_that_can_be_re_dispatched_unchanged_is_held() {
        for caller in CALLERS {
            let expected = match caller {
                CallerKind::WorkflowStep | CallerKind::OrgMember => OnRateLimit::Fail,
                _ => OnRateLimit::Hold,
            };
            assert_eq!(caller.on_rate_limit(), expected, "{caller:?}");
        }
    }

    /// The half of `CallerKind` that used to be a bare `bool`, pinned so the
    /// consolidation cannot have changed anyone's ending by accident.
    #[test]
    fn only_a_whole_run_finalizes() {
        for caller in CALLERS {
            let expected = !matches!(
                caller,
                CallerKind::TaskPlanning | CallerKind::WorkflowStep | CallerKind::OrgMember
            );
            assert_eq!(caller.finalizes(), expected, "{caller:?}");
        }
    }

    /// Why a resumed run must be created with `plan_approval = false`.
    ///
    /// Carrying it forward would send the resumed run back into the planning
    /// half — mutating tools denied, another plan written, nothing done — on a
    /// session that has already been working. Asserted here rather than
    /// trusted, because the resume path cannot see this decision.
    #[test]
    fn a_plan_first_run_with_no_stored_plan_plans_again() {
        use crate::runs::task_plan::{decide, Phase, PlanStep};
        assert_eq!(decide(true, PlanStep::Missing, true), Phase::Plan);
        assert_eq!(
            decide(false, PlanStep::Missing, true),
            Phase::Work { plan: None }
        );
    }

    /// The chat assistant must never be handed a mode its engine cannot honour.
    ///
    /// This is the bug this test was written for: chat passed a flat `Reviewed`,
    /// and an engine that cannot pause to ask answers that by rejecting every
    /// tool call. The assistant went quiet — no repository access, no task
    /// tools — and asked the user what they were working on, which reads as
    /// "it can't see my project" rather than "it was refused".
    #[test]
    fn chat_never_asks_an_engine_to_review_when_it_cannot() {
        for engine in [
            &eren_engines::claude::ClaudeEngine::default() as &dyn eren_engines::Engine,
            &eren_engines::opencode::OpenCodeEngine::default() as &dyn eren_engines::Engine,
        ] {
            let mode = chat_permission_mode(engine);
            assert_ne!(mode, PermissionMode::Reviewed, "{}", engine.label());
            assert!(
                eren_engines::vet(engine, mode, false).is_ok(),
                "{} cannot honour {mode:?}",
                engine.label()
            );
        }
    }

    /// Approve-everything is only safe because "everything" is a short list.
    #[test]
    fn the_chat_assistant_cannot_reach_a_tool_that_writes() {
        for tool in ["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"] {
            assert!(
                CHAT_DENIED_TOOLS.contains(&tool),
                "{tool} must be denied by name — an allow-list only pre-approves, \
                 it does not forbid, and chat runs in the real checkout"
            );
            assert!(!CHAT_ALLOWED_TOOLS.contains(&tool));
        }
    }

    /// The gate exists so a workflow can't grant itself more freedom than the
    /// project allows. Down, never up.
    #[test]
    fn full_auto_is_cut_to_reviewed_when_the_project_has_not_opted_in() {
        let r = resolve_step_permission(PermissionMode::FullAuto, false);
        assert_eq!(r.mode, PermissionMode::Reviewed);
        assert!(r.downgraded, "the caller has to be able to tell this apart");

        let r = resolve_step_permission(PermissionMode::FullAuto, true);
        assert_eq!(r.mode, PermissionMode::FullAuto);
        assert!(!r.downgraded);
    }

    /// The inverse would be a privilege escalation performed on the user's
    /// behalf, which is exactly what the compliance rules forbid.
    #[test]
    fn nothing_is_ever_raised_by_the_gate() {
        for asked in [
            PermissionMode::Reviewed,
            PermissionMode::AutoEdit,
            PermissionMode::FullAuto,
        ] {
            for gate in [true, false] {
                let got = resolve_step_permission(asked, gate).mode;
                assert!(
                    got == asked
                        || (asked == PermissionMode::FullAuto && got == PermissionMode::Reviewed),
                    "{asked:?} with gate={gate} became {got:?}"
                );
            }
        }
    }

    /// A step that merely *asks* for Reviewed is a different story from one
    /// that was cut down to it — the scheduled-run refusal says so, and a
    /// person can only act on the right one.
    #[test]
    fn a_step_that_asked_for_reviewed_is_not_reported_as_downgraded() {
        let r = resolve_step_permission(PermissionMode::Reviewed, false);
        assert_eq!(r.mode, PermissionMode::Reviewed);
        assert!(!r.downgraded);
    }

    /// Clipping by chars, not bytes — a a multi-byte boundary would panic.
    #[test]
    fn clipping_respects_character_boundaries() {
        assert_eq!(clip_chars("héllo wörld", 5), "héllo\n…");
        assert_eq!(clip_chars("short", 99), "short");
    }

    /// The bug this guards: cancel channels were keyed by step id while
    /// `cancel()` looked them up by run id, so cancelling any multi-step
    /// run silently did nothing.
    #[test]
    fn cancelling_reaches_every_live_step_of_a_run() {
        let cancels: Mutex<HashMap<Uuid, CancelState>> = Mutex::new(HashMap::new());
        let run = Uuid::new_v4();
        let mut receivers = vec![];

        // Three steps of one run register, as a fan-out would.
        for _ in 0..3 {
            let (tx, rx) = oneshot::channel();
            cancels
                .lock()
                .unwrap()
                .entry(run)
                .or_default()
                .steps
                .insert(Uuid::new_v4(), tx);
            receivers.push(rx);
        }

        // What cancel() does, by run id.
        let mut state = cancels.lock().unwrap();
        let entry = state.entry(run).or_default();
        entry.requested = true;
        for (_, tx) in std::mem::take(&mut entry.steps) {
            let _ = tx.send(());
        }
        drop(state);

        for mut rx in receivers {
            assert!(rx.try_recv().is_ok(), "every live step must be signalled");
        }
        assert!(
            cancels.lock().unwrap()[&run].requested,
            "intent outlives the steps"
        );
    }

    /// A cancel arriving between steps must not be lost: the flag is what a
    /// multi-step run checks before starting the next assignment.
    #[test]
    fn cancel_intent_survives_when_no_step_is_live() {
        let cancels: Mutex<HashMap<Uuid, CancelState>> = Mutex::new(HashMap::new());
        let run = Uuid::new_v4();

        cancels.lock().unwrap().entry(run).or_default().requested = true;

        // The next step to start sees the request already standing.
        let requested = cancels.lock().unwrap()[&run].requested;
        assert!(requested);
    }

    use super::slugify;

    #[test]
    fn slugify_is_branch_safe() {
        assert_eq!(slugify("Fix: the (weird) bug!!"), "fix-the-weird-bug");
        assert!(slugify(&"x".repeat(100)).len() <= 40);
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb;

    /// No loop: the test does the claiming.
    fn idle(t: &testdb::TestDb, root: &std::path::Path) -> Orchestrator {
        Orchestrator::new(
            t.db.clone(),
            EventBus::new(),
            Arc::new(WorktreeManager::new(root.to_path_buf())),
            4,
            None,
        )
    }

    async fn queued(t: &testdb::TestDb, card: Uuid, status: &str) -> Uuid {
        let run: Uuid = sqlx::query_scalar(
            "INSERT INTO runs (task_id, status, trigger, engine)
             VALUES ($1, $2, 'manual', 'mock') RETURNING id",
        )
        .bind(card)
        .bind(status)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO queue (run_id) VALUES ($1)")
            .bind(run)
            .execute(&t.db.pool)
            .await
            .unwrap();
        run
    }

    async fn started(t: &testdb::TestDb, run: Uuid) -> bool {
        sqlx::query_scalar("SELECT started_at IS NOT NULL FROM runs WHERE id = $1")
            .bind(run)
            .fetch_one(&t.db.pool)
            .await
            .unwrap()
    }

    /// A claimed run reads as started from the moment it is claimed, on the
    /// path with nothing to vet as well as the vetted one: a team run never
    /// stamps itself, and a budget made later in the window must count it.
    /// A queue row a canceled run left behind is cleared without counting.
    #[tokio::test]
    async fn a_claimed_run_counts_from_its_claim_on_either_path() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let orchestrator = idle(&t, dir.path());
        let (_, project) = t.project(dir.path(), true).await;
        let card = t.card(project, "team work").await;

        for vetted in [false, true] {
            if vetted {
                sqlx::query(
                    "INSERT INTO budget_policies (name, scope_kind, window_kind, cap_runs)
                     VALUES ('roomy', 'machine', 'day', 100)",
                )
                .execute(&t.db.pool)
                .await
                .unwrap();
            }
            let stale = queued(&t, card, "canceled").await;
            let live = queued(&t, card, "queued").await;
            assert_eq!(orchestrator.claim_next().await.unwrap(), Some(stale));
            assert!(!started(&t, stale).await, "vetted: {vetted}");
            assert_eq!(orchestrator.claim_next().await.unwrap(), Some(live));
            assert!(started(&t, live).await, "vetted: {vetted}");
        }
        t.finish().await;
    }

    /// The assistant and a team live on Eren's tools. An engine that cannot
    /// be handed them for one run is refused at the click — never started
    /// toolless — and the refusal names the installed engines that can.
    #[tokio::test]
    async fn work_that_lives_on_erens_tools_refuses_an_engine_that_cannot_carry_them() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mut orch = idle(&t, dir.path());
        orch.register_engine(Arc::new(eren_engines::mock::MockEngine::demo()));
        orch.register_engine(Arc::new(eren_engines::gemini::GeminiEngine::default()));
        orch.register_engine(Arc::new(eren_engines::qwen::QwenEngine::default()));
        orch.register_engine(Arc::new(eren_engines::cursor::CursorEngine::default()));

        let no = orch.needs_tools("gemini", "the assistant").unwrap_err();
        let said = no.to_string();
        assert!(said.starts_with("Gemini CLI can't"), "{said}");
        // Qwen can; the mock is not advice; Cursor cannot.
        assert!(said.ends_with("(Qwen Code)"), "{said}");
        orch.needs_tools("qwen", "the assistant").unwrap();
        orch.needs_tools("mock", "the assistant").unwrap();
        // Unknown is dispatch's to explain.
        orch.needs_tools("nope", "the assistant").unwrap();

        let (ws, project) = t.project(dir.path(), true).await;
        let chat: Uuid = sqlx::query_scalar(
            "INSERT INTO chats (project_id, title) VALUES ($1, 'Talk') RETURNING id",
        )
        .bind(project)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        let err = orch.enqueue_chat_turn(chat, "gemini").await.unwrap_err();
        assert!(err.is::<NoTools>(), "{err}");
        // A chat with no project is never handed the tools, so any engine
        // will do for it.
        let general: Uuid = sqlx::query_scalar(
            "INSERT INTO chats (title, workspace_id) VALUES ('General', $1) RETURNING id",
        )
        .bind(ws)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        orch.enqueue_chat_turn(general, "gemini").await.unwrap();
        sqlx::query("DELETE FROM runs WHERE chat_id = $1")
            .bind(general)
            .execute(&t.db.pool)
            .await
            .unwrap();

        // A team on a capable engine. Its members run on the team's engine,
        // so one pinned to Cursor for its own card work does not matter here.
        let member: Uuid = sqlx::query_scalar(
            "INSERT INTO agents (workspace_id, name, engine) VALUES ($1, 'Cy', 'cursor') RETURNING id",
        )
        .bind(ws)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        let team: Uuid = sqlx::query_scalar(
            "INSERT INTO teams (workspace_id, name, pattern, definition, engine)
             VALUES ($1, 'T', 'org', $2, 'gemini') RETURNING id",
        )
        .bind(ws)
        .bind(serde_json::json!({ "members": [{ "agent_id": member.to_string() }] }))
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        // On Gemini it is refused, at either door: the Teams page…
        let err = orch
            .enqueue_org_run(team, project, "ship it", false)
            .await
            .unwrap_err();
        assert!(err.is::<NoTools>(), "{err}");
        // …and a card assigned to the team.
        let card = t.card(project, "for the team").await;
        sqlx::query("UPDATE tasks SET team_id = $2 WHERE id = $1")
            .bind(card)
            .bind(team)
            .execute(&t.db.pool)
            .await
            .unwrap();
        let err = orch.enqueue_task(card).await.unwrap_err();
        assert!(err.is::<NoTools>(), "{err}");

        // Nothing was queued by any of it.
        let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM runs")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
        assert_eq!(runs, 0);

        // On Qwen it starts, the Cursor-pinned member and all.
        sqlx::query("UPDATE teams SET engine = 'qwen' WHERE id = $1")
            .bind(team)
            .execute(&t.db.pool)
            .await
            .unwrap();
        orch.enqueue_org_run(team, project, "ship it", false)
            .await
            .unwrap();
    }

    /// Without Claude Code, the default is the most capable engine installed
    /// — not the first by name, which with Amp installed was one that the
    /// assistant, a manager and a team would all refuse.
    #[tokio::test]
    async fn the_default_engine_is_chosen_by_what_it_can_do() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mut orch = idle(&t, dir.path());
        orch.register_engine(Arc::new(eren_engines::amp::AmpEngine::default()));
        orch.register_engine(Arc::new(eren_engines::cursor::CursorEngine::default()));
        assert_eq!(orch.default_engine(), "amp", "first of equals, by name");
        orch.register_engine(Arc::new(eren_engines::gemini::GeminiEngine::default()));
        assert_eq!(
            orch.default_engine(),
            "gemini",
            "it can at least edit without a shell"
        );
        orch.register_engine(Arc::new(eren_engines::codex::CodexEngine::default()));
        assert_eq!(orch.default_engine(), "codex");
    }

    /// Gemini's aliases and Amp's modes are their own catalogs; the keyword
    /// guess that serves a multi-provider engine put every tier on one of
    /// them.
    #[tokio::test]
    async fn an_engine_with_its_own_catalog_keeps_its_own_defaults() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mut orch = idle(&t, dir.path());
        for engine in [
            Arc::new(eren_engines::amp::AmpEngine::default()) as Arc<dyn Engine>,
            Arc::new(eren_engines::gemini::GeminiEngine::default()),
        ] {
            let models = match engine.id() {
                "amp" => ["low", "medium", "high", "ultra"]
                    .map(String::from)
                    .to_vec(),
                _ => ["auto", "pro", "flash", "flash-lite"]
                    .map(String::from)
                    .to_vec(),
            };
            orch.detected.insert(
                engine.id(),
                eren_engines::EngineInfo {
                    version: "1".into(),
                    authenticated: true,
                    providers: vec![],
                    models,
                },
            );
            orch.register_engine(engine);
        }
        orch.load_tier_mapping().await.unwrap();
        let tiers = |e: &str| {
            [ModelTier::Easy, ModelTier::Medium, ModelTier::Complex].map(|t| orch.model_for(e, t))
        };
        assert_eq!(tiers("amp"), ["low", "medium", "high"]);
        assert_eq!(tiers("gemini"), ["flash-lite", "flash", "pro"]);
        assert!(orch.derived_defaults("amp").is_none());
        assert!(orch.derived_defaults("gemini").is_none());
    }

    /// Where Full Auto is off for the project, a card steps down to the
    /// narrowest mode its engine can honour — and an engine with nothing
    /// narrower (Amp runs every tool, always) is refused at the click rather
    /// than handed Reviewed, which it would ignore.
    #[tokio::test]
    async fn full_auto_refused_steps_down_or_says_no() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mut orch = idle(&t, dir.path());
        orch.register_engine(Arc::new(eren_engines::amp::AmpEngine::default()));
        orch.register_engine(Arc::new(eren_engines::codex::CodexEngine::default()));
        let (_, project) = t.project(dir.path(), false).await;
        let card = t.card(project, "go").await;
        let on = |engine: &'static str| {
            sqlx::query("UPDATE tasks SET engine = $2, permission_mode = 'full_auto' WHERE id = $1")
                .bind(card)
                .bind(engine)
                .execute(&t.db.pool)
        };
        let opt_in = |yes: bool| {
            sqlx::query("UPDATE projects SET full_auto_opt_in = $2 WHERE id = $1")
                .bind(project)
                .bind(yes)
                .execute(&t.db.pool)
        };
        opt_in(false).await.unwrap();
        on("amp").await.unwrap();
        let said = orch.vet_card(card).await.unwrap().expect("refused");
        assert!(
            said.starts_with("Amp can only run with every tool allowed"),
            "{said}"
        );
        // Codex steps down to Auto-edit, which it can honour.
        on("codex").await.unwrap();
        assert_eq!(orch.vet_card(card).await.unwrap(), None);
        // With the opt-in, Amp's Full Auto is what was asked for and allowed.
        opt_in(true).await.unwrap();
        on("amp").await.unwrap();
        assert_eq!(orch.vet_card(card).await.unwrap(), None);
        assert_eq!(
            short_of_full_auto(&eren_engines::mock::MockEngine::demo().capabilities()),
            Some(PermissionMode::Reviewed)
        );
    }
}
