import { TreePage } from "./kbTree";
import type { OrgNode } from "./orgChart";
export type { OrgNode };
import type { Goal } from "./goals";
import { SIGNED_OUT_EVENT } from "./accounts";
export type { Goal };
export type Tier = "easy" | "medium" | "complex";
/**
 * What a person picked for a card, which is not the same as what a run gets.
 * `auto` means Eren decides per run and records which tier it chose and why.
 */
export type TierChoice = Tier | "auto";

export interface Workspace {
  id: string;
  name: string;
  icon: string;
  color: string;
}

export type AttentionEvent =
  | "permission"
  | "plan"
  | "rate_limited"
  | "over_budget"
  | "finished"
  | "routine"
  | "unblocked"
  | "budget_warning"
  | "question"
  | "decision"
  | "review"
  | "stalled";

export interface AttentionSettingsValue {
  enabled: boolean;
  command: string;
  events: AttentionEvent[];
  hookTimeoutSecs: number;
  /** 0 means wait indefinitely. */
  waitSecs: number;
  maxWaitSecs: number;
  /** The variables the hook will find set. Never includes the tool input. */
  envNames: string[];
  /** Set when the saved command looks like it contains a credential. */
  warning: string | null;
}

/** A model an on-machine runtime has, named the way OpenCode wants it.
 *
 *  Not an engine: Ollama and LM Studio serve models over HTTP and hold no
 *  tools, so they run *through* OpenCode rather than beside it. */
export interface LocalModel {
  provider: "ollama" | "lmstudio";
  /** `provider/model`, ready to use as an OpenCode model id. */
  id: string;
  name: string;
}

/** Where the probe looks. Defaults to the stock ports; stored in settings so
 *  a runtime on another port is a box to edit rather than a restart. */
export interface LocalHosts {
  ollama: string;
  lmstudio: string;
}

export interface Skill {
  id: string;
  name: string;
  /** When to reach for it. All you see in the picker. */
  description: string;
  instructions: string;
  /** Kept apart from the instructions, and put last in the prompt. */
  mustNot: string;
  enabled: boolean;
  updatedAt: string;
  /** `owner/repo` when this skill mirrors one installed from a registry, and
   *  null when somebody wrote it here. The installed folder is what an engine
   *  actually reads; this row is re-derived from it on every install, so an
   *  edit made here does not survive the next one. */
  sourceRepo: string | null;
  /** Whose checkout holds the files, so the UI can say where to edit them. */
  sourceProjectId: string | null;
  /** Yours rather than this workspace's: offered in every workspace you have. */
  personal: boolean;
}

/** What adding a project did with your rules: files written and committed, or why not. */
export interface RulesSeeded {
  written: string[];
  skipped: string | null;
}

/** What one registry install produced. */
export interface InstalledSkill {
  name: string;
  description: string;
  source: string;
  /** Non-markdown files the skill brought with it — the scripts an agent may
   *  run. Shown because installing a skill is handing one to an agent. */
  bundled: string[];
  skillId: string | null;
  mirrorError: string | null;
}

export interface SkillInstall {
  skills: InstalledSkill[];
  committed: boolean;
}

export interface ProjectBrain {
  body: string;
  /** Off means runs behave as though it were empty. It is still here. */
  enabled: boolean;
  /** What a save must carry back, so a stale editor is refused not merged. */
  hash: string;
  updatedAt: string | null;
  maxChars: number;
}

export interface ProjectStorage {
  checkouts: {
    bytes: number;
    count: number;
    reclaimable: number;
    reclaimableBytes: number;
    items: { branch: string; bytes: number; reclaimable: boolean; keptBecause: string | null }[];
  };
  previews: {
    /** Workspace-wide: Docker cannot attribute an image to one project. */
    bytes: number;
    reclaimable: number;
    items: { id: string; status: string; imageKept: boolean; title: string | null }[];
  };
  /** Kept on purpose — replay reads it. Reported so the total is honest. */
  history: { events: number; bytes: number };
  total: number;
}

export interface WorktreeHeld {
  worktrees: {
    path: string;
    branch: string;
    bytes: number;
    dirty: boolean;
    /** Its work is in the base branch, so the directory is a copy. */
    landed: boolean;
    reclaimable: boolean;
    /** Why it stays. Null when it can go. */
    keptBecause: string | null;
    title: string | null;
  }[];
  bytes: number;
  reclaimable: number;
  reclaimableBytes: number;
}

/** One file standing between a person and a merge. */
export interface DirtyFile {
  /** Git's staged-state letter; a space means "not staged". */
  index: string;
  worktree: string;
  path: string;
}

export interface CheckoutState {
  /** False for a project that edits in place — it has no merge to block. */
  vcs: boolean;
  path?: string;
  branch: string | null;
  /** Commits against the upstream. Null = no upstream to stand against —
   *  render as "publish", not as zeroes. */
  behind?: number | null;
  ahead?: number | null;
  hasRemote?: boolean;
  dirty: DirtyFile[];
}

export interface Project {
  id: string;
  path: string;
  name: string;
  defaultBranch: string;
  workspaceId: string;
  /** "git" — isolated worktrees, diffs, review. "none" — tasks edit in place. */
  vcs: "git" | "none";
  /** Why a project has no version control. Null for git projects. */
  vcsNote: string | null;
  /** Agents may work here without stopping to ask. Requires a git project. */
  fullAutoOptIn: boolean;
  /** Null everywhere means inherit — the machine default, or the card's own. */
  defaultEngine?: string | null;
  defaultTier?: TierChoice | null;
  defaultEffort?: Effort | null;
  /**
   * `owner/repo`, once some GitHub feature has resolved it from `origin`.
   *
   * Null means nothing has asked yet — never "this is not a GitHub project".
   * Resolved lazily and cached, so it appears after the first time a GitHub
   * surface is opened.
   */
  githubRepo?: string | null;
  /**
   * `"repo"` for code you added, `"app"` for an app's own folder.
   *
   * The three places that *list* projects filter to `repo`, because an app
   * belongs in the gallery; fetching one by id does not, because an app's
   * files still have to be reachable.
   */
  kind: "repo" | "app" | "space";
}

export type PermissionMode = "reviewed" | "auto_edit" | "full_auto";

export interface PermissionSettings {
  defaultMode: PermissionMode;
  /** Agents pinning their own mode — each one ignores `defaultMode`. */
  agentsOverriding: number;
  modes: { id: PermissionMode; label: string; blurb: string }[];
}

export interface Task {
  id: string;
  title: string;
  /** What the card asks for — the text that becomes the agent's brief. */
  prompt: string;
  /** Cards this one waits for. Unresolved until the blocker is done —
   *  landed — because a dependent run branches from main. */
  blockedBy: { id: string; title: string; boardColumn: Task["boardColumn"] }[];
  /** Start by itself once every blocker has landed. */
  startWhenUnblocked: boolean;
  /** The assignee's status: a paused one keeps the card but starts nothing. */
  agentStatus: Agent["status"] | null;
  /** What the agent reported stopped it, until the card next starts. */
  blockedNote: string | null;
  /** What was picked. `auto` means the tier is decided per run. */
  modelTier: TierChoice;
  /** True when `modelTier` is `auto` and no tier is settled until a run. */
  tierIsAuto: boolean;
  /** The tier the latest run actually used. Null before the first run. */
  tierResolved: Tier | null;
  /**
   * Why Eren picked that tier, when Eren picked it. Null when a person
   * chose — a choice that was already explicit needs no explanation.
   */
  tierReason: string | null;
  boardColumn: "backlog" | "running" | "review" | "done";
  /** Present once the card has been finished as a pull request. */
  prNumber?: number | null;
  prUrl?: string | null;
  prState?: TaskPullRequest["state"];
  prChecks?: TaskPullRequest["checks"];
  prReview?: TaskPullRequest["review"];
  /** Manual kanban ordering within a column; smaller sorts first. */
  position: number;
  branch: string | null;
  projectId: string;
  agentId: string | null;
  agentName: string | null;
  /** How this job gets done — composes with the agent. */
  skillId?: string | null;
  skillName?: string | null;
  /** The goal this card serves. */
  goalId?: string | null;
  goalTitle?: string | null;
  agentColor: string | null;
  teamId: string | null;
  teamName: string | null;
  teamPattern: string | null;
  /** The epic this is a sub-ticket of, if any. */
  parentId: string | null;
  parentTitle: string | null;
  /**
   * Sub-tickets under this card. `childCount > 0` is what makes a card an epic
   * — it is derived rather than stored, so it cannot disagree with the board.
   */
  childCount: number;
  childResolved: number;
  /**
   * The raw status of the assignment this card came from, when it came from one.
   * The four board columns have nowhere to put "failed" or "dropped", so the
   * column carries position and this carries the outcome.
   */
  stepStatus: string | null;
  /**
   * The mode this card will actually run under, and which of the three places
   * decided it — the bound agent's preset, the card's own, or the machine
   * default, in that order of precedence.
   *
   * Surfaced because the order surprises people: a project set to work without
   * asking still stops for permission when its agent carries its own preset,
   * and until this there was nothing anywhere that said so.
   */
  effectiveMode: "reviewed" | "auto_edit" | "full_auto";
  permissionSource: "agent" | "card" | "default";
  /** Set when the latest run was an organization run — opens the team room. */
  orgRunId: string | null;
  runId: string | null;
  runStatus: string | null;
  /** The last thing said about this card's run. Not always an error — a parked
   *  run carries what it is waiting for. Pair it with `runStatus` through
   *  `stopReason` rather than rendering it directly. */
  runError?: string | null;
  /** The latest run stopped short and left a session its own engine can pick
   *  up. The two remaining blockers — whether the engine can resume at all,
   *  and whether the worktree still exists — are decided on the click, and
   *  come back as a 409 saying which. */
  runResumable?: boolean;
  /** The latest run's dollars. */
  costUsd: number | null;
  /** Every run of the card, and their dollars together — retries and
   *  follow-ups cost money too. Absent from older servers. */
  runCount?: number;
  totalCostUsd?: number | null;
  /** The newest run of this project's own checks on the card — counts only.
   *  Not `prChecks`, which is GitHub's CI. */
  localChecks?: LocalChecksSummary | null;
  model: string | null;
  /** Which CLI this card runs on. */
  engine: string;
  /** Draft a plan and wait for approval before doing any work. */
  planFirst: boolean;
  /** This card's own thinking budget. Null means inherit. */
  effort: Effort | null;
  /**
   * What the card will actually think with, and which of the four places
   * decided it — the bound agent's budget, the card's own, its tier on the
   * engine it runs on, or the machine default. Surfaced for the same reason
   * `permissionSource` is: the order is not guessable from any one screen.
   */
  effectiveEffort: Effort | null;
  effortSource: "agent" | "card" | "tier" | "default";
}

/** A knowledge-base page. `contentHtml` is absent in list responses. */
export interface Article {
  id: string;
  workspaceId: string;
  title: string;
  summary: string;
  status: "draft" | "published";
  origin: "human" | "agent";
  sourceRunId: string | null;
  updatedAt: string;
  parentId: string | null;
  projectId: string | null;
  icon: string;
  position: number;
  /** The newest accepted revision. A label, not a lock. */
  currentSeq: number;
  /**
   * The token a save must match. Not `currentSeq`, which holds still while
   * rapid autosaves coalesce into one revision — so two editors both matched it
   * and the second silently overwrote the first.
   */
  bodyVersion: number;
  contentHtml?: string;
  // Present only on the single-page read.
  breadcrumb?: { id: string; title: string; icon: string }[];
  children?: { id: string; title: string; icon: string; summary: string }[];
  backlinks?: { id: string; title: string; icon: string }[];
  usedBy?: UsedBy;
  pendingRevision?: Revision | null;
  writing?: boolean;
}

/**
 * The board cards that depend on this page.
 *
 * `total` is the true count; `tasks` is capped server-side, so a page every card
 * references says how many it is not showing instead of pretending the list is
 * the whole story.
 */
export interface UsedBy {
  total: number;
  tasks: {
    id: string;
    title: string;
    projectId: string;
    projectName: string;
    boardColumn: string;
    /** Attached to the card, so every run on it is handed this page. */
    attached: boolean;
    /** How many comments linked it — a mention reaches one reply, not the card. */
    mentions: number;
  }[];
}


/**
 * A card's branch, built and running so you can look at it.
 *
 * `url` is present only while `status === "running"` — a link to a container
 * that is still building is a link to a connection refused.
 */
export interface TaskPreview {
  id: string;
  /** Null for a project's base-branch preview, which belongs to no card. */
  taskId: string | null;
  status: "building" | "running" | "stopped" | "failed";
  url: string | null;
  hostPort: number | null;
  containerPort: number | null;
  /** The Dockerfile named no port, so the one above is a guess. */
  portAssumed: boolean;
  /** The card was worked on after this was built, so it is serving history. */
  stale: boolean;
  /** Its image is still on disk, so starting again is a wake, not a rebuild. */
  canWake: boolean;
  /**
   * The hostname label this answers to, while it is running.
   *
   * Prefer this over `url`: a port is asked of the OS on every start, so the
   * port URL changes under a bookmark, and every preview on 127.0.0.1 shares
   * one cookie jar. `previewUrl()` builds the address.
   */
  slug: string | null;
  error: string | null;
}

/**
 * Where to send someone for a preview.
 *
 * The port is this page's own — Eren proxies preview hostnames on the port it
 * is already served on, so the browser's location is the authority on it and
 * the server never has to be told which port it is behind.
 */
export function previewUrl(p: TaskPreview): string | null {
  if (p.slug) {
    const port = window.location.port ? `:${window.location.port}` : "";
    return `http://${p.slug}.preview.localhost${port}`;
  }
  return p.url;
}

/**
 * A build recipe for a project with no Dockerfile of its own.
 *
 * `proposed` means an agent wrote it and nobody has read it. Nothing builds a
 * proposal — a Dockerfile's RUN lines execute on this machine, so approving one
 * is approving code, and the UI shows the whole text rather than a summary.
 */
export interface PreviewRecipe {
  /** The file's text, whichever kind it is. */
  dockerfile: string;
  /** What the agent decided this project needs. */
  kind: "dockerfile" | "compose";
  status: "proposed" | "approved";
  /** A person rewrote it rather than approving what was proposed. */
  edited: boolean;
}

/** One row in a project's Previews tab. */
export interface ProjectPreview {
  id: string;
  /** Null for the base branch, which belongs to no card. */
  taskId: string | null;
  /** The card's title, or "main". */
  title: string;
  status: "building" | "running" | "idle" | "stopped" | "failed";
  url: string | null;
  hostPort: number | null;
  containerPort: number | null;
  portAssumed: boolean;
  stale: boolean;
  canWake: boolean;
  slug: string | null;
  isStack: boolean;
  error: string | null;
}

/**
 * One of the user's plan limits, as their own CLI reported it.
 *
 * Not fetched from Anthropic — Eren holds no credential. This is telemetry
 * the CLI prints while it works, so it is as fresh as the last run.
 */
export interface PlanLimit {
  engine: string;
  /** `five_hour`, `seven_day` — the CLI's own vocabulary. */
  limitType: string;
  status: "allowed" | "warning" | "blocked";
  resetsAt: string | null;
  usingOverage: boolean;
  updatedAt: string;
}

/** One recorded change in a limit's state. */
export interface UsageEvent {
  engine: string;
  limitType: string;
  status: "allowed" | "warning" | "blocked";
  /** What it was before. `null` is the first time this limit was heard from. */
  previous: "allowed" | "warning" | "blocked" | null;
  resetsAt: string | null;
  usingOverage: boolean;
  observedAt: string;
}

/**
 * Whether a limit is a wall you meet often, or met once.
 *
 * `daysSeen` counts only the days Eren actually heard from the limit, which
 * is the days you ran something — so these are counts, never a percentage of
 * "the time". Eren learns nothing on a day it runs nothing.
 */
export interface UsagePattern {
  limitType: string;
  daysSeen: number;
  daysPinched: number;
  timesBlocked: number;
}

/**
 * The pull request a card was finished as, as Eren last saw it.
 *
 * Every field but `number` and `url` is a cache of what `gh` reported, which
 * is why `syncedAt` is here: "checks are passing" and "checks were passing an
 * hour ago" are different claims, and only one of them is ours to make.
 */
export interface TaskPullRequest {
  number: number;
  url: string | null;
  state: "open" | "draft" | "merged" | "closed" | null;
  /** `none` means the repository runs no checks — not that they passed. */
  checks: "none" | "pending" | "passing" | "failing" | null;
  review: "approved" | "changes_requested" | "review_required" | null;
  syncedAt: string | null;
}

/** What the drawer needs to render the pull request row, refusal included. */
export interface PullRequestState {
  pr: TaskPullRequest | null;
  canOpen: boolean;
  /** Why not, in a sentence — shown rather than thrown. */
  refusal: string | null;
}

/** How a clone is going. Polled; a clone can take minutes. */
export type CloneProgress =
  | { state: "cloning" }
  | {
      state: "done";
      projectId: string;
      path: string;
      name: string;
      githubRepo: string;
      defaultBranch: string;
    }
  | { state: "failed"; reason: string };

/**
 * An open issue, as offered for import.
 *
 * `body` is untrusted third-party text — it is rendered as plain text and
 * never as markup, and it becomes an agent's prompt only after somebody has
 * read it and ticked it.
 */
export interface GitHubIssue {
  number: number;
  title: string;
  body: string;
  url: string;
  labels: string[];
  author: string;
  /** The card it already became, if it has been imported. */
  importedAs: string | null;
}

/** A GitHub device flow waiting for the person to finish it. */
export interface GitHubConnect {
  id: string;
  /** The one-time code to type into GitHub. Not a credential. */
  code: string;
  url: string;
}

export type GitHubConnectProgress =
  | { state: "waiting" }
  | { state: "connected" }
  | { state: "failed"; reason: string };

/** Whether previews are possible here at all. */
export interface DockerStatus {
  installed: boolean;
  usable: boolean;
  version?: string;
  /** Why not, phrased for someone who now has to go and fix it. */
  problem?: string;
}

/** Whether this machine can talk to GitHub, and as whom. No token ever crosses this. */
export interface GitHubStatus {
  installed: boolean;
  usable: boolean;
  version?: string;
  accounts: {
    host: string;
    login: string;
    active: boolean;
    valid: boolean;
    /** gh's own words for why this login can't be used. */
    problem: string | null;
  }[];
}

/** One entry in a page's history. */
export interface Revision {
  seq: number;
  kind: "edit" | "agent" | "restore" | "import";
  state: "pending" | "accepted" | "discarded" | "superseded";
  authorKind: "human" | "agent";
  title: string;
  baseSeq: number | null;
  restoredFrom: number | null;
  runId: string | null;
  note: string;
  createdAt: string;
  chars: number;
}

export interface RevisionDiff {
  from: number | null;
  to: number;
  added: number;
  removed: number;
  diff: string;
}

/** A space is a repository; `id: null` is the workspace-wide General space. */
export interface Space {
  id: string | null;
  name: string;
  pages: number;
}

/** Raised when a page moved on while you were editing it. */
export class ConflictError extends Error {}

/** An article as it appears when tagged onto a card. */
export interface LinkedArticle {
  id: string;
  title: string;
  summary: string;
  status: string;
  origin: string;
}

/** The plan a parked run is waiting on. */
export interface TaskPlan {
  runId: string;
  content: string | null;
  /** Only while true can it be approved, edited, or sent back. */
  awaitingApproval: boolean;
  /** A person rewrote it, rather than approving what was proposed. */
  edited: boolean;
  writtenAt: string | null;
}

export interface Agent {
  id: string;
  name: string;
  icon: string;
  color: string;
  description: string;
  systemPrompt: string;
  modelTier: Tier;
  allowedTools: string[];
  /** null = inherit the workspace default. */
  permissionPreset: string | null;
  /** null = leave the CLI's own default alone. */
  effort: Effort | null;
  /** null = inherit whatever the card says. */
  engine: string | null;
  builtin: boolean;
  /** Paused keeps its cards and starts nothing; retired takes no new work. */
  status: "active" | "paused" | "retired" | "pending_approval";
  pauseReason: string | null;
  pausedAt: string | null;
  /** How hard it may be worked; null is no limit. */
  maxConcurrent: number | null;
  maxDailyRuns: number | null;
  cooldownSecs: number | null;
  /** Who it reports to on the org chart; null at the top. */
  reportsTo?: string | null;
  title?: string | null;
  /** Seconds between heartbeats; null when off. */
  heartbeatSecs?: number | null;
  lastHeartbeatAt?: string | null;
}

export interface AgentDraft {
  name: string;
  icon?: string;
  color?: string;
  description?: string;
  system_prompt?: string;
  model_tier?: Tier;
  permission_preset?: string;
  allowed_tools?: string[];
}

export interface Team {
  id: string;
  name: string;
  /** null = inherit from the card that summoned it. */
  engine: string | null;
  pattern: "pipeline" | "debate" | "swarm" | "org";
  definition: {
    manager?: string;
    members?: { agent_id: string; role?: string }[];
  };
}

export interface OrgMember {
  name: string;
  title: string;
  color: string;
  description: string;
  isManager: boolean;
}

export type Effort = "low" | "medium" | "high" | "xhigh" | "max";

export interface OrgAssignment {
  id: string;
  key: string;
  status: string;
  assignee: string | null;
  title: string | null;
  brief: string | null;
  output: string | null;
  dependsOn: string[];
  doneWhen: string[];
  /** Files this assignment declared it would change; drives parallelism. */
  touches: string[];
  size: string | null;
  origin: string;
  attempt: number;
  /** "manager" steps are planning, not work handed to a specialist. */
  kind: "manager" | "assignment";
  startedAt: string | null;
  finishedAt: string | null;
}

export interface OrgMessage {
  id: string;
  seq: number;
  from: string;
  to: string | null;
  kind: "assignment" | "message" | "question" | "answer" | "status" | "result";
  content: string;
  ts: string;
}

export interface OrgRunDetail {
  id: string;
  teamId: string;
  teamName: string;
  goal: string | null;
  status: string;
  costUsd: number | null;
  error: string | null;
  roster: OrgMember[];
  assignments: OrgAssignment[];
  messages: OrgMessage[];
}

export interface OrgRunSummary {
  id: string;
  teamId: string;
  teamName: string;
  goal: string | null;
  status: string;
  costUsd: number | null;
  error: string | null;
  createdAt: string;
}

export interface ChatMessage {
  id: string;
  role: "user" | "assistant" | "system";
  content: string;
  runId: string | null;
  ts: string;
  attachments: Attachment[];
  /** Knowledge-base pages this turn was given. Sent back with the message so
   *  that, afterwards, there is a record of what the assistant was handed. */
  articles: Array<{ id: string; title: string }>;
  /** A reply written in plan mode: a proposal, not something that happened. */
  isPlan: boolean;
  /** The person stopped this reply part-way. What is shown is not all of it. */
  stopped: boolean;
  /** Null while a plan is still open. "approved" once it has been carried
   *  out, "superseded" when a later plan replaced it. */
  planOutcome: string | null;
}

export interface Attachment {
  id: string;
  filename: string;
  mime: string;
  kind: "image" | "pdf" | "text";
  size: number;
}

/** Extensions the server accepts; mirrored here only to fail fast in the UI. */
export const ATTACHMENT_ACCEPT =
  ".png,.jpg,.jpeg,.gif,.webp,.pdf,.txt,.md,.csv,.json,.log";
export const MAX_ATTACHMENT_BYTES = 10 * 1024 * 1024;
export const MAX_ATTACHMENTS = 10;

export interface TaskComment {
  id: string;
  /** `system` is Eren itself — how a card's checks went, say. */
  author: "user" | "agent" | "system";
  agentId: string | null;
  agentName: string | null;
  agentColor: string | null;
  content: string;
  runId: string | null;
  /** Set when the note was written against a line of the diff. */
  filePath: string | null;
  line: number | null;
  hunk: string | null;
  ts: string;
}

export interface AgentMemory {
  id: string;
  kind: string;
  content: string;
  projectName: string | null;
  ts: string;
}

export interface ChatSummary {
  /** Null means inherit — the machine default, resolved when the turn runs. */
  modelTier: Tier | null;
  effort: Effort | null;
  /** Propose rather than act: the acting board tools are switched off and the
   *  reply is a plan you approve. Turned off again by approving one. */
  planMode: boolean;
  id: string;
  title: string;
  messageCount: number;
  updatedAt: string;
  /** One conversation on one model. null resolves from the tier. */
  modelId?: string | null;
  /** The agent this conversation is with; null is the assistant. */
  agentId?: string | null;
  agentName?: string | null;
}

export interface FileEntry {
  name: string;
  /** Project-relative, forward-slashed. */
  path: string;
  kind: "dir" | "file";
  size: number | null;
}

export interface FileListing {
  path: string;
  /** null at the project root. */
  parent: string | null;
  entries: FileEntry[];
}

export interface FileContent {
  path: string;
  size: number;
  tooLarge: boolean;
  binary: boolean;
  /** null when the file is binary or too large to send. */
  content: string | null;
  /**
   * The version token a save must quote back — sha256 of the exact bytes.
   *
   * Null exactly when `content` is, which makes it the single flag the save
   * button keys on: no hash, nothing to save against.
   */
  hash: string | null;
  /** Why this tree cannot be written, if it cannot. */
  readOnly: string | null;
}

/**
 * Which tree a file request is about.
 *
 * A card's worktree is the tree its diff is computed from, so an edit there is
 * an edit to the change you are about to merge.
 */
export type Tree =
  | { kind: "project"; id: string }
  | { kind: "task"; id: string };

const treeBase = (t: Tree) =>
  t.kind === "project" ? `/api/projects/${t.id}` : `/api/tasks/${t.id}`;

/** What the server says when a save lands on bytes you never saw. */
export interface FileConflict {
  error: string;
  currentHash: string;
  currentContent: string;
}

export class FileConflictError extends Error {
  constructor(readonly conflict: FileConflict) {
    super(conflict.error);
  }
}

export interface SearchHit {
  id: string;
  label: string;
  sublabel: string;
  /** Set on tasks and workflows, so the client knows where to navigate. */
  projectId?: string;
}

export interface SearchResults {
  projects: SearchHit[];
  tasks: SearchHit[];
  agents: SearchHit[];
  teams: SearchHit[];
  workflows: SearchHit[];
  /** Absent from servers older than goals. */
  goals?: SearchHit[];
}

export interface WorkflowDef {
  id: string;
  projectId: string;
  name: string;
  description: string;
  sourceYaml: string;
  cronExpr: string | null;
  enabled: boolean;
  catchUp: string;
  lastRunAt: string | null;
  nextRunAt: string | null;
  stepCount: number;
  error: string | null;
  uiLayout: Record<string, { x: number; y: number }>;
}

export interface WorkflowRun {
  id: string;
  workflowId: string;
  workflowName: string;
  status: string;
  trigger: string;
  costUsd: number | null;
  error: string | null;
  createdAt: string;
}

export interface RunStep {
  id: string;
  stepKey: string;
  status: string;
  output: string | null;
  startedAt: string | null;
  finishedAt: string | null;
}

export interface PendingPermission {
  requestId: string;
  toolName: string;
  input: unknown;
}

export interface ActivityRun {
  id: string;
  label: string;
  status: string;
  trigger: string;
  teamName: string | null;
  projectName: string | null;
  projectId: string | null;
  taskId: string | null;
  isOrg: boolean;
  costUsd: number | null;
  /** Which CLI is doing the work, and the exact model it resolved to. */
  engine: string;
  model: string | null;
  startedAt: string | null;
  createdAt: string;
  /** Why a queued run is not starting: the budget holding it. */
  holdReason: string | null;
}

/** Something that will not move until a person does something about it. */
export interface Blocker {
  runId: string;
  kind: "plan" | "permission";
  label: string;
  /** Plan blockers only: a team's plan opens the room, a card's opens the board. */
  isOrg?: boolean;
  projectId?: string | null;
  requestId?: string;
  tool?: string;
  input?: unknown;
}

/** Why the queue is or isn't dispatching. `over_budget` has no resume — it
 *  clears at midnight — so it must not be rendered as a pause. */
export type QueueGate =
  | { state: "open" }
  | { state: "paused" }
  /** A machine-wide budget is spent; `message` is the sentence to show. */
  | {
      state: "over_budget";
      policy: string;
      policyId: string;
      detail: string;
      resetsAt: string;
      message: string;
    };

export interface TeamEstimate {
  runs: number;
  medianUsd: number | null;
  worstUsd: number | null;
  medianSecs: number | null;
}

export interface Activity {
  paused: boolean;
  gate: QueueGate;
  /** Daily cap in dollars, or null when uncapped. */
  budgetUsd: number | null;
  live: ActivityRun[];
  blocked: Blocker[];
  spend: {
    today: number;
    window: number;
    daily: { day: string; cost: number; runs: number }[];
    byAgent: { name: string; cost: number; steps: number }[];
  };
}

/** One row of a spend breakdown — a project, a tier, a pattern, whatever. */
export interface SpendSlice {
  key: string;
  costUsd: number;
  runs: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
  /** Median, not mean — one runaway run shouldn't set the expectation. */
  medianUsd: number | null;
}

export interface SpendDay {
  day: string;
  costUsd: number;
  runs: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
}

export interface SpendTotals {
  costUsd: number;
  runs: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
  /** Runs whose counters include estimates nothing reconciled. */
  provisionalRuns: number;
  /** Runs that spent tokens but whose engine never reported a price. */
  unpricedRuns: number;
}

/** Which ways the spend can be sliced. Mirrors the server's dimension list. */
export type SpendDimension = "project" | "engine" | "model" | "tier" | "pattern" | "agent" | "routine";

export interface Spend {
  days: number;
  totals: SpendTotals;
  /**
   * Share of everything sent that was served from cache, or null when nothing
   * has been sent. Null and zero are different facts: "no runs yet" is not
   * "every request missed", and rendering both as 0% would say the cache is
   * broken on a fresh install.
   */
  cacheHitRate: number | null;
  byDay: SpendDay[];
  breakdowns: Record<SpendDimension, SpendSlice[]>;
}

/** Which model each complexity tier routes to, plus what may be chosen. */
/** One engine's tier routing, as the settings page edits it. */
export interface EngineModels {
  id: string;
  label: string;
  /** False for engines fronting many providers — the field is free text. */
  fixedCatalog: boolean;
  choices: { id: string; label: string; blurb: string | null }[];
  /** Model ids this install can actually reach. Suggestions, not a whitelist. */
  available: string[];
  providers: { name: string; auth: string }[];
  tiers: Record<Tier, string>;
  defaults: Record<Tier, string>;
  /**
   * How hard each tier thinks here. Null means the tier pins nothing.
   *
   * Optional because the dashboard is served from disk while the binary that
   * answers these calls is whatever is still running — a server started before
   * this field existed serves the new page and then omits it. Typed honestly
   * so the compiler makes every reader handle that.
   */
  efforts?: Record<Tier, Effort | null>;
}

/** The machine-wide thinking budget, and who ignores it. */
export interface EffortSettings {
  /** Null means every run keeps its CLI's own default — the shipped choice. */
  defaultEffort: Effort | null;
  /** Agents pinning their own budget, which outranks this. */
  agentsOverriding: number;
  levels: { id: Effort; label: string; blurb: string }[];
}

export interface ModelSettings {
  engines: EngineModels[];
}

/** One attempt in a bake-off, with the diff that decides it. */
export type CheckStatus = "queued" | "running" | "passed" | "failed" | "error" | "canceled";

export interface LocalChecksSummary {
  status: CheckStatus;
  passed: number;
  total: number;
}

/** One configured command. */
export interface CheckCommand {
  name: string;
  command: string;
}

export interface ProjectChecks {
  commands: CheckCommand[];
  timeoutSecs: number;
  autoFixAttempts: number;
}

export interface CheckResult {
  name: string;
  command: string;
  exitCode: number | null;
  timedOut: boolean;
  ms: number;
  outputTail: string;
}

export interface CheckRun {
  id: string;
  runId: string | null;
  status: CheckStatus;
  startedBy: "auto" | "person";
  results: CheckResult[];
  /** Paths the checks themselves left changed in the worktree. */
  dirtied: string[];
  error: string | null;
  createdAt: string;
  finishedAt: string | null;
}

/** One run of a card, as its history shows it. */
export interface TaskRun {
  runId: string;
  /** manual | resume | review | bakeoff | schedule | … — why this run exists. */
  trigger: string;
  status: string;
  engine: string;
  model: string | null;
  tierResolved: string | null;
  agentName: string | null;
  variantLabel: string | null;
  planFirst: boolean;
  costUsd: number | null;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
  /** The engine never sent its final count; these are the running tally. */
  tokensProvisional: boolean;
  createdAt: string;
  startedAt: string | null;
  finishedAt: string | null;
  seconds: number | null;
  error: string | null;
  sessionId: string | null;
  resumedFrom: string | null;
  rateLimitAttempts: number;
  /** The review note a follow-up acted on. */
  reviewCommentId: string | null;
  /** `cd … && …` to continue this session in your own terminal. Null when the
   *  engine offers none, the directory is gone, or the card is busy. */
  resumeCommand: string | null;
  /** The start of what the run posted on the card when it finished. */
  report: string | null;
}

export interface BakeoffVariant {
  runId: string;
  label: string;
  status: string;
  agentName: string | null;
  model: string | null;
  costUsd: number | null;
  error: string | null;
  seconds: number | null;
  linesChanged: number;
  diff: string;
}

/** An MCP server the user connected, giving agents tools beyond files+bash. */
export interface McpServer {
  id: string;
  name: string;
  transport: "stdio" | "http" | "sse";
  command: string | null;
  args: string[];
  env: Record<string, string>;
  url: string | null;
  headers: Record<string, string>;
  enabled: boolean;
  /** What the model sees its tools called, e.g. `mcp__playwright`. */
  toolPrefix: string;
}

export type McpTestResult =
  | { ok: true; tools: string[]; toolPrefix: string }
  | { ok: false; error: string };

export interface FsListing {
  path: string;
  parent: string | null;
  isGitRepo: boolean;
  dirs: { name: string; path: string; isGitRepo: boolean }[];
}

// ── Apps ────────────────────────────────────────────────────────────────────

/** The closed set of field types a manifest may declare. `ref:<model>` too. */
export type AppFieldType =
  | "text"
  | "int"
  | "decimal"
  | "bool"
  | "date"
  | "datetime"
  | "json"
  | string;

export interface AppField {
  name: string;
  label: string | null;
  type: AppFieldType;
  required: boolean;
  /** Worked out from other fields on every save, so a form must not offer it. */
  computed: boolean;
  hasDefault: boolean;
}

export interface AppModel {
  name: string;
  /** In declaration order, which is the order a form shows them in. */
  fields: AppField[];
}

export type AppViewKind = "list" | "form" | "kanban" | "chart";

export interface AppView {
  name: string;
  kind: AppViewKind;
  model: string;
  spec: {
    columns?: string[];
    sort?: { field: string; descending: boolean } | null;
    groups?: string[][];
    buttons?: string[];
    groupBy?: string;
    title?: string;
    fields?: string[];
    shape?: "bar" | "line" | "pie";
    measure?: string;
  };
}

export interface AppAction {
  name: string;
  label: string;
  /** An expression the browser evaluates; see `lib/expr.ts`. */
  showIf: string | null;
  steps: { kind: string; scope: string | null }[];
}

/**
 * Which of the two kinds of app this is.
 *
 * `module` executes nothing and Eren draws it; the other two are real code in
 * a container. The manifest's `runtime:` is what picks, and it cannot be
 * changed afterwards — that would be a different app, not an edit.
 */
export type AppRuntime = "module" | "node" | "static";

export interface AppManifest {
  name: string;
  icon: string;
  summary: string;
  runtime: AppRuntime;
  scopes: string[];
  models: AppModel[];
  views: AppView[];
  actions: AppAction[];
  menu: { label: string; view: string }[];
}

export interface App {
  id: string;
  projectId: string;
  workspaceId: string;
  slug: string;
  name: string;
  icon: string;
  summary: string;
  brief: string;
  runtime: AppRuntime;
  /** Switched on. Off keeps every row — only uninstalling drops data. */
  active: boolean;
  path: string;
  /** The manifest's own menu, so the sidebar can link straight to a screen. */
  menu: { label: string; view: string }[];
}

/** One attempt to change an app, landed or otherwise. */
export interface AppBuild {
  id: string;
  /** The card that did the work. Null once that card has been deleted. */
  taskId: string | null;
  brief: string;
  status: "running" | "landed" | "conflicted" | "failed" | "reverted";
  /** Why it did not land, or the manifest problem it landed with. */
  error: string | null;
  landedCommit: string | null;
  createdAt: string;
  /**
   * Whether this is the one build that can still be undone.
   *
   * Decided by the server, not here: which build may be reverted is a rule
   * about what `base_commit` can promise, and a second implementation of it in
   * the browser would disagree exactly once — silently discarding a later
   * change.
   */
  revertible: boolean;
}

export interface SchemaStatement {
  sql: string;
  /** True when running it loses something already stored. */
  destructive: boolean;
  /** A sentence for the person being asked, in terms of effect. */
  why: string;
}

export interface SchemaPlan {
  id: string;
  statements: SchemaStatement[];
}

export interface AppDetail extends App {
  manifest: string;
  /** Absent when the manifest no longer parses — `manifestError` says why. */
  declares?: AppManifest;
  manifestError?: string;
  pending: SchemaPlan | null;
}

/** A row, as the server projects it. Decimals arrive as strings. */
export type AppRow = Record<string, unknown>;

export interface RowQuery {
  /** `field:op:value`, repeatable. Never SQL — see `apps::query` in the core. */
  where?: string[];
  order?: string;
  limit?: number;
  offset?: number;
}

export interface ChartBucket {
  bucket: string | null;
  /** Text, so a summed decimal keeps its digits. */
  value: string | null;
}

/** An app a project offers under `.eren/apps/`. */
export interface RepoApp {
  dir: string;
  name: string;
  summary: string;
  /** Set when its manifest does not parse. Listed anyway, so it can be fixed. */
  error: string | null;
  /** The id of the app already installed under this name, if there is one. */
  installedAs: string | null;
}

export interface ContainerState {
  slug: string;
  preview: {
    status: "building" | "running" | "idle" | "stopped" | "failed";
    canWake: boolean;
    portAssumed: boolean;
    error: string | null;
  } | null;
  docker: { usable: boolean; problem: string | null };
}

export interface AppGrants {
  /** What the manifest asks for. Never itself a grant. */
  requested: string[];
  granted: { scope: string; grantedAt: string; lastUsedAt: string | null }[];
  /** Every scope Eren has, with a sentence each. */
  all: { scope: string; blurb: string; write: boolean }[];
}

export interface ActionOutcome {
  messages: string[];
  goto: string | null;
  deleted: boolean;
  /** Set when a step stopped for want of a permission. Not an error. */
  needsScope: string | null;
}



/** A document in a space's folder, with its semantic-index status. */
export interface SpaceDocument {
  id: string;
  relPath: string;
  /** pending = seen, not embedded yet. indexed = searchable. failed = the
   *  error says why (retried on the next reindex). unsupported = the agent
   *  can still Read it in the folder; it just isn't semantically searchable. */
  status: "pending" | "indexed" | "failed" | "unsupported";
  error: string | null;
  bytes: number;
  indexedAt: string | null;
}

/** Where the local embedding model stands. Downloading = the one-time fetch. */
export interface SpaceDocsStatus {
  embedder: { state: "not_ready" | "downloading" | "ready" | "failed"; detail?: string };
  counts: Record<string, number>;
}

// ── code map ────────────────────────────────────────────────────────────────

/** One indexed file, as the map sees it. */
/** Where the index stands. */
export interface RepoIndexStatus {
  /** never = nothing read yet. structure = files are being read (seconds).
   *  embedding = the file list is already complete and only meaning search is
   *  still filling in. ready = both done. */
  phase: "never" | "structure" | "embedding" | "ready" | "failed";
  /** The same embedder, and the same one-time download, as a space's. */
  embedder: { state: "not_ready" | "downloading" | "ready" | "failed"; detail?: string };
  counts: { files: number; parsed: number; embedded: number };
  structureVersion: number;
  /** The commit the index was read at — a card runs in a worktree on another
   *  branch, so a map that does not say which tree it describes can mislead. */
  indexedSha: string | null;
  indexedAt: string | null;
  error: string | null;
  /** There was nothing to read. A state, not an error. */
  note: string | null;
}

/** A file as a node of the dependency graph. */
export interface RepoGraphNode {
  path: string;
  /** The grammar that read it, or null for a language none here knows. */
  lang: string | null;
  bytes: number;
  /** 0..1 PageRank. Node size and tiebreaks only — it ranks infrastructure. */
  rank: number;
  status: string;
  symbols: number;
  importedBy: number;
  imports: number;
}

/** One file importing another, `weight` specifiers deep. */
export interface RepoGraphEdge {
  from: string;
  to: string;
  weight: number;
}

export interface RepoGraph {
  nodes: RepoGraphNode[];
  edges: RepoGraphEdge[];
  /** How many specifiers were found, and how many pointed at a file in this
   *  project. The rest are packages, and saying so is what lets a reader trust
   *  an empty neighbourhood instead of reading it as "nothing depends on me". */
  importsTotal: number;
  importsResolved: number;
  /** Bumped only when files or edges actually moved. The canvas refetches on a
   *  change and ignores the poll otherwise, so it never re-lays out under the
   *  cursor while embeddings fill in. */
  structureVersion: number;
  indexedSha: string | null;
}

/** One file's insides, fetched when it is selected. */
export interface RepoFileDetail {
  path: string;
  symbols: Array<{ name: string; kind: string; line: number; signature: string | null }>;
  imports: Array<{ path: string; weight: number }>;
  importers: Array<{ path: string; weight: number }>;
  /** Every specifier as written, resolved or not. */
  specifiers: string[];
}

/** A clarifying question the assistant asked, waiting for an answer. */
export interface OpenQuestion {
  id: string;
  questions: Array<{
    question: string;
    header?: string;
    options: Array<{ label: string; description?: string }>;
    multiSelect?: boolean;
  }>;
}

/** One semantic hit, collapsed to the best passage per file. */
export interface RepoSearchHit {
  path: string;
  /** 0..1 cosine, only comparable within one response. */
  score: number;
  /** 1-based, the way an editor counts. Null for a format with no lines. */
  line: number | null;
  /** The enclosing definition, when the chunker knew one. */
  symbol: string | null;
  excerpt: string;
}

/** A prompt that runs on a schedule. */
/**
 * The agent assigned to run this project's board on a schedule.
 *
 * A manager is a routine of kind `manage` under the covers — one per project —
 * so it shares the scheduler, the firing history and the notification when it
 * finishes. It gets its own type because the two things a manager has that no
 * other routine does are the two that matter here: who it is, and how much it
 * may spend without asking.
 */
export interface Manager {
  id: string;
  name: string;
  agentId: string | null;
  agentName: string | null;
  /** What this project's manager should care about. May be empty. */
  brief: string;
  cronExpr: string;
  catchUp: "run_once" | "skip";
  /** The kill switch. A disabled manager has no `nextAt`. */
  enabled: boolean;
  engine: string | null;
  modelTier: string | null;
  /** One conversation on one model. null means resolve from the tier. */
  modelId?: string | null;
  effort: string | null;
  /** The standing thread — the manager's memory between passes. */
  chatId: string | null;
  /** Cards one pass may start. 0 means review and report only. */
  maxStarts: number;
  /** News that wakes it before its schedule. Empty: only the schedule. */
  onEvents: WakeKind[];
  /** Least time between two passes a wake may fire. */
  cooldownSecs: number;
  /** Early passes a day, at most. */
  maxPassesPerDay: number;
  nextAt: string | null;
}

/** What can wake a manager early. See `eren_core::wake::Kind`. */
export type WakeKind =
  | "landed"
  | "unblocked"
  | "failed"
  | "checks_exhausted"
  | "review_exhausted"
  | "question"
  | "stalled";

export interface ManagerDraft {
  agentId?: string | null;
  brief: string;
  cronExpr: string;
  catchUp?: string;
  enabled?: boolean;
  engine?: string | null;
  modelTier?: string | null;
  effort?: string | null;
  maxStarts?: number;
  onEvents?: WakeKind[];
  cooldownSecs?: number;
  maxPassesPerDay?: number;
}

/** Something the manager did during one pass. */
export interface ManagerAction {
  kind: "create" | "start" | "move" | "cancel";
  taskId: string | null;
  title: string;
  detail: string;
  column: string | null;
}

/** One firing, and what came of it. */
export interface ManagerPass {
  id: string;
  firedAt: string;
  trigger: "schedule" | "manual";
  error: string | null;
  runId: string | null;
  runStatus: string | null;
  costUsd: number | null;
  actions: ManagerAction[];
}

export interface Routine {
  id: string;
  name: string;
  /** Where a firing lands: a chat turn, a research report, a board card, or
   *  a page-watch update in its thread. */
  kind: "chat" | "research" | "task" | "watch";
  projectId: string | null;
  projectName: string | null;
  prompt: string;
  cronExpr: string;
  /** "run_once": a window missed while the machine slept runs on wake. */
  catchUp: "run_once" | "skip";
  enabled: boolean;
  engine: string | null;
  modelTier: string | null;
  effort: string | null;
  /** The chat kind's standing thread, once it has fired. */
  chatId: string | null;
  /** The watch kind's target page. */
  url: string | null;
  nextAt: string | null;
  lastFiredAt: string | null;
  lastError: string | null;
  lastRunStatus: string | null;
}

export interface RoutineDraft {
  name: string;
  kind: Routine["kind"];
  projectId?: string | null;
  prompt: string;
  url?: string | null;
  cronExpr: string;
  catchUp?: string;
  engine?: string | null;
  modelTier?: string | null;
  effort?: string | null;
}

/** One firing — what it produced, or why it didn't. */
export interface RoutineRun {
  id: string;
  firedAt: string;
  trigger: "schedule" | "manual";
  error: string | null;
  runId: string | null;
  runStatus: string | null;
  costUsd: number | null;
  researchId: string | null;
  researchTitle: string | null;
  taskId: string | null;
  taskTitle: string | null;
  taskProjectId: string | null;
  chatId: string | null;
}

/** A deep research: one question, one current report. */
export interface Research {
  id: string;
  question: string;
  title: string;
  hasReport: boolean;
  kbArticleId: string | null;
  runId: string | null;
  runStatus: string | null;
  createdAt: string;
}

/** The detail view's shape — the list rows above, plus the body. */
export interface ResearchDetail {
  id: string;
  /** Null for a general (project-less) research. */
  projectId: string | null;
  question: string;
  title: string;
  reportMd: string | null;
  kbArticleId: string | null;
  runId: string | null;
  runStatus: string | null;
  runError: string | null;
  /** The choice this research was created with. Null = the defaults. */
  modelTier: string | null;
  effort: string | null;
  /** What the latest run actually ran as, and what it cost. */
  runModel: string | null;
  runCostUsd: number | null;
  createdAt: string;
  updatedAt: string;
}

async function json<T>(res: Response): Promise<T> {
  // A session that ended (signed out elsewhere, a password reset, expiry):
  // tell the auth provider, which puts the sign-in screen back.
  if (res.status === 401) window.dispatchEvent(new Event(SIGNED_OUT_EVENT));
  if (!res.ok) throw new Error(await res.text());
  return res.json() as Promise<T>;
}

const post = (url: string, body?: unknown) =>
  fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });

const patch = (url: string, body: unknown) =>
  fetch(url, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });

const put = (url: string, body: unknown) =>
  fetch(url, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });

// Deliberately no headers: the browser must set multipart/form-data itself so
// it can include the boundary. Setting Content-Type here would omit it and the
// server would fail to parse the body.
/** A write that lifts or sets a limit on spending: carries the header no
 *  cross-origin page can send, like the attention and file writes. */
const guarded = (method: "POST" | "PUT" | "PATCH" | "DELETE", url: string, body?: unknown) =>
  fetch(url, {
    method,
    headers: { "Content-Type": "application/json", "X-Eren-Write": "1" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });

/** A signed-in account. Accounts are off until an admin exists. */
export interface User {
  id: string;
  username: string;
  isAdmin: boolean;
  mustChangePassword: boolean;
  disabled: boolean;
  createdAt: string;
}

export interface AuthStatus {
  /** An admin exists, so every browser signs in. */
  accounts: boolean;
  /** Anyone who can reach the server may make an account. */
  signup: boolean;
  user: User | null;
}

/** One heartbeat: what an agent that pulls its own work did when it checked in. */
export interface Beat {
  at: string;
  reason: "timer" | "wake";
  outcome: "started" | "fired" | "idle" | "busy" | "held" | "paused";
  taskId: string | null;
  taskTitle: string | null;
  projectId: string | null;
  detail: string;
}

/** What Eren does about runs that stop showing signs of life. See `eren_core::reaper`. */
export interface Unattended {
  /** Stop a run that has said nothing for this long. 0 is off. */
  silenceMinutes: number;
  /** Pick a lost or silenced run back up — at most twice along a chain. */
  autoResume: boolean;
}

/** What a project's Merge requires. See `eren_core::review`. */
export interface ReviewPolicy {
  requireChecks: boolean;
  requireReview: boolean;
  reviewerAgentId: string | null;
  maxRounds: number;
  requirePrGreen: boolean;
  runChecksAfterEveryRun: boolean;
}

export interface ReviewNote {
  file?: string | null;
  line?: number | null;
  body: string;
}

export interface ReviewDecision {
  id: string;
  runId: string | null;
  round: number;
  verdict: "approve" | "request_changes";
  /** False: the reviewer ended without a verdict, recorded as changes requested. */
  submitted: boolean;
  summary: string;
  notes: ReviewNote[];
  reviewer: string | null;
  createdAt: string;
}

/** One requirement Merge still waits on. */
export interface Unmet {
  kind: "checks" | "review" | "pull_request";
  message: string;
}

export interface CardReviews {
  reviews: ReviewDecision[];
  rounds: number;
  maxRounds: number;
  requireReview: boolean;
  unmet: Unmet[];
}

/** A merge the review policy refused, with what it still wants. */
export class MergeGateError extends Error {
  constructor(public unmet: Unmet[]) {
    super(unmet.map((u) => u.message).join(" "));
  }
}

export type InboxKind =
  | "plan"
  | "team_plan"
  | "permission"
  | "permission_expired"
  | "question"
  | "decision"
  | "chat_question"
  | "chat_plan"
  | "schema"
  | "kb_revision"
  | "review"
  | "recipe";

/** One thing waiting on a person. See `eren_core::inbox`. */
export interface InboxItem {
  key: string;
  kind: InboxKind;
  title: string;
  detail: string | null;
  projectId: string | null;
  projectName: string | null;
  taskId: string | null;
  runId: string | null;
  /** Where "open" goes. */
  link: string;
  createdAt: string;
  /** What can be done from the inbox itself. */
  actions: string[];
  read: boolean;
  snoozedUntil: string | null;
  /** Suggested answers a question offered. */
  options: string[];
}

/** One line of the ledger. See `eren_core::audit`. */
export interface AuditEntry {
  id: number;
  at: string;
  /** "user" is a signed-in account. "api" is anything through the API while
   *  accounts are off — there is no login then, so it does not claim "a
   *  person". */
  actorKind: "api" | "user" | "agent" | "system";
  actorRunId: string | null;
  actorUsername: string | null;
  action: string;
  entityKind: string | null;
  entityId: string | null;
  summary: string;
  detail: Record<string, unknown>;
}

export interface AuditQuery {
  entityKind?: string;
  entityId?: string;
  actorKind?: string;
  before?: number;
  limit?: number;
}

export interface TimelineEvent {
  at: string;
  kind: "comment" | "run" | "checks" | "audit";
  actor: string | null;
  title: string;
  detail?: string | null;
  runId?: string;
  costUsd?: number | null;
  status?: string;
}

const auditParams = (q: AuditQuery) => {
  const p = new URLSearchParams();
  if (q.entityKind) p.set("entity_kind", q.entityKind);
  if (q.entityId) p.set("entity_id", q.entityId);
  if (q.actorKind) p.set("actor_kind", q.actorKind);
  if (q.before != null) p.set("before", String(q.before));
  if (q.limit != null) p.set("limit", String(q.limit));
  return p.toString();
};

export type RevisionKind =
  | "agent"
  | "team"
  | "routine"
  | "skill"
  | "project_checks"
  | "budget_policy"
  | "attention"
  | "review_policy"
  | "unattended";

/** A setting as it was before one change. See `eren_core::revisions`. */
export interface ConfigRevision {
  id: number;
  entityKind: RevisionKind;
  entityId: string;
  createdAt: string;
  snapshot: Record<string, unknown>;
  /** What restoring it would change, against the setting as it is now. */
  changed: string[];
}

const postForm = (url: string, form: FormData) =>
  fetch(url, { method: "POST", body: form });

export type BudgetScopeKind = "machine" | "workspace" | "project" | "agent" | "team" | "routine";
export type BudgetCap = "usd" | "tokens" | "runs";

export interface BudgetPolicy {
  id: string;
  name: string;
  scopeKind: BudgetScopeKind;
  scopeId: string | null;
  windowKind: "day" | "week" | "month";
  capUsd: number | null;
  capOutputTokens: number | null;
  capRuns: number | null;
  warnPercent: number;
  /** `true` for on_exceed = "stop": a run in flight is stopped at a token cap. */
  stopsInFlight: boolean;
  confirmAboveUsd: number | null;
  enabled: boolean;
}

export interface BudgetStanding {
  policy: BudgetPolicy;
  used: { usd: number; outputTokens: number; runs: number };
  verdict: { state: "open" } | { state: "warn"; percent: number } | { state: "exceeded"; cap: BudgetCap };
  windowStart: string;
  windowEnd: string;
  scopeLabel: string;
  /** Queued runs this policy is holding right now. */
  held: number;
  /** At this rate: which cap runs out first, and when (null: lasts the window). */
  forecast: { cap: BudgetCap; runsOutAt: string | null } | null;
}

export interface BudgetBody {
  name: string;
  scope_kind: BudgetScopeKind;
  scope_id: string | null;
  window_kind: "day" | "week" | "month";
  cap_usd: number | null;
  cap_output_tokens: number | null;
  cap_runs: number | null;
  warn_percent: number;
  on_exceed: "hold" | "stop";
  confirm_above_usd: number | null;
  enabled: boolean;
}

export interface CostEstimate {
  medianUsd: number;
  p90Usd: number;
  runs: number;
  basis: "project" | "tier" | "engine";
}

export const api = {
  // accounts
  authStatus: () => fetch("/api/auth/status").then((r) => json<AuthStatus>(r)),
  login: (username: string, password: string) =>
    guarded("POST", "/api/auth/login", { username, password }).then((r) => json<{ user: User }>(r)),
  signup: (username: string, password: string) =>
    guarded("POST", "/api/auth/signup", { username, password }).then((r) => json<{ user: User }>(r)),
  logout: () => guarded("POST", "/api/auth/logout").then(json),
  changePassword: (current: string, next: string) =>
    guarded("POST", "/api/auth/password", { current, new: next }).then((r) => json<{ user: User }>(r)),
  users: () => fetch("/api/auth/users").then((r) => json<{ users: User[] }>(r)),
  setUserDisabled: (id: string, disabled: boolean) =>
    guarded("PATCH", `/api/auth/users/${id}`, { disabled }).then(json),
  /** A temporary password, shown once: it works only to choose a new one. */
  resetPassword: (id: string) =>
    guarded("POST", `/api/auth/users/${id}/reset-password`).then((r) => json<{ password: string }>(r)),
  setSignupOpen: (open: boolean) => guarded("PUT", "/api/auth/signup-open", { open }).then(json),

  budgets: () =>
    fetch("/api/budgets").then((r) =>
      json<{ policies: BudgetStanding[]; unpricedEngines: string[] }>(r),
    ),
  configRevisions: (kind: RevisionKind, id: string) =>
    fetch(`/api/revisions?kind=${kind}&id=${encodeURIComponent(id)}`).then((r) =>
      json<{ revisions: ConfigRevision[] }>(r),
    ),
  restoreConfigRevision: (rev: number) =>
    guarded("POST", `/api/revisions/${rev}/restore`).then((r) => json<{ restored: boolean }>(r)),
  audit: (q: AuditQuery = {}) =>
    fetch(`/api/audit?${auditParams(q)}`).then((r) => json<{ entries: AuditEntry[]; next: number | null }>(r)),
  auditCsvUrl: (q: AuditQuery = {}) => `/api/audit.csv?${auditParams(q)}`,
  timeline: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/timeline`).then((r) => json<{ events: TimelineEvent[] }>(r)),
  inbox: (workspaceId: string, all = false) =>
    fetch(`/api/inbox?workspace_id=${workspaceId}${all ? "&all=true" : ""}`).then((r) =>
      json<{ items: InboxItem[]; unread: number }>(r),
    ),
  /** `acknowledgeForecast`: approving a start the person saw the cost of. */
  resolveInbox: (key: string, action: string, text?: string, acknowledgeForecast = false) =>
    guarded("POST", "/api/inbox/resolve", { key, action, text, acknowledge_forecast: acknowledgeForecast }).then((r) =>
      json<{ ok: boolean; outcome: unknown }>(r),
    ),
  readInbox: (key: string) => guarded("POST", "/api/inbox/read", { key }).then((r) => json<{ ok: boolean }>(r)),
  snoozeInbox: (key: string, hours: number) =>
    guarded("POST", "/api/inbox/snooze", { key, hours }).then((r) => json<{ ok: boolean; until: string }>(r)),
  createBudget: (body: BudgetBody) => guarded("POST", "/api/budgets", body).then((r) => json<{ id: string }>(r)),
  updateBudget: (id: string, body: BudgetBody) =>
    guarded("PATCH", `/api/budgets/${id}`, body).then((r) => json<{ updated: boolean }>(r)),
  deleteBudget: (id: string) => guarded("DELETE", `/api/budgets/${id}`).then((r) => json<{ deleted: boolean }>(r)),
  overrideBudget: (id: string, body: { usd?: number; tokens?: number; runs?: number; note: string }) =>
    guarded("POST", `/api/budgets/${id}/override`, body).then((r) =>
      json<{ overridden: boolean; released: number }>(r),
    ),
  taskEstimate: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/estimate`).then((r) => json<{ estimate: CostEstimate | null }>(r)),
  estimate: (projectId: string, tier?: string, engine?: string) => {
    const q = new URLSearchParams({ project_id: projectId });
    if (tier) q.set("tier", tier);
    if (engine) q.set("engine", engine);
    return fetch(`/api/estimate?${q}`).then((r) => json<{ estimate: CostEstimate | null }>(r));
  },
  // Probed live rather than cached: `gh auth login` happens in a terminal
  // while Eren is running, and this is what tells you to go and do it.
  github: () => fetch("/api/github").then((r) => json<GitHubStatus>(r)),
  /** Create a GitHub repository for a project that only exists on this disk. */
  publishProject: (
    projectId: string,
    body: { name?: string; visibility: "private" | "public" },
  ) =>
    post(`/api/projects/${projectId}/github/publish`, body).then((r) =>
      json<{ repo: string; url: string }>(r),
    ),

  // workspaces
  workspaces: () =>
    fetch("/api/workspaces").then((r) => json<{ workspaces: Workspace[] }>(r)),
  createWorkspace: (name: string) =>
    post("/api/workspaces", { name }).then((r) => json<{ id: string }>(r)),
  renameWorkspace: (id: string, name: string) =>
    patch(`/api/workspaces/${id}`, { name }).then(json),

  // projects
  /** kind: absent = repos (the original list); "chat" = what a conversation
   *  can be scoped to (repos + spaces); "space" = document spaces only. */
  projects: (workspaceId: string, kind?: "chat" | "space") =>
    fetch(
      `/api/projects?workspace_id=${workspaceId}${kind ? `&kind=${kind}` : ""}`,
    ).then((r) => json<{ projects: Project[] }>(r)),
  /** A space: a managed folder of documents, not a repository. */
  createSpace: (workspaceId: string, name: string) =>
    post("/api/projects/space", { workspace_id: workspaceId, name }).then((r) =>
      json<Project>(r),
    ),
  /** One project, of any kind — the list above shows only repositories. */
  project: (id: string) => fetch(`/api/projects/${id}`).then((r) => json<Project>(r)),
  // Initializes a repository server-side when the folder needs one.
  addProject: (workspaceId: string, path: string) =>
    post("/api/projects", { workspace_id: workspaceId, path }).then((r) =>
      json<{ id: string; name: string; vcs: "git" | "none"; vcsNote: string | null; rules: RulesSeeded | null }>(r),
    ),

  // your rules, written into each new repository project
  rules: () => fetch("/api/rules").then((r) => json<{ text: string; maxChars: number }>(r)),
  saveRules: (text: string) => guarded("PUT", "/api/rules", { text }).then(json),

  // fs browser
  fsList: (path?: string) =>
    fetch(`/api/fs/list${path ? `?path=${encodeURIComponent(path)}` : ""}`).then(
      (r) => json<FsListing>(r),
    ),
  gitInit: (path: string) => post("/api/fs/git-init", { path }).then(json),
  /** Create `name` inside `parent`, so a project can start from nothing. */
  fsMkdir: (parent: string, name: string) =>
    post("/api/fs/mkdir", { parent, name }).then((r) =>
      json<{ path: string; name: string; isGitRepo: boolean }>(r),
    ),

  // settings
  modelSettings: () => fetch("/api/settings/models").then((r) => json<ModelSettings>(r)),
  permissionSettings: () =>
    fetch("/api/settings/permissions").then((r) => json<PermissionSettings>(r)),
  setDefaultPermissionMode: (mode: PermissionMode) =>
    fetch("/api/settings/permissions", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ default_mode: mode }),
    }).then((r) => json<{ defaultMode: PermissionMode }>(r)),
  /** Clear every agent's own preset so they follow the workspace default. */
  applyPermissionsToAgents: () =>
    post("/api/settings/permissions/apply-to-agents").then((r) =>
      json<{ cleared: number }>(r),
    ),
  /** Let agents work in this project without stopping to ask. */
  setProjectFullAuto: (projectId: string, on: boolean) =>
    patch(`/api/projects/${projectId}`, { full_auto_opt_in: on }).then((r) =>
      json<Project>(r),
    ),
  updateProject: (
    projectId: string,
    /** A field left out is left alone; an explicit `null` clears the pin. */
    body: {
      name?: string;
      default_branch?: string;
      full_auto_opt_in?: boolean;
      default_engine?: string | null;
      default_tier?: TierChoice | null;
      default_effort?: Effort | null;
    },
  ) => patch(`/api/projects/${projectId}`, body).then((r) => json<Project>(r)),
  /** Take a project out of Eren. Its folder is not touched. */
  unloadProject: (projectId: string) =>
    fetch(`/api/projects/${projectId}`, { method: "DELETE" }).then((r) =>
      json<{ unloaded: boolean }>(r),
    ),
  /** Install Agent Skills from a registry into a project, and mirror them.
   *  Slow: it fetches a package and then a repository. */
  skillInstall: (projectId: string, reference: string) =>
    post(`/api/projects/${projectId}/skills/install`, { reference }).then((r) =>
      json<SkillInstall>(r),
    ),

  /** Models the local runtimes on this machine have pulled, ready to paste
   *  into an OpenCode model field. Empty when neither is running, which is
   *  not an error. */
  localModels: () =>
    fetch("/api/local-models").then((r) =>
      json<{ models: LocalModel[]; hosts: LocalHosts }>(r),
    ),
  /** Point the probe somewhere else. An empty string resets to the default. */
  setLocalHosts: (body: Partial<LocalHosts>) =>
    put("/api/local-models", body).then((r) =>
      json<{ models: LocalModel[]; hosts: LocalHosts }>(r),
    ),

  skills: (workspaceId: string) =>
    fetch(`/api/skills?workspace_id=${workspaceId}`).then((r) =>
      json<{ skills: Skill[] }>(r),
    ),
  createSkill: (body: {
    workspace_id: string;
    /** Yours, in every workspace you have, rather than only this one's. */
    personal?: boolean;
    name: string;
    description?: string;
    instructions?: string;
    must_not?: string;
  }) => post("/api/skills", body).then((r) => json<Skill>(r)),
  updateSkill: (
    id: string,
    body: {
      name?: string;
      description?: string;
      instructions?: string;
      must_not?: string;
      enabled?: boolean;
    },
  ) => patch(`/api/skills/${id}`, body).then((r) => json<Skill>(r)),
  deleteSkill: (id: string) =>
    fetch(`/api/skills/${id}`, { method: "DELETE" }).then((r) =>
      json<{ deleted: boolean }>(r),
    ),
  /** Run it once against a harmless prompt, with no tools and no repository. */
  trySkill: (id: string, prompt: string) =>
    post(`/api/skills/${id}/try`, { prompt }).then((r) =>
      json<{ output: string; prompt: string }>(r),
    ),
  /** What every run in this project starts with. */
  brain: (projectId: string) =>
    fetch(`/api/projects/${projectId}/brain`).then((r) => json<ProjectBrain>(r)),
  saveBrain: (
    projectId: string,
    body: { body: string; enabled: boolean; hash?: string },
  ) =>
    fetch(`/api/projects/${projectId}/brain`, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }).then((r) => json<ProjectBrain>(r)),
  brainRevisions: (projectId: string) =>
    fetch(`/api/projects/${projectId}/brain/revisions`).then((r) =>
      json<{ revisions: { id: number; body: string; savedAt: string }[] }>(r),
    ),
  /** Everything this project is holding, in one answer. */
  storage: (projectId: string) =>
    fetch(`/api/projects/${projectId}/storage`).then((r) => json<ProjectStorage>(r)),
  /** What this project's finished cards are still holding on disk. */
  worktrees: (projectId: string) =>
    fetch(`/api/projects/${projectId}/worktrees`).then((r) => json<WorktreeHeld>(r)),
  reclaimWorktrees: (projectId: string) =>
    post(`/api/projects/${projectId}/worktrees/reclaim`, {}).then((r) =>
      json<{
        released: { branch: string; bytes: number }[];
        kept: { branch: string; why: string }[];
        bytes: number;
      }>(r),
    ),
  /** What the merge guard is looking at. Read-only. */
  projectCheckout: (projectId: string) =>
    fetch(`/api/projects/${projectId}/checkout`).then((r) => json<CheckoutState>(r)),
  pullCheckout: (projectId: string) =>
    post(`/api/projects/${projectId}/checkout/pull`).then((r) =>
      json<{ pulled: boolean; detail: string }>(r),
    ),
  pushCheckout: (projectId: string) =>
    post(`/api/projects/${projectId}/checkout/push`).then((r) =>
      json<{ pushed: boolean; detail: string }>(r),
    ),
  stashCheckout: (projectId: string) =>
    post(`/api/projects/${projectId}/checkout/stash`, {}).then((r) =>
      json<{ stashed: boolean; undo: string }>(r),
    ),
  /** No message = the merge-unblock button's old wording ("Work in progress"). */
  commitCheckout: (projectId: string, message?: string) =>
    post(`/api/projects/${projectId}/checkout/commit`, message ? { message } : {}).then((r) =>
      json<{ committed: boolean; undo: string }>(r),
    ),
  effortSettings: () =>
    fetch("/api/settings/effort").then((r) => json<EffortSettings>(r)),
  setDefaultEffort: (effort: Effort | null) =>
    fetch("/api/settings/effort", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ default_effort: effort }),
    }).then((r) => json<{ defaultEffort: Effort | null }>(r)),
  setModelSettings: (
    engines: Record<string, Record<Tier, string>>,
    efforts: Record<string, Record<Tier, Effort | null>>,
  ) =>
    fetch("/api/settings/models", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ engines, efforts }),
    }).then((r) => json<{ saved: boolean }>(r)),

  // MCP servers the user connects
  mcpServers: (workspaceId: string) =>
    fetch(`/api/mcp-servers?workspace_id=${workspaceId}`).then((r) =>
      json<{ servers: McpServer[] }>(r),
    ),
  createMcpServer: (body: Record<string, unknown>) =>
    post("/api/mcp-servers", body).then((r) => json<McpServer>(r)),
  updateMcpServer: (id: string, body: Record<string, unknown>) =>
    patch(`/api/mcp-servers/${id}`, body).then((r) => json<McpServer>(r)),
  deleteMcpServer: (id: string) =>
    fetch(`/api/mcp-servers/${id}`, { method: "DELETE" }).then(json),
  /** Connect and ask what tools it offers. Slow by nature — it starts the server. */
  testMcpServer: (id: string) =>
    post(`/api/mcp-servers/${id}/test`).then((r) => json<McpTestResult>(r)),
  agentMcpServers: (agentId: string) =>
    fetch(`/api/agents/${agentId}/mcp-servers`).then((r) =>
      json<{ serverIds: string[] }>(r),
    ),
  setAgentMcpServers: (agentId: string, serverIds: string[]) =>
    fetch(`/api/agents/${agentId}/mcp-servers`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ server_ids: serverIds }),
    }).then((r) => json<{ serverIds: string[] }>(r)),

  // tasks
  tasks: (opts: { workspaceId?: string; projectId?: string }) => {
    const params = new URLSearchParams();
    if (opts.workspaceId) params.set("workspace_id", opts.workspaceId);
    if (opts.projectId) params.set("project_id", opts.projectId);
    return fetch(`/api/tasks?${params}`).then((r) => json<{ tasks: Task[] }>(r));
  },
  createTask: (body: {
    project_id: string;
    title: string;
    prompt: string;
    /** "auto" included — the card stores the choice, not the resolved tier. */
    model_tier: TierChoice;
    start: boolean;
    agent_id?: string | null;
    skill_id?: string | null;
    goal_id?: string | null;
    team_id?: string | null;
    engine?: string;
    plan_first?: boolean;
    effort?: Effort | null;
    article_ids?: string[];
    attachment_ids?: string[];
  }) => post("/api/tasks", body).then((r) => json<{ id: string; runId: string | null }>(r)),
  /** `acknowledgeForecast`: the person saw what it could cost and starts anyway. */
  startTask: (taskId: string, acknowledgeForecast = false) =>
    post(`/api/tasks/${taskId}/start`, { acknowledge_forecast: acknowledgeForecast }).then((r) =>
      json<{ runId: string }>(r),
    ),
  deleteTask: (taskId: string) =>
    fetch(`/api/tasks/${taskId}`, { method: "DELETE" }).then((r) =>
      json<{ deleted: boolean }>(r),
    ),
  /** `fresh` discards the previous attempt's worktree and its unmerged diff. */
  retryTask: (taskId: string, fresh = true) =>
    post(`/api/tasks/${taskId}/retry`, { fresh }).then((r) =>
      json<{ runId: string; fresh: boolean }>(r),
    ),
  /** Continue a run's own session in the worktree it was already working in.
   *  Refusals come back as a 409 whose message says which one. */
  resumeRun: (runId: string) =>
    post(`/api/runs/${runId}/resume`).then((r) =>
      json<{ runId: string; resumedFrom: string }>(r),
    ),
  // Kanban drag: moving into "running" from backlog starts the task.
  moveTask: (
    taskId: string,
    body: {
      board_column?: string;
      position?: number;
      engine?: string;
      plan_first?: boolean;
      /** "auto" included — a card may hand the tier choice back to Eren. */
      model_tier?: TierChoice;
      /**
       * Three states, and JSON gives us all three: omit the key to leave it
       * alone, send null to return the card to inheriting, send a value to pin
       * one. Passing `undefined` omits it, which is what you want.
       */
      effort?: Effort | null;
      /**
       * How this job gets done. Nested the same way — omit to leave it, null to
       * go back to the usual way, an id to pin a skill.
       */
      skill_id?: string | null;
      /** The card's brief. Omit to leave it; empty is refused server-side. */
      prompt?: string;
      /** Start by itself once every blocker has landed. */
      start_when_unblocked?: boolean;
      /** Dropping it into In Progress after seeing the forecast. */
      /** The goal it serves; null clears it. */
      goal_id?: string | null;
      acknowledge_forecast?: boolean;
    },
  ) =>
    patch(`/api/tasks/${taskId}`, body).then((r) =>
      json<{ moved: boolean; runId: string | null }>(r),
    ),
  projectPreviews: (projectId: string) =>
    fetch(`/api/projects/${projectId}/previews`).then((r) =>
      json<{
        previews: ProjectPreview[];
        live: number;
        maxLive: number;
        diskBytes: number;
        reclaimable: number;
      }>(r),
    ),
  /** The project's base branch, running — what a card's changes compare to. */
  basePreview: (projectId: string) =>
    fetch(`/api/projects/${projectId}/preview`).then((r) =>
      json<{ preview: TaskPreview | null }>(r),
    ),
  startBasePreview: (projectId: string) =>
    post(`/api/projects/${projectId}/preview`).then((r) =>
      json<{ preview: TaskPreview }>(r),
    ),
  stopBasePreview: (projectId: string) =>
    fetch(`/api/projects/${projectId}/preview`, { method: "DELETE" }).then((r) =>
      json<{ stopped: boolean }>(r),
    ),
  /** A Dockerfile an agent wrote for a project that has none. */
  previewRecipe: (projectId: string) =>
    fetch(`/api/projects/${projectId}/preview-recipe`).then((r) =>
      json<{ recipe: PreviewRecipe | null }>(r),
    ),
  proposeRecipe: (projectId: string) =>
    post(`/api/projects/${projectId}/preview-recipe`).then((r) =>
      json<{ recipe: PreviewRecipe }>(r),
    ),
  approveRecipe: (projectId: string, dockerfile: string) =>
    fetch(`/api/projects/${projectId}/preview-recipe`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ dockerfile }),
    }).then((r) => json<{ approved: boolean; edited: boolean }>(r)),
  previewLogs: (previewId: string) =>
    fetch(`/api/previews/${previewId}/logs`).then((r) =>
      json<{ build: string; runtime: string }>(r),
    ),
  /**
   * Begin GitHub's device flow.
   *
   * Eren never sees the token: `gh` runs the flow, GitHub hands the
   * credential straight to `gh`, and `gh` stores it. What comes back is a
   * one-time code whose whole purpose is to be shown.
   */
  githubScopes: () =>
    fetch("/api/github/scopes").then((r) =>
      json<{ required: string[]; optional: { name: string; what: string }[] }>(r),
    ),
  connectGitHub: (scopes: string[] = []) =>
    post("/api/github/connect", { scopes }).then((r) => json<GitHubConnect>(r)),
  githubConnectStatus: (id: string) =>
    fetch(`/api/github/connect/${id}`).then((r) =>
      json<GitHubConnectProgress>(r),
    ),
  cancelGitHubConnect: (id: string) =>
    fetch(`/api/github/connect/${id}`, { method: "DELETE" }),
  pullRequest: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/pull-request`).then((r) => json<PullRequestState>(r)),
  openPullRequest: (taskId: string, force = false) =>
    post(`/api/tasks/${taskId}/pull-request`, { force }).then((r) =>
      json<{ pr: TaskPullRequest }>(r),
    ),
  refreshPullRequest: (taskId: string) =>
    post(`/api/tasks/${taskId}/pull-request/refresh`).then((r) =>
      json<{ pr: TaskPullRequest }>(r),
    ),
  cloneRepo: (workspaceId: string, repo: string, parent?: string, name?: string) =>
    post("/api/github/clone", {
      workspace_id: workspaceId,
      repo,
      parent,
      name,
    }).then((r) => json<{ id: string; destination: string }>(r)),
  cloneStatus: (id: string) =>
    fetch(`/api/github/clone/${id}`).then((r) => json<CloneProgress>(r)),
  cancelClone: (id: string) => fetch(`/api/github/clone/${id}`, { method: "DELETE" }),
  githubIssues: (projectId: string) =>
    fetch(`/api/projects/${projectId}/github/issues`).then((r) =>
      json<{
        repo: string | null;
        public?: boolean;
        issues: GitHubIssue[];
        refusal: string | null;
      }>(r),
    ),
  importIssues: (projectId: string, numbers: number[]) =>
    post(`/api/projects/${projectId}/github/issues/import`, { numbers }).then((r) =>
      json<{ imported: { number: number; taskId: string }[]; skipped: number[] }>(r),
    ),
  usage: () =>
    fetch("/api/usage").then((r) =>
      json<{ limits: PlanLimit[]; worst: string | null }>(r),
    ),
  usageHistory: () =>
    fetch("/api/usage/history").then((r) =>
      json<{ days: number; events: UsageEvent[]; patterns: UsagePattern[] }>(r),
    ),
  // Previews
  attentionSettings: () =>
    fetch("/api/settings/attention").then((r) => json<AttentionSettingsValue>(r)),
  /**
   * Carries the write header for the same reason the file editor does: the
   * stored value is a command this machine will run, so a cross-origin page
   * must not be able to set it. Without CORS the preflight gets no
   * `Access-Control-Allow-*` and the browser never sends the real request.
   */
  setAttentionSettings: (v: {
    enabled: boolean;
    command: string;
    events: AttentionEvent[];
    waitSecs: number;
  }) =>
    fetch("/api/settings/attention", {
      method: "PUT",
      headers: { "Content-Type": "application/json", "X-Eren-Write": "1" },
      body: JSON.stringify({
        enabled: v.enabled,
        command: v.command,
        events: v.events,
        wait_secs: v.waitSecs,
      }),
    }).then((r) => json<AttentionSettingsValue>(r)),
  previewLimits: () =>
    fetch("/api/previews/limits").then((r) =>
      json<{ maxLive: number; idleMinutes: number; live: number }>(r),
    ),
  setPreviewLimits: (maxLive: number, idleMinutes: number) =>
    fetch("/api/previews/limits", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ max_live: maxLive, idle_minutes: idleMinutes }),
    }).then((r) => json<{ maxLive: number; idleMinutes: number }>(r)),
  previewDisk: () =>
    fetch("/api/previews/disk").then((r) =>
      json<{ bytes: number; reclaimable: number }>(r),
    ),
  reclaimPreviewDisk: () =>
    fetch("/api/previews/disk", { method: "DELETE" }).then((r) =>
      json<{ reclaimed: number }>(r),
    ),
  dockerStatus: () => fetch("/api/docker").then((r) => json<DockerStatus>(r)),
  taskPreview: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/preview`).then((r) =>
      json<{ preview: TaskPreview | null }>(r),
    ),
  startPreview: (taskId: string) =>
    post(`/api/tasks/${taskId}/preview`).then((r) =>
      json<{ preview: TaskPreview }>(r),
    ),
  stopPreview: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/preview`, { method: "DELETE" }).then((r) =>
      json<{ stopped: boolean }>(r),
    ),
  // Knowledge base
  articles: (workspaceId: string, q?: string) =>
    fetch(
      `/api/kb/articles?workspace_id=${workspaceId}` +
        (q ? `&q=${encodeURIComponent(q)}` : ""),
    ).then((r) => json<{ articles: Article[] }>(r)),
  article: (id: string) => fetch(`/api/kb/articles/${id}`).then((r) => json<Article>(r)),
  createArticle: (body: {
    workspace_id: string;
    title: string;
    content_html?: string;
    status?: string;
    parent_id?: string | null;
    project_id?: string | null;
    icon?: string;
    asset_ids?: string[];
  }) => post("/api/kb/articles", body).then((r) => json<Article>(r)),
  updateArticle: async (
    id: string,
    body: {
      title?: string;
      content_html?: string;
      status?: string;
      icon?: string;
      project_id?: string | null;
      /** What the new revision diffs against. */
      base_seq?: number;
      /** The concurrency guard. Required whenever content_html is sent. */
      base_version?: number;
      asset_ids?: string[];
    },
  ) => {
    const r = await patch(`/api/kb/articles/${id}`, body);
    // A stale editor is a decision for the user, not a failure to swallow.
    if (r.status === 409) throw new ConflictError(await r.text());
    return json<Article>(r);
  },

  // Structure
  kbSpaces: (workspaceId: string) =>
    fetch(`/api/kb/spaces?workspace_id=${workspaceId}`).then((r) =>
      json<{ spaces: Space[] }>(r),
    ),
  kbTree: (workspaceId: string, projectId: string | null) =>
    fetch(
      `/api/kb/tree?workspace_id=${workspaceId}` +
        (projectId ? `&project_id=${projectId}` : ""),
    ).then((r) => json<{ pages: TreePage[] }>(r)),
  movePage: (id: string, parentId: string | null, afterId?: string | null) =>
    post(`/api/kb/articles/${id}/move`, {
      parent_id: parentId,
      after_id: afterId ?? null,
    }).then((r) => json<{ moved: boolean }>(r)),

  // History
  revisions: (id: string) =>
    fetch(`/api/kb/articles/${id}/revisions`).then((r) =>
      json<{ revisions: Revision[] }>(r),
    ),
  revisionDiff: (id: string, to: number, from?: number) =>
    fetch(
      `/api/kb/articles/${id}/diff?to=${to}` + (from !== undefined ? `&from=${from}` : ""),
    ).then((r) => json<RevisionDiff>(r)),
  acceptRevision: (id: string, seq: number) =>
    post(`/api/kb/articles/${id}/revisions/${seq}/accept`).then((r) =>
      json<{ accepted: boolean; seq: number }>(r),
    ),
  discardRevision: (id: string, seq: number, note: string) =>
    post(`/api/kb/articles/${id}/revisions/${seq}/discard`, { note }).then((r) =>
      json<{ discarded: boolean }>(r),
    ),
  restoreRevision: (id: string, seq: number) =>
    post(`/api/kb/articles/${id}/restore`, { seq }).then((r) =>
      json<{ restored: boolean; seq: number }>(r),
    ),
  deleteArticle: (id: string) =>
    fetch(`/api/kb/articles/${id}`, { method: "DELETE" }).then((r) =>
      json<{ deleted: boolean }>(r),
    ),
  /** Ask an agent to write one. Returns the run that will fill it in. */
  generateArticle: (body: {
    workspace_id: string;
    project_id: string;
    brief: string;
    engine?: string;
    parent_id?: string;
  }) =>
    post("/api/kb/generate", body).then((r) =>
      json<{ runId: string; articleId: string | null }>(r),
    ),
  reviseArticle: (
    id: string,
    body: { project_id: string; brief: string; engine?: string },
  ) =>
    post(`/api/kb/articles/${id}/generate`, body).then((r) =>
      json<{ runId: string }>(r),
    ),

  /** Articles tagged onto a card — the agent reads them before it starts. */
  taskArticles: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/articles`).then((r) =>
      json<{ articles: LinkedArticle[] }>(r),
    ),
  setTaskArticles: (taskId: string, articleIds: string[]) =>
    fetch(`/api/tasks/${taskId}/articles`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ article_ids: articleIds }),
    }).then((r) => json<{ linked: number }>(r)),

  // Plan-first cards
  taskPlan: (runId: string) =>
    fetch(`/api/runs/${runId}/plan`).then((r) => json<TaskPlan>(r)),
  saveTaskPlan: (runId: string, content: string) =>
    patch(`/api/runs/${runId}/plan`, { content }).then((r) =>
      json<{ saved: boolean }>(r),
    ),
  approveTaskPlan: (runId: string) =>
    post(`/api/runs/${runId}/plan/approve`).then((r) =>
      json<{ approved: boolean }>(r),
    ),
  reviseTaskPlan: (runId: string, note: string) =>
    post(`/api/runs/${runId}/plan/revise`, { note }).then((r) =>
      json<{ revising: boolean }>(r),
    ),

  taskComments: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/comments`).then((r) =>
      json<{ comments: TaskComment[]; pendingReplies: number }>(r),
    ),
  /** `anchor` attaches the note to a line of the diff; `fix: true` turns it
   *  into a scoped run in the task's existing worktree. */
  postComment: (
    taskId: string,
    content: string,
    engine?: string,
    articleIds?: string[],
    anchor?: { file_path?: string; line?: number; hunk?: string; fix?: boolean },
  ) =>
    post(`/api/tasks/${taskId}/comments`, {
      content,
      engine,
      article_ids: articleIds,
      ...anchor,
    }).then((r) =>
      json<{ id: string; runIds: string[]; fixRunId?: string }>(r),
    ),
  attachToTask: (taskId: string, attachmentIds: string[]) =>
    post(`/api/tasks/${taskId}/attachments/claim`, { attachment_ids: attachmentIds }).then(
      (r) => json<{ attached: number }>(r),
    ),
  diff: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/diff`).then((r) => json<{ diff: string }>(r)),
  /** `force` + `note`: merge past what the review policy still wants. A
   *  refusal by the policy throws `MergeGateError` with what is unmet. */
  merge: (taskId: string, force?: { note: string }) =>
    post(`/api/tasks/${taskId}/merge`, force ? { force: true, note: force.note } : undefined).then(async (r) => {
      if (r.status === 409) {
        const text = await r.text();
        let gate: { kind?: string; unmet?: Unmet[] } | null = null;
        try {
          gate = JSON.parse(text);
        } catch {
          // plain-text refusal
        }
        if (gate?.kind === "gate" && Array.isArray(gate.unmet)) throw new MergeGateError(gate.unmet);
        throw new Error(text);
      }
      return json<{ merged: boolean }>(r);
    }),
  reviewPolicy: (projectId: string) =>
    fetch(`/api/projects/${projectId}/review-policy`).then((r) => json<{ policy: ReviewPolicy }>(r)),
  saveReviewPolicy: (projectId: string, policy: ReviewPolicy) =>
    guarded("PUT", `/api/projects/${projectId}/review-policy`, policy).then((r) => json<{ policy: ReviewPolicy }>(r)),
  cardReviews: (taskId: string) => fetch(`/api/tasks/${taskId}/reviews`).then((r) => json<CardReviews>(r)),
  /** A person asks for a review now — one round past the cap if need be. */
  startReview: (taskId: string) => post(`/api/tasks/${taskId}/reviews`).then((r) => json<{ runId: string }>(r)),
  cancelRun: (runId: string) => post(`/api/runs/${runId}/cancel`),

  /** Hand a card to someone else, or to nobody.
   *
   *  `null` clears the assignment; omitting a field leaves it alone. The two
   *  are different requests, so both ids are always sent explicitly. */
  reassignTask: (taskId: string, assignee: { kind: "agent" | "team"; id: string } | null) =>
    patch(`/api/tasks/${taskId}`, {
      agent_id: assignee?.kind === "agent" ? assignee.id : null,
      team_id: assignee?.kind === "team" ? assignee.id : null,
    }).then(json),

  unattended: () =>
    fetch("/api/settings/unattended").then((r) => json<{ unattended: Unattended; maxSilenceMinutes: number }>(r)),
  saveUnattended: (u: Unattended) =>
    guarded("PUT", "/api/settings/unattended", u).then((r) => json<{ unattended: Unattended }>(r)),
  /** Stop the running agent and hand the work to another, with a note that
   *  is its brief. It continues in the same worktree once the old run ends. */
  handOff: (taskId: string, agentId: string, note: string) =>
    patch(`/api/tasks/${taskId}`, { agent_id: agentId, team_id: null, handoff_note: note }).then((r) =>
      json<{ handingOff: boolean }>(r),
    ),

  // bake-off: one brief, several attempts, keep the best
  startBakeoff: (
    taskId: string,
    variants: { label: string; agent_id?: string; tier?: string; engine?: string }[],
  ) =>
    post(`/api/tasks/${taskId}/bakeoff`, { variants }).then((r) =>
      json<{ runIds: string[] }>(r),
    ),
  projectChecks: (projectId: string) =>
    fetch(`/api/projects/${projectId}/checks`).then((r) => json<ProjectChecks>(r)),
  /** Stores commands this machine will run — hence the write header. */
  saveProjectChecks: (projectId: string, v: ProjectChecks) =>
    fetch(`/api/projects/${projectId}/checks`, {
      method: "PUT",
      headers: { "Content-Type": "application/json", "X-Eren-Write": "1" },
      body: JSON.stringify({
        commands: v.commands,
        timeout_secs: v.timeoutSecs,
        auto_fix_attempts: v.autoFixAttempts,
      }),
    }).then((r) => json<ProjectChecks>(r)),
  taskChecks: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/checks`).then((r) =>
      json<{ configured: boolean; latest: CheckRun | null }>(r),
    ),
  /** A person running the checks is the consent; the header proves a person's page sent it. */
  runChecks: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/checks`, {
      method: "POST",
      headers: { "X-Eren-Write": "1" },
    }).then((r) => json<{ checkRunId: string }>(r)),
  fixChecks: (taskId: string) =>
    post(`/api/tasks/${taskId}/checks/fix`).then((r) => json<{ runId: string }>(r)),
  /** How the card's branch stands against the base. Nulls when it has no worktree. */
  baseStatus: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/base`).then((r) =>
      json<{ base?: string; behind: number | null; merging: string[] | null }>(r),
    ),
  /** Bring the base into the card's branch; a conflict starts an agent run to resolve it. */
  updateFromBase: (taskId: string) =>
    post(`/api/tasks/${taskId}/update-from-base`).then((r) =>
      json<{ outcome: "up_to_date" | "merged" | "conflicted"; files?: string[]; runId?: string }>(r),
    ),
  taskRuns: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/runs`).then((r) => json<{ runs: TaskRun[] }>(r)),
  bakeoff: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/bakeoff`).then((r) =>
      json<{ variants: BakeoffVariant[] }>(r),
    ),
  /** Adopt this variant's worktree as the task's; the others are discarded. */
  keepVariant: (runId: string) =>
    post(`/api/runs/${runId}/keep`).then((r) => json<{ kept: string }>(r)),
  pendingPermissions: (runId: string) =>
    fetch(`/api/runs/${runId}/pending-permissions`).then((r) =>
      json<{ pending: PendingPermission[] }>(r),
    ),
  resolvePermission: (requestId: string, allowed: boolean) =>
    post(`/api/permissions/${requestId}/resolve`, { allowed }),

  // activity
  activity: (workspaceId?: string) =>
    fetch(`/api/activity${workspaceId ? `?workspace_id=${workspaceId}` : ""}`).then((r) =>
      json<Activity>(r),
    ),
  /** Stops the queue handing out new runs. In-flight work is left alone. */
  pauseQueue: (paused: boolean) =>
    post(`/api/queue/${paused ? "pause" : "resume"}`).then((r) =>
      json<{ paused: boolean }>(r),
    ),
  /** Dollars per day; null removes the cap. */
  setBudget: (capUsd: number | null) =>
    guarded("POST", "/api/queue/budget", { cap_usd: capUsd }).then((r) =>
      json<{ capUsd: number | null }>(r),
    ),
  teamEstimate: (teamId: string) =>
    fetch(`/api/teams/${teamId}/estimate`).then((r) => json<TeamEstimate>(r)),

  /**
   * Where the tokens went. Deliberately not folded into `activity()`, which is
   * polled every few seconds — this is fetched when someone opens the page.
   */
  spend: (workspaceId?: string, days = 30) => {
    const q = new URLSearchParams({ days: String(days) });
    if (workspaceId) q.set("workspace_id", workspaceId);
    return fetch(`/api/spend?${q}`).then((r) => json<Spend>(r));
  },

  // agents
  /** The agents you can hand work to — every picker's list. A retired agent
   *  is left out: it takes no new work, and its history names it already. */
  agents: (workspaceId: string) =>
    fetch(`/api/agents?workspace_id=${workspaceId}`)
      .then((r) => json<{ agents: Agent[] }>(r))
      .then((r) => ({ agents: r.agents.filter((a) => a.status !== "retired") })),
  /** Every agent, retired ones included — for the Agents page. */
  allAgents: (workspaceId: string) =>
    fetch(`/api/agents?workspace_id=${workspaceId}`).then((r) =>
      json<{ agents: Agent[] }>(r),
    ),
  pauseAgent: (id: string, body: { reason?: string; stop_now?: boolean }) =>
    post(`/api/agents/${id}/pause`, body).then((r) => json<{ paused: boolean; stopped: number }>(r)),
  resumeAgent: (id: string) =>
    post(`/api/agents/${id}/resume`).then((r) => json<{ resumed: boolean }>(r)),
  retireAgent: (id: string) =>
    post(`/api/agents/${id}/retire`).then((r) => json<{ retired: boolean; stopped: number }>(r)),
  createAgent: (body: Record<string, unknown>) =>
    post("/api/agents", body).then((r) => json<Agent>(r)),
  goals: (workspaceId: string) =>
    fetch(`/api/workspaces/${workspaceId}/goals`).then((r) => json<{ goals: Goal[]; maxDepth: number }>(r)),
  createGoal: (
    workspaceId: string,
    body: { title: string; description?: string; parentId?: string | null; targetDate?: string | null },
  ) => post(`/api/workspaces/${workspaceId}/goals`, body).then((r) => json<{ id: string }>(r)),
  updateGoal: (
    id: string,
    body: Partial<{ title: string; description: string; status: Goal["status"]; parentId: string | null; targetDate: string | null }>,
  ) => patch(`/api/goals/${id}`, body).then((r) => json<{ updated: boolean }>(r)),
  deleteGoal: (id: string) => fetch(`/api/goals/${id}`, { method: "DELETE" }).then((r) => json<{ deleted: boolean }>(r)),
  goal: (id: string) =>
    fetch(`/api/goals/${id}`).then((r) =>
      json<{
        goal: Goal;
        chain: string[];
        cards: { id: string; title: string; column: string; projectId: string; agent: string | null; direct: boolean }[];
        projects: { id: string; name: string }[];
        spendUsd: number;
        runs: number;
      }>(r),
    ),
  agentHeartbeats: (agentId: string) =>
    fetch(`/api/agents/${agentId}/heartbeats`).then((r) => json<{ beats: Beat[] }>(r)),
  workspaceHeartbeats: (workspaceId: string) =>
    fetch(`/api/workspaces/${workspaceId}/heartbeats`).then((r) => json<{ beats: { agent: string; beat: Beat }[] }>(r)),
  orgChart: (workspaceId: string) =>
    fetch(`/api/workspaces/${workspaceId}/org-chart`).then((r) => json<{ nodes: OrgNode[]; maxDepth: number }>(r)),
  updateAgent: (id: string, body: Record<string, unknown>) =>
    patch(`/api/agents/${id}`, body).then((r) => json<Agent>(r)),
  deleteAgent: (id: string) => fetch(`/api/agents/${id}`, { method: "DELETE" }),
  agentMemories: (agentId: string) =>
    fetch(`/api/agents/${agentId}/memories`).then((r) =>
      json<{ memories: AgentMemory[] }>(r),
    ),
  forgetMemory: (id: string) =>
    fetch(`/api/agent-memories/${id}`, { method: "DELETE" }).then((r) =>
      json<{ deleted: boolean }>(r),
    ),
  generateAgents: (description: string, engine?: string, modelTier?: Tier) =>
    post("/api/agents/generate", { description, engine, model_tier: modelTier }).then((r) =>
      json<{ drafts: AgentDraft[] }>(r),
    ),

  // teams
  teams: (workspaceId: string) =>
    fetch(`/api/teams?workspace_id=${workspaceId}`).then((r) => json<{ teams: Team[] }>(r)),
  createTeam: (body: Record<string, unknown>) =>
    post("/api/teams", body).then((r) => json<Team>(r)),
  updateTeam: (id: string, body: Record<string, unknown>) =>
    patch(`/api/teams/${id}`, body).then((r) => json<Team>(r)),
  deleteTeam: (id: string) => fetch(`/api/teams/${id}`, { method: "DELETE" }),

  // workflows
  workflows: (projectId: string) =>
    fetch(`/api/workflows?project_id=${projectId}`).then((r) =>
      json<{ workflows: WorkflowDef[] }>(r),
    ),
  createWorkflow: (projectId: string, sourceYaml: string) =>
    post("/api/workflows", { project_id: projectId, source_yaml: sourceYaml }).then((r) =>
      json<WorkflowDef>(r),
    ),
  updateWorkflow: (id: string, sourceYaml: string) =>
    patch(`/api/workflows/${id}`, { source_yaml: sourceYaml }).then((r) =>
      json<WorkflowDef>(r),
    ),
  setWorkflowEnabled: (id: string, enabled: boolean) =>
    patch(`/api/workflows/${id}`, { enabled }).then((r) => json<WorkflowDef>(r)),
  setWorkflowCatchUp: (id: string, catchUp: "skip" | "run_once") =>
    patch(`/api/workflows/${id}`, { catch_up: catchUp }).then((r) => json<WorkflowDef>(r)),
  deleteWorkflow: (id: string) => fetch(`/api/workflows/${id}`, { method: "DELETE" }),
  saveWorkflowLayout: (id: string, layout: Record<string, { x: number; y: number }>) =>
    post(`/api/workflows/${id}/layout`, layout),
  runWorkflow: (id: string) =>
    post(`/api/workflows/${id}/run`).then((r) => json<{ runId: string }>(r)),
  syncWorkflows: (projectId: string) =>
    post(`/api/projects/${projectId}/workflows/sync`).then((r) =>
      json<{ imported: WorkflowDef[]; errors: { file: string; error: string }[]; note?: string }>(r),
    ),
  workflowRuns: (projectId: string) =>
    fetch(`/api/workflow-runs?project_id=${projectId}`).then((r) =>
      json<{ runs: WorkflowRun[] }>(r),
    ),
  runSteps: (runId: string) =>
    fetch(`/api/runs/${runId}/steps`).then((r) => json<{ steps: RunStep[] }>(r)),
  runTeam: (teamId: string, projectId: string, goal: string) =>
    post(`/api/teams/${teamId}/run`, { project_id: projectId, goal }).then((r) =>
      json<{ runId: string; workflowId: string; steps: number }>(r),
    ),

  // organizations
  runOrg: (teamId: string, projectId: string, goal: string, reviewPlan = false) =>
    post(`/api/teams/${teamId}/run-org`, {
      project_id: projectId,
      goal,
      review_plan: reviewPlan,
    }).then((r) => json<{ runId: string }>(r)),
  // Both answer with the run they just changed, so the caller can render the
  // new state instead of racing its own poll for it.
  approvePlan: (runId: string) =>
    post(`/api/org-runs/${runId}/plan/approve`).then((r) => json<OrgRunDetail>(r)),
  rejectPlan: (runId: string, reason?: string) =>
    post(`/api/org-runs/${runId}/plan/reject`, { reason }).then((r) =>
      json<OrgRunDetail>(r),
    ),
  updateAssignment: (
    runId: string,
    stepId: string,
    body: Partial<{
      title: string;
      brief: string;
      assignee: string;
      done_when: string[];
      position: number;
    }>,
  ) => patch(`/api/org-runs/${runId}/assignments/${stepId}`, body).then(json),
  dropAssignment: (runId: string, stepId: string) =>
    fetch(`/api/org-runs/${runId}/assignments/${stepId}`, { method: "DELETE" }).then(json),
  orgRuns: (opts: { projectId?: string; workspaceId?: string }) => {
    const params = new URLSearchParams();
    if (opts.projectId) params.set("project_id", opts.projectId);
    if (opts.workspaceId) params.set("workspace_id", opts.workspaceId);
    return fetch(`/api/org-runs?${params}`).then((r) =>
      json<{ runs: OrgRunSummary[] }>(r),
    );
  },
  orgRun: (runId: string) =>
    fetch(`/api/org-runs/${runId}`).then((r) => json<OrgRunDetail>(r)),

  // project and worktree files
  files: (tree: Tree, path?: string) =>
    fetch(
      `${treeBase(tree)}/files${path ? `?path=${encodeURIComponent(path)}` : ""}`,
    ).then((r) => json<FileListing>(r)),
  file: (tree: Tree, path: string) =>
    fetch(`${treeBase(tree)}/file?path=${encodeURIComponent(path)}`).then((r) =>
      json<FileContent>(r),
    ),
  /**
   * Save, quoting the hash the file had when it was opened.
   *
   * `baseHash` is not optional in spirit: pass what you were given, or `null`
   * only when creating a file that does not exist. A 409 means the bytes moved
   * underneath you and is thrown as `FileConflictError` rather than flattened
   * into a bare message, because the UI has something useful to do with it.
   */
  saveFile: (tree: Tree, path: string, content: string, baseHash: string | null) =>
    fetch(`${treeBase(tree)}/file`, {
      method: "PUT",
      headers: {
        "Content-Type": "application/json",
        // A header a cross-origin simple request cannot set. There is no CORS
        // layer, so the preflight for it is never answered.
        "X-Eren-Write": "1",
      },
      body: JSON.stringify({ path, content, base_hash: baseHash }),
    }).then(async (r) => {
      if (r.status === 409) {
        const text = await r.text();
        try {
          throw new FileConflictError(JSON.parse(text) as FileConflict);
        } catch (e) {
          if (e instanceof FileConflictError) throw e;
          throw new Error(text);
        }
      }
      return json<{ path: string; size: number; hash: string }>(r);
    }),

  // attachments
  uploadAttachments: (projectId: string, files: File[]) => {
    const form = new FormData();
    for (const f of files) form.append("files", f);
    return postForm(`/api/projects/${projectId}/attachments`, form).then((r) =>
      json<{ attachments: Attachment[] }>(r),
    );
  },
  /** A general chat's uploads: it has no project, so they belong to the workspace. */
  uploadWorkspaceAttachments: (workspaceId: string, files: File[]) => {
    const form = new FormData();
    for (const f of files) form.append("files", f);
    return postForm(`/api/workspaces/${workspaceId}/attachments`, form).then((r) =>
      json<{ attachments: Attachment[] }>(r),
    );
  },
  /** Only works while unclaimed — 409 once a task or message owns it. */
  deleteAttachment: (id: string) =>
    fetch(`/api/attachments/${id}`, { method: "DELETE" }).then((r) =>
      json<{ deleted: boolean }>(r),
    ),
  taskAttachments: (taskId: string) =>
    fetch(`/api/tasks/${taskId}/attachments`).then((r) =>
      json<{ attachments: Attachment[] }>(r),
    ),
  attachmentUrl: (id: string) => `/api/attachments/${id}`,

  /** Recursive filename search, for the @-mention picker. */
  searchFiles: (projectId: string, q: string) =>
    fetch(`/api/projects/${projectId}/files/search?q=${encodeURIComponent(q)}`).then((r) =>
      json<{ files: { path: string; name: string }[]; truncated: boolean }>(r),
    ),

  // global search
  search: (workspaceId: string, q: string) =>
    fetch(`/api/search?workspace_id=${workspaceId}&q=${encodeURIComponent(q)}`).then((r) =>
      json<SearchResults>(r),
    ),

  // space documents
  spaceDocuments: (projectId: string) =>
    fetch(`/api/projects/${projectId}/documents`).then((r) =>
      json<{ documents: SpaceDocument[] }>(r),
    ),
  uploadSpaceDocuments: (projectId: string, files: File[]) => {
    const form = new FormData();
    for (const f of files) form.append("files", f);
    return postForm(`/api/projects/${projectId}/documents`, form).then((r) =>
      json<{ stored: number; documents: SpaceDocument[] }>(r),
    );
  },
  deleteSpaceDocument: (projectId: string, docId: string) =>
    fetch(`/api/projects/${projectId}/documents/${docId}`, { method: "DELETE" }).then(json),
  reindexSpace: (projectId: string) =>
    post(`/api/projects/${projectId}/documents/reindex`).then(json),
  spaceDocsStatus: (projectId: string) =>
    fetch(`/api/projects/${projectId}/documents/status`).then((r) => json<SpaceDocsStatus>(r)),

  // deep research
  /** scope: {projectId} for a project's researches, {workspaceId} for the
   *  workspace's general (project-less) ones. */
  researchList: (scope: { projectId?: string; workspaceId?: string }) =>
    fetch(
      scope.projectId
        ? `/api/research?project_id=${scope.projectId}`
        : `/api/research?workspace_id=${scope.workspaceId}`,
    ).then((r) => json<{ researches: Research[] }>(r)),
  researchCreate: (
    scope: { projectId?: string; workspaceId?: string },
    question: string,
    opts?: { engine?: string; modelTier?: Tier; effort?: Effort | null },
  ) =>
    post("/api/research", {
      project_id: scope.projectId,
      workspace_id: scope.workspaceId,
      question,
      engine: opts?.engine,
      model_tier: opts?.modelTier,
      effort: opts?.effort ?? undefined,
    }).then((r) => json<{ id: string; runId: string }>(r)),
  researchGet: (id: string) =>
    fetch(`/api/research/${id}`).then((r) => json<ResearchDetail>(r)),
  researchRerun: (id: string, engine?: string) =>
    post(`/api/research/${id}/rerun`, engine ? { engine } : undefined).then((r) =>
      json<{ runId: string }>(r),
    ),
  researchCancel: (id: string) => post(`/api/research/${id}/cancel`).then(json),
  researchDelete: (id: string) =>
    fetch(`/api/research/${id}`, { method: "DELETE" }).then(json),
  // Idempotent: the second click returns the article the first one filed.
  researchSaveToKb: (id: string) =>
    post(`/api/research/${id}/save-to-kb`).then((r) =>
      json<{ articleId: string; created: boolean }>(r),
    ),

  // code map
  /** Also the on-open trigger: reading this is what keeps the index honest
   *  about edits made outside Eren. */
  repoIndexStatus: (projectId: string) =>
    fetch(`/api/projects/${projectId}/map/status`).then((r) => json<RepoIndexStatus>(r)),
  /** A strict read — unlike the status call, this never triggers a reconcile. */
  repoGraph: (projectId: string) =>
    fetch(`/api/projects/${projectId}/map/graph`).then((r) => json<RepoGraph>(r)),
  repoFile: (projectId: string, path: string) =>
    fetch(`/api/projects/${projectId}/map/file?path=${encodeURIComponent(path)}`).then((r) =>
      json<RepoFileDetail>(r),
    ),
  /** POST because a question is a body — a natural-language query in a URL
   *  ends up in access logs. */
  repoSearch: (projectId: string, q: string, limit = 12) =>
    post(`/api/projects/${projectId}/map/search`, { q, limit }).then((r) =>
      json<{ hits: RepoSearchHit[]; note?: string }>(r),
    ),
  reindexRepoMap: (projectId: string) =>
    post(`/api/projects/${projectId}/map/reindex`).then((r) =>
      json<{ indexed: number; unchanged: number; failed: number; removed: number; vectorsDeferred: boolean }>(r),
    ),

  // dependencies
  addTaskBlocker: (taskId: string, blockedBy: string) =>
    post(`/api/tasks/${taskId}/blockers`, { blockedBy }).then(json),
  removeTaskBlocker: (taskId: string, blockerId: string) =>
    fetch(`/api/tasks/${taskId}/blockers/${blockerId}`, { method: "DELETE" }).then(json),

  // the project's manager
  manager: (projectId: string) =>
    fetch(`/api/projects/${projectId}/manager`).then((r) =>
      json<{ manager: Manager | null }>(r),
    ),
  managerSave: (projectId: string, body: ManagerDraft) =>
    put(`/api/projects/${projectId}/manager`, body).then(json),
  managerRemove: (projectId: string) =>
    fetch(`/api/projects/${projectId}/manager`, { method: "DELETE" }).then(json),
  managerRunNow: (projectId: string) =>
    post(`/api/projects/${projectId}/manager/run`).then(json),
  managerPasses: (projectId: string) =>
    fetch(`/api/projects/${projectId}/manager/passes`).then((r) =>
      json<{ passes: ManagerPass[] }>(r),
    ),

  // routines
  routines: (workspaceId: string) =>
    fetch(`/api/workspaces/${workspaceId}/routines`).then((r) =>
      json<{ routines: Routine[] }>(r),
    ),
  routineCreate: (workspaceId: string, body: RoutineDraft) =>
    post(`/api/workspaces/${workspaceId}/routines`, body).then((r) =>
      json<{ id: string }>(r),
    ),
  routineUpdate: (id: string, body: Partial<RoutineDraft> & { enabled?: boolean }) =>
    patch(`/api/routines/${id}`, body).then(json),
  routineDelete: (id: string) =>
    fetch(`/api/routines/${id}`, { method: "DELETE" }).then(json),
  routineRunNow: (id: string) => post(`/api/routines/${id}/run`).then(json),
  routineHistory: (id: string) =>
    fetch(`/api/routines/${id}/runs`).then((r) => json<{ runs: RoutineRun[] }>(r)),
  /** Validity + the next three local firings, from the same cron parser that
   *  will fire the routine — not a JS lookalike. */
  routinePreview: (cronExpr: string) =>
    post("/api/routines/preview", { cronExpr }).then((r) =>
      json<{ valid: boolean; next: string[] }>(r),
    ),

  // chat
  openChat: (projectId: string) =>
    post(`/api/projects/${projectId}/chats`).then((r) => json<{ id: string }>(r)),
  // General chats: workspace-scoped, attached to no project.
  openGeneralChat: (workspaceId: string) =>
    post(`/api/workspaces/${workspaceId}/chats`).then((r) => json<{ id: string }>(r)),
  generalChats: (workspaceId: string) =>
    fetch(`/api/workspaces/${workspaceId}/chats`).then((r) =>
      json<{ chats: ChatSummary[] }>(r),
    ),
  /** A new conversation — with the assistant, or with `agentId`. */
  newGeneralChat: (workspaceId: string, agentId?: string) =>
    post(`/api/workspaces/${workspaceId}/chats/new`, agentId ? { agent_id: agentId } : undefined).then((r) =>
      json<{ id: string }>(r),
    ),
  chats: (projectId: string) =>
    fetch(`/api/projects/${projectId}/chats`).then((r) =>
      json<{ chats: ChatSummary[] }>(r),
    ),
  newChat: (projectId: string, agentId?: string) =>
    post(`/api/projects/${projectId}/chats/new`, agentId ? { agent_id: agentId } : undefined).then((r) =>
      json<{ id: string }>(r),
    ),
  renameChat: (chatId: string, title: string) =>
    patch(`/api/chats/${chatId}`, { title }).then(json),
  // Throws on 409 when a turn is still running, so the UI can surface why.
  deleteChat: (chatId: string) =>
    fetch(`/api/chats/${chatId}`, { method: "DELETE" }).then((r) =>
      json<{ deleted: boolean }>(r),
    ),
  chatMessages: (chatId: string) =>
    fetch(`/api/chats/${chatId}/messages`).then((r) =>
      json<{
        messages: ChatMessage[];
        activeRunId: string | null;
        /** The clarifying question waiting for an answer, if any. Read back
         *  each poll rather than held client-side, so it survives a refresh. */
        openQuestion: OpenQuestion | null;
      }>(r),
    ),
  // Options object rather than trailing positionals: two optional args in a
  // row is exactly how a caller silently passes an engine as attachment ids.
  sendChat: (
    chatId: string,
    content: string,
    opts: {
      attachmentIds?: string[];
      /** Knowledge-base pages for this turn. Workspace-scoped, so a general
       *  chat can carry them even though it cannot carry a file. */
      articleIds?: string[];
      engine?: string;
      modelTier?: Tier;
      effort?: Effort | null;
      /** Propose rather than act. Sticks to the chat, like the two above. */
      planMode?: boolean;
      /** One conversation on one model, overriding the tier mapping. Empty
       *  string clears it — undefined leaves it alone. */
      modelId?: string;
    } = {},
  ) =>
    post(`/api/chats/${chatId}/messages`, {
      content,
      engine: opts.engine,
      model_tier: opts.modelTier,
      effort: opts.effort ?? undefined,
      plan_mode: opts.planMode,
      model_id: opts.modelId,
      attachment_ids: opts.attachmentIds ?? [],
      article_ids: opts.articleIds ?? [],
    }).then((r) => json<{ messageId: string; runId: string }>(r)),
  /** Carry out a plan. Leaves plan mode — the next turn is the one that acts.
   *  `plan` only when the person edited it; the session already holds the
   *  version the assistant wrote. */
  approveChatPlan: (chatId: string, messageId: string, plan?: string) =>
    post(`/api/chats/${chatId}/plan/${messageId}/approve`, { plan }).then((r) =>
      json<{ messageId: string; runId: string }>(r),
    ),
  /** Answer a clarifying question. One call: it records the answer and sends
   *  the turn, which must not come apart. `answers` is one list of chosen
   *  labels per question, in the order they were asked. */
  answerQuestion: (chatId: string, questionId: string, answers: string[][]) =>
    post(`/api/chats/${chatId}/questions/${questionId}/answer`, { answers }).then((r) =>
      json<{ messageId: string; runId: string }>(r),
    ),

  // Apps
  apps: (workspaceId?: string) =>
    fetch("/api/apps" + (workspaceId ? `?workspace_id=${workspaceId}` : "")).then((r) =>
      json<{ apps: App[] }>(r),
    ),
  app: (id: string) => fetch(`/api/apps/${id}`).then((r) => json<AppDetail>(r)),
  // Returns the manifest unsaved, with `error` set when it does not parse —
  // the point of a declaration is that a person reads it before it is real.
  generateApp: (description: string, runtime: AppRuntime = "module", engine?: string) =>
    post("/api/apps/generate", { description, runtime, engine }).then((r) =>
      json<{ manifest: string; error: string | null }>(r),
    ),
  installApp: (workspaceId: string, manifest: string, brief = "") =>
    post("/api/apps", { workspace_id: workspaceId, manifest, brief }).then((r) =>
      json<App>(r),
    ),
  setAppManifest: (id: string, manifest: string) =>
    fetch(`/api/apps/${id}/manifest`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ manifest }),
    }).then((r) => json<AppDetail>(r)),
  setAppActive: (id: string, active: boolean) =>
    post(`/api/apps/${id}/active`, { active }).then((r) => json<{ active: boolean }>(r)),
  uninstallApp: (id: string) =>
    fetch(`/api/apps/${id}`, { method: "DELETE" }).then((r) => json<{ ok: boolean }>(r)),

  /** Hand the app to an agent. It lands by itself when the card completes. */
  changeApp: (id: string, brief: string, engine?: string) =>
    post(`/api/apps/${id}/builds`, { brief, engine }).then((r) =>
      json<{ buildId: string; taskId: string; runId: string }>(r),
    ),
  appBuilds: (id: string) =>
    fetch(`/api/apps/${id}/builds`).then((r) => json<{ builds: AppBuild[] }>(r)),
  revertAppBuild: (id: string, buildId: string) =>
    post(`/api/apps/${id}/builds/${buildId}/revert`).then((r) => json<AppDetail>(r)),

  applyAppSchema: (id: string, planId: string) =>
    post(`/api/apps/${id}/schema/apply`, { plan_id: planId }).then((r) =>
      json<{ applied: number }>(r),
    ),
  discardAppSchema: (id: string, planId: string) =>
    post(`/api/apps/${id}/schema/discard`, { plan_id: planId }).then(json),

  // `where` repeats, so the query is built by hand rather than from an object —
  // URLSearchParams keeps every value for a repeated key, a plain object does
  // not, and losing all but the last filter would quietly return wrong rows.
  appRows: (id: string, model: string, q: RowQuery = {}) => {
    const params = new URLSearchParams();
    for (const f of q.where ?? []) params.append("where", f);
    if (q.order) params.set("order", q.order);
    if (q.limit !== undefined) params.set("limit", String(q.limit));
    if (q.offset !== undefined) params.set("offset", String(q.offset));
    const qs = params.toString();
    return fetch(`/api/apps/${id}/data/${model}${qs ? `?${qs}` : ""}`).then((r) =>
      json<{ rows: AppRow[]; total: number }>(r),
    );
  },
  addAppRow: (id: string, model: string, values: AppRow) =>
    post(`/api/apps/${id}/data/${model}`, values).then((r) => json<AppRow>(r)),
  changeAppRow: (id: string, model: string, rowId: string, values: AppRow) =>
    patch(`/api/apps/${id}/data/${model}/${rowId}`, values).then((r) => json<AppRow>(r)),
  removeAppRow: (id: string, model: string, rowId: string) =>
    fetch(`/api/apps/${id}/data/${model}/${rowId}`, { method: "DELETE" }).then(json),

  // A URL rather than a fetch: the browser's own download machinery names the
  // file from Content-Disposition, which a blob built here would not.
  appExportUrl: (id: string, withData: boolean) =>
    `/api/apps/${id}/export${withData ? "?data=true" : ""}`,
  importApp: (workspaceId: string, bundle: string) =>
    post("/api/apps/import", { workspace_id: workspaceId, bundle }).then((r) => json<App>(r)),
  repoApps: (projectId: string) =>
    fetch(`/api/projects/${projectId}/apps`).then((r) => json<{ apps: RepoApp[] }>(r)),
  syncRepoApp: (projectId: string, dir: string) =>
    post(`/api/projects/${projectId}/apps/sync`, { dir }).then((r) => json<App>(r)),

  appContainer: (id: string) =>
    fetch(`/api/apps/${id}/run`).then((r) => json<ContainerState>(r)),
  startAppContainer: (id: string) =>
    post(`/api/apps/${id}/run`).then((r) => json<ContainerState>(r)),
  stopAppContainer: (id: string) =>
    fetch(`/api/apps/${id}/run`, { method: "DELETE" }).then(json),
  appDockerfile: (id: string) =>
    fetch(`/api/apps/${id}/dockerfile`).then((r) =>
      json<{ text: string | null; drifted: boolean; sha: string | null }>(r),
    ),
  approveAppDockerfile: (id: string, sha: string) =>
    post(`/api/apps/${id}/dockerfile`, { sha }).then((r) => json<{ approved: string }>(r)),

  appGrants: (id: string) => fetch(`/api/apps/${id}/grants`).then((r) => json<AppGrants>(r)),
  setAppGrants: (id: string, scopes: string[]) =>
    fetch(`/api/apps/${id}/grants`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ scopes }),
    }).then((r) => json<{ granted: string[] }>(r)),
  // A step needing an ungranted scope comes back as `needsScope` rather than
  // an error: it is something the person is allowed to fix, so the screen
  // offers the grant instead of a complaint.
  runAppAction: (id: string, action: string, model: string, row?: string) =>
    post(`/api/apps/${id}/actions/${action}`, { model, row }).then((r) =>
      json<ActionOutcome>(r),
    ),

  appChart: (id: string, view: string, where: string[] = []) => {
    const params = new URLSearchParams();
    for (const f of where) params.append("where", f);
    const qs = params.toString();
    return fetch(`/api/apps/${id}/chart/${view}${qs ? `?${qs}` : ""}`).then((r) =>
      json<{ buckets: ChartBucket[] }>(r),
    );
  },
};

/**
 * The tier to colour and label a card with.
 *
 * An `auto` card has no tier of its own until it runs, so the last run's
 * resolved tier stands in. Before the first run there is genuinely nothing to
 * show, and Medium is the honest placeholder — it is what the card would get
 * if nothing about it stood out. Callers that need to say "not settled yet"
 * should read `tierIsAuto` rather than inferring it from this.
 */
export function displayTier(t: {
  modelTier: TierChoice;
  tierResolved?: Tier | null;
}): Tier {
  if (t.modelTier !== "auto") return t.modelTier;
  return t.tierResolved ?? "medium";
}

export const tierColor: Record<Tier, string> = {
  easy: "var(--color-tier-easy)",
  medium: "var(--color-tier-medium)",
  complex: "var(--color-tier-complex)",
};

export const tierSoft: Record<Tier, string> = {
  easy: "var(--color-tier-easy-soft)",
  medium: "var(--color-tier-medium-soft)",
  complex: "var(--color-tier-complex-soft)",
};

// Tier → model labels are no longer a constant: the mapping is a user
// setting, so a baked-in label would name a model the run isn't using.
// See `useTierModel` in lib/models.
