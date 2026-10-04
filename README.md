# Eren

**A local-first multi-agent workflow platform for coding agents — no API keys.**

> Eren was called **aichip** until October 2026. An existing install upgrades itself the
> first time you run `eren serve`; see [Upgrading from aichip](#upgrading-from-aichip-to-eren).

Eren is a dashboard for running the coding-agent CLIs you already have installed.
It spawns [Claude Code](https://code.claude.com), [OpenCode](https://opencode.ai),
[Codex](https://developers.openai.com/codex/cli), [Gemini CLI](https://github.com/google-gemini/gemini-cli),
[Cursor CLI](https://cursor.com/cli), [Qwen Code](https://github.com/QwenLM/qwen-code) and
[Amp](https://ampcode.com) as child processes on your own machine,
under your own subscription login, and gives them a board, a queue, git worktrees, a diff
to review, and a record of what everything cost. Models served locally by
[Ollama](https://ollama.com) or [LM Studio](https://lmstudio.ai) are offered the same way,
and cost nothing to run.

It is for people who already work with one of these CLIs in a terminal and want more than
one thing happening at a time — several cards in flight, each in its own worktree, with a
place to see what they did before any of it reaches your working copy — and, increasingly,
for leaving some of that work running while nobody is watching, with the rails that makes
safe.

## What makes it different

- **It runs your CLI, not an API.** No API key goes anywhere near it, because none is
  needed: the binary on your `PATH` is already logged in, and Eren just starts it.
- **Everything is local.** Postgres runs under `~/.eren`, the code index and document
  embeddings are computed on your machine, and nothing is sent anywhere Eren controls.
  The dashboard answers only this machine unless you open it to your network — and then
  every other device needs its [access link](#using-eren-from-other-devices).
- **The review surface is git.** A board task runs in an isolated worktree, so the thing
  you approve is an ordinary diff on an ordinary branch, and the thing you reject costs
  you a deleted branch rather than an undo.
- **Refusals are up front.** An engine that cannot pause to ask for permission is refused
  at the click, not silently downgraded; a schema change that drops a column waits for
  you; a plan-first run has the mutating tools *denied*, not merely left off a list; a
  spent budget answers the Start button with a sentence, not a card that sits in the queue.
- **An agent proposes; a person decides.** Merging, answering a question, approving a plan
  or a decision is always a person's click. No agent tool can resolve anything waiting on
  you.

## Contents

- [How it stays within the terms of service](#how-it-stays-within-the-terms-of-service)
- [Status](#status)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [A tour](#a-tour) — the board, review, the inbox, agents and organisation, unattended
  work, budgets, chat and research, knowledge, apps, previews, the dashboard
- [Engines](#engines)
- [Settings and environment variables](#settings-and-environment-variables) — including
  [using Eren from your phone or another computer](#using-eren-from-other-devices)
- [Upgrading from aichip to Eren](#upgrading-from-aichip-to-eren)
- [Database](#database)
- [Running in Docker](#running-in-docker)
- [Development](#development)
- [Contributing](#contributing) · [Licence](#licence)

## How it stays within the terms of service

Eren is **process orchestration, not API access**. The compliance model is structural:

1. Every user runs Eren locally and brings their **own installed CLI** and their **own
   subscription login**. Eren never provides, shares, proxies, or resells model access.
2. Eren **never reads, stores, extracts, or forwards credentials** — it does not touch
   `~/.claude` or any engine's config or credential files, does not set auth environment
   variables, and does not proxy network traffic.
3. Eren only spawns the **official binaries found on `PATH`** (e.g. `claude -p
   --output-format stream-json`, `opencode run --format json`, `codex exec --json`) and
   reads their stdout. No engine has an HTTP control API in the loop.
4. `eren doctor` verifies each CLI is installed and logged in **by running it**, never by
   inspecting its config files. Where a CLI can name its providers (`opencode providers
   list`), Eren shows the **name and auth type only** — never a credential.

These four invariants are stated at the top of
[`crates/eren-engines/src/lib.rs`](crates/eren-engines/src/lib.rs) and are contribution
rules. PRs that violate them will not be merged. Two pieces of code hold them in place:
every process Eren starts goes through `eren_shared::env_guard::command`, which strips
Eren's own object-storage keys from the child's environment (a `Command::new` anywhere else
fails a source-scanning test and `clippy.toml`), and `env_guard::is_auth_env` is the one
place that decides whether a variable name is a secret Eren must refuse to set.

## Status

Early development, version 0.1. Everything in the tour below is exercised end to end by
the test suite on the mock engine. The Claude Code, OpenCode and Codex adapters have been
run against their real CLIs; Gemini CLI, Cursor CLI, Qwen Code and Amp are newer and have
**not yet been run against the real binaries** — see
[their section](#gemini-cli-cursor-cli-qwen-code-and-amp).

Interfaces still move between commits. The database migrates itself forward, and state
from before the rename to Eren is carried across on first boot; beyond that there is no
compatibility promise yet.

## Requirements

- **macOS or Linux.** Windows is not supported; nothing has been tested there.
- **A Rust toolchain** (stable, 2021 edition) to build the workspace. On Debian or Ubuntu
  you also need `pkg-config` and `libssl-dev`.
- **Node 22 and pnpm 10** to build the dashboard. pnpm's exact version is pinned in
  `web/package.json` (`packageManager`), so corepack, CI and the Docker build all run the same
  one. The server serves
  `web/dist`, so a source checkout needs `pnpm build` once before `serve` has a UI to hand
  out.
- **git** on `PATH`. It is not optional: worktrees are how a task stays reviewable.
- **At least one agent CLI on `PATH`** — `claude`, `opencode`, `codex`, `gemini`,
  `cursor-agent`, `qwen` or `amp` — already logged in. `eren doctor` tells you which ones it
  found, and where to get the ones it didn't. Ollama and LM Studio need OpenCode as well.

**`./scripts/setup.sh` installs all of the above** on macOS (with Homebrew) or Linux (apt,
dnf, pacman or zypper): git, Rust, the build tools, Node 22, pnpm 10 and — if no agent CLI
is on `PATH` yet — Claude Code. Anything already present at a good enough version is left
alone, so it is safe to run again. `--dry-run` prints what it would do, `--yes` stops it
asking, and `--with-optional` adds `gh`. It never logs a CLI in; run it once yourself.

Optional:

- **`gh`**, for cloning from GitHub, importing issues as cards, publishing a folder as a
  repository, and finishing a card as a pull request. Everything else works without it, and
  `doctor` reports a missing `gh` as a note, not a failure.
- **Docker**, for branch previews and container apps, for running Postgres yourself, or for
  the object storage below. Nothing else needs it.
- **Any S3-compatible store** (compose ships RustFS), only for files pasted into
  knowledge-base pages.
  Without one the wiki still works; uploads are refused with a message saying why.
- **Node's `npx`**, only to install Agent Skills from a registry.

The first document you index and the first project you map download a small embedding
model into `~/.eren/models`. That is an artifact download, the same class as a cargo
dependency; no content leaves the machine in either direction.

## Quick start

```bash
./scripts/setup.sh                # installs the requirements, skipping what you have
cargo run -p eren-cli -- doctor   # checks git, gh, and every agent CLI it can find
cd web && pnpm install && pnpm build && cd ..
cargo run -p eren-cli -- serve    # the dashboard on http://127.0.0.1:4820
```

The first `serve` downloads and initializes a private Postgres under `~/.eren/pgdata`,
so there is nothing to install or configure. `serve` is also what runs when `eren` is given
no subcommand. It takes two flags:

- `--port <n>` — listen somewhere other than 4820.
- `--headless` — don't try to open a browser.

To use it from a phone, tablet or another computer on your network as well, start it with
`EREN_BIND=0.0.0.0 EREN_ALLOWED_HOSTS=<this machine's address> eren serve` and open the
access link it prints on each device once — see
[using Eren from other devices](#using-eren-from-other-devices).

`doctor` ends with `All good. Start with: eren serve` when git and at least one engine are
present, and exits non-zero otherwise. For each engine it finds it also says what that
engine cannot do (ask permission mid-run, signal a rate limit, carry Eren's tools), so the
refusals you meet later are not a surprise.

For a binary you can put on your `PATH`, `cargo build --release -p eren-cli` produces
`target/release/eren`. Run from anywhere other than the repository root, point it at the
dashboard build with `EREN_WEB_DIST` (see [settings](#settings-and-environment-variables)).

Working on the dashboard itself:

```bash
cargo run -p eren-cli -- serve --headless   # the API on :4820
cd web && pnpm dev                           # Vite on :5173, proxying /api and /ws to :4820
```

## A tour

Each part below names where it lives, so you can read the code behind a claim.

- [The board](#the-board) · [Adding a folder](#adding-a-folder) ·
  [Attachments and file references](#attachments-and-file-references)
- [Review: diffs, checks and follow-ups](#review-diffs-checks-and-follow-ups) ·
  [Review policy and the agent reviewer](#review-policy-and-the-agent-reviewer) ·
  [Merge conflicts](#merge-conflicts) · [Dependencies and landing](#dependencies-and-landing) ·
  [Handing a running card over](#handing-a-running-card-over)
- [The inbox](#the-inbox) · [The audit log and a card's timeline](#the-audit-log-and-a-cards-timeline) ·
  [Config revisions](#config-revisions)
- [Agents](#agents) · [The org chart](#the-org-chart) · [Goals](#goals) ·
  [Heartbeats](#heartbeats)
- [Routines](#routines) · [A project manager](#a-project-manager) ·
  [Wakes: early manager passes](#wakes-early-manager-passes) · [Teams](#teams) ·
  [Workflows](#workflows)
- [Unattended runs](#unattended-runs) · [Budgets and spend](#budgets-and-spend)
- [Chat](#chat) · [Research](#research) · [The Map tab](#the-map-tab) ·
  [Document spaces](#document-spaces) · [The Brain and skills](#the-brain-and-skills) ·
  [Knowledge base](#knowledge-base)
- [Apps](#apps) · [Previews](#previews) · [The dashboard](#the-dashboard)

### The board

The task board is a real kanban: drag cards between columns and reorder them
within one. Dropping a backlog card into **In Progress** starts its agent —
drag is the verb for "go". A card whose agent is still working refuses to
leave the column until you cancel the run, or [hand it over](#handing-a-running-card-over).

Every card has a comment thread. Type `@` to mention an agent by name and it
replies in the thread — after reading the repository, so its answer is grounded
in the code rather than in the question. Mentioned agents can't edit anything
from a comment; they answer, and real changes still go through tasks. Files can
be attached to a card at creation or later from its drawer; the next run sees
them.

A card can be assigned to an agent or to a [team](#teams). A **bake-off** runs the same
brief several ways at once — different agents, tiers or engines, each in its own worktree —
and you keep the one you like.

While it works, a card's agent has a small toolbox of Eren's own, served on
`/mcp/run/{run_id}` ([`run_tools.rs`](crates/eren-server/src/mcp/run_tools.rs)):
`comment`, `report_blocker`, `ask_person`, `propose_decision`, `submit_review` (for a
reviewer), `search_kb`, `read_article` and `recall`. These pass permission without asking a
person, so nothing in that toolbox can merge, start a run, or write settings or check
commands; a test fails the build for a tool named `resolve`, `approve` or `decide`. What a
run is offered is read from its row (a planning or summary pass only reads), and every MCP
endpoint refuses calls once its run has ended.

#### Plan first

A card can be set to **plan first**. The agent reads the code and writes down
what it means to do — what it found, which files it will touch, what it is
leaving alone, what it had to guess — then stops. Nothing has changed yet.

You get three answers, and the middle one is why this exists:

- **Approve** — work starts from the plan.
- **Edit, then approve** — rewrite the plan in place and start from *your*
  version. When a plan is 90% right, fixing the line beats paying for another
  planning pass to fix it for you.
- **Ask for changes** — send it back with a note; the next pass gets your
  feedback alongside what it proposed.

The planning pass is genuinely read-only: `Edit`, `Write`, `Bash` and friends
are *denied*, not merely left off the allow-list. That distinction is load
bearing — Claude Code's `--allowedTools` pre-approves rather than restricts, so
a planning pass "allowed" only `Read` will still reach for `Bash` unless told
it cannot.

The work pass resumes the planning session, so the agent keeps everything it
learned reading the code. When you edited the plan it is told so explicitly,
because it is resuming a conversation in which it remembers proposing something
else, and would otherwise follow its own memory over the text in front of it.

While parked the run holds no queue slot: planning finishes, the run waits, and
approving re-queues it. A parked plan is one of the things in [the inbox](#the-inbox).

#### Memories

Agents keep **memories**: when one finishes a task or answers a mention, a
compact note of what happened is stored and fed into its next runs, so an agent
you work with knows what it has been doing. Memories are visible (and prunable)
in the agent's editor drawer.

### Adding a folder

Point Eren at any folder — it does not need to be a git repository. If it
isn't one, Eren runs `git init` and makes a first commit of whatever is
already there when you add it. You can also clone from GitHub, or publish a local folder as
a new GitHub repository, when `gh` is logged in.

That isn't ceremony. Coding tasks run in an isolated worktree so an agent never
touches your working copy, and that worktree is also what produces the diff you
review before anything is merged back. A repository is the price of that
safety, so Eren creates one rather than asking you to.

The Files tab is an editor and does save — to your checkout, and to a card's
worktree so you can fix up what an agent produced before merging it. That is
you writing your own files, deliberately; the guarantee above is about agents,
and it is unchanged. The write path has its own gates (no `.git`, a root allow-list, a
content hash so two saves cannot clobber each other, and a header no cross-origin page can
set), documented at the top of [`routes/files.rs`](crates/eren-server/src/routes/files.rs).

A project page also has a **Terminal** tab — a real shell in the project's folder, over a
WebSocket, for exactly as long as the tab is open — and a **Storage** tab that shows what
the project is holding on disk (worktrees, preview images, per-run leftovers) and lets you
give it back.

A folder occasionally can't have its own repository — most often because it sits
inside another one, where nesting a second repo would confuse every later git
command. Those projects still work, but their tasks **edit the folder directly**:
no worktree, no diff, no review step, and no undo. They're marked
*no version control — edits in place* in the UI, and their cards go straight to
done because there is nothing to review. Full Auto stays refused for
them regardless of project settings, since the worktree that made Full Auto safe
isn't there.

### Attachments and file references

Drag an image, PDF, or text file onto the chat composer or the new-task form —
or paste a screenshot straight from the clipboard. The agent reads the file
itself, so a design mock, a spec PDF, or a CSV can go into a prompt instead of
being described in prose.

Attachments are stored under `~/.eren/attachments/`, **outside your repository**,
and the run is granted read access to them with `--add-dir`. They are never
copied into a task worktree: an untracked file there would show up in
`git status`, and an agent that runs `git add -A` would commit your PDF to the
branch and then to `main` on squash-merge.

Accepted: `png jpg jpeg gif webp pdf txt md csv json log`, up to 10 MB each and
10 per message. The type is decided by the extension and confirmed against the
file's magic bytes, so a renamed binary is rejected. Uploads you abandon are
swept after 24 hours.

To point at code that is already in the repo, type `@` in either composer and
search by filename. Press `:` on a highlighted result (or type it inline) to
pick a specific line or range:

```
compare `web/src/lib/api.ts:120-160` with the screenshot I attached
```

The reference is inserted as a backticked path, which resolves against the
repository root in both chat and task runs.

### Review: diffs, checks and follow-ups

A finished card lands in **Review** with its diff, measured from the `merge-base` with its
base branch rather than the base's tip, so work that landed on `main` meanwhile does not
show up as yours. **Merge** squash-merges the card's `eren/…` branch; with `gh` you can
instead finish it as a pull request and watch its checks from the card.

**Checks** ([`eren_core::checks`](crates/eren-core/src/checks.rs)) are a project's own test
and lint commands — up to ten — run in the card's worktree after an agent has finished, so
you review work that is known to pass. Two rules decide everything about them:

- **Only a person sets the commands.** They live in `project_checks`, which has exactly one
  writer — the settings route behind the write header — and a test that fails if anything
  else writes it. A shell command on a row an agent, an importer or an app build can write
  would hand an agent a shell.
- **Only a Full Auto run starts them unasked**, or a project whose review policy says
  *run checks after every run*. A check executes code the agent may have just edited — a
  `package.json` script, a `build.rs`, the tests themselves. A Full Auto agent already had
  an unprompted shell, so running its checks grants nothing new. Anywhere else a person
  clicks **Run checks**, and that click is the consent; the policy switch is the standing
  form of it.

Each check runs in its own process group with `CI=1` and `NO_COLOR=1`, so a timeout kills
the whole tree — `cargo test` and the test binaries it started, not just the `sh` in front
of them.

A **follow-up** ([`runs/follow_up.rs`](crates/eren-core/src/runs/follow_up.rs)) is a run that
goes back into a card's existing worktree to act on something said *about* its diff, so the
fix lands in the same diff you were reading instead of a fresh attempt that starts over.
The kinds are: your review note on a line or on the whole change; failing checks (by hand,
or up to three automatic fix attempts if the project allows them); a merge conflict; a
summary for a run that finished without saying what it did; the answer to a question the
agent asked; an agent review; and a hand-over.

### Review policy and the agent reviewer

Migration 0081. A project's review policy ([`eren_core::review`](crates/eren-core/src/review.rs),
written only by [`routes/reviews.rs`](crates/eren-server/src/routes/reviews.rs)) decides what
a person's Merge requires:

- **passing checks**,
- **an approving agent review**, and/or
- **a green pull request** —

each of the *latest* work. An approval or a green run from before the last change does not
count. Merge stays a person's click: an unmet gate answers `409` with `{kind: "gate",
unmet}`, and the dashboard offers to merge anyway with a written note, which goes on the
card and into the [audit log](#the-audit-log-and-a-cards-timeline).

The reviewer is another agent, never the card's author, running a read-only pass in the
card's worktree on its own engine. That engine must declare `enforces_denied_tools` — a
review that could edit the diff it is judging is not a review — which today means Claude
Code. Its verdict comes back only through the `submit_review` tool; a reviewer that ends
without one is recorded as *changes requested*, failing closed. The loop is bounded: each
round of changes requested starts **one** fix run, and after `max_rounds` (one to three,
two by default) the card waits in the inbox for you. **Review again** gives it one round
past the cap.

### Merge conflicts

Conflicts are met on the card's branch, never in your checkout. **Update from main** merges
the base into the card's branch inside its worktree; on conflict it leaves the merge in
progress and starts a `conflict` follow-up so the agent resolves it there. Committing a
worktree is refused while conflict markers remain, and a squash-merge refuses any diff that
adds them — so markers can never land on your base branch.

### Dependencies and landing

A card can be blocked by others; it waits until each blocker reaches **done**, not merely
review. Six different things write *done* — a merge, a drag, an in-place run, an app build,
a pull request merged on GitHub, an epic's mirror — and they share no code path, so the
seam is a column: `tasks.landed_at`, set once by whichever notices first, with a sweep on
every scheduler tick for the rest ([`eren_core::landing`](crates/eren-core/src/landing.rs)).

What a dependent does next is its own setting. With **start when unblocked** it starts
through the same door as the Start button — blocker check, capability vet and all — so a
chain of cards left overnight keeps moving. Otherwise its thread says it can start and the
[attention hook](#unattended-runs) tells you if you are away.

### Handing a running card over

Migration 0082. Reassigning a card whose agent is working used to be refused outright.
Now it is refused *unless it carries a note* ([`eren_core::handoff`](crates/eren-core/src/handoff.rs)):
the request is recorded on the card first, so a restart in between still completes it;
then the running agent is stopped; and once nothing of the card is live **and** the old
run has finished its post-work (its report, checks and review touch the same worktree),
the new agent continues in that worktree with your note as its brief — without the old
agent's session, which is not its memory to carry. A card whose run never got as far as a
worktree simply starts over under the new agent. The swap is one claiming `UPDATE`, so the
immediate attempt and the scheduler's sweep cannot both start it.

### The inbox

Migration 0078. Everything waiting on you is one list at **Inbox**
([`eren_core::inbox`](crates/eren-core/src/inbox.rs)): parked plans and team plans,
permission prompts (and ones a restart interrupted, which offer to resume the run),
questions an agent asked with `ask_person`, decisions an agent proposed, the chat
assistant's questions and plans, app schema changes that would lose data, agents' edits to
knowledge-base pages, preview recipes an agent wrote, and agent reviews that ran out of
rounds.

It is a query over the rows that already say they are waiting, never an index of its own —
a second table written beside each transition would be a second truth, free to disagree.
Only what had no row got one: permission prompts (which used to live in memory), an
agent's questions, and its proposed decisions. You can mark items read and snooze them.

Every answer goes through the function the thing's own button calls (the transitions live
in [`eren_core::approvals`](crates/eren-core/src/approvals.rs)), so answering from the inbox
is answering from the card. An answered question goes back to the same agent, in the same
worktree and session. A proposed decision has a closed set of effects — start a card, move
one to backlog, review or done, assign one, add a blocker, pause an agent — and approving
it runs exactly that, through the normal door.

### The audit log and a card's timeline

Migration 0079. **Audit log** is an append-only ledger with exactly one writer
([`eren_core::audit::record`](crates/eren-core/src/audit.rs)); a source scan fails the build
for any other write to the table. Three things feed it:

- every mutating `/api` request — the route template, the ids in the path and the status,
  **never the body**, which carries prompts, file contents and the occasional secret;
- every tool call through Eren's three MCP endpoints — the tool name and outcome, **never
  the input**;
- Eren's own actions: a routine firing, a run reaped, a hand-over, an automatic check.

With [accounts](#accounts) on it names the person who signed in, and only the admin can read
it. Without them it records `api`, not "a person": there is no login — on this machine any
local process can call the API, and from another device anything holding the
[access token](#using-eren-from-other-devices) can.
Agent and system entries are pruned after 90 days; API entries are kept. The page filters
and exports CSV (`GET /api/audit.csv`). A single card's merged story — runs, comments,
reviews, checks and audit entries in order — is `GET /api/tasks/{id}/timeline`, shown on
the card.

### Config revisions

Migration 0080. Editing an agent, a team, a routine (managers included), a skill, a
project's checks or review policy, a budget, the attention setting or the unattended-runs
setting keeps the row it replaced — the last 20 per thing
([`eren_core::revisions`](crates/eren-core/src/revisions.rs)). A test names every writer and
fails for one added without a revision.

**Restore is an edit.** The snapshot is mapped onto that thing's own update and sent
through its own handler, so validation, the write header, the single writer of
`project_checks` and the audit entry all apply — and the restore is itself a revision you
can undo.

### Agents

An agent is a name, an engine, a persona, a tier, a permission preset, and optionally
skills and MCP servers. It is **active**, **paused** or **retired**
([`eren_core::agents`](crates/eren-core/src/agents.rs)); every function that inserts a run asks
`agents::assert_can_run` first, and a source scan fails the build for one that does not. A
team run or a workflow asks again at each assignment, so pausing an agent stops its *next*
piece of work wherever it was coming from. A paused agent can still be assigned cards; a
retired one cannot and disappears from every picker. Deleting an agent that anything
references retires it instead.

Three optional limits shape *when* an agent works, not whether (migration 0077): how many
runs at once, how many a day, and a cooldown after each. A run over a limit waits in the
queue saying why, and the next run is claimed instead.

### The org chart

Migration 0085. Agents can report to other agents, which makes a workspace's agents a tree
at **Org chart** ([`eren_core::org_chart`](crates/eren-core/src/org_chart.rs)). It is kept a
tree by its one writer, under a per-workspace lock: same workspace, no cycle, at most eight
levels, no retired manager. Retiring a manager lifts its reports to its own manager.

Delegation flows down: a manager agent's pass is told its reports, and when an agent with
reports files a card it may bind only agents in its own subtree. Trouble flows up: a failed
run, a question, a stalled run or an exhausted review on an agent's card also wakes its
manager's routine. A workspace that never draws a chart works exactly as before.

### Goals

Migration 0086. **Goals** are a per-workspace tree — a company goal, the goals under it —
that cards and manager routines point at ([`eren_core::goals`](crates/eren-core/src/goals.rs)).
Every run of a card is told *why*: the chain from the top goal down to the card's own, and
its epic, appended after the brief as a fenced, capped "Why this matters", so an agent
settles small choices the way you would. A card split from an epic inherits the epic's goal
(a database trigger, because epics are split in more than one place). A manager pass sees
the active goals with their progress.

Progress is **counted, never stored** — done cards over all cards in the goal's subtree,
whenever it is asked — so it cannot drift from the board. Deleting a goal moves its children
and cards up to its parent.

### Heartbeats

Migration 0087. An agent can be given a **heartbeat** — every 5 minutes, 15 minutes, an hour
or four hours — and then pulls its own work ([`eren_core::heartbeat`](crates/eren-core/src/heartbeat.rs)).
It also beats early when a card is assigned to it or one of its cards is unblocked. Each
beat does at most one thing:

1. nothing, if the agent is paused, retired or already working (one card at a time unless
   its own limit says more);
2. fire the manager routine it runs, if any, under that routine's cooldown and daily cap;
3. otherwise start its next unblocked backlog card **through the Start button's own door**,
   so budgets, agent limits, capabilities and the Full Auto opt-in all apply;
4. with nothing to do, record an idle beat. Idle beats call no model and cost nothing.

The last 500 beats per agent are kept, which answers "is it picking up work, or just
idling?".

### Routines

A prompt that runs on a schedule. Each kind lands where that work naturally lives:

- **Chat** — a turn in the routine's own standing thread, so the replies collect in one
  place and this morning's answer knows what yesterday's said.
- **Research** — a fresh cited report each time.
- **Task** — a card created and started on a project's board.
- **Watch** — check a page on a schedule and report what changed.
- **Manage** — a [project manager](#a-project-manager) pass.

A routine never executes anything itself. Firing only *enqueues* through the same doors a
person uses, so concurrency limits, backoff, permission prompts, budgets and spend
accounting behave exactly as they do for manual work. The history row is written *before*
the work, which is why a firing that produced nothing still shows up saying why — "didn't
run: the assistant was still working" — instead of the list quietly thinning out.

The "next run" time on the page comes from the same cron parser that will fire it, so it
is not a second implementation that can disagree.

### A project manager

A project can be given a **manager**: an agent that reviews the board on a cron, with
nobody watching.

A pass is a turn in the project's standing manager thread, so the session resumes and this
morning's pass can say what moved since yesterday's — the difference between a manager and
a series of strangers each meeting the board for the first time. It lists the cards, reads
diffs and statuses to find out what actually happened rather than assuming it went well,
sees the project's [goals](#goals) and its [reports](#the-org-chart), and finishes with a
few lines: what changed, what it did, what it left alone, what it wants you to look at.

What keeps it honest is a **cap on how many cards one pass may start** — two by default,
ten at the most, and zero is a legitimate setting for a repository you are not ready to
let it touch. The cap is enforced by counting rows in the actions log inside the tool
handler, so the model cannot talk its way past it; it is also stated in the prompt,
because an agent that discovers its budget by being refused wastes the refusal and tends
to retry. Cards it creates without starting cost nothing and are the right answer whenever
it is unsure.

Two more rails worth knowing:

- **It cannot start a card that came from outside Eren.** An imported issue was written
  by somebody who is not the owner of this machine, and a person belongs between that text
  and an agent that can write files.
- **Board text is material, not instructions.** A card telling the manager to ignore its
  limits is content somebody typed; the manager is told to report it rather than obey it.

Every acting tool call is recorded, and the project's **Manager** tab shows what each pass
did — which is the question a manager has to answer that a plain routine does not.

### Wakes: early manager passes

Migration 0083. A manager on a nightly cron would otherwise hear about a card that failed
at 9:05 the next night. A **wake** is a row saying what happened
([`eren_core::wake`](crates/eren-core/src/wake.rs)) — a card landed, was unblocked or
failed; checks or review rounds ran out; an agent asked a question; a run stalled — and the
scheduler fires the manager early for it.

Only for the kinds you ticked on that routine: the list is empty by default, because an
early pass costs a run. Only when the manager's thread is idle (a busy thread keeps the
news for later rather than dropping it). Only after a cooldown (15 minutes by default) and
up to a number of early passes a day (six by default). The same kind of news about the
same card coalesces into one row, so a card that fails ten times overnight is one line in
the morning. Every pass, early or scheduled, opens with a fenced "Since your last pass" and
consumes it. Raising a wake never fails the thing that raised it.

### Teams

A team is several agents with a pattern: **pipeline**, **debate**, **swarm**, or
**org** — an organization, which is a team with a manager. Give an organization a goal and
the manager reads your repository, splits the work into briefed assignments, and delegates
each to the specialist best suited to it. Assignments share one worktree — like real
teammates on one codebase, so you get a single coherent diff instead of branches to merge —
and run in parallel exactly when the manager declared non-overlapping file scopes and no
dependency between them. Each assignment also appears on the board as a card under the
original, its epic.

They talk while they work, and you watch it happen:

- `post_message` — tell the team what you're doing or what you found
- `read_messages` — catch up on what teammates have said
- `ask_manager` — escalate a decision; this blocks the specialist while the
  manager answers from the context of the plan it wrote (capped per assignment,
  so nobody stalls forever)

The live view shows the roster with each teammate's state, the conversation as
it happens, and the assignment board filling in. Build one on the **Teams** page.

**Board tasks can be assigned to a team**, not just a single agent. Pick a team
in the "Assign to" dropdown and the task runs as that team — an organization
delegates it, a pipeline or debate team runs its pattern. Either way the work
happens in the task's own worktree, so the card lands on Review and you diff
and squash-merge it exactly like a solo task. Open the card and hit **Open team
room** to watch or replay the conversation behind it.

### Workflows

Build workflows on a canvas — drag between node handles to say "run after",
click a node to edit its prompt, model, agent, and fan-out. The canvas is a view
over YAML, which stays the source of truth: flip to the YAML tab any time, or
commit files to `.eren/workflows/` in your repo and press **Sync from repo**.
(Canvas edits regenerate the YAML, so comments in a hand-written file don't
survive a round trip through the canvas.)

```yaml
name: nightly-dep-audit
on: { schedule: "0 3 * * *" }     # standard 5-field cron
defaults: { permission_mode: auto_edit }
steps:
  - id: audit
    model: easy                    # easy | medium | complex, mapped per engine
    prompt: "List outdated dependencies and flag any with security advisories."
  - id: fix
    needs: [audit]
    model: medium
    session: continue              # resume the previous step's session
    prompt: "Upgrade the safe ones:\n{{ steps.audit.output }}"
```

Steps run in dependency order and outputs flow forward. Add
`strategy: { parallel: 3, isolated_worktrees: true }` to a step and it fans out into
independent attempts in separate worktrees — a step that `needs` it then sees every
attempt via `{{ steps.<id>.outputs }}`, which is how debate-with-a-judge works. A step can
also name an `agent` or an `engine`.

Scheduled workflows fire from the cron in `on.schedule`. If the machine was asleep past
a scheduled time, the missed run is skipped by default rather than stampeding on wake;
set the workflow's catch-up to *run once* to run one catch-up instead. A workflow asks the
agent gate and the budgets again before each step.

### Unattended runs

Most of the features above exist so work can carry on while you are away. This is what
keeps that safe.

**Full Auto** (*Don't ask* in the agent editor) runs with every tool allowed and no
prompts. It is honoured only in an Eren-managed worktree **and** in a project that has
opted in ([`runs/orchestrator.rs`](crates/eren-core/src/runs/orchestrator.rs)). Anywhere
else it steps *down* — to Reviewed on an engine that can ask, Auto-edit on one that can't —
and an engine that has nothing narrower (Cursor, Amp) is refused at the click with a reason.
Stepping down is de-escalation and safe; the reverse, quietly widening Reviewed to
Auto-edit because an engine cannot ask, would be privilege escalation performed on your
behalf, so `vet` refuses it instead.

What else stands around an unattended run:

- **Scheduled runs never park.** A step that stops to ask holds its concurrency permit for
  the whole run, so a 3am workflow blocking on a prompt eats one of `EREN_MAX_CONCURRENT`
  slots until someone answers. A *scheduled* run whose step resolves to Reviewed fails with
  a reason naming the step instead. Manual runs still park: someone chose to start them.
  The common way to hit this is writing `full_auto` on a project that hasn't opted in,
  which the gate cuts to Reviewed; the message says which of the two happened.
- **Waiting is bounded, and costs no slot.** A permission prompt waits as long as the
  attention setting says — a day by default, indefinitely if you set 0, at most a week —
  and a parked run lends its queue slot back while it waits.
- **The attention hook reaches you away from the screen.** Settings → *When a run needs you* runs a
  shell command of yours (`notify-send "$EREN_TITLE" "$EREN_BODY"`, a `curl` to ntfy) when
  something needs you: a permission prompt, a plan, a question, a decision, a rate limit, a
  budget warning or hold, a routine delivering, a card unblocked, a review that stopped, a
  stalled run. The hook is given a title, a body and a link — [never the tool
  input](#variables-eren-sets-on-processes-it-starts), which carries file contents and
  commands.
- **The reaper** (migration 0084, [`eren_core::reaper`](crates/eren-core/src/reaper.rs)).
  Every scheduler tick, a run whose row says it is running but that this process is not
  executing — its task died — is failed as **lost** after two minutes. That part is always
  on, since nothing else could ever finish it. Two more are opt-in, under Settings →
  **Unattended runs**: stopping a run that has said nothing for N minutes (never while it
  waits on a person; a long build is silent too, so this is off by default), and resuming
  lost or silenced runs by themselves — through the Resume button's own door, once per
  stopped run and at most twice along a chain. Every stall is said on the card, raised as a
  `stalled` wake and sent through attention.
- **Budgets** stop new work, and on a `stop` policy can interrupt a run mid-stream (below).
- **Checks** run themselves after Full Auto, and a bounded number of automatic fix runs can
  follow if the project allows them.

### Budgets and spend

The binding constraint here is usually a subscription rate limit you cannot see, so
Eren keeps what your CLI says about its own usage rather than discarding it.
It asks no provider anything, holds no credential, and prices nothing itself: a
dollar figure here is one the binary printed.

**Budgets** (migration 0076, [`eren_core::budgets`](crates/eren-core/src/budgets.rs)): a
policy covers the machine, a workspace, a project, an agent, a team or a routine, over a
calendar day, week or month, and caps any of dollars, output tokens and runs. It warns at a
percentage (80% by default) and bites in four places, cheapest first:

1. **at the click** — every door that starts work refuses a spent scope with a `409`
   naming the policy, rather than queueing work to sit there until midnight;
2. **at the queue** — a queued run whose scope is spent is held, saying which policy holds
   it, and the next run is claimed instead, so one spent project does not stop the others;
3. **between steps** — a team or a workflow starts nothing more;
4. **mid-run** — on a `stop` policy, a run crossing a token cap is interrupted.

**Dollars cannot stop a run midway**: no engine prices a run until it ends. And an engine
that never reports a price (Codex, and the four newer engines) is visible only to token and
run caps; its runs are recorded as unpriced rather than as $0. A policy can also ask before
a start whose forecast could cross what is left, and an override adds headroom for the
current window. Warnings and holds are written once per policy per window, which is what
keeps their notifications from repeating. The old single "daily budget" is now simply a
machine-wide policy by that name.

**Where the tokens went.** The Activity page breaks the window down by project, engine,
model, tier, agent, routine, goal, and which *feature* spent it. The last one is often the
useful one, because the dearest line is usually a pattern rather than a project: a
bake-off is several runs on one brief, a debate team is several attempts plus a judge, a
plan-first card is two passes. None of that shows up in a per-run cost.

**Cache hit rate** leads the panel. Cached input costs a fraction of fresh input, so the
share of your tokens served from cache — not the token count — is what separates a cheap
run from an expensive one. It reads `—`, never `0%`, when nothing has been sent: a fresh
install has not proven its cache broken. The totals name their own gaps: runs whose engine
never reported a price are counted separately, and runs that ended without a final tally
are marked as carrying estimates.

#### Auto tier

A card's tier can be set to **Auto**, and Eren picks per run.

This is worth having because `Medium` is the default and maps to Opus on Claude Code, so
every card nobody thought about runs on the dearest ordinary model. Auto is opt-in and never
the default — a router that switched itself on for everyone would be making exactly the
choice it exists to surface.

The rules are ordered and first-match-wins, not a score, because a score
cannot be explained to the person whose card it just routed:

- A **retry never routes below what already failed.** Rerunning a failure on a
  cheaper model pays twice to lose twice.
- **Planning gets the strong model; carrying out an approved plan gets a
  cheaper one.** This is the biggest honest saving — the judgment already
  happened and you approved it.
- A **long brief with several attachments or runbooks** is a briefing, not a
  chore, and goes up.
- A **short brief with nothing attached** goes down.
- Anything else stays at Medium, exactly as before.

Two rules are deliberately missing. Historical project cost is *not* an input:
it measures the model that was used, so routing on it closes a loop where
cheap runs keep justifying the cheap tier. And shortness alone never buys the
cheap tier — a short brief is as likely to be under-specified as simple.

Every automatic choice is recorded on the run before it starts and shown on
the card: *Auto → easy: a short brief with nothing attached*.

### Chat

Every project has an assistant beside its board, and there is a project-less General chat
for everything that is not about one repository. It reads the code and the board, and it
can file and start cards. It runs in your **real checkout**, not a worktree, so it is held
to `Read`, `Grep`, `Glob` and Eren's own task tools, with `Edit`, `Write`, `MultiEdit`,
`NotebookEdit` and `Bash` denied outright. Because it lives on Eren's tools, it is offered
only on engines that can carry them for one run (`mcp_tools`).

#### Plan mode

Filing and starting cards is where a misunderstanding turns into money: the assistant
creates a card, assigns an agent, starts it, and the first sign it had the wrong idea is a
run that has already spent. **Plan mode** takes the four acting tools away for a turn —
create, start, move and cancel — asks for a plan instead, and gives you a button.

The plan turn *finishes* like any other reply rather than parking, because a parked run
would block the next message in the conversation it is meant to be part of. So you can
argue with a plan in the next sentence. Approve carries it out; **Edit first** opens the
plan as text and the edited version is what is authoritative; there is no Reject button
because closing the plan and typing something else is already the answer.

#### It asks instead of guessing

In plan mode the assistant is told to ask before writing a plan on a wrong assumption.
The question arrives as a card with **options**, not as prose: a closed set is unambiguous
in both directions, and it forces the assistant to have thought of the alternatives rather
than merely noticing it was unsure. A single question with a single answer is answered by
clicking the option. Anything else (several questions, or a multi-select) keeps a Send
button, because nothing else can know when you have finished choosing. At most four
questions at once, with two to four options each; past that it is interviewing rather than
clarifying. There is always a way out that is not one of the options, because the composer
is right below. Unanswered questions and plans also sit in [the inbox](#the-inbox).

### Research

Ask a question about a project and get back a cited markdown report. A research run is
read-only over your **real checkout** — no worktree, no branch — with `Read`, `Grep`,
`Glob` and the CLI's own `WebSearch` and `WebFetch`, which is the half no other run type
gets. The five mutating tools plus `Task` are denied outright: research runs at the strong
tier, and a subagent fan-out would multiply that spend invisibly while making the live
transcript unreadable.

The report lands on the Research page and can be filed into the knowledge base with one
click. Filing is idempotent — the second click returns the article the first one made.

### The Map tab

What a project is made of, read out of the code rather than written down. The index is
built on demand — when the project page opens, when a card's work lands, when `HEAD`
moves — and a sha256 hash-diff makes the repeat passes cheap: an unchanged file costs one
read and one hash.

Three ways in, answering different questions:

- **Search by meaning.** "Where does the thing that rate-limits live" is a question
  `grep` cannot answer, because grep needs the word and not knowing the word is the whole
  problem. Ranking is cosine similarity over embeddings computed locally with ONNX
  inference; no model API is called and no vector extension is required. Agents get the
  same search as an MCP tool, so a run does not start by reading the directory tree.
- **A graph you can open up.** Symbols and imports come from tree-sitter — a real parser,
  because a regex cannot tell a definition from the same words inside a comment. Rust,
  TypeScript, TSX and Python have their insides drawn; other files are still listed and
  still searchable. An import that cannot be resolved against the project's real file list
  becomes nothing rather than a guess.
- **A list**, which answers the same as the graph without needing a mouse.

Node size is PageRank over the import edges. It sizes dots and breaks ties; it deliberately
does not order search results, because "most depended upon" reliably names the
infrastructure everybody already knows about and never the file you should be editing.
Everything here is derived and never authored, so each rendering names the branch and
commit it was read at.

### Document spaces

A project does not have to be a repository. A **space** is a folder of documents: drop in
`md txt csv json log pdf docx pptx xlsx xlsm xls ods` and they are chunked, embedded and
searchable. Legacy `.doc` and `.ppt` are refused at upload rather than accepted and left
permanently unreadable.

Chatting in a space retrieves the passages that answer the question and folds them into
the prompt, cited by file and position — the same local pipeline the Map tab uses.
Retrieved passages are fenced and labelled as reference material before they reach a run:
the text arrived from a file somebody else wrote, and a run holding Edit and Bash should not
treat it as instructions.

### The Brain and skills

Two ways to stop retyping the same context into every card.

**The Brain** is a project's standing context — *"the API lives in `/backend`"*, *"we do
not add dependencies without asking"*. It reaches every run in that project without
anybody remembering to attach it, which is the point: a thing you must remember every time
is a thing you use no times.

**A skill** is how one particular job is done here — the release checklist, the way
migrations get written, what a bug report has to contain. An agent is *who* does the work;
a skill is *how* this job goes. It applies when you name it — `@its-name` in chat, or
picked on a card — and never because something matched a description: a skill that only
applies when you name it cannot steer a request that never mentioned it.

Both are user-editable text pasted into a run holding Edit, Write and Bash, so both get the
same treatment: framed as background rather than orders, unable to close their own fence,
and capped so neither can bury the actual task. Text that looks like a credential is
refused on save.

#### Your rules and personal skills

Two things that follow *you* rather than a project — yours alone once [accounts](#accounts)
are on, the one local person's before that.

- **Your rules** (Settings → Your rules) are how you want agents to work, written once. Every
  new repository project, added or cloned, starts with them as `AGENTS.md` — the file Codex,
  OpenCode, Cursor and Amp read — plus a one-line `CLAUDE.md` (`@AGENTS.md`) so Claude Code
  reads the same text. Both are committed (only those two files, whatever else is staged),
  because an agent's worktree is cut from the branch and would not see a file that is only in
  the checkout. From then on they are the repository's files: edit them there; changing your
  rules changes projects made later. A repository that already has an `AGENTS.md` is left
  alone. In a clone of someone else's repository the commit is on your local branch, so a pull
  request from a card there includes it — delete the files first if that is not wanted.
- **A personal skill** (Skills → New personal skill) is offered in every workspace you have —
  the Skills page, the card picker and `@` in chat — instead of one. It still applies only where
  you name it, and its name must be free in each of your workspaces.

**Generating skills.** Skills → Generate with AI drafts one to three narrow skills from a
description of the job, on your own CLI login (Medium tier by default). Nothing is saved by
generating: you edit each draft and save it — as a workspace skill or a personal one — through
the same checks a hand-written skill meets.

#### Installing a skill from a registry

Eren can install Agent Skills into a project: `npx skills add owner/repo` is run in the
project, and what lands is a real skill — `.agents/skills/<name>/` with its `SKILL.md` and
whatever it bundles, symlinked into `.claude/skills/` so Claude Code reads it natively.
Each `SKILL.md` is then mirrored into an Eren skill row, so the same skill can be `@name`d
in a chat, bound to a card, and carried to an engine that has never heard of the format.
**The folder is what wins**: the row is re-derived from disk on every install and sync, so
copy it into a skill of your own if you want to change it.

It never installs globally — `-g` writes to `~/.claude/skills`, and not touching an
engine's own directory is the second compliance invariant. The result is committed, because
a worktree is branched from `HEAD` and an uncommitted skill never reaches a card run.
Whatever arrived that is not markdown — scripts and data an agent may execute — is listed
back to you for review.

### Knowledge base

A wiki, not a folder of documents. Pages have **a place, an address, and a
memory**:

- **A place.** Pages nest under each other in a tree, grouped into **spaces** —
  and a space is a repository, because the pages worth writing are about a
  codebase. Pages that belong to no one repo live in *General*.
- **An address.** `/knowledge/:pageId` is a real route with a read view,
  breadcrumbs, a contents list, child pages, and "linked from". Type `@` in the
  editor to link another page; the backlink appears on the other end.
- **A memory.** Every version is kept. The history shows what changed, when, and
  whether a person or an agent wrote it — including the versions that were
  turned down.

Editing has no Save button: typing saves. Every save carries the revision it
started from, so if the page moved under you the server refuses rather than
overwriting, and you get a diff and a choice instead of a lost afternoon.

**Agents propose; they never overwrite.** An agent can read your repository and write
documentation for it, but an agent's write is always a proposal, waiting in [the
inbox](#the-inbox). You get it as a diff over the page's **text**, never its HTML — two
model passes over identical prose emit different markup — and three answers: accept, *accept
and edit*, or discard with a note.

**What agents read.** Attach a page to a card and its text is folded into that run's
prompt, so attaching one is how you say "read the runbook before you touch anything".
Those bodies reach a process holding Edit, Write and Bash, and another agent may have
written them — so the prompt fences each page, says it is reference material rather than
instructions, and says whether a person has published it. Runs can also search and read
pages themselves with the `search_kb` and `read_article` tools.

**Storage.** Page text lives in Postgres and is fully searchable. Images and files pasted
into a page (up to 25 MB each) go to any S3-compatible endpoint, so a screenshot
doesn't end up in every database backup. Eren does not read a `.env` file itself — the
variables have to be in the environment of the `eren serve` process:

```bash
export EREN_S3_ENDPOINT=http://127.0.0.1:9100
export EREN_S3_ACCESS_KEY=eren
export EREN_S3_SECRET_KEY=eren-dev-secret
docker compose --profile storage up -d   # RustFS, given the same two keys as its root login
```

Compose's object store is [RustFS](https://rustfs.com), pinned to a release, with its console
on `http://127.0.0.1:9101`. It used to be MinIO, which no longer publishes its image —
`minio/minio` is gone from Docker Hub. Anything that speaks S3 with path-style addressing
works the same; point `EREN_S3_ENDPOINT` at it.

**Attachments uploaded to the old MinIO** stay where they were: in Eren's old `aichip-minio`
volume, which compose no longer mounts and nothing deletes. If you have the MinIO image cached
locally, start it against that volume and copy the bucket across with any S3 tool, for
example `rclone sync old:aichip new:eren`; the keys are the same on both.

The bucket is created on boot. Without these variables the wiki still works — you just
can't attach files, and the upload endpoint says so.

Bodies are sanitised **on write**, not on render: editor HTML is stored and served back to
other readers, which is the textbook stored-XSS shape. Embeds are allowed from a short host
allowlist; an `<iframe>` pointing anywhere else does not survive being saved. The editor is
[TipTap](https://tiptap.dev) — MIT, self-hosted, no account and no licence key.

### Apps

Everything else here changes code you already have, and lands as a diff you
review. An app is the other thing: something you ask for, install, switch on,
and **use**.

An app is a manifest — one YAML file, `eren.app.yaml`. Models in it become **real Postgres
tables** (a schema per app); views become screens Eren's own dashboard draws. Nothing you
get handed executes:

```yaml
name: Expenses
icon: "▤"
runtime: module

models:
  expense:
    fields:
      description: { type: text, required: true }
      amount:      { type: decimal }
      qty:         { type: int, default: 1 }
      total:       { type: decimal, compute: "amount * qty" }
      spent_on:    { type: date, default: "today()" }
      category:    { type: text }
    indexes: [spent_on]

views:
  list:  { columns: [spent_on, description, category, total], sort: "-spent_on" }
  chart: { shape: bar, group_by: category, measure: "sum(total)" }

menu:
  - { label: Expenses, view: list }
  - { label: By category, view: chart }
```

Describe what you want and an agent writes that file. It comes back **in the
editor, not installed** — being able to read the thing before it is real is the
entire reason an app is a declaration rather than code.

Field types are a closed set (`text int decimal bool date datetime json` and
`ref:<model>`), and names are lower-case letters, digits and underscores. That
narrowness is load bearing: these identifiers are interpolated into DDL, and the
defence is the charset rather than the quoting. Unknown keys are refused rather
than ignored — an agent that writes `colums:` has written a view with no
columns, and silently rendering an empty table is worse than saying which key.

A **container** app (`runtime: node` or `runtime: static`) is the escape hatch for work that
genuinely needs code: real source written by the agent, built by Docker from a Dockerfile
Eren owns rather than one the agent wrote, served on its own `<slug>.app.localhost`
hostname, and reaching its rows only through Eren's bridge at `/__eren/…`.

An app is a **project** under `~/.eren/apps/<slug>`, which is what gives it worktrees,
diffs and the files editor for free.

#### Changing one, and undoing it

**Change this app** hands it back to an agent: say what should be different, and
it works in a worktree of the app's own folder like any other card. That change **lands on
its own** when the card finishes — there is no review step, because the diff *is* the app.
The repository being merged into is the one Eren created for that app, never your code,
and every write Eren makes to an app's folder is committed.

What makes that bargain honest is that the undo is real. Every change records
where the app stood before it, and **Undo** on the newest one puts the folder
back exactly there. Only the newest, deliberately: an older change's starting
point knows nothing about the ones after it.

Landing files is not landing schema. The manifest is read back off disk and goes
through the same gate below, so a change that drops a column still waits for you
even though the file it came in has already merged.

#### Sharing one

An app is a folder, so sharing is a file. **Share** exports the app with empty
tables; **Export with data** carries the rows too. Import regenerates the DDL from the
manifest and never runs the bundled `schema.sql`, which is there to be read. For a team,
commit it: anything under `.eren/apps/` in a repository you have added shows up on the
gallery page with an **Install** next to it.

#### Your tables are yours

New tables, new columns and new indexes apply themselves. Anything that **destroys**
something — a dropped column, a dropped table, a changed type, a field turned into a
reference whose existing values have to be cleared — waits, whole, in [the
inbox](#the-inbox): you get the literal SQL and a sentence saying what it costs, and
nothing has run until you say so. What you approve is byte for byte what executes. The
comparison is against `information_schema`, not against a registry of what Eren thinks
the schema is, which would drift.

**Deactivate** takes an app out of the sidebar and keeps every row. **Uninstall** is the
only verb that drops a schema, and it asks first.

An app never gets a database connection, and never writes SQL. It says
`amount:gt:10`; the grammar is closed, identifiers are looked up among declared
fields, and every value is a bound parameter. Anything of *yours* — putting a card on your
board, starting a run — is a scope the manifest requests and you grant. Decimals stay
text the whole way, so a ledger does not lose cents to a double on its way to a browser.

### Previews

A card in review is a diff, and reading a diff is a poor way to answer "does this look
right". A **preview** ([`eren_core::previews`](crates/eren-core/src/previews/mod.rs)) builds
the card's branch — its root `Dockerfile`, or its compose file — and serves it on
`<name>.preview.localhost`, so the question can be answered by looking. It needs Docker —
and when Eren itself runs in a container, the host's Docker handed in; see
[previews in Docker](#previews-in-docker).

It is deliberately not a deployment feature. One container per card; memory, CPU and
process caps on each; loopback-only publishing; three live previews at once by default;
and a preview nobody has looked at for 30 minutes stops itself, keeping its image so coming
back costs seconds. At boot, Eren reconciles against *Docker* rather than its own table,
so a container no row claims is swept rather than orphaned.

A project with neither a Dockerfile nor a compose file can ask an agent to write a
**recipe**. A Dockerfile is not configuration — `RUN` executes arbitrary commands on this
machine — so a written recipe is a proposal in [the inbox](#the-inbox), shown in full and
never built until you approve it. The agent writing it is given no tools and never runs in
the project directory.

### The dashboard

React 18, Vite and Tailwind 4, organised as four questions in the sidebar: what is
happening (**Home**, **Inbox**, **Projects**, **Chat**, **Activity**), who does it (**Org
chart**, **Agents**, **Teams**, **Goals**, **Routines**), what they know (**Knowledge**,
**Research**, **Apps**, **Skills**), and how it is wired (**Connections** — MCP servers and
GitHub — **Audit log**, **Settings**).

- **Light and dark themes**, or follow the system — switch from the top bar or the palette.
  Every colour is a token that dark mode redefines, so switching is one attribute write,
  and a design scan in the web tests refuses hard-coded colours and home-made overlays that
  would break in the theme nobody checked.
- **A command palette** on ⌘K / Ctrl+K: jump to any page, search projects, cards, agents,
  teams, workflows and goals, switch theme, and the handful of common actions.
- It works at phone width, with the navigation behind a menu — open it on your phone through
  the [access link](#using-eren-from-other-devices).

The dashboard talks to the server through one client (`web/src/lib/api.ts`) and one
WebSocket (`web/src/lib/ws.ts`). The server persists every event to Postgres before it
publishes it, so a client that reconnects replays from the database rather than missing
what happened while it was away.

## Engines

Which ones you're offered depends on what's installed — `eren doctor` and
`GET /api/engines` both answer by *running* each CLI, never by reading its config. An
engine that isn't found is simply not offered. Differences between engines are declared
in each adapter's `Capabilities` (in `crates/eren-engines/src/<engine>/mod.rs`), and
behaviour is gated on the capability, never on the engine's name.

| Engine | id | Binary | Install | Notes |
|---|---|---|---|---|
| Claude Code | `claude-code` | `claude` | https://code.claude.com | The fullest: asks permission mid-run, structured rate-limit signal with a reset time, resumes sessions, appends to the system prompt, reports dollars, enforces denied tools, carries Eren's tools. Fixed model catalog: the CLI's aliases (`opus`, `sonnet`, … — the newest the installed CLI knows, and the defaults) or a pinned id (`claude-opus-5-5`, …). |
| OpenCode | `opencode` | `opencode` | https://opencode.ai | **Cannot ask mid-run**, so Reviewed is refused. Auto-edit works from a generated allow-list. `provider/model` ids from `opencode models`; reports dollars; carries Eren's tools via `OPENCODE_CONFIG`; rate limits by text match only. |
| Codex | `codex` | `codex` (`EREN_CODEX_BIN`) | `npm i -g @openai/codex` — https://developers.openai.com/codex/cli | **Cannot ask mid-run.** Driven by `codex exec --json` with `-c key=value` overrides; reports **tokens only**; carries Eren's tools; free-text model ids, tier defaults derived from the install. |
| Gemini CLI | `gemini` | `gemini` (`EREN_GEMINI_BIN`) | `npm i -g @google/gemini-cli` — https://github.com/google-gemini/gemini-cli | **Not yet run against the real binary.** Cannot ask; persona folded into the prompt; **no Eren tools**; tokens only. Model aliases `flash-lite` / `flash` / `pro`. |
| Cursor CLI | `cursor` | `cursor-agent` (`EREN_CURSOR_BIN`) | `curl https://cursor.com/install -fsS \| bash` — https://cursor.com/cli | **Not yet run against the real binary.** Cannot ask; **no Auto-edit** (`--force` allows commands too), so Full Auto or nothing; **no Eren tools**; tokens only; model `auto`. |
| Qwen Code | `qwen` | `qwen` (`EREN_QWEN_BIN`) | `npm i -g @qwen-code/qwen-code` — https://github.com/QwenLM/qwen-code | **Not yet run against the real binary.** Cannot ask; the closest to Claude Code otherwise — `--append-system-prompt` and Eren's tools via `--mcp-config`; tokens only; runs whichever model you configured Qwen for. |
| Amp | `amp` | `amp` (`EREN_AMP_BIN`) | `npm i -g @sourcegraph/amp` — https://ampcode.com (headless runs need `AMP_API_KEY`, which you set and Eren never does) | **Not yet run against the real binary.** Never asks about anything: **no Auto-edit, no read-only pass** (plans, summaries and reviews are refused), **no Eren tools** yet; tiers pick a mode (`low` · `medium` · `high`); no cost in the stream. |
| Ollama | `ollama` | `opencode` + `ollama` | https://ollama.com — needs OpenCode too, to drive it | OpenCode with Ollama as its provider; same capabilities as OpenCode; costs nothing. |
| LM Studio | `lmstudio` | `opencode` + `lms` | https://lmstudio.ai — needs OpenCode too, to drive it | OpenCode with LM Studio as its provider; same capabilities as OpenCode; costs nothing. |

The install column is what `doctor` prints beside an engine it didn't find. A `mock`
engine is also always registered: it replays a hand-written transcript, costs nothing, and
is what the test suite runs on.

### What "cannot ask" means

Only Claude Code can pause mid-run and wait for you to allow one tool call. Starting a
**Reviewed** card on any other engine is refused with a `409` and a reason, at the click
that caused it, rather than silently downgraded — quietly turning Reviewed into Auto-edit
would be a privilege escalation performed on your behalf. Auto-edit works where the engine
has a setting for "edit, but no commands"; Full Auto works where the project has opted in.

The other refusals follow the same rule. An engine that can't be handed Eren's MCP server
for one run (`mcp_tools: false` — Gemini, Cursor, Amp) is refused at the click for the chat
assistant, a project manager and a team member, and a card run on it simply goes without
its toolbox. Those tools could only reach Gemini or Cursor through a config file in the
run's folder, which would land in the diff — so they don't. An agent reviewer needs an
engine that enforces denied tools itself, which today is Claude Code.

Codex is driven through `codex exec --json`, and everything Eren needs to say about a
run — the sandbox, the approval stance, the persona, Eren's own MCP endpoint — is passed
as `-c key=value` overrides rather than written to `~/.codex/config.toml`, which the second
compliance invariant forbids touching. Your own config still merges in underneath.

Tier defaults for a multi-provider engine are derived at boot from the models that install
can actually reach, rather than hard-coded: an `anthropic/…` default is wrong for someone
whose only provider is Google, and they'd discover it when their first task failed.

### Gemini CLI, Cursor CLI, Qwen Code and Amp

These four adapters were written from each CLI's own source or documentation and tested
against **synthetic** fixtures and stand-in binaries — **not yet against the real CLIs**.
No binary was available where they were written. Their fixture folders
(`crates/eren-engines/src/{gemini,cursor,qwen,amp}/fixtures/README.md`) say exactly what was
guessed. The first person with one of these CLIs to hand can do the following, and a PR
replacing the fixtures with real recordings is very welcome:

- **Record a real run** and replace `task.jsonl`, then fix whatever the parser got wrong:
  - Gemini: `gemini --prompt=… --output-format=stream-json --approval-mode=auto_edit --skip-trust > task.jsonl`
  - Cursor: `cursor-agent -p --output-format stream-json --force --trust "…" > task.jsonl`
  - Qwen: `qwen --output-format=stream-json --approval-mode=auto-edit "…" > task.jsonl`
  - Amp: `amp -x "…" --stream-json --dangerously-allow-all > task.jsonl`
- **Check an error ending**: Gemini's quota error (`TerminalQuotaError`, `quota.jsonl`),
  Qwen with no provider key (`auth_error.jsonl`), Amp's error `result`, whose `error` is a
  string (`error.jsonl`).
- **Gemini:** confirm a read-only pass (a plan, a summary) cannot write — it runs in
  `default` mode with an admin-tier policy denying the writing tools, because a headless
  plan-mode run switches itself to YOLO to carry its plan out.
- **Cursor:** confirm the shape of a `function` tool call's `result` and the names of the
  `usage` fields on `result` (both guesses, marked in the parser); whether `-p` without
  `--force` writes (the docs disagree, so Eren assumes it can); and that `--mode ask` is
  really read-only.
- **Qwen:** watch `--exclude-tools` refuse a denied tool (until someone has, it does not
  declare `enforces_denied_tools`), and confirm Eren's tools arrive through `--mcp-config`.
- **Amp:** find out what `--mcp-config` accepts for a remote server; until someone checks,
  Amp declares no `mcp_tools` rather than run with tools it thinks it has.
- **All four:** resume a session, and confirm a rate-limit or quota failure is recognised.

### Local models: Ollama and LM Studio

Both appear in the engine picker alongside the others, and a run on either costs nothing
and leaves the machine at no point.

Under the hood they are not separate agents — they can't be, because an inference server
serves a model and holds no tools. Picking **Ollama** or **LM Studio** runs the `opencode`
binary with that runtime declared as its provider and the model resolved from what the
runtime actually reports (`ollama list`, `lms ls --json`) — read again before every run, so a
model you pull while Eren is running is there without a restart, and one you have removed is
named in the error rather than failing inside OpenCode — as explained at the top of
[`crates/eren-engines/src/local/mod.rs`](crates/eren-engines/src/local/mod.rs). So both need
OpenCode installed as well; `doctor` says so when it's the missing piece, and distinguishes
*not installed* from *installed, but its server isn't running*.

Two things to know before you pick one:

- **The model has to support tool calling.** A coding agent reads and edits files through
  tools, so a chat-only or pure-reasoning model can't do the job — Ollama's `deepseek-r1`,
  for instance, answers `does not support tools` and the run fails. Eren does not filter
  these out, because older Ollama can't say which models are which.
- **The context window has to fit the prompt.** Eren's chat prompt is around 13k tokens
  before your message; a model loaded with an 8k window will refuse it. Raise it in
  LM Studio, or `num_ctx` in Ollama.

Discovery is separate from all of this: Settings → *Local model runtimes* asks both servers
over HTTP so the model fields can offer what you have, and is where you tell Eren a runtime
listens somewhere other than its stock port.

## Settings and environment variables

Almost everything is set in the dashboard and stored in Postgres: model tiers per engine,
the default permission mode, attention, unattended runs, preview limits, local runtimes,
budgets, review policies. The environment is for what has to be decided before the server
starts. Every variable is optional, and `.env.example` lists them with comments. Eren itself
does not read a `.env` file — only Docker Compose does.

| Variable | Default | What it does |
|---|---|---|
| `EREN_BIND` | `127.0.0.1` | Address the dashboard listens on. Anything but loopback (`0.0.0.0` for every interface) turns on the access token, so other devices need the access link — see [using Eren from other devices](#using-eren-from-other-devices). An address that does not parse falls back to loopback. |
| `EREN_ALLOWED_HOSTS` | unset | Names other devices reach this machine by, separated by commas: `192.168.1.20`, `mybox.local`. Without one, the Host check refuses every other device. No ports, paths or wildcards. |
| `EREN_ACCESS_TOKEN` | generated | The token other devices must present when Eren listens beyond loopback. Unset, Eren makes a random one and keeps it in `~/.eren/access_token`; set it to choose your own (16+ letters, digits, `-_.~`), or to `off` for no token — which then also needs `EREN_TRUST_NETWORK`. |
| `EREN_TRUST_NETWORK` | unset | Set to anything but empty or `0` to acknowledge running beyond loopback **without** a token (`EREN_ACCESS_TOKEN=off`), where anyone who can reach the port can use this machine's agents. Not needed with the token on. |
| `EREN_MAX_CONCURRENT` | `2` | How many agent processes run at once. Higher values burn through a subscription's rolling rate limits faster. |
| `EREN_WEB_DIST` | `web/dist` | Where the dashboard build is served from, relative to the working directory unless absolute. |
| `EREN_BROWSE_ROOT` | `$HOME` | The only tree the folder browser may show. In a container, point it at wherever your code is mounted. |
| `EREN_APPS_DIR` | `~/.eren/apps` | Where apps live. |
| `EREN_PREVIEW_HOST` | `127.0.0.1` | The host Eren connects to a preview or container app on. Only Eren in a container needs another: `host.docker.internal`, which `docker-compose.previews.yml` sets — see [previews in Docker](#previews-in-docker). A host name or IP address, nothing else. |
| `EREN_PREVIEW_PUBLISH_IP` | `127.0.0.1` | The host address Docker publishes previews on. Only Eren in a container on a Linux host needs another, the bridge gateway (`172.17.0.1`). Every interface (`0.0.0.0`, `::`) is refused, falling back to loopback. |
| `EREN_S3_ENDPOINT` | unset | Object storage for knowledge-base files, e.g. `http://127.0.0.1:9100`. Storage is on only when this, the access key and the secret key are all set. |
| `EREN_S3_ACCESS_KEY` | unset | Its access key. Stripped from every process Eren starts. |
| `EREN_S3_SECRET_KEY` | unset | Its secret key. Stripped from every process Eren starts. |
| `EREN_S3_BUCKET` | `eren` | The bucket, created on boot. See [upgrading](#upgrading-from-aichip-to-eren) for the fallback when it is unset. |
| `EREN_S3_REGION` | `us-east-1` | Region used to sign requests. |
| `EREN_CODEX_BIN` | `codex` | The Codex binary to run. |
| `EREN_GEMINI_BIN` | `gemini` | The Gemini CLI binary to run. |
| `EREN_CURSOR_BIN` | `cursor-agent` | The Cursor CLI binary to run. |
| `EREN_QWEN_BIN` | `qwen` | The Qwen Code binary to run. |
| `EREN_AMP_BIN` | `amp` | The Amp binary to run. |
| `DATABASE_URL` | unset | Use this Postgres instead of the managed one under `~/.eren/pgdata`. Also what the database tests run against. |
| `RUST_LOG` | `info,sqlx=warn` | Log filter, in `tracing`'s `EnvFilter` syntax. |

Each `EREN_*` variable above is also read under its old `AICHIP_*` spelling, so a shell
profile from before the rename keeps working; when both are set, the Eren name wins, and
`eren serve` logs a warning at boot for every old name still set.

Compose reads a few more, which Eren itself does not read: `EREN_PROJECTS_DIR`, `EREN_PORT`,
`EREN_PUBLISH_IP`,
`CLAUDE_CODE_OAUTH_TOKEN`, `UID`, `GID`, `POSTGRES_USER`, `POSTGRES_PASSWORD`, `POSTGRES_DB`,
`POSTGRES_PORT`, `S3_PORT` and `S3_CONSOLE_PORT` (`MINIO_PORT` and `MINIO_CONSOLE_PORT` are
still read). See
[Running in Docker](#running-in-docker).

### Using Eren from other devices

To open the dashboard on a phone, tablet or another computer on your network:

```bash
EREN_BIND=0.0.0.0 EREN_ALLOWED_HOSTS=192.168.1.20 eren serve
```

with `192.168.1.20` replaced by this machine's address (if you leave `EREN_ALLOWED_HOSTS`
out, `eren serve` prints its best guess). It then logs an **access link** for each allowed
name — `http://192.168.1.20:4820/?access=<token>`. Open it once on each device: Eren keeps
the token in a cookie and takes it out of the address bar, and the device is remembered from
then on. Bookmark `http://192.168.1.20:4820` rather than the link.

- **This machine never needs the token.** Who is local is decided by the connection's address,
  not by anything a request says, so the agent CLIs Eren starts and the browser here carry on
  exactly as before.
- **Scripts** on another machine send `Authorization: Bearer <token>`.
- **Signing every device out**: delete `~/.eren/access_token` and restart; a new token is made.
  Set `EREN_ACCESS_TOKEN` instead to choose the token yourself.
- **Previews and apps** open on names under `localhost`, which another device resolves to
  itself, so they are only reachable from this machine.
- The token protects the dashboard over plain HTTP: anyone who can watch your network traffic
  can read it. That is fine on a home network you trust. On anything else, or to reach it away
  from home, put Eren behind a private network such as Tailscale (and allow the name it gives
  this machine), or use an SSH tunnel and leave `EREN_BIND` alone.

### Accounts

The access token lets devices in; it does not tell people apart. For a server several people
use — a home server, a shared box — turn on accounts:

```bash
eren admin create --username alice          # in Docker: docker compose exec eren eren admin create --username alice
```

It asks for a password twice (`--password-stdin` reads one line instead) and makes the one
**admin**. From then on:

- **Every browser signs in**, this machine's included, and the access token is no longer
  consulted. The agent CLIs Eren starts still reach `/mcp` over loopback without one.
- **Anyone who can reach the dashboard can create an account**, and starts with a workspace of
  their own. The admin closes sign-up under **Users** once everyone who should have an account
  has one.
- **Each account sees only its own workspaces** and everything in them: projects, cards, runs,
  agents, chats, the knowledge base, budgets, the inbox. Everything made before accounts were
  turned on belongs to the admin.
- **The admin** resets a password (the account gets a temporary one, is signed out everywhere,
  and must choose a new one at its next sign-in), disables an account, and is the only one who
  can change what belongs to the whole machine: Settings, the queue, machine-wide budgets and
  the audit log. A lost admin password is reset from this machine's shell:
  `eren admin reset-password alice`.
- Accounts take effect within a few seconds of `eren admin create`; there is nothing to restart,
  and nothing turns them off again.

**What an account does not buy.** Every account's agents run as the same user on the same
machine. Someone who can run an agent with a shell — Full Auto on a project of their own, or the
project terminal — can read what is on disk, other accounts' checkouts included. Accounts keep
people out of each other's boards, runs and chats; they do not make a shared Eren safe from
someone you would not trust with a shell on it. The same goes for a session cookie over plain
HTTP as for the token above.

### Variables Eren sets on processes it starts

Hook scripts, MCP servers and anything else a CLI starts inherit these from the engine
process, so a script can tell which run it belongs to:

| Variable | Set on | Value |
|---|---|---|
| `EREN_RUN_ID` | card runs, team members, workflow steps, research, comment replies, knowledge-base runs | the run's id |
| `EREN_CHAT_ID` | chat turns (including routine and manager passes) | the chat thread's id |
| `EREN_STEP` | workflow steps | the step's key |

The attention hook — the command you set under Settings → *When a run needs you* — gets `EREN_EVENT`
(`permission`, `plan`, `rate_limited`, `over_budget`, `finished`, `routine`, `unblocked`,
`budget_warning`, `question`, `decision`, `review` or `stalled`), `EREN_TITLE`, `EREN_BODY`,
`EREN_PROJECT`, `EREN_CARD`, `EREN_TOOL`, `EREN_RUN_ID` and `EREN_URL` (a link straight to the
card). There is deliberately no `EREN_INPUT`: the tool input carries file contents and
commands, and a hook is an arbitrary program that may forward them anywhere.

For one release, Eren also sets every one of these under its old `AICHIP_*` name,
so hooks and scripts written before the rename keep working.

Project checks are not given any of these; they run with `CI=1` and `NO_COLOR=1`. None of
these is ever an authentication secret: adapters refuse any extra variable that
`env_guard::is_auth_env` flags.

## Upgrading from aichip to Eren

Eren was called aichip, and that name is written into state that already exists — a home
folder, a database, branches, files in repositories, shell profiles, scripts. The rule for
each is the same: **write the new name, read both, prefer the new.** Every old spelling
lives in one place, [`crates/eren-shared/src/brand.rs`](crates/eren-shared/src/brand.rs)
(and `web/src/lib/brand.ts` for the dashboard), and a source-scanning test fails the build
when the old name turns up anywhere else.

Nothing to do by hand: run `eren serve` once. Here is what it does and what it keeps
working.

**On the first `eren serve`** (or `eren doctor`, for the home folder and the variables):

- **The home folder moves.** Eren renames `~/.aichip` to `~/.eren` before the managed
  Postgres starts, and leaves a relative symlink at the old path — because the database
  stores absolute paths into it, and git records each worktree's absolute path inside your
  repositories; with the link, every one of those paths keeps resolving with nothing
  rewritten. If the link cannot be made the move is undone and Eren refuses to start. If both
  folders already exist as real folders, Eren leaves both alone, logs a warning, and uses
  `~/.eren`. (`eren doctor` does the same move, so running a check right after upgrading
  shows it too.)
- **The managed database is renamed** in place, from `aichip` to `eren` (`ALTER DATABASE …
  RENAME`, instant whatever its size), before the pool opens. A Postgres you point Eren at
  with `DATABASE_URL` is left exactly as it is.
- **Stored tool names are rewritten.** Migration `0088_rename_tools.sql` rewrites
  `mcp__aichip__*` entries in agents' allowed-tools lists to Eren's `mcp__eren__*`, so a
  saved agent keeps every tool it had; names arriving later from a file are normalised the
  same way, and old transcripts show tool calls under their current names.
- **Apps Eren made get their manifest renamed** from `aichip.app.yaml` to `eren.app.yaml`,
  in each folder under `~/.eren/apps`, and the rename is committed, so the next build's
  worktree opens on the name its agent is told to edit.
- **Environment variables are read under both names**: `EREN_*` first,
  Eren's old `AICHIP_*` second, with a warning at boot for each old name still set (see
  [settings](#settings-and-environment-variables)).

**Read under both names from now on:**

- **Card branches** named `aichip/…` are still recognised as cards' branches, beside Eren's `eren/…`.
- **In your repositories**, Eren reads `.eren/` first and `.aichip/` second (workflows,
  apps), and still accepts an `aichip.app.yaml` manifest or an exported `aichip-app` bundle in Eren.
  Nothing in a person's repository is moved or rewritten.
- **Apps built before the rename** still reach Eren's bridge at `/__aichip/…` as well as
  `/__eren/…`, and Eren's `client.js` defines `window.aichip` as an alias of `window.eren`.
- **Scripts** sending the old `x-aichip-write` or `x-aichip-app` headers are let through by Eren
  exactly like `x-eren-write` and `x-eren-app`.
- **Child processes** get every `EREN_*` variable under its old name too, for one release.
- **Preview containers**, images and compose stacks started under the old name are found by
  Eren's boot sweep rather than left running and holding their ports.
- **Object storage**: when `EREN_S3_BUCKET` is unset and the default `eren` bucket does not
  exist but an `aichip` one does, Eren uses that one, so stored attachments stay reachable.
  Name the bucket explicitly to stop the fallback.
- **Dashboard settings** the browser saved under `aichip.*` (and `aichip:*`) keys move to `eren.*`
  on the first load of the dashboard; a value already saved under the new name wins.

**Docker Compose keeps its old names on purpose.** Eren's compose file still names the
volumes `aichip-pgdata` and `aichip-state` (Eren keeps these), and still
defaults the Postgres role, password and database to `aichip` (Eren keeps these too). They
name data that already exists: a volume is found by its name, and the Postgres image creates
its role and database only when the volume is new — renaming either would leave an existing
install looking at an empty volume, or at a role that does not exist. The container's state
volume is now mounted at `/home/eren/.eren`, and the Dockerfile links the old
`/home/aichip/.aichip` path to Eren's new one, so paths stored before the rename still resolve.
Compose also falls back to `AICHIP_PROJECTS_DIR`, `AICHIP_PORT`, `AICHIP_MAX_CONCURRENT` (Eren's
old names) and the old `AICHIP_S3_*` keys when the `EREN_*` ones are unset; with neither set,
the object store's root login defaults to the credentials Eren used before the rename too.

**The GitHub repository is being renamed to Eren as well.** GitHub redirects the old
URLs, so existing clones, links and remotes keep working; update your remote with
`git remote set-url` whenever convenient.

When the compatibility window closes, deleting the `LEGACY*` items in `brand.rs` and
following the compile errors removes all of this.

## Database

`eren serve` manages its own Postgres by default, under `~/.eren/pgdata`, with a generated
password kept beside it in `~/.eren/pg_password`. To use your own instead, set
`DATABASE_URL`. The compose file brings one up on port 5433:

```bash
docker compose up -d
export DATABASE_URL=postgres://aichip:aichip@localhost:5433/aichip   # legacy names Eren's compose keeps
cargo run -p eren-cli -- serve
```

That URL still says the old name because compose's role and database do — see
[above](#upgrading-from-aichip-to-eren) for why. On a fresh machine with no volume yet, you can set
`POSTGRES_USER`, `POSTGRES_PASSWORD` and `POSTGRES_DB` in `.env` before the first `up` and
use those in `DATABASE_URL` instead.

Migrations live in `crates/eren-core/migrations/` and run on connect. sqlx embeds them at
compile time, and adding a file does not always retrigger a rebuild: if a new column comes
back as `ColumnNotFound`, `touch crates/eren-core/src/db.rs` and rebuild.

## Running in Docker

Eren works by spawning *your* `claude` CLI under *your* login, so the interesting
question is how a container authenticates. On macOS the login lives in the **Keychain**
(there is no credentials file to mount), and a container has no keychain and no browser
to log in with. A container with your `~/.claude` mounted still reports
`Not logged in · Please run /login`.

The one way in is a long-lived token:

```bash
claude setup-token
```

Put it in `.env` as `CLAUDE_CODE_OAUTH_TOKEN`, set `EREN_PROJECTS_DIR` to the folder
holding your code (one parent directory, not your whole home), then:

```bash
docker compose --profile app up -d --build
```

That builds the image from the `Dockerfile` — the dashboard with Node 22 and pnpm, the
server with Rust on Debian trixie, and a runtime with git, Node and the `claude` CLI — and
runs everything, dashboard, orchestrator and agents, in containers, reachable at
`http://localhost:4820` (`EREN_PORT` to change it). The image runs `eren serve --headless`
as a normal user. Only Claude Code is installed in it; other engines would need adding to
the image. Previews and container apps need one more decision, described under
[previews in Docker](#previews-in-docker).

Postgres and object storage are published on `127.0.0.1` only; the dashboard is published on
every interface (`EREN_PUBLISH_IP`, default `0.0.0.0`), with the access token in front of it.
Inside the container Eren binds `0.0.0.0` (the image sets `EREN_BIND` and
`EREN_TRUST_NETWORK=1`, because the container's own loopback is not the host's); what is
actually reachable is decided by the port mapping. Your own browser reaches the container
through Docker's gateway rather than from its loopback, so it needs the access link too: open
the one `docker compose logs eren | grep "open this link"` shows, once per browser. The token
is kept in the state volume, so the link survives a redeploy. Set `EREN_ALLOWED_HOSTS` to the
address other devices reach it by, and turn [accounts](#accounts) on —
`docker compose exec eren eren admin create --username <name>` — if more than one person will
use it. To keep it to this machine, set `EREN_PUBLISH_IP=127.0.0.1` (and then, if you like,
`EREN_ACCESS_TOKEN=off`).

**Know what you're trading.** The token is a real credential sitting in a file, valid
until you revoke it, rather than a keychain entry scoped to your machine. Eren itself
still never reads, stores, or forwards it — the container inherits it from the environment
you set — but a token in `.env` is a broader exposure than the ordinary login, so keep
`.env` out of version control (it is gitignored) and revoke the token when you're done.

Three things the compose file handles that are easy to get wrong alone: your projects are
mounted at the **same absolute path** inside and out, because a git worktree records
absolute paths and a repo mounted elsewhere has a broken worktree link; the container
runs as your `UID`/`GID`, so files the agents write stay yours instead of root's; and
Eren's state lives in a named volume, so a restart doesn't strand half-finished work. The
`claude` CLI's own folder, `~/.claude`, is a volume too (`eren-claude`): it holds the session
every chat resumes, and without it each `up` that recreated the container broke every
existing chat with "No conversation found". A chat that meets that anyway — its session
deleted, or from before the volume existed — lets go of it and says so; the next message
starts a fresh conversation.

The app container's `DATABASE_URL` is built from the same `POSTGRES_*` defaults —
`postgres://aichip:aichip@postgres:5432/aichip` — which Eren keeps for existing volumes, as
explained under [upgrading](#upgrading-from-aichip-to-eren).

**The recommended shape is still Postgres in a container and the server on your machine**
(`docker compose up -d`, [above](#database)). You keep the ordinary keychain login, no token
exists to leak, every engine you have installed is available, and your paths are simply
real. Containerize the whole thing when you want it on a Linux box, running unattended, or
away from your laptop — not because it's tidier.

### Previews in Docker

[Previews](#previews) and container [apps](#apps) run on Docker, and a container has none of
its own. The image carries the Docker CLI; what it needs is the host's daemon, which
`docker-compose.previews.yml` hands in as its socket:

```bash
docker compose -f docker-compose.yml -f docker-compose.previews.yml --profile app up -d --build
```

or `COMPOSE_FILE=docker-compose.yml:docker-compose.previews.yml` in `.env`, after which every
plain `docker compose` command, and `docker-deploy.sh`, includes it.

**This is root on the host, for every agent.** Whatever can use Docker's socket can start a
privileged container with `/` mounted. Eren itself only builds and runs previews with it —
and those still get no socket, no mount and no privileges — but the agents in this container
have a shell, so with the socket any agent run can do the same. Without it they are confined
to the container and the folder you mounted. That is why this is a separate file and not part
of `docker-compose.yml`. If that trade is wrong for you, run Eren on the host instead (the
recommended shape, above), where previews need nothing extra.

What the override sets, and why:

- the socket, `/var/run/docker.sock` (`EREN_DOCKER_SOCKET` for another path) — Docker Desktop
  and OrbStack understand that path even on a Mac, where no such file exists on the host;
- the socket's group for the container user (`EREN_DOCKER_GID`, default `0`: Docker Desktop and
  OrbStack hand the socket in as `root:root 0660`; on Linux,
  `EREN_DOCKER_GID=$(stat -c %g /var/run/docker.sock)`);
- `EREN_PREVIEW_HOST=host.docker.internal`, because Docker publishes a preview on the *host*,
  and the container's `127.0.0.1` is its own.

On a **Linux host**, also set `EREN_PREVIEW_PUBLISH_IP=172.17.0.1` (your bridge gateway, if your
daemon moved it): a port on the host's loopback is not reachable from a container there, and
the gateway is still not your network. Docker Desktop and OrbStack keep loopback.

Two things stay different from a host install. The preview's own link on the card,
`http://<publish address>:<port>`, opens only on the Docker host; the
`<name>.preview.localhost` address goes through Eren and works wherever the dashboard does.
And a compose stack that bind-mounts its source (`./src:/app`) gets an empty folder when
previewing a card: card worktrees live in Eren's state volume, which the host's daemon cannot
see. A build that `COPY`s its source, which is what most Dockerfiles do, is unaffected. When
Docker still isn't usable, the Previews tab says which of these is missing.

### From a published image

To build once and run the image elsewhere — a Linux box, a server — without the source or a
toolchain there, push it to Docker Hub (or any registry) and deploy from it:

```bash
docker login
./scripts/docker-publish.sh
```

```bash
./scripts/docker-deploy.sh
```

Both use `neiellcare71/eren` unless told otherwise — a name as the publish script's argument,
or `EREN_IMAGE` in `.env` for both. A name with no namespace, like plain `eren`, never leaves
this machine: publish builds it into the local Docker, and deploy runs that local build
without pulling it.

`docker-publish.sh` builds the same `Dockerfile` with buildx and pushes it tagged with the
commit's short hash and `latest` (`--tag` adds more, `--no-latest` leaves `latest` alone,
`--no-push` loads it into the local Docker instead). It builds for the Docker daemon's own
architecture unless told otherwise, so name the server's when it differs —
`--platform linux/amd64` from an Apple silicon Mac, or `linux/amd64,linux/arm64` for both;
a foreign architecture is compiled under emulation and takes much longer. The image runs as
uid/gid 1000; `--uid`/`--gid` change it for a server where your user is someone else. It never
logs in for you, and `.env` never reaches the build.

`docker-deploy.sh` needs only `docker-compose.yml`, `.env`, itself and `docker-backup.sh`,
laid out as in the repository. It reads `EREN_IMAGE` and `EREN_TAG` (default `latest`) from `.env`, pulls,
and starts Postgres and Eren with `--no-build`, so it never falls back to building from
source; it warns when `CLAUDE_CODE_OAUTH_TOKEN` or `EREN_PROJECTS_DIR` is missing.
`--tag <hash>` deploys (or rolls back to) one build, `--with-storage` adds object storage,
`--with-previews` adds `docker-compose.previews.yml` (copy it alongside; read
[previews in Docker](#previews-in-docker) first), `--down` stops everything and keeps the volumes. To deploy to another machine from this one,
`DOCKER_HOST=ssh://you@server ./scripts/docker-deploy.sh` — `.env` is then read locally and
`EREN_PROJECTS_DIR` names a path on the server. Everything above about the token, ports and
mounts applies unchanged: it is the same compose service, pulled instead of built.

### Keeping your data across redeploys

Everything you would miss lives in three named volumes and one folder of your own:

| Where | What |
|---|---|
| the Postgres volume | projects, chats and every message, cards, agents, settings |
| the state volume (`~/.eren` in the container) | worktrees with agents' work in progress, attachments, apps, spaces |
| `eren-claude` (`~/.claude` in the container) | the Claude Code sessions each chat resumes |
| `EREN_PROJECTS_DIR`, bind-mounted | your code itself — on the host, never in a volume |

`up -d --build`, a new image, a restart and a plain `down` all keep them. What does not:

- **`docker compose down -v`**, `docker volume rm`, `docker volume prune -a` and
  `docker system prune --volumes` delete volumes, and the data in them. Never use them on
  Eren's.
- **A different compose project name.** Compose names volumes `<project>_<volume>`, and the
  project is the folder's name unless `COMPOSE_PROJECT_NAME` says otherwise. Deploy from a copy
  in another folder and Eren starts on new, empty volumes — the data is still there, beside
  them, looking lost. Set `COMPOSE_PROJECT_NAME` in `.env` to pin it.
- **A different `EREN_PROJECTS_DIR`.** The path is stored with every project and worktree, and
  in git's own worktree links. Move the folder and keep the path, or keep the setting.
- Anything written elsewhere in the container — a `git config --global`, a `gh auth login` in
  the terminal — is in its throwaway layer. The image sets a git identity (`eren`) so landing a
  card commits without one; a repository's own `git config user.name` still wins and is kept in
  the repository.

`docker-deploy.sh` holds the line on the middle two and takes a backup before every deploy:
it refuses (and changes nothing) when this deploy would start on empty volumes while yours
exist under another project name, or when `EREN_PROJECTS_DIR` differs from what the running
container has, saying which setting brings it back — `--force` when the change is intended.

Backups are `scripts/docker-backup.sh`, run by the deploy script first (`--no-backup` to skip),
or by you at any time. Each is a folder under `backups/` (`EREN_BACKUP_DIR`) on the machine
running the script — with `DOCKER_HOST=ssh://…`, your machine and not the server — holding a
`pg_dump` of the database and the state and session volumes as tarballs (the re-downloadable
model cache left out); the newest `EREN_BACKUP_KEEP`, 10 by default, are kept. It finds the
volumes from the running containers, so it backs up whatever is actually mounted. Backups hold
the database — settings, check commands, every conversation — so `backups/` is gitignored and
readable by you alone.

To put one back:

```bash
./scripts/docker-restore.sh backups/20261005-101500-pre-deploy --yes
```

It replaces the database, `~/.eren` and `~/.claude` with the backup's, exactly: the database is
dropped and replayed in one transaction, so a restore that fails leaves it as it was. Before
that it backs up the current state (`…-pre-restore`), and it refuses a folder without the
`COMPLETE` marker a finished backup writes. On a new machine, deploy first, then restore.

## Development

```bash
cargo build                          # build the Rust workspace
cargo test                           # all Rust tests (mock engine — no model usage, no rate limits)
cargo test -p eren-core              # one crate
cargo test -p eren-core backoff_escalates_and_caps   # one test by name

cd web && pnpm install && pnpm dev   # dashboard dev server; proxies /api and /ws to :4820
cd web && pnpm test                  # vitest
cd web && pnpm build                 # tsc -b && vite build → web/dist (what the server serves)
```

The **mock engine** ([`crates/eren-engines/src/mock/`](crates/eren-engines/src/mock/))
replays stream-json fixtures with configurable pacing and is the backbone of the
Rust suite, so a full `cargo test` spends nothing and cannot be rate limited. Rust tests
live inline in `#[cfg(test)] mod tests` next to the code they cover, not in a `tests/`
directory. Several of them read the source rather than run it: nothing spawns a process
except through `env_guard`, nothing inserts a run without asking the agent gate, nothing
but its one writer touches `project_checks` or `audit_log`, every config writer keeps a
revision, the old name appears only in `brand.rs`, and the documents mention every engine,
setting and migration.

**Database tests** live in inline `mod db_tests` blocks. Each makes its own throwaway,
fully migrated database on the server `DATABASE_URL` names (so the role needs `CREATEDB`)
and drops it afterwards; without `DATABASE_URL` they skip, saying so. CI runs them
against a Postgres service. Locally, against compose:

```bash
docker compose up -d
DATABASE_URL=postgres://aichip:aichip@localhost:5433/aichip cargo test   # compose's legacy names, kept by Eren
```

The **web tests** are pure vitest over the logic in `web/src/lib` — the canvas ↔ YAML round
trip, diffs, mentions, the knowledge-base tree, the expression language, inbox, goals, org
chart, theme, the old-name storage move — plus the design scan.

### Project layout

```
crates/
  eren-shared/    no dependencies on the others: event types (ErenEvent, EventEnvelope),
                  model tiers and per-engine tier mappings, permission modes and run
                  statuses, workflow YAML and interpolation, env_guard, brand (every name
                  Eren has had), rate-limit parsing, effort
  eren-engines/   the Engine trait, RunSpec and Capabilities; one adapter per CLI —
                  claude, opencode, codex, gemini, cursor, qwen, amp, local (Ollama and
                  LM Studio through OpenCode) — and the mock engine with its fixtures
  eren-core/      Postgres (db, migrations/), the run orchestrator and its queue, worktrees,
                  the scheduler, EventBus, PermissionBroker; agents, org_chart, goals,
                  heartbeat, teams (runs/org), routines, manager, wake, reaper, landing,
                  handoff, checks, review, budgets, inbox, approvals, decisions, audit,
                  revisions, attention; apps, kb, rag, repo (the code map), previews,
                  skills, storage (S3), github; legacy (the rename's database half)
  eren-server/    axum: /api routes (routes/), /ws event fan-out, /mcp (Eren's MCP
                  endpoints the engines call back into), the audit layer, the app bridge,
                  the preview and app reverse proxy, the terminal socket
  eren-cli/       the `eren` binary: `serve` and `doctor`; registers the engines at boot
web/              React 18 + Vite + Tailwind 4 dashboard; src/lib/api.ts is the single API
                  client, src/lib/ws.ts the socket, src/pages one file per page
docs/             architecture.md — read this before your first change
```

[`docs/architecture.md`](docs/architecture.md) explains how these fit together and why
they have the shapes they do; [`SECURITY.md`](SECURITY.md) covers the trust model and how
to report a problem.

## Contributing

Issues and pull requests are welcome. Start with [CONTRIBUTING.md](CONTRIBUTING.md) — it
covers the build, the test story, and the four compliance invariants that decide whether a
change to the engine layer can be merged at all.

## Licence

MIT. See [LICENSE](LICENSE).
