import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router-dom";
import { motion } from "framer-motion";
import { api, Effort, Research, ResearchDetail as Detail, Project, Tier } from "../lib/api";
import { TierPicker } from "../components/TierPicker";
import { EffortPicker } from "../components/EffortPicker";
import { useWorkspace } from "../lib/workspace";
import { EnginePicker } from "../lib/engines";
import { NARROW, useMediaQuery } from "../lib/useMediaQuery";
import { useRunStream, StreamEvent } from "../lib/ws";
import { Markdown } from "../components/Markdown";
import { isActive } from "../lib/runStatus";
import { Button, buttonClasses } from "../components/ui/Button";
import { Select } from "../components/ui/Field";
import { toolName } from "../lib/brand";
import { readStored, writeStored } from "../lib/storage";
import { safeHttpUrl } from "../lib/url";

/**
 * Deep research: ask a question about a project, watch the investigation,
 * read the cited report — and file it into the knowledge base when it is
 * worth keeping.
 *
 * The rail lists past researches for the picked project; the main pane is
 * either the composer (nothing selected), the live run, or the report. One
 * page for both routes — `/research` and `/research/:researchId` — so a
 * report is linkable.
 */
const PROJECT_KEY = "eren.research.project";
/** The picker value for a research attached to no project: web-only. */
const GENERAL = "general";

export default function ResearchPage() {
  const { active } = useWorkspace();
  const narrow = useMediaQuery(NARROW);
  const navigate = useNavigate();
  const { researchId } = useParams();
  const [params, setParams] = useSearchParams();

  const [projects, setProjects] = useState<Project[]>([]);
  const [projectId, setProjectId] = useState<string | null>(null);
  const [list, setList] = useState<Research[]>([]);
  const [question, setQuestion] = useState("");
  const [engine, setEngine] = useState<string | null>(null);
  // Complex is the research default — the thinking-heavy kind of run — and
  // shown as such rather than hidden behind a hardcode.
  const [tier, setTier] = useState<Tier>("complex");
  const [effort, setEffort] = useState<Effort | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [railOpen, setRailOpen] = useState(false);

  const workspaceId = active?.id ?? null;
  useEffect(() => {
    if (!workspaceId) return;
    api
      .projects(workspaceId)
      .then((r) => {
        setProjects(r.projects);
        const fromUrl = params.get("project");
        const remembered = readStored(PROJECT_KEY);
        // General is the default: a question does not need a repository, and
        // a general research answers it from the web alone.
        const pick =
          (fromUrl === GENERAL ? GENERAL : r.projects.find((p) => p.id === fromUrl)?.id) ??
          (remembered === GENERAL ? GENERAL : r.projects.find((p) => p.id === remembered)?.id) ??
          GENERAL;
        setProjectId(pick);
      })
      .catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspaceId]);

  const general = projectId === GENERAL;
  const refreshList = useCallback(() => {
    if (!projectId) return Promise.resolve();
    const scope =
      projectId === GENERAL
        ? workspaceId
          ? { workspaceId }
          : null
        : { projectId };
    if (!scope) return Promise.resolve();
    return api
      .researchList(scope)
      .then((r) => setList(r.researches))
      .catch(() => {});
  }, [projectId, workspaceId]);

  useEffect(() => {
    refreshList();
  }, [refreshList]);

  const pickProject = (id: string) => {
    setProjectId(id);
    writeStored(PROJECT_KEY, id);
    setParams({ project: id }, { replace: true });
    navigate(`/research?project=${id}`);
  };

  const start = async () => {
    if (!projectId || !question.trim() || busy) return;
    setBusy(true);
    setError(null);
    try {
      const scope = general ? { workspaceId: workspaceId! } : { projectId };
      const r = await api.researchCreate(scope, question.trim(), {
        engine: engine ?? undefined,
        modelTier: tier,
        effort,
      });
      setQuestion("");
      refreshList();
      navigate(`/research/${r.id}`);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const rail = (
    <div className="flex min-h-0 flex-col gap-3 p-3">
      <Select
        value={projectId ?? GENERAL}
        onChange={(e) => pickProject(e.target.value)}
        title="General researches the web alone; pick a project to ground the answer in its repository"
      >
        <option value={GENERAL}>General — web only</option>
        {projects.map((p) => (
          <option key={p.id} value={p.id}>
            {p.name}
          </option>
        ))}
      </Select>
      <Link
        to={projectId ? `/research?project=${projectId}` : "/research"}
        onClick={() => setRailOpen(false)}
        className={buttonClasses({ variant: "secondary", size: "md" })}
      >
        + New research
      </Link>
      <div className="min-h-0 flex-1 overflow-y-auto">
        {list.length === 0 && (
          <div className="px-2 py-2 text-xs text-fg-muted">Nothing researched yet.</div>
        )}
        {list.map((r) => (
          <Link
            key={r.id}
            to={`/research/${r.id}`}
            onClick={() => setRailOpen(false)}
            className={`block rounded-lg px-2 py-1.5 text-sm ${
              r.id === researchId ? "bg-panel-2 font-medium" : "hover:bg-panel-2"
            }`}
          >
            <span className="flex items-center gap-1.5">
              <StatusDot status={r.runStatus} hasReport={r.hasReport} />
              <span className="min-w-0 flex-1 truncate">{r.title || r.question}</span>
            </span>
          </Link>
        ))}
      </div>
    </div>
  );

  const main = researchId ? (
    <ResearchView
      id={researchId}
      onChanged={refreshList}
      // A deep link opens a research whose project the rail has never heard
      // of — sync the rail to it, or the list beside the report shows some
      // other project's work.
      onProject={(pid) => {
        const target = pid ?? GENERAL;
        if (target !== projectId) {
          setProjectId(target);
          writeStored(PROJECT_KEY, target);
        }
      }}
    />
  ) : (
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl px-5 py-10">
        <h1 className="text-[26px] font-bold leading-tight tracking-tight">Deep research</h1>
        <p className="mt-1.5 text-sm leading-relaxed text-fg-muted">
          {general
            ? "Ask anything. The agent searches the web, reads the sources, and writes a report that cites every claim."
            : "Ask a question about this project. The agent reads the repository first, then the web, and writes a report that cites both — every web claim with its URL, every repo claim with its file."}
        </p>
        {error && (
          <div className="mt-4 rounded-lg bg-danger-subtle px-3 py-1.5 text-xs text-danger-fg">{error}</div>
        )}
        <div className="mt-6 flex flex-col gap-2 rounded-xl border border-border bg-panel p-3 focus-within:border-accent">
          <textarea
            value={question}
            onChange={(e) => setQuestion(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                start();
              }
            }}
            rows={3}
            placeholder="e.g. What test framework does this repo use, and is it the current recommended one?"
            className="min-w-0 resize-none bg-transparent text-sm outline-none"
          />
          <div className="flex flex-wrap items-center gap-2">
            {/* Which CLI, which model, how hard it thinks — the same three
                knobs chat has, because a research is one expensive turn and
                the person paying for it should choose its weight. */}
            <EnginePicker value={engine} onChange={setEngine} inheritLabel="Default engine" />
            <TierPicker value={tier} onChange={setTier} engine={engine ?? undefined} />
            <EffortPicker value={effort} onChange={setEffort} />
            <Button
              variant="primary"
              onClick={start}
              disabled={busy || !question.trim()}
              className="ml-auto"
            >
              {busy ? "Starting…" : "Research"}
            </Button>
          </div>
        </div>
      </div>
    </div>
  );

  if (narrow) {
    return (
      <div className="flex h-full min-h-0 flex-col">
        <button
          onClick={() => setRailOpen((o) => !o)}
          className="border-b border-border px-4 py-2 text-left text-sm font-medium"
        >
          Research <span className="text-[10px] text-fg-muted">{railOpen ? "▴" : "▾"}</span>
        </button>
        {railOpen && <div className="max-h-64 overflow-y-auto border-b border-border">{rail}</div>}
        {main}
      </div>
    );
  }

  return (
    <div className="grid h-full min-h-0 grid-cols-[280px_minmax(0,1fr)]">
      <div className="min-h-0 overflow-hidden border-r border-border bg-panel">{rail}</div>
      {main}
    </div>
  );
}

function StatusDot({ status, hasReport }: { status: string | null; hasReport: boolean }) {
  const color = isActive(status)
    ? "bg-tier-medium animate-pulse"
    : hasReport
      ? "bg-tier-easy"
      : "bg-danger";
  return <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${color}`} />;
}

/** One research: the live run while it works, the report when it is done. */
function ResearchView({
  id,
  onChanged,
  onProject,
}: {
  id: string;
  onChanged: () => void;
  onProject: (projectId: string | null) => void;
}) {
  const navigate = useNavigate();
  const [detail, setDetail] = useState<Detail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const live = detail != null && isActive(detail.runStatus);
  const events = useRunStream(live ? detail.runId : null);

  const load = useCallback(() => {
    api
      .researchGet(id)
      .then((d) => {
        setDetail(d);
        onProject(d.projectId);
      })
      .catch((e) => setError(String(e)));
    // onProject is a fresh closure every render; the id is what matters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id]);

  useEffect(() => {
    setDetail(null);
    setError(null);
    load();
  }, [load]);

  // While the run is live, poll for the finished report; the stream shows
  // progress but the report row is what carries the result.
  useEffect(() => {
    if (!live) return;
    const t = setInterval(load, 3000);
    return () => clearInterval(t);
  }, [live, load]);

  const saveToKb = async () => {
    setSaving(true);
    setError(null);
    try {
      await api.researchSaveToKb(id);
      load();
      onChanged();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const rerun = async () => {
    setError(null);
    try {
      await api.researchRerun(id);
      load();
      onChanged();
    } catch (e) {
      setError(String(e));
    }
  };

  const cancel = async () => {
    try {
      await api.researchCancel(id);
      load();
      onChanged();
    } catch (e) {
      setError(String(e));
    }
  };

  const remove = async () => {
    try {
      await api.researchDelete(id);
      onChanged();
      navigate("/research");
    } catch (e) {
      setError(String(e));
    }
  };

  if (!detail) {
    return (
      <div className="flex min-h-0 flex-1 items-center justify-center text-sm text-fg-muted">
        {error ?? "Loading…"}
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl px-5 py-8">
        <div className="text-xs text-fg-muted">Research</div>
        <h1 className="mt-0.5 text-xl font-bold leading-tight tracking-tight">
          {detail.title || detail.question}
        </h1>
        {detail.title && (
          <p className="mt-1 text-sm text-fg-muted">“{detail.question}”</p>
        )}

        <div className="mt-3 flex flex-wrap items-center gap-2">
          {live ? (
            <Button variant="secondary" size="sm" onClick={cancel}>
              Cancel
            </Button>
          ) : (
            <>
              {detail.reportMd &&
                (detail.kbArticleId ? (
                  <Link
                    to={`/knowledge/${detail.kbArticleId}`}
                    className="rounded-lg bg-tier-easy px-3 py-1.5 text-xs font-medium text-bg"
                  >
                    Open in knowledge base →
                  </Link>
                ) : (
                  <Button variant="primary" size="sm" onClick={saveToKb} disabled={saving}>
                    {saving ? "Saving…" : "Save to knowledge base"}
                  </Button>
                ))}
              <Button
                variant="secondary"
                size="sm"
                onClick={rerun}
                title="Ask again — the report is replaced by the new answer"
              >
                ↻ Re-run
              </Button>
              <Button variant="danger" size="sm" onClick={remove} className="ml-auto">
                Delete
              </Button>
            </>
          )}
        </div>

        {error && (
          <div className="mt-3 rounded-lg bg-danger-subtle px-3 py-1.5 text-xs text-danger-fg">{error}</div>
        )}
        {!live && !detail.reportMd && detail.runError && (
          <div className="mt-3 rounded-lg bg-danger-subtle px-3 py-2 text-xs text-danger-fg">
            The run stopped: {detail.runError}
          </div>
        )}

        {live && <LiveInvestigation events={events} startedAt={detail.createdAt} />}

        {!live && detail.reportMd && <ReportView detail={detail} />}
      </div>
    </div>
  );
}


// ── The live investigation ──────────────────────────────────────────────────

/** What the agent is doing right now, told from its own tool calls. */
function phaseOf(events: StreamEvent[]): { label: string; icon: string } {
  for (let i = events.length - 1; i >= 0; i--) {
    const e = events[i];
    if (e.type === "assistant_text") return { label: "Writing the report", icon: "✍️" };
    if (e.type === "tool_call") {
      const t = toolName(String(e.tool_name ?? ""));
      if (t === "WebSearch") return { label: "Searching the web", icon: "🔎" };
      if (t === "WebFetch") return { label: "Reading sources", icon: "📖" };
      if (t === "mcp__eren__search_documents")
        return { label: "Searching the documents", icon: "🗂" };
      if (["Read", "Grep", "Glob"].includes(t))
        return { label: "Reading the repository", icon: "📁" };
    }
  }
  return { label: "Getting started", icon: "🧭" };
}

function hostOf(url: string): string {
  try {
    return new URL(url).hostname.replace(/^www\./, "");
  } catch {
    return url.slice(0, 40);
  }
}

/**
 * The run as an investigation, not a log: a phase line, the searches and
 * sources as chips (sources are links — the person can read along), running
 * counters, and the report streaming in as it is written.
 */
function LiveInvestigation({
  events,
  startedAt,
}: {
  events: StreamEvent[];
  startedAt: string;
}) {
  // A ticking clock reads as "alive" in a way a static panel never does.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  const elapsed = Math.max(0, Math.floor((now - new Date(startedAt).getTime()) / 1000));
  const mm = Math.floor(elapsed / 60);
  const ss = String(elapsed % 60).padStart(2, "0");

  const tools = events.filter((e) => e.type === "tool_call");
  const searches = tools.filter((e) => e.tool_name === "WebSearch");
  const sources = tools.filter((e) => e.tool_name === "WebFetch");
  const files = tools.filter((e) => ["Read", "Grep", "Glob"].includes(String(e.tool_name)));
  const liveText = events
    .filter((e) => e.type === "assistant_text")
    .map((e) => String(e.text))
    .join("\n");
  const phase = phaseOf(events);

  return (
    <div className="mt-5 flex flex-col gap-3">
      {/* The phase line: one sentence, a pulse, and the clock. */}
      <div className="flex items-center gap-2 rounded-xl border border-border bg-panel px-3 py-2">
        <motion.span
          animate={{ opacity: [0.4, 1, 0.4] }}
          transition={{ repeat: Infinity, duration: 1.6 }}
          className="text-base"
        >
          {phase.icon}
        </motion.span>
        <span className="text-sm font-medium">{phase.label}…</span>
        <span className="ml-auto font-mono text-xs text-fg-muted">
          {mm}:{ss}
        </span>
      </div>

      {/* Counters, only once there is something to count. */}
      {tools.length > 0 && (
        <div className="flex flex-wrap gap-2 text-[11px] text-fg-muted">
          {searches.length > 0 && (
            <span className="rounded-full bg-panel-2 px-2 py-0.5">
              {searches.length} {searches.length === 1 ? "search" : "searches"}
            </span>
          )}
          {sources.length > 0 && (
            <span className="rounded-full bg-panel-2 px-2 py-0.5">
              {sources.length} {sources.length === 1 ? "source" : "sources"} read
            </span>
          )}
          {files.length > 0 && (
            <span className="rounded-full bg-panel-2 px-2 py-0.5">
              {files.length} repo {files.length === 1 ? "lookup" : "lookups"}
            </span>
          )}
        </div>
      )}

      {/* The trail: searches as quoted chips, sources as clickable domains. */}
      <div className="flex flex-col gap-1.5">
        {tools.slice(-12).map((e, i) => {
          const t = toolName(String(e.tool_name ?? ""));
          const args = (e.input ?? {}) as Record<string, unknown>;
          if (t === "WebSearch")
            return (
              <motion.div
                key={`${e.seq}-${i}`}
                initial={{ opacity: 0, x: -6 }}
                animate={{ opacity: 1, x: 0 }}
                className="self-start rounded-full border border-border bg-panel px-3 py-1 text-xs"
              >
                🔎 “{String(args.query ?? "")}”
              </motion.div>
            );
          if (t === "WebFetch") {
            const url = String(args.url ?? "");
            return (
              <motion.a
                key={`${e.seq}-${i}`}
                // The model chose this URL; a `javascript:` one is a click
                // away from running on this origin.
                href={safeHttpUrl(url)}
                target="_blank"
                rel="noreferrer"
                initial={{ opacity: 0, x: -6 }}
                animate={{ opacity: 1, x: 0 }}
                className="self-start rounded-full border border-border bg-panel px-3 py-1 text-xs text-accent-fg hover:underline"
              >
                📖 {hostOf(url)}
              </motion.a>
            );
          }
          if (["Read", "Grep", "Glob"].includes(t)) {
            const label =
              t === "Read"
                ? String(args.file_path ?? "a file").split("/").pop()
                : t === "Grep"
                  ? `grep “${String(args.pattern ?? "")}”`
                  : "listing files";
            return (
              <motion.div
                key={`${e.seq}-${i}`}
                initial={{ opacity: 0, x: -6 }}
                animate={{ opacity: 1, x: 0 }}
                className="self-start rounded-full border border-border bg-panel px-3 py-1 font-mono text-[11px] text-fg-muted"
              >
                📁 {label}
              </motion.div>
            );
          }
          return null;
        })}
      </div>

      {/* The report, streaming in. */}
      {liveText && (
        <div className="rounded-xl border border-border bg-panel p-4">
          <div className="mb-2 text-[11px] font-medium uppercase tracking-wide text-fg-muted">
            Draft
          </div>
          <div className="md">
            <Markdown>{liveText}</Markdown>
          </div>
        </div>
      )}
    </div>
  );
}

// ── The finished report ─────────────────────────────────────────────────────

/** Markdown links, deduped by host — the report's bibliography as chips. */
function sourcesOf(md: string): { host: string; url: string }[] {
  const seen = new Map<string, string>();
  for (const m of md.matchAll(/\]\((https?:\/\/[^)\s]+)\)/g)) {
    const host = hostOf(m[1]);
    if (!seen.has(host)) seen.set(host, m[1]);
  }
  return [...seen.entries()].map(([host, url]) => ({ host, url }));
}

function headingsOf(md: string): string[] {
  return md
    .split("\n")
    .filter((l) => /^##\s+/.test(l))
    .map((l) => l.replace(/^##\s+/, "").trim())
    .slice(0, 12);
}

function ReportView({ detail }: { detail: Detail }) {
  const md = detail.reportMd ?? "";
  const sources = sourcesOf(md);
  const headings = headingsOf(md);
  const bodyRef = useRef<HTMLDivElement>(null);
  const [copied, setCopied] = useState(false);

  const jumpTo = (i: number) => {
    const els = bodyRef.current?.querySelectorAll("h2");
    els?.[i]?.scrollIntoView({ behavior: "smooth", block: "start" });
  };

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(md);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard denied — the button just doesn't confirm */
    }
  };

  const words = md.split(/\s+/).length;

  return (
    <div className="mt-5">
      {/* What this run was: the stats a person actually asks about. */}
      <div className="flex flex-wrap items-center gap-2 text-[11px] text-fg-muted">
        {detail.runModel && (
          <span className="rounded-full bg-panel-2 px-2 py-0.5">{detail.runModel}</span>
        )}
        {detail.effort && (
          <span className="rounded-full bg-panel-2 px-2 py-0.5">{detail.effort} thinking</span>
        )}
        {detail.runCostUsd != null && (
          <span className="rounded-full bg-panel-2 px-2 py-0.5">
            ${detail.runCostUsd.toFixed(2)}
          </span>
        )}
        <span className="rounded-full bg-panel-2 px-2 py-0.5">~{Math.ceil(words / 200)} min read</span>
        {sources.length > 0 && (
          <span className="rounded-full bg-panel-2 px-2 py-0.5">
            {sources.length} {sources.length === 1 ? "source" : "sources"}
          </span>
        )}
        <Button variant="secondary" size="xs" onClick={copy} className="ml-auto">
          {copied ? "✓ Copied" : "Copy markdown"}
        </Button>
      </div>

      {/* The bibliography, up front and clickable. */}
      {sources.length > 0 && (
        <div className="mt-2 flex flex-wrap gap-1.5">
          {sources.map((s) => (
            <a
              key={s.host}
              href={s.url}
              target="_blank"
              rel="noreferrer"
              className="rounded-full border border-border bg-panel px-2.5 py-0.5 text-[11px] text-accent-fg hover:underline"
            >
              {s.host}
            </a>
          ))}
        </div>
      )}

      {/* Sections, as jump pills — a TOC that earns its space only when the
          report actually has sections. */}
      {headings.length > 1 && (
        <div className="mt-3 flex flex-wrap gap-1.5 border-t border-border pt-3">
          {headings.map((h, i) => (
            <button
              key={`${h}-${i}`}
              onClick={() => jumpTo(i)}
              className="rounded-lg bg-panel-2 px-2.5 py-1 text-[11px] text-fg-muted hover:text-fg"
            >
              {h}
            </button>
          ))}
        </div>
      )}

      <div ref={bodyRef} className="md mt-4">
        <Markdown>{md}</Markdown>
      </div>
    </div>
  );
}
