import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import { parseForecastAsk } from "../lib/forecast";
import { useParams, useSearchParams } from "react-router-dom";
import { AnimatePresence, motion } from "framer-motion";
import { api, Project, Task } from "../lib/api";
import { useWorkspace } from "../lib/workspace";
import { Board } from "../components/Board";
import { GitSync } from "../components/GitSync";
import { NewTaskModal } from "../components/NewTaskModal";
import { TaskDrawer } from "../components/TaskDrawer";
import { ChatPanel } from "../components/chat/ChatPanel";
import { WorkflowsPanel } from "../components/workflows/WorkflowsPanel";
import { FilesPanel } from "../components/files/FilesPanel";
import { RepoMapPanel } from "../components/map/RepoMapPanel";
// Lazy for the same reason Monaco is: xterm only downloads for people who
// actually open the tab.
const TerminalPanel = lazy(() => import("../components/terminal/TerminalPanel"));
import { OrgRunView } from "../components/orgs/OrgRunView";
import { NARROW, useMediaQuery } from "../lib/useMediaQuery";
import { PreviewsPanel } from "../components/previews/PreviewsPanel";
import { ImportIssuesModal } from "../components/ImportIssuesModal";
import { ProjectSettings } from "../components/ProjectSettings";
import { BrainPanel } from "../components/BrainPanel";
import { StoragePanel } from "../components/StoragePanel";
import { PublishModal } from "../components/PublishModal";
import { gradientFor } from "../components/ui/Surface";
import { ManagerPanel } from "../components/ManagerPanel";
import { Tabs } from "../components/ui/Tabs";
import { Button, IconButton } from "../components/ui/Button";
import { Badge } from "../components/ui/Badge";
import { useCrumbs } from "../lib/crumbs";
import { boardGoal } from "../lib/goals";
import {
  Brain,
  FileCode2,
  CircleDot,
  HardDrive,
  KanbanSquare,
  MessageSquare,
  MonitorPlay,
  Network,
  Plus,
  Settings2,
  SquareTerminal,
  UserCog,
  Workflow,
} from "lucide-react";

const TABS = [
  { key: "board", label: "Board", icon: KanbanSquare },
  { key: "workflows", label: "Workflows", icon: Workflow },
  { key: "files", label: "Files", icon: FileCode2 },
  { key: "map", label: "Map", icon: Network },
  { key: "terminal", label: "Terminal", icon: SquareTerminal },
  { key: "previews", label: "Previews", icon: MonitorPlay },
  { key: "brain", label: "Brain", icon: Brain },
  { key: "manager", label: "Manager", icon: UserCog },
  { key: "storage", label: "Storage", icon: HardDrive },
  // Docked beside the board on a wide screen; below `lg` there is no room for
  // a 380px column, so the chat becomes a tab like the others.
  { key: "chat", label: "Chat", icon: MessageSquare, narrowOnly: true },
] as const;

type Tab = (typeof TABS)[number]["key"];

export default function ProjectPage() {
  const { projectId } = useParams<{ projectId: string }>();
  const { active } = useWorkspace();
  const [project, setProject] = useState<Project | null>(null);
  const [tasks, setTasks] = useState<Task[]>([]);
  // Another project's cards and header must not stay on screen while this
  // one's load. Reset during render, not in an effect: an effect runs after
  // the paint, which is the tick of the old board this exists to prevent.
  const [shownFor, setShownFor] = useState(projectId);
  if (shownFor !== projectId) {
    setShownFor(projectId);
    setTasks([]);
    setProject(null);
  }
  // Only the newest board request lands, and only for the project still open:
  // a poll that left before a switch (or before a move) is older news.
  const openProject = useRef(projectId);
  openProject.current = projectId;
  const boardGen = useRef(0);
  // Narrow the board to the cards serving one goal. Offered only once a card
  // on this board serves one.
  const [goalFilter, setGoalFilter] = useState("");
  const goal = boardGoal(tasks, goalFilter);
  useEffect(() => setGoalFilter(""), [projectId]);
  const [showNew, setShowNew] = useState(false);
  const [showImport, setShowImport] = useState(false);
  const [settings, setSettings] = useState(false);
  const [publishing, setPublishing] = useState(false);
  const [tab, setTab] = useState<Tab>("board");
  // Handed to the Files tab when the Map sends a file there.
  const [filePath, setFilePath] = useState<string | null>(null);
  const [teamRoom, setTeamRoom] = useState<string | null>(null);
  const narrow = useMediaQuery(NARROW);
  useCrumbs(project ? [{ label: project.name }] : [], project?.name ?? "");

  // Which card is open lives in the URL, not in state.
  //
  // That makes a card addressable — the knowledge base links straight to one,
  // and back/forward and a pasted link all work. Deriving the open card from
  // the URL rather than mirroring it into state is deliberate: the board
  // refreshes every 2.5s, and two sources of truth for "what is open" is how you
  // get a drawer that reopens itself after you close it.
  const [params, setParams] = useSearchParams();
  const selected = tasks.find((t) => t.id === params.get("task")) ?? null;
  const openTask = useCallback(
    (task: Task | null) =>
      setParams(
        (prev) => {
          const next = new URLSearchParams(prev);
          if (task) next.set("task", task.id);
          else next.delete("task");
          return next;
        },
        // Opening a card is not a place you should have to press Back out of
        // twice — it replaces the entry rather than stacking one per click.
        { replace: true },
      ),
    [setParams],
  );

  const refresh = useCallback(async () => {
    if (!projectId) return;
    const gen = ++boardGen.current;
    const t = await api.tasks({ projectId });
    if (gen !== boardGen.current || openProject.current !== projectId) return;
    setTasks(t.tasks);
  }, [projectId]);

  const [moveError, setMoveError] = useState<string | null>(null);
  const move = useCallback(
    async (taskId: string, column: Task["boardColumn"], position: number) => {
      // Optimistic: the card lands where it was dropped, then the server
      // refresh either confirms it or puts it back (409 while a run is live).
      setTasks((prev) =>
        prev.map((t) => (t.id === taskId ? { ...t, boardColumn: column, position } : t)),
      );
      try {
        setMoveError(null);
        await api.moveTask(taskId, { board_column: column, position });
      } catch (e) {
        // A budget question needs a choice the board has no room for: say it,
        // and where to make it.
        const ask = parseForecastAsk(String(e));
        setMoveError(ask ? `${ask.message} — open the card to start it anyway` : String(e));
        setTimeout(() => setMoveError(null), 5000);
      }
      refresh().catch(() => {});
    },
    [refresh],
  );

  // Fetched by id, not found in the list: the list filters to `kind='repo'`,
  // so scanning it left an app's own project resolving to null — a page with a
  // header reading "Project" and every setting silently defaulted.
  useEffect(() => {
    if (!projectId) return;
    let stale = false;
    api
      .project(projectId)
      .then((p) => !stale && setProject(p))
      .catch(() => !stale && setProject(null));
    return () => {
      stale = true;
    };
  }, [projectId]);

  useEffect(() => {
    refresh().catch(() => {});
    const interval = setInterval(() => refresh().catch(() => {}), 2500);
    return () => clearInterval(interval);
  }, [refresh]);

  if (!projectId) return null;

  // The chat tab only exists while it has nowhere to dock, so a viewport that
  // widens while it is selected must not leave the page on a dead tab.
  const activeTab: Tab = tab === "chat" && !narrow ? "board" : tab;

  return (
    <div className="grid h-full grid-cols-[minmax(0,1fr)] lg:grid-cols-[380px_minmax(0,1fr)]">
      {!narrow && <ChatPanel projectId={projectId} workspaceId={project?.workspaceId} projectKind={project?.kind} />}

      <div className="flex min-h-0 min-w-0 flex-col">
        <header className="border-b border-border bg-panel">
          {/* Identity on the left — name, then quiet facts — and the project's
              own controls on the right. The breadcrumb above already says
              "Projects", so this does not repeat it. */}
          <div className="flex items-center gap-3 px-4 pt-3 lg:px-5">
            <span
              className="size-7 shrink-0 rounded-md"
              style={{ background: gradientFor(project?.name ?? "Project") }}
              aria-hidden
            />
            <div className="min-w-0 flex-1">
              <div className="truncate text-[15px] font-semibold leading-tight">{project?.name ?? "Project"}</div>
              <div className="mt-0.5 flex flex-wrap items-center gap-x-2.5 gap-y-1 text-xs text-fg-muted">
                {project?.githubRepo && (
                  <a
                    href={`https://github.com/${project.githubRepo}`}
                    target="_blank"
                    rel="noreferrer"
                    title="Open this repository on GitHub"
                    className="truncate font-mono text-[11px] hover:text-fg"
                  >
                    {project.githubRepo}
                  </a>
                )}
                {project?.vcs === "none" && (
                  <Badge tone="warning" title={project.vcsNote ?? undefined}>
                    edits in place
                  </Badge>
                )}
                {project?.vcs === "git" && !project.githubRepo && (
                  <button
                    onClick={() => setPublishing(true)}
                    title="Create a GitHub repository for this project"
                    className="hover:text-accent-fg"
                  >
                    Publish to GitHub
                  </button>
                )}
                {project?.vcs === "git" && <GitSync projectId={project.id} onOpenFiles={() => setTab("files")} />}
              </div>
            </div>
            <div className="flex shrink-0 items-center gap-1.5">
              {project && <AutonomyToggle project={project} onChanged={setProject} />}
              {project && (
                <IconButton label="Project settings" onClick={() => setSettings(true)}>
                  <Settings2 className="size-4" />
                </IconButton>
              )}
            </div>
          </div>

          <div className="mt-2 flex items-end gap-3 px-2 lg:px-3">
            <Tabs<Tab>
              className="min-w-0 flex-1"
              listClassName="border-b-0"
              value={activeTab}
              onValueChange={setTab}
              tabs={TABS.filter((t) => narrow || !("narrowOnly" in t)).map((t) => ({
                value: t.key,
                label: (
                  <>
                    <t.icon className="size-3.5" />
                    {t.label}
                  </>
                ),
              }))}
            />
            {activeTab === "board" && (
              <div className="mb-1.5 flex shrink-0 items-center gap-2 pr-1">
                {/* Only when the project actually is a GitHub repository — a
                    button that could only refuse is worse than no button. */}
                {project?.githubRepo && (
                  <Button size="sm" variant="secondary" icon={<CircleDot className="size-3.5" />} onClick={() => setShowImport(true)}>
                    Import issues
                  </Button>
                )}
                <Button size="sm" variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setShowNew(true)}>
                  New card
                </Button>
              </div>
            )}
          </div>
        </header>

        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          {/* mode="wait" so the leaving panel fades before the next enters —
              two boards cross-fading on top of each other is not a transition,
              it is a glitch. Kept to 150ms: this must never make the tabs feel
              slower than they were. */}
          <AnimatePresence mode="wait" initial={false}>
          <motion.div
            key={activeTab}
            initial={{ opacity: 0, y: 6 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -4 }}
            transition={{ duration: 0.15, ease: "easeOut" }}
            className="flex min-h-0 min-w-0 flex-1 flex-col"
          >
          {activeTab === "board" && (
            <>
              {moveError && (
                <div className="mx-4 mt-3 rounded-md border border-[color-mix(in_oklab,var(--color-danger)_25%,transparent)] bg-danger-subtle px-3 py-1.5 text-xs text-danger-fg lg:mx-5">
                  {moveError}
                </div>
              )}
              <GoalFilter tasks={tasks} value={goal} onChange={setGoalFilter} />
              <Board
                tasks={goal ? tasks.filter((t) => t.goalId === goal) : tasks}
                onSelect={openTask}
                onMove={move}
              />
            </>
          )}
          {activeTab === "workflows" && <WorkflowsPanel projectId={projectId} />}
          {activeTab === "files" && (
            <FilesPanel projectId={projectId} tasks={tasks} initialPath={filePath} />
          )}
          {activeTab === "map" && (
            <RepoMapPanel
              projectId={projectId}
              onOpenFile={(path) => {
                setFilePath(path);
                setTab("files");
              }}
            />
          )}
          {activeTab === "terminal" && (
            <Suspense
              fallback={
                <div className="flex h-full items-center justify-center bg-panel text-xs text-fg-muted">
                  Loading terminal…
                </div>
              }
            >
              <TerminalPanel projectId={projectId} />
            </Suspense>
          )}
          {activeTab === "previews" && <PreviewsPanel projectId={projectId} />}
          {activeTab === "brain" && <BrainPanel projectId={projectId} />}
          {activeTab === "manager" && (
            <ManagerPanel projectId={projectId} workspaceId={project?.workspaceId} />
          )}
          {activeTab === "storage" && <StoragePanel projectId={projectId} />}
          {activeTab === "chat" && (
            <ChatPanel projectId={projectId} workspaceId={project?.workspaceId} projectKind={project?.kind} />
          )}
          </motion.div>
          </AnimatePresence>
        </div>
      </div>

      {/* Keyed because AnimatePresence tracks children by key, and these three
          conditional siblings appear and disappear independently — two can be
          on screen at once. Unkeyed, it has only child order to go on.

          The drawer's key is constant rather than the task id, so opening a
          different card is a prop change that keeps the panel's scroll and tab
          where they were, instead of tearing it down and sliding a new one in. */}
      <AnimatePresence>
        {settings && project && (
          <ProjectSettings
            key="project-settings"
            project={project}
            onChanged={setProject}
            onClose={() => setSettings(false)}
          />
        )}
        {publishing && project && (
          <PublishModal
            key="publish"
            projectId={project.id}
            onClose={() => setPublishing(false)}
            onDone={() => {
              setPublishing(false);
              api.project(project.id).then(setProject).catch(() => {});
            }}
          />
        )}
        {showImport && project && (
          <ImportIssuesModal
            key="import-issues"
            projectId={project.id}
            onClose={() => setShowImport(false)}
            onImported={() => {
              setShowImport(false);
              refresh();
            }}
          />
        )}
        {showNew && project && (
          <NewTaskModal
            key="new-task"
            project={project}
            onClose={() => setShowNew(false)}
            onCreated={() => {
              setShowNew(false);
              refresh();
            }}
          />
        )}
        {selected && (
          <TaskDrawer
            onOpenPreviews={() => setTab("previews")}
            key="task-drawer"
            task={selected}
            workspaceId={project?.workspaceId ?? ""}
            onClose={() => openTask(null)}
            onChanged={refresh}
            onOpenTeamRoom={setTeamRoom}
            boardTasks={tasks}
            onOpenTask={openTask}
          />
        )}
        {teamRoom && (
          <OrgRunView key="team-room" runId={teamRoom} onClose={() => setTeamRoom(null)} />
        )}
      </AnimatePresence>
    </div>
  );
}

/**
 * Whether agents may work in this project without stopping to ask.
 *
 * The orchestrator has always consulted `full_auto_opt_in` before honouring
 * a no-prompts run, but nothing could ever set it — so every run asked about
 * everything and there was no way to say "just get on with it". This is that
 * switch.
 *
 * Only offered for git projects: the reason skipping prompts is reasonable at
 * all is that the work happens in an isolated worktree you read as a diff
 * before it touches your branch. A project that edits in place has neither.
 */
function AutonomyToggle({
  project,
  onChanged,
}: {
  project: Project;
  onChanged: (p: Project) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (project.vcs !== "git") return null;

  const toggle = async () => {
    setBusy(true);
    setError(null);
    try {
      onChanged(await api.setProjectFullAuto(project.id, !project.fullAutoOptIn));
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  };

  return (
    <button
      onClick={toggle}
      disabled={busy}
      title={
        error ??
        (project.fullAutoOptIn
          ? "Agents may work straight through here — unless the agent on a card carries its own permission setting, which wins. Each card shows which applies. Click to make them ask again."
          : "Agents stop to ask before edits and commands. Click to let them work uninterrupted — the run stays in an isolated worktree you review.")
      }
      className={`ring-focus inline-flex h-6 items-center gap-1.5 rounded-md border px-2 text-[11px] font-medium transition-colors disabled:opacity-50 ${
        project.fullAutoOptIn
          ? "border-[color-mix(in_oklab,var(--color-success)_30%,transparent)] bg-success-subtle text-success-fg"
          : "border-border bg-panel text-fg-muted hover:text-fg"
      }`}
    >
      <span className={`size-1.5 rounded-full ${project.fullAutoOptIn ? "bg-success" : "bg-fg-subtle"}`} />
      {/* "allows" rather than "works": this unlocks working without asking, it
          does not guarantee it. An agent's own preset outranks the project, so
          the unqualified promise this used to make was one it could not keep. */}
      {project.fullAutoOptIn ? "Allows working without asking" : "Asks before acting"}
    </button>
  );
}

/** "Serving: [goal]" above the board, when any card here serves a goal. */
function GoalFilter({ tasks, value, onChange }: { tasks: Task[]; value: string; onChange: (v: string) => void }) {
  const goals = new Map<string, string>();
  for (const t of tasks) if (t.goalId && t.goalTitle) goals.set(t.goalId, t.goalTitle);
  if (goals.size === 0) return null;
  return (
    <div className="flex items-center gap-2 px-4 pt-3 text-xs text-fg-muted lg:px-5">
      Serving
      <select
        aria-label="Filter by goal"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="ring-focus h-7 rounded-md border border-border bg-panel px-2 text-xs text-fg"
      >
        <option value="">any goal</option>
        {[...goals].map(([id, title]) => (
          <option key={id} value={id}>
            {title}
          </option>
        ))}
      </select>
    </div>
  );
}
