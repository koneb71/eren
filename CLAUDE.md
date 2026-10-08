# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Eren orchestrates official coding-agent CLIs — Claude Code, OpenCode, Codex, Gemini CLI, Cursor CLI, Qwen Code and Amp, plus Ollama and LM Studio models driven through OpenCode — as child processes on the user's own machine under their own subscription login. It is **process orchestration, not API access**.

## Compliance invariants (contribution rules — non-negotiable)

Stated at the top of [crates/eren-engines/src/lib.rs](crates/eren-engines/src/lib.rs) and enforced across the codebase. Code violating these is rejected:

1. Adapters spawn official binaries found on `PATH` and read their stdout. Nothing else — no HTTP control API, no proxying of engine traffic.
2. Never read, store, extract, or forward credentials. Never touch `~/.claude` or any engine's config/credential files. `eren doctor` decides "is this CLI logged in?" by *running* it.
3. Never set authentication environment variables on a spawned process. The single source of truth for "is this an auth secret" is [crates/eren-shared/src/env_guard.rs](crates/eren-shared/src/env_guard.rs) — use `is_auth_env` / `auth_env_refusal`, never a hand-rolled prefix list. Eren's own secrets (`env_guard::OWN_SECRETS`, under every name `own_secrets()` spells out) are stripped from every child (a spawned CLI inherits the server's environment) — which is why every process starts through `env_guard::command`, never `Command::new`. `clippy.toml` and a source-scanning test in `env_guard.rs` both refuse the latter; CI does not block on clippy, so the test is the one that fails the build. Two children get less than that: a project's checks and Eren's own `git` start through `env_guard::command_without_auth`, which also drops every inherited variable `is_auth_env` matches, because the code they run is the agent's — and Eren's git runs with repository hooks disabled (`core.hooksPath` pointed at nothing).
4. Never proxy, intercept, or replay engine network traffic.

## Git conventions

Commits and PRs in this repository carry **no AI attribution of any kind**. This overrides any default commit-message behavior:

- No `Co-Authored-By: Claude ...` trailer.
- No "Generated with Claude Code" / "🤖 Generated with ..." footer, and no link back to claude.com or claude.ai.
- No "written by an AI agent", "AI-assisted", or similar note in the commit body, PR description, or code comments.

Write the message as the human author would: what changed and why, nothing about what produced it. The same applies to PR bodies opened via `gh` — fill in [.github/pull_request_template.md](.github/pull_request_template.md) and add nothing to it.

(Product-level mentions of Claude Code are a different thing and stay — it is one of the engines Eren drives, so `ClaudeEngine`, model ids, README text, and UI labels are normal code, not attribution.)

## Names and the rename

Eren was called aichip, and that name is written into state already on people's machines and in their repositories: the home folder (whose absolute paths are stored in the database and in git's worktree links), `AICHIP_*` variables in shell profiles and hook scripts, `aichip/…` card branches, `.aichip/` folders and `aichip.app.yaml` manifests committed to repositories, the managed database and default bucket, the write and app headers, the app bridge path, saved browser settings. A rename that only rewrote the source would strand all of it, so for Eren every old spelling lives in exactly three files:

- [crates/eren-shared/src/brand.rs](crates/eren-shared/src/brand.rs) — every name, current and legacy;
- [crates/eren-core/src/legacy.rs](crates/eren-core/src/legacy.rs) — the boot-time adoption that needs a database (renaming the managed Postgres, renaming the manifests of Eren's own apps);
- [web/src/lib/brand.ts](web/src/lib/brand.ts) — the dashboard's half (localStorage keys, old tool names in transcripts).

The rule for each kind of state is **write the new name, read both, prefer the new**. Code asks `brand` for a name and never spells one itself: `brand::var("BIND")` / `brand::var_os` (reads `EREN_BIND`, then the legacy prefix) rather than `std::env::var("EREN_…")`, `brand::env_name` in a message telling a person what to set, `brand::home()` rather than `join(".eren")`, `brand::is_card_branch` / `BRANCH_PREFIX`, `brand::repo_dir`, `brand::app_manifest`, `brand::mcp_tool` / `MCP_TOOL_PREFIX`, `WRITE_HEADERS` / `APP_HEADERS`, `BRIDGE_PREFIXES`, and `brand::with_legacy_env` for variables handed to a spawned process. A name hand-rolled anywhere else reads only the new spelling and silently strands someone's existing setup. At boot, before the managed Postgres starts, `serve` calls `brand::adopt_legacy_home()` and logs every variable still set under the old prefix: the old home folder is moved to `~/.eren` once, with a link left behind so every stored path keeps resolving.

`no_old_name_outside_brand` (in `brand.rs`) reads every tracked file and fails the build where the old name appears outside those three files and the few it allows for naming existing data (brand's tests, migrations, `docker-compose.yml`, `.env.example`, `Dockerfile`, `web/index.html`). In markdown a line may name the old name only if the same line also says Eren/eren — an upgrade note passes, a stale `cargo run -p` command does not. When the compatibility window closes, deleting the `LEGACY*` items in brand and following the compile errors removes it.

## Docs move with the code

A change that adds or alters behaviour updates the docs it affects **in the same PR**: `README.md`, `docs/architecture.md`, `CLAUDE.md`, `SECURITY.md`, `.env.example`. A document that lags the code is worse than none, because it is believed. [crates/eren-cli/src/docs_tests.rs](crates/eren-cli/src/docs_tests.rs) checks the parts that can be checked — presence, not correctness, which is review's job:

- every engine id `real_engines()` registers appears in README.md and CLAUDE.md as `` `id` ``;
- every field of `Capabilities` is named in CLAUDE.md in backticks;
- every `EREN_*` the code reads through `brand::var("…")` / `brand::var_os("…")` is in `.env.example` and README.md (it finds them by that literal call, which is one more reason never to read the environment any other way);
- every migration number is in `docs/architecture.md`.

## Commands

```bash
cargo build                          # build the Rust workspace
cargo test                           # all tests (mock engine — no model usage, no rate limits)
cargo test -p eren-core            # one crate
cargo test -p eren-core backoff_escalates_and_caps   # one test by name
cargo test -p eren-cli docs_tests  # the docs checks above
cargo test -p eren-shared no_old_name_outside_brand  # the rename scan
cargo run -p eren-cli -- doctor    # check git + every agent CLI it can find
cargo run -p eren-cli -- serve     # dashboard on http://127.0.0.1:4820 (--port, --headless)

cd web && pnpm install && pnpm dev   # dashboard dev server; proxies /api and /ws to :4820
cd web && pnpm test                  # vitest (canvas ↔ YAML round-trip, diff, design scan, contrast, …)
cd web && pnpm test src/lib/workflowGraph.test.ts   # single file
cd web && pnpm exec tsc -b           # types, as CI runs them
cd web && pnpm build                 # tsc -b && vite build → web/dist (what the server serves)

./scripts/docker-publish.sh          # build the image with buildx and push it to neiellcare71/eren (--platform, --tag, --no-push)
./scripts/docker-deploy.sh           # back up, check, pull and run it with Postgres via compose (EREN_IMAGE, --tag, --down, --force)
./scripts/docker-backup.sh           # pg_dump + the state and ~/.claude volumes into backups/ (run before every deploy)
./scripts/docker-restore.sh <dir> --yes   # put a backup back, after backing up the current state
```

CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) runs `cargo fmt --all -- --check`, clippy (advisory, not blocking yet), `cargo test --workspace` against a Postgres service, and `pnpm exec tsc -b`, `pnpm test`, `pnpm build`.

Postgres: `eren serve` boots and manages its own under `~/.eren/pgdata`. To use your own for Eren, `docker compose up -d` and export `DATABASE_URL=postgres://aichip:aichip@localhost:5433/aichip` — compose's legacy names, kept for Eren because they name an existing volume (see the comment at the top of `docker-compose.yml`). See `.env.example` for the other knobs (`EREN_MAX_CONCURRENT`, `EREN_WEB_DIST`, `EREN_BIND`, `EREN_S3_*`, `EREN_<ENGINE>_BIN`, …).

**Migration gotcha:** sqlx embeds `crates/eren-core/migrations/` at compile time and adding a file does not always retrigger a rebuild. If a new column comes back as `ColumnNotFound`, `touch crates/eren-core/src/db.rs` and rebuild.

## Architecture

Five crates plus a React dashboard:

- **eren-shared** — no dependencies on the others. Event types (`ErenEvent`, `EventEnvelope`), `ModelTier`/`EngineTierMapping` (per-engine tier defaults and `is_known_model_for`), `PermissionMode`/`RunStatus`, workflow YAML types + `interpolate`, `env_guard`, `brand`, rate-limit parsing, effort.
- **eren-engines** — the `Engine` trait, `RunSpec`, `Capabilities`, `vet`, the shared `pump`, and one adapter per engine (below).
- **eren-core** — Postgres (`db`), the run orchestrator + state machine, worktree manager, queue backoff, cron scheduler, `EventBus`, `PermissionBroker`, org/team delegation, knowledge base, previews, S3 storage, and the subsystems described below.
- **eren-server** — axum: `/api` REST routes, `/ws` event fan-out, `/mcp` (a hand-rolled MCP-over-HTTP endpoint the engines call back into), preview reverse proxy, the audit layer. Handlers take `AppState { db, bus, orchestrator, permissions, storage, … }`.
- **eren-cli** — the `eren` binary: `serve` and `doctor`. Registers engines with the orchestrator at boot from `real_engines()` — one list for both commands, so a new adapter is never missing from the doctor.
- **web/** — React 18 + Vite + Tailwind 4, React Router, `@xyflow/react` for the workflow canvas, TipTap for the KB editor, `cmdk` for the command palette. `web/src/lib/api.ts` is the single API client; `web/src/lib/ws.ts` the socket.

### Engines

Adapters live in `crates/eren-engines/src/<engine>/`; each spawns its CLI, parses its stream, and normalizes into `ErenEvent`. The ids, in `real_engines()` order:

- `claude-code` — Claude Code (`claude -p --output-format stream-json`). The only real engine that can ask a person mid-run.
- `opencode` — OpenCode (`opencode run --format json`).
- `codex` — Codex (`codex exec`).
- `gemini` — Gemini CLI (`gemini`), its own stream schema.
- `cursor` — Cursor CLI (`cursor-agent`). No edit-only mode and no MCP.
- `qwen` — Qwen Code (`qwen`), a Claude-compatible stream.
- `amp` — Amp (`amp`), a Claude-compatible stream; tiers map to its modes, it has no read-only mode (a pass that must not write is refused when it starts), no edit-only mode and no MCP.
- `ollama`, `lmstudio` — `LocalEngine`, a different shape, and the reason is written at the top of [crates/eren-engines/src/local/mod.rs](crates/eren-engines/src/local/mod.rs): a local runtime serves a model but holds no tools, so it **delegates to the OpenCode binary** with the provider declared and the model resolved from what `ollama list` / `lms ls --json` actually report. Invariant 1 still holds. `eren_core::local_models` is the older HTTP discovery used by the settings page; an adapter must not use it.
- `mock` — replays fixtures for the tests (see Testing); `serve` also registers it as a demo engine.

The four newer adapters share [crates/eren-engines/src/pump.rs](crates/eren-engines/src/pump.rs): `pump::spawn` takes a command built by `env_guard::command` and a `pump::LineParser` (`line()` → zero or more events, never failing on an unknown line; `finish()` → the terminal event from the exit status when the stream gave none), reads stdout through it, and keeps a stderr tail for a reason. **stderr never ends a run on its own** — Gemini announces a quota fallback there and then finishes the work. Qwen and Amp parse with `claude::compat::ClaudeCompat`; Gemini and Cursor have their own `stream_parser.rs`. Claude Code, OpenCode and Codex keep their own pumps. A binary can be pointed elsewhere with `EREN_<ENGINE>_BIN` (Codex, Gemini, Cursor, Qwen, Amp).

### Capabilities, not `if engine == "..."`

Engine differences are declared in `Capabilities`; there is deliberately no `Default` impl — a new adapter must answer for itself. Gate behavior on the capability, never on the engine id. The fields:

- `interactive_permissions` — can park mid-run on `mcp__eren__approve` and ask a person about one tool call. `false` ⇒ `Reviewed` cannot be honoured, so `vet` refuses it.
- `structured_rate_limit` — emits a rate-limit event carrying a reset time, so the queue waits exactly that long. `false` ⇒ the shared stderr matcher still catches a limit, and the queue falls back to its escalating backoff.
- `resume_sessions` — can resume a session by id. `false` ⇒ `vet` refuses a resume rather than silently starting over without the earlier context, and a follow-up runs in the same worktree on a fresh session.
- `append_system_prompt` — can add to the system prompt without replacing the CLI's own. `false` ⇒ the adapter folds the persona in front of the prompt (`prompt_with_persona`), or every persona and recalled memory would vanish.
- `fixed_model_catalog` — model ids come from a fixed list (the install's own when it reports one, else Claude Code's). `false` ⇒ the picker is free text.
- `reports_cost` — says what a run cost in dollars when it ends. `false` ⇒ `cost_usd` stays NULL (not $0) and only token caps can see its runs.
- `enforces_denied_tools` — the CLI itself refuses a denied tool, so a read-only pass is read-only by enforcement. `false` ⇒ it cannot be an agent reviewer (`peer_review`).
- `mcp_tools` — can be handed Eren's MCP server for one run without writing a file into the run's folder. `false` ⇒ the assistant, a project manager and a team are refused at the click (`Orchestrator::needs_tools`, a `409` naming the installed engines that can), and a card run simply goes without its toolbox.
- `auto_edit` — can edit files without also being handed a shell. `false` ⇒ `vet` refuses Auto-edit rather than widening it to Full Auto.
- `read_only_passes` — can take a pass that must not write (a plan, a summary, a drafting call) in a mode where nothing can. `false` ⇒ a plan-first card (`vet_card`) and an AI drafting call (`utility_run`, `CantHonour`) are refused at the click with a `409`, rather than by the adapter once it starts. Not `enforces_denied_tools`: Cursor's ask mode cannot write, yet does not refuse one named tool.

Two rules follow, both in `eren_engines::vet` and the orchestrator:

- **Refuse, never widen — at the click.** OpenCode's `interactive_permissions: false` is why starting a Reviewed card on it is refused with a `409` **at the click** (`vet_card` → `run_refused`), rather than silently downgraded to Auto-edit — a silent downgrade would be privilege escalation. The same goes for Auto-edit on an engine without `auto_edit`, and for resuming on one without `resume_sessions`. Dispatch vets again, because a card's mode can change after it queued.
- **Full Auto steps down, never up.** Full Auto needs the project's opt-in *and* an Eren-managed worktree. Where either is missing, the run steps down to `short_of_full_auto`: `Reviewed` if the engine can ask, else `AutoEdit` if it has it, else nothing — so Amp and Cursor, which allow every tool or none, are **refused** (at the click by `vet_card`, and again at dispatch) rather than handed a narrower mode they would ignore. Stepping down is de-escalation and safe; there is no path that steps up.

### Event flow

The orchestrator persists **every** event envelope to the `events` table *before* publishing it to the in-process `EventBus` — the DB is the source of truth, so a reconnecting WS client replays from it. Permission events are the exception: `seq: -1`, ephemeral, never part of the replay log.

### Agent status

An agent is `active`, `paused`, `retired` or `pending_approval` (`eren_core::agents`). Every function that inserts a run asks `agents::assert_can_run` (or its team / workflow-step form) **before** the insert; a source-scanning test fails the build for one that does not, unless it is on the short list of runs with no agent. A team run and a workflow ask again at each assignment, so a pause stops the agent's *next* piece of work wherever it was coming from. A paused agent can still be assigned cards; a retired one cannot, and is hidden from every picker. Deleting an agent that anything references retires it instead.

### Org chart

`eren_core::org_chart`: `agents.reports_to` makes a workspace's agents a tree, kept one by `org_chart::set_manager` — the only writer — under a per-workspace advisory lock: same workspace, no cycle, depth ≤ `MAX_DEPTH`, no retired manager. Retiring (or deleting) a manager lifts its reports to its own manager in the same transaction. Delegation flows down (a manager pass is told its reports, and `create_task` from a pass whose agent has reports may bind only agents in its subtree — `may_delegate`); trouble flows up (`wake::ESCALATES` kinds also wake the manager routine of the card's agent's manager). A workspace that never draws a chart works exactly as before.

### Goals

`eren_core::goals`: a per-workspace tree (one writer of `parent_id`, an advisory lock, no cycle, depth ≤ `MAX_DEPTH`). Cards and manager routines point at the goal they serve; a card split from an epic inherits the epic's goal by a `BEFORE INSERT` trigger (epics are split in more than one place), and a card a manager pass files takes the routine's goal unless it names one. Every card run gets a fenced, capped "Why this matters" — the goal chain top-down plus the epic — appended after the brief, Brain and skill; a manager pass gets the active goals with progress. Progress is counted from the cards in a goal's subtree on every read, never stored. Deleting a goal moves its children and cards up to its parent.

### Heartbeats

`eren_core::heartbeat`: an agent with `heartbeat_secs` (a person's standing decision) beats on a timer — claimed with a compare-and-set on `last_heartbeat_at` — and early through `wakeups` with an `agent_id` target (a card assigned to it, one of its cards unblocked). A beat does at most one thing: nothing if paused or already working (one card at a time unless `max_concurrent` says more); fire the manager routine it runs (same cooldown and daily cap as wakes); or start its next unblocked backlog card **through `start_card`**, so every gate the Start button meets applies. Idle beats call no model. Every beat is logged to `heartbeats`, 500 per agent.

### Budgets

A budget policy (`eren_core::budgets`) covers the machine, a workspace, a project, an agent, a team or a routine over a calendar day, week or month, and caps any of dollars, output tokens and runs. It bites in four places, cheapest first: every door that starts work refuses a spent scope with a 409 naming the policy (`budgets::check`, mapped by `run_refused`); `claim_next` holds a queued run whose scope is spent (`queue.hold_reason` / `held_by`) and claims the next instead; a team or workflow starts nothing more between steps; and on a `stop` policy a run crossing a token cap is interrupted mid-stream. That stop measures each run against the headroom left when it started (`budgets::token_headroom`), so runs in flight together can overrun the cap by their combined size; a run is claimed (and counted against a run cap) when it leaves the queue, which is why `budget_allows_more` and a run back from a rate limit do not count it twice. **Dollars cannot stop a run midway** — no engine prices a run until it ends — and an engine with `reports_cost == false` (Codex, Gemini, Cursor, Qwen, Amp) is only visible to token caps, so its runs stay unpriced (`cost_usd` NULL) rather than recorded as $0. Warnings and exceeded notices are written once per policy per window (`budget_incidents`), which is what keeps notifications from repeating. Agents also carry their own limits (`max_concurrent`, `max_daily_runs`, `cooldown_secs`), checked in the same claim step; with no policies and no limits, claiming is unchanged.

### Inbox

Everything waiting on a person is one list, `eren_core::inbox::list` — a query over the rows that already say they are waiting (parked plans, chat questions and plans, pending app schema plans, KB revisions, proposed preview recipes), never an index of its own, which would drift. Only what had no row got one (migration 0078): `permission_requests` (written by the broker through `RunGate::record`/`close`; `recover_orphans` marks open ones `expired`, and the inbox offers to resume those runs), `run_questions` (a card agent's `ask_person`, `eren_core::asks`; the answer returns as `FollowUp::Answer` in the same worktree and session, fenced as `fence::ANSWER_*`), and `decisions` (an agent's `propose_decision`, a closed `decisions::Effect`). Every answer goes through the function the thing's own button calls — the transitions live in `eren_core::approvals` (status check in the `WHERE` of the write, so a double click lands once), and `routes/inbox.rs` dispatches to them, the broker, or the route handler itself. **No agent toolbox may resolve an inbox item**: an agent proposes, a person decides; `run_tools`' test forbids `resolve`, `approve` and `decide` in tool names.

### Audit log

`audit_log` (migration 0079) is append-only and has exactly one writer, `eren_core::audit::record` — a source scan fails the build for any other `INSERT`/`UPDATE`/`DELETE` on it, and the only delete is its retention `prune` (agent and system rows after `RETENTION_DAYS`, 90; API rows kept). A failed audit write is logged and dropped, never failing the action it records. Three things feed it: the server's `audit_layer` on every mutating `/api` request (route template, path ids and status — **never the body**; the few read-only POSTs are listed in `audit_layer::QUIET`, each with its reason), the three MCP dispatches (tool name and outcome — **never the input**), and Eren's own actions (routine fires, sweeps, reaps, handoffs). With accounts on it records the signed-in `user`; with accounts off, `api`, not "a person": there is no login then, and any local process can call the API. A card's merged story is `GET /tasks/{id}/timeline`.

### Config revisions

`eren_core::revisions`: an edit to an agent, team, routine (manager included), skill, project checks, budget policy, review policy, or the attention or unattended-runs setting keeps the row it replaced in `config_revisions` (migration 0080, last `KEEP` = 20 per entity) — `revisions::keep`, called by the entity's own update and delete handlers; `revisions::tests::every_writer_keeps_a_revision` names each one and fails for a writer added without it. Rows are read generically (`to_jsonb`) from a table chosen by the closed `EntityKind` enum, never from request text. **A restore is an edit**: `routes/revisions.rs` maps the snapshot onto that entity's own update body and calls its own handler, so validation, the write header, the single writer of `project_checks` and the audit entry all apply, and the restore is itself a revision.

### Permissions

`RunSpec.allowed_tools` is an *auto-approval* list, not a restriction — Claude Code will still reach for `Bash` even if only `Read` was "allowed". Anything that must not happen goes in `denied_tools`, which adapters apply last. This is why chat runs (which execute in the user's **real checkout**, not a worktree) carry both `CHAT_ALLOWED_TOOLS` and `CHAT_DENIED_TOOLS` in [crates/eren-core/src/runs/orchestrator.rs](crates/eren-core/src/runs/orchestrator.rs) — never add Bash/Edit/Write there. Plan-first passes deny the mutating tools for the same reason. An engine without a per-tool vocabulary translates the denial into whatever mode it has where nothing can write (Codex: a read-only sandbox); one with no such mode (Amp, `read_only_passes: false`) refuses the pass — at the click where the pass is a person's request.

Mid-run permission prompts flow: engine → `--permission-prompt-tool mcp__eren__approve` → `crates/eren-server/src/mcp/` → `PermissionBroker` parks the call, records it in `permission_requests` and emits an event → dashboard (or inbox) Allow/Deny resolves the oneshot. Unanswered, it is denied after the attention setting's window — 24 hours by default, `0` waits indefinitely, capped at seven days (`attention::Attention::window`).

A card's run also gets Eren's own toolbox on `/mcp/run/{run_id}` ([crates/eren-server/src/mcp/run_tools.rs](crates/eren-server/src/mcp/run_tools.rs)): `comment`, `report_blocker`, `ask_person`, `propose_decision`, `search_kb`, `read_article`, `recall`, and — only on a review pass — `submit_review`. What a run is offered is read from its row (a planning, summary or review pass only reads), and every MCP endpoint refuses calls once its run has ended. These tools pass `approve` without asking a person, so **nothing added there may merge, start a run, resolve an inbox item, or write settings or check commands** — the test refuses tool names containing `merge`, `start`, `setting`, `check`, `run`, `resolve`, `approve` or `decide`.

### Network access

Until accounts are on (below), on loopback (the default) there is no login, and the only caller is this machine. The access token (`eren_server::access`, the outermost layer) exists on every bind, not only a wide one — a container reaches even a loopback-bound port through Docker's gateway, and its peer is not loopback: a caller whose **TCP peer** is loopback passes, every other caller presents the token — an access link (`/?access=…`) trades it for an `HttpOnly` cookie, scripts send a bearer header. `/mcp` answers loopback peers only (`mcp::this_machine_only`), whatever the token or the account state: its callers are identified by a run id alone and are always on this machine. `EREN_ALLOWED_HOSTS` adds the names the Host and Origin checks in `reject_non_local_callers` accept (names only, exact authority). Two rules: **"local" is decided by the peer address, never a header** (a header is whatever the caller says); and the token is one of `env_guard::OWN_SECRETS`, so no child sees it. `EREN_ACCESS_TOKEN=off` restores no-token behaviour, which a wide bind must then acknowledge with `EREN_TRUST_NETWORK` (`eren_server::exposure`).

### Accounts

Off until `eren admin create` makes the one admin (migration 0089, `eren_core::users`), who adopts every ownerless workspace. Then `eren_server::auth::require_session` asks every caller — loopback included — for a session cookie (`eren_core::sessions`), the token steps aside, and only the sign-in page's requests and `/mcp` from a loopback peer pass without one. A preview's or app's hostname (bridge included) answers a loopback peer or a signed-in owner — `preview_proxy` asks for itself, since `auth` cannot tell those paths from the dashboard's files. Sign-up is closed until the admin opens it (`users::signup_open`, absent means closed), and anyone who can reach the dashboard may sign up while it is; each account gets a workspace and sees only the workspaces it owns. Two rules that are easy to break:

- **Every route handler takes `Caller` or `Admin`** (`routes::tests::every_handler_takes_a_caller` fails the build otherwise) and **checks every id it is handed** — path, query *and body* — with `caller.require(&state, Owned::…)` before using it; a list uses `caller.workspace_filter`, never `$1 IS NULL OR workspace_id = $1`, which hands a signed-in user every workspace when the parameter is left off. `eren_core::scope::Owned` is the closed set of kinds and their fixed queries; add a variant there rather than a hand-rolled ownership query. Someone else's id is a 404.
- **What belongs to the machine is `Admin`'s**: settings writes, the queue, machine-scope budgets, the audit log. With accounts off, `Caller::Local` passes every check, so nothing changes for a single-person install.

Accounts separate cooperating people, not a hostile one: every account's agents share an OS user and a filesystem (SECURITY.md says so).

### Apps

An app is a **project** under `~/.eren/apps/<slug>` (`projects.kind='app'`, the folder from `brand::home()`; `EREN_APPS_DIR` overrides it),
which is what gives it worktrees, diffs and the files editor for free. The three
places that *list* projects filter on `kind='repo'`; the spend and activity joins
deliberately do not, because generating an app costs real money.

Its manifest (`eren.app.yaml`, found through `brand::app_manifest`;
[crates/eren-core/src/apps/manifest.rs](crates/eren-core/src/apps/manifest.rs))
is parsed by hand, not derived: declaration order is display order, errors name
the offending key, and unknown keys are refused rather than ignored. Field types
and identifier charset are closed sets — those identifiers are interpolated into
DDL, and the defence is the charset, not the quoting.

Two rules that are easy to break:

- **Nothing an app sends is ever an identifier.** `apps::query` looks field names
  up among the declared fields and emits the *manifest's* copy; operators come
  from an enum; values are always bound. Never build a fragment from request text.
- **Additive schema changes apply; destructive ones wait.** `apps::schema::plan`
  diffs declared models against `information_schema` — never against a registry,
  which would drift. A plan with any destructive statement runs *nothing* and is
  stored whole, so what a person approved is byte-for-byte what executes.

Changing an app is an ordinary card on the app's own project — worktree, diff and
all — with two differences, both in
[crates/eren-core/src/apps/build.rs](crates/eren-core/src/apps/build.rs):

- **It lands without review, and that is why the undo has to work.** `settle()`
  squash-merges when the run completes, so `app_builds.base_commit` is read
  *before* the card exists; afterwards there is no way to ask git where the
  branch stood. Only the newest landed build is revertible (`revertible()`) —
  resetting to an older one would discard every build after it in silence.
  Landing does *not* bypass the schema gate: the manifest is re-read from disk
  and goes through `set_manifest`, so a dropped column still waits.
- **Every write Eren makes to an app's folder is committed** (`apps::commit`).
  A file written but not committed is not on `main`, so `git worktree add` hands
  the next agent an empty folder — which is exactly what happened before it
  existed, and cost a paid run.

The expression language exists twice, in `apps/expr.rs` and `web/src/lib/expr.ts`,
because `show_if` cannot afford a round trip and computed values cannot be
decided by a browser. `crates/eren-core/src/apps/expr_cases.json` is the
specification and both test suites read it — add a case there, not to one side.

### Worktrees

Board tasks run in an isolated git worktree (on an `eren/…` branch, `brand::BRANCH_PREFIX`) so an agent never touches the working copy, and that worktree produces the reviewable diff. A project that can't have its own repo (e.g. nested inside another) edits in place — no worktree, no diff, no undo, and full-auto is refused there regardless of project settings.

The Files tab writes to both trees — the checkout and a card's worktree — when
a **person** saves. That does not weaken the rule above, which is about agents:
they still only ever work in a worktree, which is what keeps a run reviewable.
The write path carries its own gates (no `.git`, a root allow-list, a content
hash, and a header no cross-origin request can set); they are documented at the
top of [crates/eren-server/src/routes/files.rs](crates/eren-server/src/routes/files.rs).
The root allow-list is `fs::may_open` — under `EREN_BROWSE_ROOT`, or Eren's own apps and
spaces folders — and it is the one answer for loading a folder (`POST /api/projects`), reading
or writing its files, and opening a terminal in it, so the three cannot disagree.

### Checks and follow-ups

A **follow-up** (`runs/follow_up.rs`) is a run that goes back into a card's existing worktree to act on something said about its diff — a review note, failing checks, a merge conflict, an answer, a peer review, a handoff — so the fix lands in the same diff. It records its note in `runs.review_comment_id`, never `comment_id`, which every reader takes to mean "a comment reply".

**Checks** (`eren_core::checks`) are a project's own test/lint commands, run in a card's worktree after an agent finishes. Two rules that are easy to break:

- **Only `routes/checks.rs` writes `project_checks`.** It holds shell commands this machine runs; a test fails if any other file writes it. Never put a check command on a row an agent, an importer or an app build can write.
- **Checks start unasked only after a Full Auto run — or where the project's review policy sets `run_checks_after_every_run`.** They execute code the agent may have edited, so otherwise a person clicks "Run checks" — that click is the consent, and the policy switch is the standing form of it. The same goes for the bounded auto-fix.

### Review policy and the merge gate

`eren_core::review` (migration 0081), written only by `routes/reviews.rs` — a test fails otherwise — decides what a person's Merge requires: an agent review's approval, passing checks, a green pull request, each of the *latest* work (`review::last_work`) — an older green or approval does not count. Merge stays a person's click; an unmet gate answers `409 {kind:"gate", unmet}`, and `{force, note}` merges anyway with the note on the card and in the audit log. The reviewer is a `peer_review` follow-up: read-only like a planning pass, run as the reviewer agent on its engine, refused for the card's own author or an engine without `enforces_denied_tools`. Its verdict comes back only through `submit_review`; a review that ends without one is recorded as changes requested (fail closed). `settle_review` is idempotent — called after every completed run and after checks settle, it starts a review only when nothing of the card is live and no verdict covers the latest work — and the loop is bounded: one `ReviewNote` fix per round, then at `max_rounds` the card waits in the inbox. A person's "Review again" gets one round past the cap.

### Handoff

`eren_core::handoff` (migration 0082): reassigning a running card is refused unless it carries a note — then the request is recorded on the card first (so a restart in between still completes it), the running agent is stopped, and `settle` hands over only once no run of the card is live *and* the old run's `execute` has returned (`Orchestrator::is_executing`, an in-memory registry held by a drop guard): its post-work touches the same worktree, so a terminal status alone is not enough. The new agent continues in the same worktree through `FollowUp::Handoff` (no old session), or starts fresh with the note beside the brief when there is no worktree yet. The swap is a single claiming `UPDATE`, so the immediate attempt and the scheduler sweep cannot both start it.

### Wakes

`eren_core::wake` (migration 0083): news a project's manager would want before its next scheduled pass — a card landed, unblocked or failed, checks or review rounds exhausted, a question asked, a run stalled. `wake::raise` writes a row only for a manager that ticked that kind (`routines.on_events`, empty by default — an early pass costs a run), coalesced one open row per (target, kind, card). `drain_wakeups` fires through `routines::fire("wake")` only when the manager's thread is idle (checked before firing, so news is kept, not dropped), its cooldown has passed and it has early passes left today. Every manage pass, early or scheduled, opens with a fenced "Since your last pass" and consumes it only once its turn queued. A raise never fails the thing that raised it, and never starts anything except through `routines::fire`, whose gates all apply.

### Reaper

`eren_core::reaper` (migration 0084, each scheduler tick): a run whose row says starting/running/waiting but that this process is not executing (`is_executing` false for over `LOST_AFTER`) is failed as lost — always on, since nothing else could ever finish it. Stopping a run for silence (no event for N minutes, never while it waits on a person) and resuming stopped runs are both opt-in ("Unattended runs", a revisioned setting). Auto-resume goes through `runs::resume::resume_dead_run` — the Resume button's own door, which the route calls too — once per stopped run (`auto_resumed_at`) and at most twice along a `resumed_from` chain. Every stall is said on the card, raised as a `stalled` wake and sent through attention.

### Merge conflicts and landing

**Merge conflicts** are met on the card's branch, never the person's checkout: "Update from main" runs `update_from_base`, which merges the base into the card's `eren/…` branch inside its worktree and, on conflict, leaves the merge in progress for a `conflict` follow-up. `commit_worktree` refuses while conflict markers remain (and concludes the merge unconditionally once they're gone), and `squash_merge` refuses any diff that adds them — so markers can never land. A card's diff is measured from `merge-base`, not the base's tip.

**Landing** (`eren_core::landing`): a card blocked by another waits for it to reach *done*. Six things write `done` and share no code path, so the seam is `tasks.landed_at`, set once by whichever notices first. A writer of done calls `orchestrator.landed(task_id)` (a no-op if the card is not done or already landed); `settle_landings` sweeps every scheduler tick for the ones that do not. A dependent with `start_when_unblocked` starts through `start_card` — the Start button's vet and door — and every other dependent gets a note and an `unblocked` attention event.

Attachments live under `~/.eren/attachments/` and are granted via `--add-dir`, deliberately never copied into a worktree (an agent running `git add -A` would commit them).

### Web: the design system

Every colour comes from a token in `web/src/index.css` — the `@theme` block for light, redefined under `:root[data-theme="dark"]` for dark (`bg`, `panel`, `panel-2`, `raised`, `border`, `fg`, `fg-muted`, `fg-subtle`, `accent`, `success`/`warning`/`danger`/`info` with `-fg` and `-subtle` pairs, the tier accents, the tint pairs, the shadows). Never a raw colour: no Tailwind palette class (`bg-gray-50`), no hex literal, no `bg-white`/`text-black` (right in one theme only); add a token instead. Build screens from the kit in `web/src/components/ui/` (`Button`, `Badge`, `Dialog`/`Sheet`, `Field`/`Input`/`Select`, `Menu`/`Popover`/`Tooltip`, `Tabs`, `Toast`, `Surface`, `Layout`, `Icon`, `cn`) — overlays especially, since the kit is what gives one a focus trap, Escape and a label. Two tests hold this:

- [web/src/lib/design-scan.test.ts](web/src/lib/design-scan.test.ts) reads every non-test source file and refuses palette classes, hex outside `theme/editorThemes.ts` and `lib/swatches.ts`, white/black outside the kit, and `fixed inset-0` scrims outside the kit and `components/shell/`.
- [web/src/lib/contrast.test.ts](web/src/lib/contrast.test.ts) holds every text token to WCAG AA against every surface it sits on, in both themes — a palette tweak that makes a label unreadable fails here.

The theme is light, dark or system (`web/src/lib/theme.tsx`): one `data-theme` attribute on `<html>`, so switching recolours nothing in React; the choice is a per-viewer `localStorage` value read with every access guarded, and an inline script in `web/index.html` sets it before first paint. The command palette (`web/src/components/shell/CommandPalette.tsx`, ⌘K / Ctrl+K from `AppShell.tsx`) offers pages, server search and common actions. Its pages come from `NAV` in `web/src/lib/nav.ts` — one list read by the sidebar, the breadcrumb and the palette, so a new page is added there once and the three cannot disagree. Icons are `lucide-react`, or the kit's hand-drawn `Icon`, whose `IconName` is a closed union — add a glyph to both the union and the record, on the shared 24 grid, 1.75 stroke, `currentColor`. Pure logic goes in `web/src/lib/*.ts`, where vitest can reach it without a DOM.

## Testing

The mock engine ([crates/eren-engines/src/mock/](crates/eren-engines/src/mock/)) replays recorded stream-json `.ndjson`/`.jsonl` fixtures with configurable pacing and is the backbone of all testing — zero model usage. Each real adapter is tested against fixtures in its own `fixtures/` (the four newer ones are **synthetic**, built from each CLI's source or docs, and their `fixtures/README.md` says so and how to replace them with a recording) and end to end against a stand-in binary (`crate::stand_in` / `crate::replaying` in `eren-engines/src/lib.rs`: a shell script that records its argv and prints the fixture). Rust tests are inline `#[cfg(test)] mod tests` next to the code, not a `tests/` directory.

Tests of SQL live in an inline `mod db_tests` and start with `let Some(t) = testdb::fresh().await else { return };` ([crates/eren-core/src/testdb.rs](crates/eren-core/src/testdb.rs)): each gets its own migrated database on the server `DATABASE_URL` names, an orchestrator with the mock engine if it asks, and skips when `DATABASE_URL` is unset. CI runs them against a Postgres service. Against `docker compose up -d`, run Eren's tests with `DATABASE_URL=postgres://aichip:aichip@localhost:5433/aichip cargo test` (compose's legacy names).

Several invariants are held by source-scanning tests rather than review: `Command::new` (`env_guard`), the old name (`brand`), writers of `audit_log`, `project_checks`, review policy and config revisions, run inserts without an agent check, agent tool names, the docs (`docs_tests`) and the files outside `web/` the Docker build's dashboard stage must copy in (also `docs_tests`), and on the web side the design scan. When one fails, fix the code, not the scan.
