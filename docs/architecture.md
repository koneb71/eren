# Architecture

This is the document to read before your first change. It describes how Eren is put
together and, where the shape is unobvious, why it is that shape rather than the obvious
one. The README describes what Eren does; this describes what you are about to edit.

Every module, table, route and test named here exists in the tree at the time of writing.
When you add a migration, add a row to [Schema history](#schema-history) — a test checks
that every migration's number appears in this file.

## Contents

- [The one thing that constrains everything else](#the-one-thing-that-constrains-everything-else)
- [The five crates](#the-five-crates)
- [Engines](#engines)
- [The run lifecycle](#the-run-lifecycle)
- [Worktrees](#worktrees)
- [Permissions and MCP](#permissions-and-mcp)
- [Prompt composition](#prompt-composition)
- [Agents](#agents)
- [Budgets](#budgets)
- [Checks, follow-ups and landing](#checks-follow-ups-and-landing)
- [Review policy and the merge gate](#review-policy-and-the-merge-gate)
- [Hand-off](#hand-off)
- [The inbox and approvals](#the-inbox-and-approvals)
- [Accounts](#accounts)
- [The audit log](#the-audit-log)
- [Config revisions](#config-revisions)
- [Wakes](#wakes)
- [The reaper and liveness](#the-reaper-and-liveness)
- [The org chart](#the-org-chart)
- [Goals](#goals)
- [Heartbeats](#heartbeats)
- [Routines and the project manager](#routines-and-the-project-manager)
- [Teams and workflows](#teams-and-workflows)
- [Apps](#apps)
- [The knowledge base](#the-knowledge-base)
- [Previews](#previews)
- [The dashboard](#the-dashboard)
- [The rename's compatibility layer](#the-renames-compatibility-layer)
- [Testing](#testing)
- [Before your first pull request](#before-your-first-pull-request)
- [Schema history](#schema-history)

## The one thing that constrains everything else

Eren drives official coding-agent CLIs — `claude`, `opencode`, `codex`, `gemini`,
`cursor-agent`, `qwen`, `amp` — as child processes on the user's own machine, under the
user's own subscription login. It is **process orchestration, not API access**, and four
invariants keep it that way. They are stated at the top of
[`crates/eren-engines/src/lib.rs`](../crates/eren-engines/src/lib.rs) and code that
violates them is rejected:

1. Adapters spawn official binaries found on `PATH` and read their stdout. Nothing else.
2. Never read, store, extract, or forward credentials. Never touch `~/.claude` or any
   engine's config or credential files.
3. Never set authentication environment variables on a spawned process.
4. Never proxy, intercept, or replay the engine's network traffic.

These are not decorative. Invariant 2 is why `eren doctor` answers "is this CLI logged
in?" by *running* the CLI rather than by reading its config. Invariant 3 is why there is a
single function — `eren_shared::env_guard::is_auth_env` — that decides whether a name
looks like a secret, instead of a prefix list per call site. The first version of that check
lived in two places and knew only about Anthropic prefixes, which stopped nothing the moment
a second provider existed. The same file lists `OWN_SECRETS` (the S3 keys and the access
token) and `OWN_UNPREFIXED` (`DATABASE_URL`, Eren's own database), which are stripped from
every child: a spawned CLI inherits the server's whole environment, so the day
Eren acquired a credential of its own (object storage for the knowledge base) it would
otherwise have handed that credential to every agent it launched. That is also why every
process starts through `env_guard::command` and never `Command::new` — `clippy.toml` and a
source-scanning test in `env_guard.rs` both refuse the latter.

## The five crates

```
eren-shared  ←  eren-engines  ←  eren-core  ←  eren-server  ←  eren-cli
```

Dependencies point strictly leftwards. `eren-shared` depends on none of the others;
`eren-core` never depends on `eren-server`. The practical consequence: anything both the
server and the CLI need, and anything you want to unit-test without a database, belongs in
`eren-shared`.

**`eren-shared`** — the vocabulary. `ErenEvent` and `EventEnvelope` (the normalized
event stream), `ModelTier` / `TierChoice` / `EngineTierMapping`, `PermissionMode` /
`RunStatus`, the workflow YAML types and `interpolate`, `McpWiring`, `env_guard`,
rate-limit parsing, reasoning effort, secret detection, the auto-tier router, and `brand` —
the product's name in every form it takes (see
[the compatibility layer](#the-renames-compatibility-layer)). No I/O, no database, no
engine.

**`eren-engines`** — the `Engine` trait, `RunSpec`, `Capabilities`, `vet`, the shared
`pump`, and the adapters (`claude/`, `opencode/`, `codex/`, `gemini/`, `cursor/`, `qwen/`,
`amp/`, `local/`, `mock/`). Each adapter spawns its CLI, parses that CLI's native stream
format, and normalizes it into `ErenEvent`. Everything downstream consumes only
`ErenEvent`, which is what lets a second engine exist at all.

**`eren-core`** — the substance. Postgres access and embedded migrations (`db`), the run
orchestrator and state machine (`runs/`), the queue and its backoff (`queue/`), the worktree
manager (`worktrees/`), the `EventBus`, the `PermissionBroker`, team runs (`runs/org/`),
the knowledge base (`kb/`), retrieval (`rag/`, `repo/`), apps (`apps/`), previews, GitHub
integration, the cron scheduler, spend and usage accounting — and the modules the rest of
this document is about one by one: `agents`, `budgets`, `checks`, `landing`, `review`,
`handoff`, `inbox`, `approvals`, `asks`, `decisions`, `audit`, `revisions`, `wake`,
`reaper`, `org_chart`, `goals`, `heartbeat`, `routines`, `manager`, `legacy`.

**`eren-server`** — axum. `/api` REST routes, `/ws` event fan-out, `/mcp` (a hand-rolled
MCP-over-HTTP endpoint the engines call back into), the preview reverse proxy, the app
bridge, the audit layer, and the SPA fallback that serves the built dashboard. Every handler
takes `AppState`, which carries `db`, `bus`, `orchestrator`, `permissions`, `storage` and a
mutex serializing Files-tab saves.

**`eren-cli`** — the `eren` binary: `serve` and `doctor`. Both adopt state from before the
rename first; `serve` then brings up the database, registers the engines with the orchestrator, runs the
boot-time sweeps (previews, worktrees, per-run files, orphaned runs, interrupted checks) and
spawns the long-running loops (queue, scheduler, attachments, preview idling).

`serve` manages its own Postgres under `~/.eren/pgdata` unless `DATABASE_URL` is set, so a
fresh checkout has nothing to install. It binds loopback unless `EREN_BIND` says otherwise.
There is no login, so on loopback the only thing keeping transcripts and files private is that
the only caller is this machine. The access token (`eren_server::access`) exists on every bind:
the outermost layer passes a caller whose TCP peer is loopback and asks every other one for the
token — once, through an access link that becomes a cookie, or as a bearer header — while
`EREN_ALLOWED_HOSTS` adds the names the Host and Origin checks accept. On a loopback bind the
only such peer is a container reaching the port through Docker's gateway, which is exactly who
should be asked; a wide bind (`Exposure::Protected`) is what makes other machines meet it.
`/mcp` answers loopback peers only, whatever the token or the account state. `EREN_ACCESS_TOKEN=off` goes back to no token, and then a wide bind
is refused unless `EREN_TRUST_NETWORK` is set too (`eren_server::exposure`). Once an admin
exists (`eren admin create`), accounts replace all of that: `auth::require_session`, just inside
the token layer, asks every caller for a session cookie, puts the `Caller` in the request, and
every handler checks each id it is handed against `eren_core::scope` — see **Accounts** below.
`serve` is run
with `into_make_service_with_connect_info`, which is where the peer address comes from. Migrations live in
`crates/eren-core/migrations/` and are embedded by sqlx **at compile time** — adding a file
does not always retrigger a rebuild, so if a new column comes back as `ColumnNotFound`,
`touch crates/eren-core/src/db.rs` and rebuild.

## Engines

The trait is small:

```rust
fn id(&self) -> &'static str;
fn label(&self) -> &'static str;
fn capabilities(&self) -> Capabilities;
async fn detect(&self) -> Option<EngineInfo>;
fn start(&self, spec: RunSpec) -> anyhow::Result<EngineProcess>;
fn interactive_resume_argv(&self, session_id: &str) -> Option<Vec<String>>;
```

`detect` is implemented by running the binary — never by inspecting its config. It reports a
version, whether the CLI is logged in, provider names with their auth *type* (never a
credential), and the model ids the install can actually reach when the CLI can say. An empty
model list means "we don't know", not "none available", which is why tier defaults for a
multi-provider engine are derived at boot rather than hard-coded: an `anthropic/…` default
is simply wrong for someone whose only provider is Google.

`start` returns an `EngineProcess` — a receiver of normalized events plus a handle that can
`interrupt` (SIGINT, so the CLI can checkpoint its session) or `kill`.
`interactive_resume_argv` is what a person types to pick the session up in their own
terminal; it has no default for the same reason `Capabilities` has none — a wrong command
shown with a Copy button is worse than none.

### The nine engines

`real_engines()` in [`crates/eren-cli/src/main.rs`](../crates/eren-cli/src/main.rs) is the
one list, used by both `serve` and `doctor`, in the order they are offered:

| id | adapter | spawns | stream |
|---|---|---|---|
| `claude-code` | `claude/` | `claude` | Claude Code `stream-json` |
| `opencode` | `opencode/` | `opencode` | OpenCode's own |
| `codex` | `codex/` | `codex` | Codex's own |
| `gemini` | `gemini/` | `gemini` | Gemini CLI's own JSON Lines |
| `cursor` | `cursor/` | `cursor-agent` | close to Claude Code's, tool calls of its own |
| `qwen` | `qwen/` | `qwen` | Claude Code's, read by `claude::compat` |
| `amp` | `amp/` | `amp` | Claude Code's, read by `claude::compat` |
| `ollama` | `local/` | `opencode` | OpenCode's |
| `lmstudio` | `local/` | `opencode` | OpenCode's |

`serve` registers only what `detect` finds on `PATH` (`register_if_available`), so an
engine that is not installed is simply not offered rather than accepted and then failing at
spawn time. The mock engine is registered unconditionally for the fixtures-driven demo.

The four newer adapters were written from their CLIs' source and documentation rather than
observed against the binary, and their module docs say so and name the things most likely
to be wrong. `claude/compat.rs` wraps the Claude parser for Qwen and Amp, adding the three
things those CLIs do that Claude Code does not (an error `result` whose reason is in `error`,
Amp's `system` error lines, and an exit-status sentence naming the CLI).

`local/` is the odd one and worth reading before you copy it: Ollama and LM Studio serve a
model but hold no tools, so `LocalEngine` **delegates to `OpenCodeEngine`** — it resolves a
model from what the runtime's own CLI reports (`ollama list`, `lms ls --json`, re-read before
every run through `Engine::refresh`; a named id the runtime lacks is refused), declares
that runtime as OpenCode's provider, and hands the run over. Its `capabilities()` returns
OpenCode's. It is an engine because that is how a person thinks about the choice, and it
keeps invariant 1 because the process it spawns is still an official agent binary from
`PATH`. `eren_core::local_models` is the older HTTP discovery the settings page uses; an
adapter must not use it.

### The shared pump

[`crates/eren-engines/src/pump.rs`](../crates/eren-engines/src/pump.rs) is the part of an
adapter that is the same for every JSON-lines CLI: spawn the command (built by the caller
through `env_guard::command`), read stdout line by line through the adapter's `LineParser`,
keep the last twenty lines of stderr, and decide the terminal event from the exit status when
the stream did not. Gemini, Cursor, Qwen and Amp use it; the older three keep their own pumps
with engine-specific history.

```rust
pub trait LineParser: Send + 'static {
    fn line(&mut self, line: &str) -> Vec<ErenEvent>;
    fn finish(&mut self, exit_ok: bool, exit_code: Option<i32>, stderr_tail: &[String]) -> ErenEvent;
}
```

`line` never fails — a line the parser cannot read thins the stream rather than killing the
run. Two rules in `spawn` are load-bearing. **An ending is held until the process has
exited, and only the last one counts**: Qwen writes a failed `result` line for a failed
sub-agent and carries on, and sent at once that line ended a live run. **stderr never ends a
run on its own**: Gemini prints a quota warning, falls back to a smaller model and finishes
the work, so a rate-limit line on stderr only decides the outcome of a run that exited
non-zero *without* the stream saying how it ended. `reason_from` turns the stderr tail into
the sentence a person sees.

### Capabilities

`Capabilities` declares nine things per adapter. Behaviour is gated on these, never on
`if engine == "claude-code"`.

| flag | means | when `false` |
|---|---|---|
| `interactive_permissions` | can pause and ask a person to approve one tool call | `Reviewed` is refused by `vet` |
| `structured_rate_limit` | reports a reset time with a rate limit | the queue guesses with the backoff ladder |
| `resume_sessions` | can resume a prior session by id | a resume is refused by `vet` |
| `append_system_prompt` | can add to the system prompt | the persona is folded in front of the prompt (`prompt_with_persona`) |
| `fixed_model_catalog` | model ids come from a fixed list | free-text `provider/model` |
| `reports_cost` | says what a run cost in dollars | only token budgets see its runs; `cost_usd` stays NULL |
| `enforces_denied_tools` | the CLI refuses a denied tool itself | an agent reviewer cannot run on it |
| `mcp_tools` | can be handed Eren's MCP server per run without writing into the run's folder | chat, managers and teams are refused at the click (`Orchestrator::needs_tools`); a card run goes without its toolbox |
| `auto_edit` | can edit files without also being handed a shell | Auto-edit is refused by `vet` rather than widened to Full Auto |
| `read_only_passes` | can take a pass that must not write (a plan, a summary, a drafting call) in a mode where nothing can | a plan-first card (`vet_card`) and an AI drafting call (`utility_run`) are refused at the click with a 409 |

| engine | interactive | rate limit | resume | append | fixed catalog | cost | enforces deny | mcp | auto_edit | read-only |
|---|---|---|---|---|---|---|---|---|---|---|
| claude-code | yes | yes | yes | yes | yes | yes | yes | yes | yes | yes |
| opencode | no | no | yes | yes | no | yes | no | yes | yes | yes |
| codex | no | no | yes | yes | no | no | no | yes | yes | yes |
| gemini | no | no | yes | no | yes | no | no | no | yes | yes |
| cursor | no | no | yes | no | no | no | no | no | no | yes |
| qwen | no | no | yes | yes | no | no | no | yes | yes | yes |
| amp | no | no | yes | no | yes | no | no | no | no | no |
| ollama, lmstudio | OpenCode's | | | | | | | | | |

There is deliberately **no `Default` impl**. A new adapter has to answer for itself, because
inheriting "yes, I can do everything" by omission is exactly how a descriptor like this rots
into a lie — and the lie is only discovered when a run fails in a way nobody can explain.

The cost of getting this wrong is on record. `RunSpec` used to carry a path to a file
already written in *Claude's* `{"mcpServers": …}` dialect, so the orchestrator gated MCP
wiring on the literal string `"claude-code"`. Any other engine silently received no MCP at
all. Now the spec says *what* the run should reach (`McpWiring`) and each adapter renders
its own dialect — and an adapter never writes a config into a worktree or the user's
checkout, where it would land in the diff.

`Orchestrator::default_engine` ranks installed engines by `(mcp_tools, auto_edit)` when
Claude Code is absent. Alphabetical order alone made Amp the default beside Codex — an
engine the assistant, a manager and a team would all refuse.

### `vet`, the step-down rule, and why refusals are never upgrades

`eren_engines::vet` is the single place a capability mismatch is decided, so the answer is
the same whether you arrive from the board, a chat, a team run or a bake-off. It refuses
`Reviewed` on an engine without `interactive_permissions`, refuses `AutoEdit` on an engine
without `auto_edit`, and refuses a resume on an engine without `resume_sessions`, each with
a message naming the engine and offering a way forward.

What it deliberately does not do is change the mode. The orchestrator *does* cut `FullAuto`
down to `Reviewed` when the safety gate is not satisfied —
`resolve_step_permission(asked, gate_satisfied)` in `runs/orchestrator.rs`, where the gate
is `full_auto_opt_in && worktrees.manages(cwd)` — and that is fine: it is a de-escalation,
and the result says `downgraded: true` so a scheduled run can say which it was. Quietly
turning `Reviewed` into `AutoEdit`, or `AutoEdit` into `FullAuto`, because the engine cannot
do what was asked would be the opposite: a privilege escalation performed on the user's
behalf. So starting a Reviewed card on OpenCode is refused with a `409` at the click that
caused it, rather than failing forty minutes in or silently running with more authority
than was asked for. The test `nothing_is_ever_raised_by_the_gate` pins the direction.

## The run lifecycle

Everything an engine does happens inside a *run*. A run is a row in `runs`; a run waiting to
start is also a row in `queue` (`run_id` primary key, `priority`, `not_before`,
`enqueued_at`).

```
  enqueue_*()                  run_loop                    execute_*_run
      │                           │                              │
      ▼                           ▼                              ▼
  runs row  ──▶  queue row ──▶ acquire slot ──▶ claim_next ──▶ build RunSpec
                                  │                │              │
                          (Slots semaphore,   (gate: paused?      ▼
                           EREN_MAX_          budget spent?   engine.start()
                           CONCURRENT=2)       agent limits?)     │
                                                                  ▼
                                                            ErenEvent stream
                                                                  │
                                                    persist to `events` table
                                                                  │
                                                          bus.publish(envelope)
                                                                  │
                                                        /ws ──▶ dashboard
```

### Enqueue and priority

Every start funnels through an `enqueue_*` method on the orchestrator (`enqueue_task`,
`enqueue_chat_turn`, `enqueue_comment_reply`, `enqueue_bakeoff`, `enqueue_workflow`,
`enqueue_kb_article`, `enqueue_research`, `enqueue_org_run`, `enqueue_follow_up`), which is
why the guards live there rather than in the routes. `enqueue_task` locks the card row
(`FOR NO KEY UPDATE`) so a double click or a drag racing the Start button cannot put two
agents in one worktree, refuses a card whose work is already running under a step of its
epic, and refuses a card whose blockers have not *landed* — `done`, not `review`, because a
blocker in review has a diff nobody merged and a dependent run started then would branch
from `main` without the work it builds on. The routes answer these with a `409`, but they
are not the only door: dragging a card into In Progress, Retry, the chat MCP's `start_task`,
a heartbeat and a landed blocker all arrive here. `Orchestrator::start_card` is the Start
button's vet and door, and every automatic start goes through it so every gate the button
meets applies.

Priorities are integers, highest first, ties broken by `enqueued_at`:

| Priority | What |
|---|---|
| 20 | a chat turn — a person is sitting there |
| 15 | an agent's reply to an `@`-mention in a comment |
| 14 | a fix requested from a diff review — someone is reading the diff |
| 10 | a board task, a resume, a manually triggered workflow |
| 8 | one attempt of a bake-off — exploratory, and several runs against one rate limit |
| 5 | a run put back behind a rate-limit backoff |
| 1 | a scheduled workflow — it yields to anything a human is waiting on |

### Claiming

`claim_next` first consults the queue gate: `Paused` (someone pressed pause) or over the
machine budget. Then it claims atomically:

```sql
DELETE FROM queue WHERE run_id = (
    SELECT run_id FROM queue
    WHERE not_before IS NULL OR not_before <= now()
    ORDER BY priority DESC, enqueued_at ASC
    FOR UPDATE SKIP LOCKED LIMIT 1
) RETURNING run_id
```

A claimed run whose budget scope has since been spent, or whose agent is at its limit, is
*held* (`queue.hold_reason`, `held_by`) and the next one claimed instead, so one spent
project does not stop the others. `execute` then re-reads the run's status and drops
anything that is no longer waiting to start. That guard is not paranoia: a queue row that
outlived its run — cancelled, already finished, claimed twice — used to dispatch a second
engine against it, and the user was charged for it.

### `CallerKind`

Eight things stream an engine: `TaskWork`, `TaskPlanning`, `WorkflowStep`, `OrgMember`,
`Chat`, `CommentReply`, `KbGeneration`, `Research`. Rather than a bare `finalize: bool`, a
caller says what it *is* and two answers follow:

- `finalizes()` — does this dispatch own the run's ending? False where a *step* ending is not
  a *run* ending. A completed planning pass is not a completed run, and marking it terminal
  would send the card to review with nothing done.
- `on_rate_limit()` — can this run be handed back to `execute` later, unchanged? A task run
  can: it reuses the card's worktree and rebuilds its spec from the row. A workflow step
  cannot: step rows and outputs are written per dispatch, so re-running a half-finished
  pipeline is charged for in full and duplicates its own rows.

Both matches are exhaustive on purpose. A new caller is a compile error rather than a silent
leak.

### Events, and why they are persisted before they are published

`persist_and_publish` writes the envelope to the `events` table and *then* calls
`bus.publish`. The database is the source of truth; the bus is a live convenience.

That ordering is what makes the WebSocket honest. A client connects to
`/ws?run_id=<uuid>&after_seq=<n>`; the handler subscribes to the bus **before** replaying
from the database (so nothing falls in the gap), sends every persisted event past
`after_seq`, then switches to live fan-out, skipping anything already delivered. Omitting
`run_id` streams live events for all runs, which is what the activity tickers read. A reader
who closes their laptop mid-run and comes back gets the whole transcript, in order, with no
special case — because the events were never only in memory. `EventBus::publish` ignores a
send failure for the same reason: no subscribers is fine, the database already has it.

`seq` is per-run and allocated by `SeqAlloc`, an atomic counter, because a fan-out has
several steps writing concurrently and `(run_id, seq)` is unique. Permission events are the
deliberate exception: `seq: -1`, ephemeral, never part of the replay log, and always passed
through live.

Nothing trims the log unless the operator says so: `EREN_EVENT_RETENTION_DAYS`
(`eren_core::retention`) deletes the events of runs that *finished* more than that many days
ago, hourly from the scheduler, and leaves the run row — status, cost, tokens, error — and
everything said about the card. Unset, every event is kept forever, which is also what every
backup carries.

### Concurrency, parking and rate limits

`run_loop` holds one permit from `Slots` for the whole of `execute`. That is right while a
run is working and wrong while it is waiting for a person: with the default budget of two
(`EREN_MAX_CONCURRENT`), two runs parked on unanswered permission prompts froze the entire
queue.

So a parked run **lends** its slot back and takes it again on the way out. Reclaiming
records a debt that the queue loop pays on its next turn, rather than shrinking the
semaphore on the spot — because awaiting `acquire` inside the resolve path would make the
person's **Allow** click block until whatever run took the borrowed slot finishes. The click
would be answered by the very deadlock it was meant to break. `Slots::take_debt` is a
compare-and-swap loop and never `fetch_sub`, which on zero would wrap to `usize::MAX` and
stop the queue for the life of the process.

A rate limit puts the run back on the queue at priority 5 behind a backoff of 5m → 15m → 45m
(`queue::rate_limit_backoff`, capped), with jitter so a burst of held runs does not stampede
when the window resets. When the engine reports a structured reset time — a `Capabilities`
flag — the run waits exactly that long instead of guessing; a reset time already past falls
back to the ladder. `stream_run` notes a limit as it arrives (a stderr watcher reports one
mid-stream) and acts on it once, after the stream has ended: held — or failed, for a caller
that cannot be re-dispatched — unless the run completed anyway or Eren stopped it. Holding
mid-stream wrote a queue row under a run still streaming. On boot, `recover_orphans`
deletes queue rows whose run has already finished, re-queues `rate_limited` runs that have no
queue row, fails runs this process was never executing, and marks their open permission
prompts `expired` so the inbox can offer to resume them.

Scheduled runs never park. A step of a scheduled workflow that resolves to `Reviewed` fails
at dispatch with a message naming the step, because nobody is at the keyboard at 3am and
getting to the question costs tokens before the wait times out. Manual runs still park:
someone chose to start them and is there to answer.

`Orchestrator::is_executing` is an in-memory registry of the runs this process is actually
executing, held by a drop guard for the whole of `execute` — post-work included, and the
checks a run starts in the background, which hold a guard of their own until they settle. It
is what the reaper and a hand-off read when a row's status is not enough.

## Worktrees

Board tasks run in an isolated git worktree under
`~/.eren/worktrees/<project-hash>/<task-id>` — outside the user's repository, on a branch
with the `eren/` prefix. Two things follow, and they are the same thing seen from two sides:
an agent never touches the working copy, and the branch it produces *is* the reviewable
diff. `diff`, `diff_stat`, `diff_file`, `squash_merge`, `push` and `discard` all hang off
that. A card's diff is measured from `merge-base`, not the base's tip. Every git invocation
uses an explicit argument vector, never a shell string.

This is why Eren runs `git init` on a folder that is not a repository yet, rather than
refusing it: the repository is the price of the worktree, and the worktree is what buys
review. A bake-off variant gets a worktree keyed by *run* rather than task, since the whole
point is that the attempts cannot see each other.

**The in-place fallback.** A folder occasionally cannot have its own repository — most often
because it is nested inside another one. Those projects still work, but the run's `cwd` is
the project folder itself. There is no worktree, no diff, no undo, and the card goes
straight to `done` because there is nothing to review. Full-auto stays refused there
regardless of project settings: without a managed worktree the thing that made full-auto
safe is absent.

**Merge conflicts are met on the card's branch, never the person's checkout.** "Update from
main" (`POST /tasks/{id}/update-from-base`) merges the base into the card's branch inside
its worktree and, on conflict, leaves the merge in progress for a `MergeConflict`
follow-up. `commit_worktree` refuses while conflict markers remain, and `squash_merge`
refuses any diff that adds them — so markers can never land.

Two adjacent rules worth knowing before you touch this area:

- **Attachments are never copied into a worktree.** They live under
  `~/.eren/attachments/` and are granted with `--add-dir`. An untracked file in the
  worktree would show up in `git status`, and an agent running `git add -A` would commit the
  user's PDF to the branch and then to `main` on squash-merge.
- **The Files tab writes to both trees** — the checkout and a card's worktree — when a
  *person* saves. That does not weaken the rule above, which is about agents. The write path
  carries its own gates (no `.git`, a root allow-list, a content hash, and the
  `X-Eren-Write` header no cross-origin request can set), documented at the top of
  [`crates/eren-server/src/routes/files.rs`](../crates/eren-server/src/routes/files.rs).

`worktrees::sweep::reconcile` runs at boot and reclaims worktrees nothing can reach any more
— Postgres can drop a row but not a directory, and these are the largest thing Eren puts on
disk.

## Permissions and MCP

### `allowed_tools` does not restrict anything

`RunSpec.allowed_tools` is an **auto-approval** list. Claude Code's `--allowedTools`
pre-approves; it does not forbid. A run "allowed" only `Read` will still reach for `Bash`.
Anything that must not happen goes in `denied_tools`, which adapters apply last, so it beats
anything the allow-list or the permission mode would otherwise permit. `WRITE_TOOLS` in the
engines crate is the shared vocabulary for "read-only"; an engine with no per-tool vocabulary
translates a denial of those into whatever mode it has where nothing can write.

Two places depend on this and will break quietly if it is forgotten:

- Chat runs execute in the user's **real checkout**, not a worktree, so they carry both
  `CHAT_ALLOWED_TOOLS` and `CHAT_DENIED_TOOLS` in
  [`crates/eren-core/src/runs/orchestrator.rs`](../crates/eren-core/src/runs/orchestrator.rs).
  Never add `Bash`, `Edit` or `Write` to the allowed list.
- A plan-first pass, and an agent review, are genuinely read-only because the mutating tools
  are *denied*, not merely left off the allow-list.

Chat's permission mode is derived from the engine's capabilities (`chat_permission_mode`)
rather than fixed. It was once a flat `Reviewed`, which is why chat looked broken on
OpenCode: with no way to answer a prompt mid-run it rejected every tool call.

### The mid-run prompt path

```
engine  ──(--permission-prompt-tool mcp__eren__approve)──▶  POST /mcp/run/{run_id}
                                                                     │
                                        PermissionBroker::request  ◀──┘
                                                 │
                    park the run · lend its queue slot · emit PermissionRequested (seq -1)
                    RunGate::record writes the permission_requests row
                                                 │
                                        dashboard or inbox: Allow / Deny
                                                 │
                                   oneshot resolves ──▶ HTTP response to the engine
                                                 │
                          ParkGuard drops: unpark, reclaim the slot, close the row
```

[`crates/eren-server/src/mcp/`](../crates/eren-server/src/mcp/) is a hand-rolled
MCP-over-HTTP router with three endpoints, each scoped by its URL so the server always knows
who is speaking without trusting anything the model says:

- `/mcp/run/{run_id}` — `approve`, and a card run's own toolbox (`run_tools.rs`);
- `/mcp/chat/{chat_id}/{run_id}` — the project assistant's workspace tools
  (`chat_tools.rs`), every one resolving through the chat's own project;
- `/mcp/org/{run_id}/{step_id}` — team tools for one teammate (`org_tools.rs`).

Two details of the broker are load-bearing. **A timeout is not a refusal.** `Decision` has
four variants — `Allowed`, `Denied`, `Unanswered { waited }`, `RunGone`. The wire protocol
has only allow and deny, so three of them travel as a denial, but the *message* keeps them
apart: an engine told "denied by the user" works around the refusal and spends real money
doing it; an engine told "nobody answered, so Eren stopped the run" stops. **`ParkGuard`
exists because the future can be dropped.** `request` is awaited inside an axum handler, so
cancelling a run drops that future mid-`await`; the guard undoes the park, the borrowed slot
and the prompt once however the wait ended. The broker talks to the rest of the world
through two traits, `RunGate` and `Window` ([`runs/gate.rs`](../crates/eren-core/src/runs/gate.rs)),
so those properties are asserted directly against fakes rather than inferred from an
integration run.

### A card run's toolbox

A card's run gets Eren's own tools on `/mcp/run/{run_id}`
([`run_tools.rs`](../crates/eren-server/src/mcp/run_tools.rs)): `comment`,
`report_blocker`, `ask_person`, `propose_decision`, `search_kb`, `read_article`, `recall`,
and — for a review pass only — `submit_review`. What a run is offered is read from its row,
never from anything the model says: a planning, summary or workflow pass gets the read tools
only, and every MCP endpoint refuses calls once its run has ended, so a CLI that outlives its
run cannot keep writing on the card. These tools pass `approve` without asking a person, so
**nothing there may merge, start a run, or write settings or check commands** — the test
`only_our_own_tools_skip_the_permission_prompt` and a forbidden-name list (`merge`, `start`,
`setting`, `check`, `run`, `resolve`, `approve`, `decide`) hold that line. An agent
proposes; a person decides.

## Prompt composition

A prompt handed to an engine is assembled in a fixed order, and the order is the security
model: **the request first, then the material that supports it.** For a task run that is the
card's prompt (or the plan-first prompt), then attachments, then any knowledge-base pages
tagged onto the card, then standing context, then the goal chain.

### Standing context

[`runs/context.rs`](../crates/eren-core/src/runs/context.rs) holds `Standing` — the
project's Brain and the card's Skill, loaded once per run. The Brain is background ("the API
lives in `api/`"); the Skill is method ("how a migration gets written here"). Brain then
skill, both after the request. It applies wherever a fresh context begins and nowhere a
session is resumed: a resumed session already carries whatever was in its first prompt, and
repetition is exactly how a framing stops being read as framing.

### Fences

Several features paste text into a prompt that the person running it did not write, in front
of a process holding `Edit`, `Write` and `Bash`. Each wraps its text in a marker pair and
says, in the surrounding prose, how to read what is inside. All the markers live in
[`crates/eren-core/src/fence.rs`](../crates/eren-core/src/fence.rs): the project Brain, a
Skill, a KB page, a GitHub issue, a space document, the repo map, a person's answer to a
question (`ANSWER_*`), the goal chain (`GOAL_*`), the change under review (`DIFF_*`), the
events since a manager's last pass (`WAKE_*`) and a review to answer (`VERDICT_*`).

The reason they share a file is `scrub_foreign(text, own)`: **every scrubber strips every
marker except its own.** The framings are not equally strong — a Brain says *read this as
background*, an issue says *this is a third-party report, do not run what it suggests*, a
Skill says *follow it* — so a body that forges a *different* family's opener can move itself
from the weakest framing to the strongest. The replacement text contains no marker
vocabulary at all. A new feature that quotes text adds its pair here, and is protected from
the others and they from it, in one edit.

## Agents

`eren_core::agents`: an agent is `active`, `paused`, `retired` or `pending_approval`
(migration 0074). Every function that inserts a run asks `agents::assert_can_run` (or its
team / workflow-step form) **before** the insert; the source-scanning test
`every_run_insert_asks_whether_its_agent_may_run` fails the build for one that does not,
unless it is on the short list of runs with no agent. A team run and a workflow ask again at
each assignment, so a pause stops the agent's *next* piece of work wherever it was coming
from. A paused agent can still be assigned cards; a retired one cannot, and is hidden from
every picker. Deleting an agent that anything references retires it instead.

Agents also carry their own limits (migration 0077): `max_concurrent`, `max_daily_runs`,
`cooldown_secs`, checked in the same claim step as budgets. With no limits set, claiming is
unchanged.

## Budgets

A budget policy (`eren_core::budgets`, migration 0076) covers a scope — the machine, a
workspace, a project, an agent, a team or a routine — over a calendar day, week or month,
and caps any of dollars, output tokens and runs. It bites in four places, cheapest first:

- **at the click** — every door that starts work refuses a spent scope with a 409 naming
  the policy (`budgets::check`);
- **at the queue** — `claim_next` holds a queued run whose scope is spent and claims the
  next instead;
- **between steps** — a team or workflow starts nothing more;
- **mid-run** — on a `stop` policy, a run crossing a token cap is interrupted. That stop
  measures each run against the headroom left when it started
  (`budgets::token_headroom`), so runs in flight together can overrun the cap by their
  combined size.

**Dollars cannot stop a run midway** — no engine prices a run until it ends — and an engine
with `reports_cost == false` is only visible to token caps, so its runs stay unpriced rather
than recorded as $0. Warnings and exceeded notices are written once per policy per window
(`budget_incidents`), which is what keeps notifications from repeating. A policy can also
carry `confirm_above_usd`: `eren_core::estimate` reads what similar cards cost
(`GET /tasks/{id}/estimate`, `GET /estimate`) and a start likely to cross it asks first.

## Checks, follow-ups and landing

A **follow-up** (`runs/follow_up.rs`) is a run that goes back into a card's existing
worktree to act on something said about its diff, so the fix lands in the same diff. The
kinds are `ReviewNote`, `FailingChecks`, `MergeConflict`, `Summarize`, `Answer`, `Review`
and `Handoff`, and all go through `enqueue_follow_up` so the agent and budget gates apply. A
follow-up records its note in `runs.review_comment_id`, never `comment_id`, which every
other reader takes to mean "a comment reply" (migration 0071).

**Checks** (`eren_core::checks`, migration 0072) are a project's own test/lint commands,
run in a card's worktree after an agent finishes. Two rules are easy to break:

- **Only `routes/checks.rs` writes `project_checks`** — it holds shell commands this
  machine runs, and `only_this_file_writes_project_checks` fails the build otherwise. Never
  put a check command on a row an agent, an importer or an app build can write.
- **Checks start unasked only after a Full Auto run — or where the review policy sets
  `run_checks_after_every_run`.** They execute code the agent may have edited, so otherwise
  a person clicks "Run checks" and that click is the consent. The same goes for the bounded
  auto-fix (`POST /tasks/{id}/checks/fix`).
- **A check sees none of the person's credentials.** It starts through
  `env_guard::command_without_auth`, which also drops every inherited variable
  `is_auth_env` matches; so does every `git` Eren runs (`worktrees::manager::git`), with
  repository hooks disabled — both run code an agent wrote, as the server.

**Landing** (`eren_core::landing`, migration 0073): a card blocked by another waits for it
to reach *done*. Six things write `done` and share no code path, so the seam is
`tasks.landed_at`, set once by whichever notices first (`landing::land` only sets it where
NULL). A writer of done calls `Orchestrator::landed`; `settle_landings` sweeps every
scheduler tick for the ones that do not. A dependent with `start_when_unblocked` starts
through `start_card`, and every other dependent gets a note and an `unblocked` attention
event.

## Review policy and the merge gate

Module: `eren_core::review`. Migration **0081** adds `project_review_policy`,
`review_decisions` (one per run) and `runs.review_round`.

The policy (`review::Policy`: `require_checks`, `require_review`, `reviewer_agent_id`,
`max_rounds`, `require_pr_green`, `run_checks_after_every_run`) is written only by
`routes/reviews.rs` (`PUT /projects/{id}/review-policy`, behind the write header) —
`only_this_file_writes_the_review_policy` fails the build otherwise. Like a check command it
is a person's setting, never something an agent can call.

**Merge stays a person's click.** The policy decides what that click requires: an agent
review's approval, passing checks, a green pull request — each of the *latest* work
(`review::last_work`); an older green or approval does not count. `review::gate` answers
with the unmet list; `POST /tasks/{id}/merge` returns `409 {kind:"gate", unmet}`, and
`{force, note}` merges anyway with the note on the card and in the audit log.

The reviewer is a `Review` follow-up (`PEER_REVIEW`): read-only like a planning pass, run as
the reviewer agent on its engine, refused for the card's own author or an engine without
`enforces_denied_tools` — a review that could edit the diff it is judging is not a review.
Its verdict comes back only through `submit_review` (`review::submit`, vetted by
`review::vet`); a review that ends without one is recorded as changes requested
(`NO_VERDICT`) — fail closed, never "no news is good news". `Orchestrator::settle_review`
is idempotent: called after every completed run and after checks settle, it starts a review
only when nothing of the card is live and no verdict covers the latest work. The loop is
bounded: one `ReviewNote` fix per round, then at `max_rounds` the card waits in the inbox
(`Kind::Review`). A person's "Review again" gets one round past the cap.

## Hand-off

Module: `eren_core::handoff`. Migration **0082** adds `tasks.handoff_agent_id`,
`handoff_note`, `handoff_from_run_id`, `handoff_requested_at` and `runs.handed_to_run_id`.

Reassigning a running card used to be refused outright. Now `PATCH /tasks/{id}` with a new
agent on a running card is refused unless it carries a note (`vet_note`, 2000 chars); then
`handoff::request` records the request on the card and stops the running agent — recorded
first, so a restart in between still completes it. `handoff::settle` hands over only once no
run of the card is live *and* the old run's `execute` has returned (`is_executing`): its
post-work touches the same worktree, so a terminal status alone is not enough. The new agent
continues in the same worktree through `FollowUp::Handoff` with the note as its brief, or
starts fresh through `enqueue_task` when there is no worktree yet. The swap is a single
claiming `UPDATE`, so `settle_handoff_soon` (tried right after the request) and the
scheduler's `settle_handoffs` sweep cannot both start it.

## The inbox and approvals

Modules: `eren_core::inbox`, `eren_core::approvals`, `eren_core::asks`,
`eren_core::decisions`; `routes/inbox.rs`. Migration **0078** adds `permission_requests`,
`run_questions`, `decisions` and `inbox_marks`.

Everything waiting on a person is one list, `inbox::list` — **a query over the rows that
already say they are waiting, never an index of its own**, which would drift the first time a
transition forgot to write it. The kinds: a card's `Plan`, a `TeamPlan`, a live
`Permission`, a `PermissionExpired` prompt the server restarted under (the inbox offers to
resume the run), a card agent's `Question`, an agent's `Decision`, a `ChatQuestion`, a
`ChatPlan`, an app `Schema` plan, a `KbRevision`, a preview `Recipe`, and a `Review` that
stopped for a person. `Kind::actions` says what each can be answered with from the list;
anything else is "open". `inbox_marks` holds read and snooze state by key (`inbox/read`,
`inbox/snooze`); marking read is in the audit layer's quiet list.

Only what had no row got one. `permission_requests` is written by the broker through
`RunGate::record` / `close`, and `recover_orphans` marks open ones `expired`.
`run_questions` is a card agent's `ask_person` (`asks::ask`, vetted to 600 characters and
five options): the run does not wait — the card goes to review with the question on it, and
the answer (`asks::answer`) returns as `FollowUp::Answer` in the same worktree and session,
fenced as `ANSWER_*`. `decisions` is an agent's `propose_decision`: a closed
`decisions::Effect` — `StartCard`, `MoveCard` (never to running), `AssignCard`,
`AddBlocker`, `PauseAgent` — with a reason, at most five open per run.

**Every answer goes through the function the thing's own button calls.** The transitions
for plans, team plans, chat questions and chat plans live in `approvals` (each keeps its
status check in the `WHERE` of the write, so a double click lands once), and
`POST /inbox/resolve` dispatches to them, to the broker, or to the route handler itself.
Approving a decision runs its effect through the same function the matching button does, so
a proposal can never do what a click could not. **No agent toolbox may resolve an inbox
item.**

The dashboard polls `/api/inbox` once (`lib/inbox.tsx`), shared by the sidebar count, the
top bar's bell and the inbox page.

## Accounts

Off until an admin exists. With no row in `users` (migration 0089) Eren is the single-person
server the rest of this document describes: `auth::require_session` marks every request
`Caller::Local` — who owns every workspace and may change every setting — and the access token
decides who reaches it. `eren admin create` (in `crates/eren-cli/src/main.rs`, reaching the
database through `DATABASE_URL`, the port a running server leaves in `~/.eren/pg_port`, or a
managed Postgres it starts itself) makes the one admin — a partial unique index holds "one" —
and, in the same transaction, gives it every workspace that has no `owner_id`.

`auth::Accounts` notices within five seconds (it asks again while off, never once on) and from
then on:

- **`require_session`** reads the `eren_session` cookie (`eren_core::sessions`: 32 random bytes,
  stored as SHA-256, sliding 30 days), puts `Caller::User` in the request, and refuses anything
  without one but the sign-in page's own requests (`auth::open_path`) and `/mcp` from a
  loopback peer. The token layer outside it steps aside. An account the admin reset reaches
  only `/api/auth/…` until it picks a new password.
- **Every handler takes `Caller` or `Admin`** — `routes::tests::every_handler_takes_a_caller`
  fails the build otherwise — and calls `caller.require(&state, Owned::…)` on each id it is
  handed before reading or writing. `eren_core::scope::Owned` is a closed enum with one fixed
  query per kind from the row to its workspace; a run's is the first of its nine possible
  parents that answers. Someone else's id is a 404. Lists take
  `caller.workspace_filter(workspace_id)`: the one asked for, checked, or all the caller's own
  — never "every workspace" because a parameter was left off.
- **`Admin`** gates what belongs to the machine: settings writes, the queue, machine-scope
  budgets, the audit log, the account list. With accounts off everybody is the admin.
- **`/ws`** requires a `run_id` it can check; the firehose of every run's events is gone.

Sign-up (`users::sign_up`) is closed until the admin opens it (`settings` key `signup`, absent
means closed), and makes a workspace for the new account. The admin's reset sets a temporary password with
`must_change_password` and deletes the account's sessions. Agents, routines and the queue do not
know about accounts: they act within the workspace of what they run for, and the people who
can see that workspace are the ones who own it. What accounts do not separate — one OS user,
one filesystem — is set out in SECURITY.md.

## The audit log

Module: `eren_core::audit`; `crates/eren-server/src/audit_layer.rs`. Migration **0079**
adds `audit_log` — no foreign keys and no cascades, deliberately: the ledger outlives what it
describes.

`audit_log` is append-only and has exactly one writer, `audit::record`;
`only_this_module_writes_the_ledger` fails the build for any other `INSERT`/`UPDATE`/`DELETE`
on it, and the only delete is its retention `prune` (agent and system rows after
`RETENTION_DAYS` = 90; API rows kept indefinitely), run hourly by the scheduler. Recording never fails the
thing it records — a row that could not be written is logged and dropped.

Three things feed it, as `audit::Actor`:

- `Api` — the server's `audit_layer` on every mutating `/api` request: the route template,
  the path ids (`audit_layer::entity`) and the status — **never the body**, which carries
  prompts, file contents and the occasional secret. The few read-only POSTs are listed in
  `audit_layer::QUIET`, each with its reason, and a test keeps that list honest.
- `Agent(run)` — the three MCP dispatches, by tool name and outcome — **never the input**.
- `System` — Eren's own actions: routine fires, sweeps, a reaped run, a hand-off.

With accounts on it records `User(id)`; with accounts off, `api`, not "a person": there is
no login then, and any local process can call the API. `GET /audit` lists with a filter, `GET /audit.csv` exports, and a card's merged story —
comments, runs, checks and what was done to it — is `GET /tasks/{id}/timeline`.

## Config revisions

Module: `eren_core::revisions`; `routes/revisions.rs`. Migration **0080** adds
`config_revisions`.

An edit to an agent, team, routine (manager included), skill, project checks, budget
policy, review policy, the attention setting or the unattended setting keeps the row it
replaced — `revisions::keep`, called by the entity's own update and delete handlers just
before they write, `KEEP` = 20 per entity. `every_writer_keeps_a_revision` names each writer
and fails for one added without it. Rows are read generically (`to_jsonb`) from a table
chosen by the closed `EntityKind` enum, never from request text: those names are
interpolated into SQL.

**A restore is an edit**: `POST /revisions/{rev}/restore` maps the snapshot onto that
entity's own update body and calls its own handler, so validation, the write header, the
single writer of `project_checks` and the audit entry all apply — and the restore is itself a
revision, so undoing an undo is one more click.

## Wakes

Module: `eren_core::wake`. Migration **0083** adds `wakeups` and, on `routines`,
`on_events`, `cooldown_secs` and `max_passes_per_day`.

A manager routine runs on a schedule, so a card that fails at 9:05 waits for tomorrow's pass
to be noticed. A wake is a row saying what happened: `wake::Kind` is `Landed`, `Unblocked`,
`Failed`, `ChecksExhausted`, `ReviewExhausted`, `Question` or `Stalled`. `wake::raise`
writes a row only for a manager that ticked that kind (`routines.on_events`, empty by
default — an early pass costs a run), coalesced to one open row per (target, kind, card)
whose `count` goes up, so a card that fails ten times overnight is one line in the morning.
`ESCALATES` (`Failed`, `Question`, `ReviewExhausted`, `Stalled`) also wakes the manager
routine of the card's agent's manager, up the org chart. A raise never fails the thing that
raised it.

`Orchestrator::drain_wakeups` runs every scheduler tick and fires through
`routines::fire("wake")` only when the manager's thread is idle (checked before firing, so
news is kept, not dropped), its cooldown has passed and it has early passes left today
(`may_wake`). Every manage pass, early or scheduled, opens with a fenced "Since your last
pass" (`wake::render`, `WAKE_*`) and `consume`s it only once its turn has queued. A wake can
also target an agent rather than a routine — that is how a heartbeat is woken early.

## The reaper and liveness

Module: `eren_core::reaper`. Migration **0084** adds `runs.last_event_at`, `runs.reaped`
(`lost` | `silent`) and `runs.auto_resumed_at`.

Two ways a run can be stuck, told apart by different things:

- **Lost.** The row says starting/running/waiting but nothing in this process is executing
  it (`is_executing` false for over `LOST_AFTER` = 120 s). Its task died — a panic, a dropped
  future — and nobody will ever write its ending. `recover_orphans` catches this across a
  restart; `Orchestrator::reap` catches it while the server keeps running. Always on: a run
  nothing is executing cannot finish by itself.
- **Silent.** It is executing but has said nothing for longer than the person allows.
  Off by default — a long build is silent too — and never for a run waiting on a person's
  permission. `last_event_at` is written at most every fifteen seconds for the dashboard;
  the reaper itself reads the in-memory registry.

Both are opt-in through "Unattended runs" (`reaper::Unattended`, the `unattended` settings
key, `GET/PUT /settings/unattended`, revisioned). Auto-resume goes through
`runs::resume::resume_dead_run` — the Resume button's own door, which `POST
/runs/{id}/resume` calls too — once per stopped run (`auto_resumed_at`) and at most
`MAX_AUTO_RESUMES` = 2 along a `resumed_from` chain. Every stall is said on the card, raised
as a `stalled` wake and sent through attention. Resume writes a new run row rather than
re-queueing the dead one, because `cost_usd` accumulates, `events` is unique per
`(run_id, seq)`, and `error_reason` is the thing you want to still be able to read.

## The org chart

Module: `eren_core::org_chart`. Migration **0085** adds `agents.reports_to`, `title`,
`heartbeat_secs` and `last_heartbeat_at`.

`reports_to` makes a workspace's agents a tree, kept one by `org_chart::set_manager` — the
only writer — under a per-workspace advisory lock: same workspace, no cycle, depth ≤
`MAX_DEPTH` (8), no retired manager (`vet_manager`, `Refused`). It is reached through
`PATCH /agents/{id}`; `GET /workspaces/{id}/org-chart` reads the tree with each agent's
live cards. Retiring or deleting a manager lifts its reports to its own manager in the same
transaction (`lift`).

Delegation flows down: a manager pass is told its reports (`org_chart::reports`,
`render_reports`), and `create_task` from a pass whose agent has reports may bind only
agents in its subtree (`may_delegate`). Trouble flows up: `wake::ESCALATES` kinds also wake
the manager routine of the card's agent's manager. A workspace that never draws a chart
works exactly as before. The shapes are copied from `kb::tree`, which keeps knowledge pages
a tree.

## Goals

Module: `eren_core::goals`; `routes/goals.rs`. Migration **0086** adds `goals`,
`project_goals`, `tasks.goal_id`, `routines.goal_id` and the `tasks_inherit_goal` trigger.

A per-workspace tree (one writer of `parent_id`, an advisory lock, no cycle, depth ≤
`MAX_DEPTH` = 6; status `active` | `achieved` | `abandoned`). Cards and manager routines
point at the goal they serve; a card split from an epic inherits the epic's goal by a
`BEFORE INSERT` trigger, because epics are split in more than one place; a card a manager
pass files takes the routine's goal unless it names one (`goals::of_pass`).

Every card run gets a fenced, capped (`MAX_CONTEXT_CHARS` = 1800) "Why this matters" —
the goal chain top-down plus the epic (`why_this_matters`, `GOAL_*`) — appended after the
brief, Brain and skill; a manager pass gets the active goals with progress
(`render_for_pass`). **Progress is counted, never stored**: done cards over all cards in the
goal's subtree, on every read, so it cannot drift from the board. Deleting a goal moves its
children and cards up to its parent. `GET /workspaces/{id}/goals`, `GET /goals/{id}`.

## Heartbeats

Module: `eren_core::heartbeat`. Migration **0087** adds `heartbeats` (the interval column
itself came with 0085).

An agent with `heartbeat_secs` (a person's standing decision, like `start_when_unblocked`;
5, 15, 60 or 240 minutes) beats on a timer — claimed with a compare-and-set on
`last_heartbeat_at`, so two ticks cannot both beat — and early through `wakeups` with an
`agent_id` target (a card assigned to it, one of its cards unblocked). `heartbeat::beat`
does at most one thing, in order:

1. nothing if the agent is paused or retired, or already working (one card at a time unless
   `max_concurrent` says more);
2. fire the manager routine it runs, through `routines::fire` with its own cooldown and
   daily cap;
3. start its next unblocked backlog card **through `start_card`**, so every gate the Start
   button meets applies;
4. otherwise record an idle beat. No model is called; an idle heartbeat costs nothing.

Every beat is logged to `heartbeats` (`reason` timer | wake; `outcome` started | fired |
idle | busy | held | paused), `KEEP` = 500 per agent — enough to read a week of a
15-minute heartbeat. `GET /agents/{id}/heartbeats`, `GET /workspaces/{id}/heartbeats`.
`Orchestrator::heartbeats` runs every scheduler tick.

## Routines and the project manager

`eren_core::routines` (migrations 0057–0059): a prompt that runs on a schedule — a chat
turn in a standing thread, a fresh research report, a board card, or a watch on a page.
`fire` only *enqueues* through the same doors a person uses, so concurrency limits, backoff,
permission prompts and spend accounting behave exactly as for manual work; what a firing
produced is a `routine_runs` row pointing at the ordinary artifact. The scheduler evaluates
routines in the server's local time (a person writing `0 9 * * *` means their own 9am) and
workflows in UTC.

`eren_core::manager` (migration 0067) is the project manager: an agent that reviews the
board on a schedule and acts on it with nobody watching. A pass is an ordinary chat turn in
the project's standing manager thread, wearing the prompt this module composes; session
resume is what makes a *manager* rather than a series of strangers. The cap on how many
cards a pass may start is enforced by counting `manager_actions` rows in the MCP tool
handler — the model cannot talk its way past it — and stated in the prompt as well, because
an agent that discovers its budget by being refused tends to retry. `POST
/projects/{id}/manager/run`, `GET /projects/{id}/manager/passes`. Wakes, the org chart and
goals all feed this pass.

## Teams and workflows

**Teams** (`runs/org/`, migrations 0007, 0008, 0012, 0013): a team with a manager. The
manager reads the goal and the roster, splits the work into briefed assignments, and hands
them out; specialists can talk to the team and escalate decisions back; a failure goes to
the manager for a decision rather than killing the run. **The plan lives in the database,
not on a stack frame** — which is what makes a human editing the plan before work starts
(`/org-runs/{id}/plan/approve`, `/reject`), the manager revising it mid-run, and a crashed
run picking up where it left off the same mechanism. Assignments share one worktree, so a
run produces a single diff to review; within that, work runs in parallel exactly when the
manager declared non-overlapping file scopes and no dependency (`schedule`). A board card
handed to a team (`POST /teams/{id}/run-org`) keeps the card's worktree, so review and
merge work as for a solo card. Epics (`org/epic.rs`, migration 0028) are the board's
hierarchy for the same decomposition.

**Workflows** (`eren_shared::workflow`, migrations 0004–0006): the YAML users write in
`.eren/workflows/*.yaml`, with validation, dependency ordering and prompt interpolation.
One format covers every pattern — a plain sequence is a pipeline, a step with
`strategy.parallel` fans out, and a step that `needs` a fan-out step sees all of its outputs.
Steps default to Auto-edit (`workflow_permission_mode`), under the same Full Auto gate as a
card: where it does not hold, a step steps down to `short_of_full_auto` for its own engine,
and a step on an engine with nothing narrower (Amp, Cursor) is refused. The canvas (`@xyflow/react`) round-trips to that YAML in `web/src/lib/workflowGraph.ts`;
node positions live in the database (0006) so committed workflows stay clean.

## Apps

An app is a **project** under `~/.eren/apps/<slug>` (`projects.kind='app'`, migrations
0040–0043), which is what gives it worktrees, diffs and the files editor for free. Two
runtimes sit behind one manifest: a *module* declares models, views and actions and Eren's
own dashboard renders it — nothing arbitrary executes; a *container* app is the escape hatch
for work that needs code. Neither ever receives a database connection: an app declares what
it stores and reaches its rows through Eren.

Its manifest (`apps/manifest.rs`, `eren.app.yaml`) is parsed by hand: declaration order is
display order, errors name the offending key, and unknown keys are refused. Field types and
the identifier charset are closed sets — those identifiers are interpolated into DDL, and the
defence is the charset, not the quoting. Two rules that are easy to break:

- **Nothing an app sends is ever an identifier.** `apps::query` looks field names up among
  the declared fields and emits the *manifest's* copy; operators come from an enum; values
  are always bound.
- **Additive schema changes apply; destructive ones wait.** `apps::schema::plan` diffs
  declared models against `information_schema` — never a registry. A plan with any
  destructive statement runs *nothing* and is stored whole (`app_schema_plans`), so what a
  person approves in the inbox is byte-for-byte what executes. Approving claims the plan
  (`pending` → `applying`, 0093) before its DDL runs, so two approvals run it once.
  Foreign keys are read back from `pg_constraint` like everything else: a field that becomes
  a `ref:`, or points at another model, has its key moved — dropped before any column
  changes type, added after every table exists — and where the column already holds values,
  those that are not ids in the new target are cleared first, which makes that plan a
  question. Index and key names longer than Postgres's 63 bytes are shortened with a hash
  (`schema::object_name`); names that always fitted are unchanged, and an index Postgres had
  already cut short is recognised by its column and renamed rather than rebuilt.

Changing an app is an ordinary card on its own project with two differences, both in
`apps/build.rs`: **it lands without review, so the undo has to work** — `settle()`
squash-merges on completion, `app_builds.base_commit` is read *before* the card exists, and
only the newest landed build is `revertible()`; and **every write Eren makes to an app's
folder is committed** (`apps::commit`), because a file written but not committed is not on
`main`, and `git worktree add` hands the next agent an empty folder. Landing does not bypass
the schema gate. The page reaches Eren on `/__eren/*` through `app_bridge.rs`, answered in
Eren's process before anything is forwarded.

The expression language exists twice — `apps/expr.rs` and `web/src/lib/expr.ts` — because
`show_if` cannot afford a round trip and computed values cannot be decided by a browser.
`crates/eren-core/src/apps/expr_cases.json` is the specification and both suites read it.

## The knowledge base

`kb/` (migrations 0024–0027, 0050, 0055, 0061–0063): a wiki people write, or ask an agent
to write. Pages are a tree (`kb/tree.rs` — cycles checked before a move is written, walks
depth-bounded). Bodies are HTML from TipTap and live in Postgres so they are searchable and
transactional; only pasted assets go to object storage. **A body is never written
directly**: the live body is the newest `accepted` revision (`kb/revisions.rs`). A person's
save is accepted immediately; an agent's is only ever *proposed* and waits in the inbox
(`KbRevision`) — the asymmetry that makes it safe for an agent to have write access to a
wiki a person also edits. `kb::search` is what a run's `search_kb` calls. The project Brain
(`brain.rs`, 0050) is the durable context every run on a project starts with; spaces and the
repository index (`rag/`, `repo/`) give the assistant retrieval, with embeddings computed
locally — Eren never calls a model API.

## Previews

`previews/` (migrations 0030–0036): run a card's branch and look at it. A card in review is
a diff, and reading a diff is a poor way to answer "does this look right". One container
per card (enforced by a partial unique index) on one loopback port, with hard memory/CPU/pid
caps; previews nobody is looking at stop themselves (`idle_loop`), keeping their image so
coming back costs seconds. A compose stack (`previews/compose.rs`) is agent-written code, so
`vet` checks it against a closed allow-list of keys before it is rendered — anything that
reaches the host (`privileged`, capabilities, devices, shared namespaces, `security_opt`,
`extends`, host-path mounts, external volumes and networks, build contexts outside the
stack's folder) or that the list does not know is refused with the service and key named,
host ports and `container_name` are stripped, and `render` writes the single-container caps
onto every service. A Dockerfile or compose recipe an agent wrote is kept in
`preview_recipes`, never written into the branch, and waits in the inbox (`Recipe`) for a
person to read before it is built. `previews::reconcile` at boot reads Docker rather than the
table, so a container no row claims is swept rather than orphaned — under either of the
product's names (`brand::NAMES`), so stacks started before the rename are still found.

## The dashboard

`web/` is React 18 + Vite + Tailwind 4, with React Router, `@xyflow/react` for the workflow
canvas, TipTap for the knowledge-base editor, Monaco for the files editor (pinned into its
own chunk), `cmdk` for the palette, Radix for dialogs and `framer-motion` for motion.

### The shell and the palette

`web/src/AppShell.tsx` is sidebar, top bar, page. On a wide screen the sidebar
(`components/shell/Sidebar.tsx`) is docked and can fold to an icon rail; on a narrow one it
is a drawer behind the top bar's menu button. `lib/nav.ts` is the one list of pages in four
groups — Work (Home, Inbox, Projects, Chat, Activity), Organization (Org chart, Agents,
Teams, Goals, Routines), Knowledge (Knowledge, Research, Apps, Skills) and System
(Connections, Audit log, Settings) — with keywords, so the sidebar and the palette cannot
disagree about what a page is called. The top bar carries the inbox bell and the theme
switch. ⌘K opens `components/shell/CommandPalette.tsx` from anywhere: pages filtered locally,
search hits from `/api/search` once there are two characters (debounced, with a stale guard),
and the handful of actions people reach for from anywhere.

### Talking to the server

`web/src/lib/api.ts` is the single API client: every call to `/api` goes through it, and a
person's writes carry the `X-Eren-Write` header the server requires. `web/src/lib/ws.ts` is
the socket, whose `useRunStream` hook opens `/ws?run_id=…&after_seq=…` and merges the
replayed frames with the live tail. It reconnects with backoff and resumes from
`SeqLedger.floor` — the highest seq below which nothing is missing, not the highest seen,
because concurrent steps publish out of order. Replay frames nest the payload under `event`
while live frames are flat, and `step_id` sits on the envelope in both cases — `parseFrame`
lifts it out explicitly, or every multi-agent view silently loses the ability to say *who*
acted.

In development, `pnpm dev` serves the dashboard from :5173 and proxies `/api` and `/ws`
through to :4820. The proxy deliberately leaves `changeOrigin` off: the server admits a page
only when its `Origin` names the same authority as the request's `Host`, and forwarding the
browser's own `Host` is what keeps the Vite page same-origin. `pnpm build` produces
`web/dist`, which the server serves as a fallback (`EREN_WEB_DIST` overrides the path).

### The design system

Every colour on screen comes from a token in `web/src/index.css`. The `@theme { … }` block
defines the light palette as Tailwind theme variables — surfaces (`bg`, `panel`,
`raised`), borders, text (`fg`, `fg-muted`, `fg-subtle`), `accent`, the four status
families (`success`, `warning`, `danger`, `info`, each with a `-fg` and a `-subtle`), the
tier colours, the tint/ink pairs agent avatars use, shadows, fonts and motion durations —
and `:root[data-theme="dark"] { … }` redefines the same names. That attribute on `<html>`
is the whole mechanism (`lib/theme.tsx`, key `eren.theme`, light / dark / system): switching
is one attribute write and nothing re-renders to recolour, and an inline script in
`index.html` sets it before first paint so a dark reload never flashes white.

The component kit in `web/src/components/ui/` — `Button`, `IconButton`, `Badge`, `Kbd`,
`StatusDot`, `Avatar`, `Field`, `Input`, `Select`, `Textarea`, `Checkbox`, `Switch`,
`Dialog`, `Sheet`, `Popover`, `Menu`, `Tooltip`, `Tabs`, `Table`, `Card`, `Page`,
`PageHeader`, `EmptyState`, `Skeleton`, `Meter`, `Progress`, `Toaster` / `toast`, `RunError`,
`Icon` —
is the only place that may paint an overlay, because that is what gives every overlay a
focus trap, Escape and a label.

Two tests hold the system in place:

- **`web/src/lib/design-scan.test.ts`** reads every `.ts`/`.tsx` the app is built from
  through Vite's own glob and refuses the shortcuts: Tailwind's named palette
  (`bg-gray-50`), raw hex outside the code-surface themes (`theme/editorThemes.ts`) and
  agent swatches (`lib/swatches.ts`), `bg-white` / `text-black` (only right in one theme)
  outside the kit, and `fixed inset-0` scrims outside the kit and the shell. It also checks
  that it read more than 150 files, so a path mistake cannot make it pass on nothing.
- **`web/src/lib/contrast.test.ts`** reads `index.css` from disk, resolves every text token
  against every surface it is meant to sit on, in both themes, and holds each pair to WCAG AA
  (4.5:1) — so a palette tweak that makes a label unreadable in dark mode fails here rather
  than in someone's eyes.

### Why pure logic lives in `web/src/lib/*.ts`

There is no jsdom and no DOM testing library — vitest runs in Node, every test file is a
`.ts`, and nothing renders a component. That is a constraint to design around: anything worth
asserting has to be extractable from the component that uses it. Almost all of it lives in
`web/src/lib/` as a `foo.ts` beside a `foo.test.ts` — `workflowGraph`, `expr`, `diff`,
`mention`, `kbTree`, `cron`, `repoGraph`, `spend`, `usage`, `runStatus`, `pullRequest`,
`apps`, `language`, `checks`, `forecast`, `goals`, `orgChart`, `inbox`, `nav`, `theme`,
`brand`, `mergeRefusal`, `runHistory`, `ws`. The one test outside that folder,
`components/RunStream.test.ts`, imports only the pure helpers the component exports. If your
change puts a decision inside a component's body, the test for it cannot exist.

## The rename's compatibility layer

Eren was called aichip, and the old name is written into state that already exists on people's machines: Eren's home folder (whose absolute paths are stored in the database and in git's own worktree links), environment variables in shell profiles and hook scripts, card branches, the managed database, a storage bucket, folders committed to repositories, headers sent by scripts, app manifests, and app files that call the bridge by its old path. A rename that only rewrote the source would strand all of it. The rule for every kind of state is the same: **write the new name, read both, prefer the new.** Nothing a person made is moved or rewritten, with one exception — the home folder.

**[`crates/eren-shared/src/brand.rs`](../crates/eren-shared/src/brand.rs)** is where every
old spelling lives, and nowhere else. `NAME` / `LABEL` / `NAMES`; `env_name`, `env_names`,
`var` and `var_os` (`EREN_<key>`, else the old prefix); `with_legacy_env`, which sets each
`EREN_*` variable a child gets under its old name too, so a check command or hook script
written against the old API keeps working; `BRANCH_PREFIXES` and `is_card_branch`;
`REPO_DIRS` and `repo_dir` (`.eren/workflows`, or the old folder a repository committed);
`APP_MANIFESTS` and `app_manifest`; `BUNDLE_KINDS`; `MCP_TOOL_PREFIX` (`mcp__eren__`),
`tool_name` and `rename_tools` (an old tool name in a saved allow-list still means the same
tool — the orchestrator runs every agent's list through it); `WRITE_HEADERS` and
`APP_HEADERS`; `BRIDGE_PREFIXES` (`/__eren/` and the old path apps built earlier load
`client.js` from); `DATABASE` / `LEGACY_DATABASE`; `BUCKET` / `LEGACY_BUCKET`. When the
compatibility window closes, deleting the `LEGACY*` items and following the compile errors
removes it completely.

**The scan test.** `no_old_name_outside_brand`, in the same file, lists the repository with `git ls-files` and fails the build if the old name turns up anywhere but `brand.rs`, `legacy.rs`, `web/src/lib/brand.ts` and its test, the migration history (never edited), and the configuration that names data already on people's machines (`docker-compose.yml`, `.env.example`, `Dockerfile`, `web/index.html`). A document may say what Eren used to be called — but only beside the name it is now: in any `.md` line that mentions aichip, the word `eren` must appear too, so a stale `cargo run -p <old>-cli` cannot hide as history. This document follows that rule; so must yours.

**The home folder** is Eren's own, so it is moved once. `brand::adopt_legacy_home`, called first thing by `serve` and `doctor` through `adopt_legacy_state` in `crates/eren-cli/src/main.rs` — before the managed Postgres starts, so no file in the folder is open — renames `~/.aichip` to `~/.eren` and leaves a relative symlink at the old path. **The link is the point.** The database stores absolute paths into the home folder (a card's worktree, an app's folder), and git records each worktree's absolute path in the repository it belongs to; rewriting all of that would mean editing files inside people's repositories, and a link makes every one of those paths keep resolving with nothing rewritten. A rename within one directory is atomic, so an interruption leaves either layout, never half; if the link cannot be made the move is undone and the command refuses to start, because a moved folder without its link would break every stored path. Two real folders are left alone (`Adoption::Both`) — merging two homes is not something to guess at. `adopt_legacy_state` is never silent: the move is logged, and every `AICHIP_*` variable still set in the environment gets a warning naming its `EREN_*` replacement (`legacy_env_in_use`). The `Dockerfile` makes the same link for the container's volume.

**[`crates/eren-core/src/legacy.rs`](../crates/eren-core/src/legacy.rs)** is the part that
needs a database, and both halves run at boot and are no-ops the second time.
`adopt_database` renames the managed Postgres database in place (`ALTER DATABASE … RENAME`,
instant whatever its size) from an admin connection before the pool opens, because Postgres
refuses to rename a database anything is connected to. `adopt_legacy_manifests` gives every
app Eren made its manifest's new name (`eren.app.yaml`) and commits the rename, so the next
build's worktree opens on the name the prompt tells its agent to edit; a manifest in
somebody's repository is only ever read under either name.

**Migration 0088** (`0088_rename_tools.sql`) rewrites the rows already in the database: an
agent's `allowed_tools` holds `mcp__eren__<tool>` names, and an agent saved before the
rename would otherwise have quietly lost every Eren tool it was allowed. It matches the whole
server or a tool of it, never a different server whose name merely starts the same way, and
keeps the order. `brand::tool_name` catches whatever arrives later from a file.

**The dashboard's half** is `web/src/lib/brand.ts`. `adoptLegacyStorage(storage)` moves
every `localStorage` key saved under the old prefix to the new one, generic over the key so
a remembered workspace, a draft and the theme all come across; a value already saved under
the new name wins. `web/src/lib/adoptLegacyStorage.ts` runs it and is the **first import in
`main.tsx`**, so settings are in place before any module reads one, and it swallows the
throw a private window gives. `toolName` shows a transcript recorded before the rename the
way a new one is shown. `web/index.html` is on the scan's allow-list because its inline theme
script reads the old key on the one load before `brand.ts` moves it.

## Testing

`cargo test` runs the whole workspace against the **mock engine**
([`crates/eren-engines/src/mock/`](../crates/eren-engines/src/mock/)), which replays
recorded stream-json fixtures with configurable pacing. No model usage, no rate limits, no
credentials. The newer adapters are tested end to end — argv, spawn, pump, parser — against
a shell-script stand-in for the binary (`stand_in`, `replaying` in the engines crate), so
they need no CLI installed. `cd web && pnpm test` runs vitest.

Rust tests are inline `#[cfg(test)] mod tests` next to the code they test — there is no
`tests/` directory in any crate. Tests of SQL live in an inline `mod db_tests` and start with
`let Some(t) = testdb::fresh().await else { return };`
([`crates/eren-core/src/testdb.rs`](../crates/eren-core/src/testdb.rs)): each gets its own
migrated database on the server `DATABASE_URL` names, an orchestrator with the mock engine
if it asks, and skips when `DATABASE_URL` is unset. CI runs them against a Postgres service;
locally, against `docker compose up -d`, run Eren's tests with
`DATABASE_URL=postgres://aichip:aichip@localhost:5433/aichip cargo test` — Eren's compose
keeps those legacy names because they name an existing volume.

The house style is still to make the interesting decision pure so it can be asserted
directly: `resolve_step_permission`, `Standing::apply`, `fence::scrub_foreign`,
`rate_limit_backoff`, `claude_args`, `Slots`, `vet`, `wake::may_wake`,
`reaper::is_silent`, `goals::render_why`, `manager`'s prompt. When you find yourself
wanting a database to test a rule, that is usually a sign the rule wants extracting.

Several tests read the source rather than run it, because review alone does not catch a
string: the `Command::new` scan in `env_guard.rs`, `every_run_insert_asks_whether_its_agent_may_run`,
`only_this_module_writes_the_ledger`, `every_writer_keeps_a_revision`,
`only_this_file_writes_project_checks`, `only_this_file_writes_the_review_policy`, the
forbidden tool names in `run_tools.rs`, `no_old_name_outside_brand`, and the dashboard's
design scan. Test names are sentences about the property, and several name the bug they pin —
`an_unbalanced_reclaim_never_underflows`,
`the_engine_is_never_told_a_person_refused_something_nobody_saw`,
`a_run_ends_once_with_the_last_word`. Follow that: a test whose name says what would break is
a test the next person will not delete by accident.

## Before your first pull request

- Re-read the four invariants at the top of `crates/eren-engines/src/lib.rs`. A change
  that touches process spawning, environment, or engine detection is judged against them
  first.
- Gate on a `Capabilities` flag, never on an engine id. If the capability you need does not
  exist yet, add it — and answer for it in every adapter, since there is no `Default`.
- Anything that must not happen goes in `denied_tools`. Naming it in `allowed_tools` grants
  nothing and forbids nothing.
- An agent proposes; a person decides. Nothing reachable from an MCP toolbox may merge,
  start a run, resolve an inbox item, or write a setting or a check command.
- A new way to start a run asks `agents::assert_can_run` before the insert and goes through
  `start_card` where a card is involved. A new table that holds configuration gets a
  `revisions::keep` in its writer. A new mutating route is recorded by the audit layer for
  free; a read-only POST goes in `QUIET` with its reason.
- A new migration gets a row in the table below.
- Ask `eren_shared::brand` for the product's name; never spell the old one.
- Commits and pull requests in this repository carry no AI attribution of any kind — no
  trailers, no footers, no notes in the body. Write the message as the author would: what
  changed and why.

## Schema history

One row per file in `crates/eren-core/migrations/`. The number is the filename's prefix.

| # | Purpose |
|---|---|
| `0001` | Initial schema: `projects`, `agents`, `teams`, `tasks`, `workflows`, `runs`, `steps`, `events`, `queue`, `schedules`, `settings`. |
| `0002` | Workspaces and chat: `workspaces`, `chats`, `chat_messages`; name uniqueness becomes per-workspace. |
| `0003` | A chat session id is only resumable by the engine that produced it. |
| `0004` | Steps of a workflow run are displayed in creation order. |
| `0005` | Scheduling state lives on the workflow itself; the unused `schedules` table goes. |
| `0006` | Canvas node positions, kept out of the YAML so committed workflows stay clean. |
| `0007` | Organizations: a team with a manager; an org run's steps are its assignments; `org_messages`. |
| `0008` | A board task can be handed to a whole team, keeping the task's worktree. |
| `0009` | `attachments`: files attached to a task prompt or chat message, stored outside every git tree. |
| `0010` | Whether a project is under version control (the in-place fallback). |
| `0011` | `task_comments` and `agent_memories`: card conversations and what an agent remembers. |
| `0012` | Org delegation: the plan moves into the database, so approval, re-planning and recovery are one mechanism. |
| `0013` | Which files an assignment expects to touch, so non-overlapping assignments can run in parallel. |
| `0014` | A global pause on starting new work, stored so it survives a restart. |
| `0015` | Backfill: step rows left non-terminal under a run that already ended. |
| `0016` | A machine-wide daily spending ceiling. |
| `0017` | `mcp_servers` and `agent_mcp_servers`: MCP servers the user brings themselves. |
| `0018` | Review comments anchored to a line of the diff. |
| `0019` | Bake-offs: one task run several ways at once, keeping the best result. |
| `0020` | An agent may defer to the workspace permission default. |
| `0021` | A card may inherit the workspace permission default instead of pinning one. |
| `0022` | Engines become plural: a team or an agent can prefer an engine. |
| `0023` | Plan-first cards: the agent writes its plan and the run parks for approval. |
| `0024` | The knowledge base: `kb_articles`, `kb_assets`, `task_articles`, `comment_articles`. |
| `0025` | The knowledge base becomes a wiki: a tree, addresses, `kb_revisions`, `kb_links`. |
| `0026` | A page's optimistic-concurrency token, separate from `current_seq`. |
| `0027` | Reverse lookups for "which tasks use this page". |
| `0028` | Epics: the board's first hierarchy. |
| `0029` | Model tier and reasoning effort, choosable everywhere work is started. |
| `0030` | `previews`: run a task's branch in one container on its own port. |
| `0031` | Preview capacity and idling, so a forgotten preview does not hold the machine. |
| `0032` | Preview slugs: a name that survives a rebuild. |
| `0033` | `preview_recipes`: a Dockerfile an agent wrote that a person reads before it is built. |
| `0034` | Base previews: preview the branch a card will merge into. |
| `0035` | Compose previews: the rewritten compose file and its path, recorded for teardown. |
| `0036` | Which shape a recipe is (Dockerfile or compose). |
| `0037` | `usage_limits`: where the user's plan stands, as their own CLI reports it. |
| `0038` | Token accounting, including cache counters, per run. |
| `0039` | Which tier a run used, who chose it, and why. |
| `0040` | Apps: `apps` and `app_builds`; an app is a project under the home folder. |
| `0041` | `app_schema_plans`: DDL a manifest implies, waiting for someone to read it. |
| `0042` | `app_grants`: what a person has let an app do, one row per grant. |
| `0043` | A build that was undone (`reverted`). |
| `0044` | `usage_events`: when each limit window changed state. |
| `0045` | The pull request a card was finished as. |
| `0046` | Which GitHub repository a project is, kept rather than re-derived. |
| `0047` | Where a card came from (an imported issue), as a discriminator. |
| `0048` | `chat_message_agents`: which agents a chat message mentioned. |
| `0049` | Per-project defaults for what a card runs on. |
| `0050` | The project Brain: `project_brain` and its revisions. |
| `0051` | `skills` and `chat_message_skills`: a named way of doing something, smaller than an agent. |
| `0052` | How many times a run has been held for a rate limit (`rate_limit_attempts`), and `runs.resumed_from`. |
| `0053` | `researches`: ask a question about a project, get a cited report. |
| `0054` | General chats and researches, scoped to a workspace rather than a project. |
| `0055` | `space_documents` and `space_chunks`: local embeddings over a space's documents. |
| `0056` | What a research runs as: tier and effort. |
| `0057` | `routines` and `routine_runs`: a prompt that runs on a schedule. |
| `0058` | Index for "was this run a routine firing?". |
| `0059` | The watch kind: a routine that checks a page, with its URL as a column. |
| `0060` | `task_deps`: a card blocked by other cards, which must have landed. |
| `0061` | The semantic index grows from spaces to repositories (`project_index`). |
| `0062` | `project_symbols`, `project_imports`, `project_edges`: what files contain and what depends on what. |
| `0063` | `chat_message_articles`: which pages a chat message was sent with. |
| `0064` | Plan mode for the assistant chat. |
| `0065` | `chat_questions`: the assistant asks with options instead of guessing. |
| `0066` | A reply the person stopped part-way is kept. |
| `0067` | The project manager: `manager_actions`, and a routine's agent and start cap. |
| `0068` | A skill that came from somewhere else (`npx skills add`). |
| `0069` | One conversation on one model, without moving a tier. |
| `0070` | Indexes for the paths every run walks; a duplicate `events` index dropped. |
| `0071` | A follow-up run keeps its note in `review_comment_id`. |
| `0072` | Checks: `project_checks` and `check_runs`. |
| `0073` | Landing: `tasks.landed_at` and `start_when_unblocked`. |
| `0074` | Agent status: active, paused, retired, pending approval. |
| `0075` | What an agent said stopped it (`blocked_note`). |
| `0076` | Budgets: `budget_policies` and `budget_incidents`. |
| `0077` | Per-agent limits: at a time, per day, rest between runs. |
| `0078` | The inbox: `permission_requests`, `run_questions`, `decisions`, `inbox_marks`. |
| `0079` | `audit_log`: what happened, by whom, append-only. |
| `0080` | `config_revisions`: the row as it was before each configuration change. |
| `0081` | Review policy: `project_review_policy`, `review_decisions`, `runs.review_round`. |
| `0082` | Hand-off: the request on the card and `runs.handed_to_run_id`. |
| `0083` | `wakeups`, and which news wakes a manager early (`routines.on_events`, cooldown, daily cap). |
| `0084` | The reaper: `runs.last_event_at`, `reaped`, `auto_resumed_at`. |
| `0085` | The org chart (`agents.reports_to`, `title`) and the per-agent heartbeat interval. |
| `0086` | Goals: `goals`, `project_goals`, `goal_id` on tasks and routines, the inherit trigger. |
| `0087` | `heartbeats`: what an agent did on each beat. |
| `0088` | Rename: `mcp__eren__` tool names rewritten in every agent's allow-list. |
| `0089` | Accounts: `users` (one admin), `sessions`, `workspaces.owner_id`, the `user` audit actor. |
| `0090` | `attachments.workspace_id`: a general chat's uploads belong to its workspace (exactly one of project or workspace). |
| `0091` | `chats.agent_id`: a conversation with one of the workspace's agents — its persona, memories, engine, tier and effort; the run carries the agent, so its gate, limits and budgets apply. |
| `0092` | Personal rules (`users.rules`, written into each new repository project as AGENTS.md + CLAUDE.md) and personal skills (`skills.workspace_id` NULL, `owner_id`; `skill_in_workspace`). |
| `0093` | `app_schema_plans.status` may be `applying`: a plan is claimed before its DDL runs, so two approvals cannot both run it. |
