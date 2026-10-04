import { useCallback, useEffect, useState } from "react";
import { Outlet, useNavigate, useParams } from "react-router-dom";
import { api, Space } from "../../lib/api";
import { TreePage } from "../../lib/kbTree";
import { useWorkspace } from "../../lib/workspace";
import { readStored, removeStored, writeStored } from "../../lib/storage";
import { PageTree } from "../../components/kb/PageTree";
import { GenerateModal } from "../../components/kb/GenerateModal";
import { NARROW, useMediaQuery } from "../../lib/useMediaQuery";
import { Icon } from "../../components/ui/Icon";
import { Button } from "../../components/ui/Button";

/** What the tree rail shares with whichever page is open beside it. */
export interface KnowledgeContext {
  workspaceId: string;
  pages: TreePage[];
  spaceId: string | null;
  reload: () => void;
}

/**
 * The wiki: a rail of pages, and whatever page you're reading beside it.
 *
 * The tree lives here rather than in the app's global sidebar so the space
 * selector, the search box and the page list stay together — and so this state
 * can read the route, which the app's providers cannot: they are mounted
 * outside the router.
 */
const SPACE_KEY = "eren.kb.space";

export default function KnowledgeLayout() {
  const { active } = useWorkspace();
  const { pageId } = useParams();
  const navigate = useNavigate();
  const narrow = useMediaQuery(NARROW);

  const [spaces, setSpaces] = useState<Space[]>([]);
  const [spaceId, setSpaceId] = useState<string | null>(
    () => readStored(SPACE_KEY) || null,
  );
  const [pages, setPages] = useState<TreePage[]>([]);
  const [query, setQuery] = useState("");
  const [hits, setHits] = useState<TreePage[] | null>(null);
  const [generating, setGenerating] = useState(false);
  const [railOpen, setRailOpen] = useState(false);

  const reload = useCallback(async () => {
    if (!active) return;
    try {
      const [tree, sp] = await Promise.all([
        api.kbTree(active.id, spaceId),
        api.kbSpaces(active.id),
      ]);
      setPages(tree.pages);
      setSpaces(sp.spaces);
    } catch {
      /* the rail is navigation; a failure here must not blank the page */
    }
  }, [active, spaceId]);

  useEffect(() => {
    reload();
  }, [reload]);

  useEffect(() => {
    if (spaceId) writeStored(SPACE_KEY, spaceId);
    else removeStored(SPACE_KEY);
  }, [spaceId]);

  // Search is server-side over titles *and* bodies, so it finds the page that
  // mentions a thing rather than only the page named after it.
  useEffect(() => {
    if (!active) return;
    const q = query.trim();
    if (!q) {
      setHits(null);
      return;
    }
    let stale = false;
    const t = setTimeout(async () => {
      try {
        const r = await api.articles(active.id, q);
        if (stale) return;
        setHits(
          r.articles.map((a) => ({
            id: a.id,
            parentId: null, // a result list is a list, not a slice of the tree
            title: a.title,
            icon: a.icon,
            position: 0,
            status: a.status,
            origin: a.origin,
            childCount: 0,
            hasPending: false,
            writing: false,
          })),
        );
      } catch {
        /* leave the previous results up rather than flashing empty */
      }
    }, 180);
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [query, active]);

  const createPage = async (parentId: string | null) => {
    if (!active) return;
    const page = await api.createArticle({
      workspace_id: active.id,
      title: "Untitled",
      parent_id: parentId,
      project_id: spaceId,
    });
    await reload();
    navigate(`/knowledge/${page.id}/edit`);
  };

  if (!active) return null;

  const rail = (
    <div className="flex h-full min-h-0 flex-col gap-2 border-r border-border bg-panel p-3">
      <select
        value={spaceId ?? ""}
        onChange={(e) => setSpaceId(e.target.value || null)}
        className="ring-focus rounded-xl border border-border bg-panel px-2.5 py-2 text-sm text-fg outline-none transition-colors focus:border-accent"
      >
        {spaces.map((s) => (
          <option key={s.id ?? "general"} value={s.id ?? ""}>
            {s.name} ({s.pages})
          </option>
        ))}
      </select>

      {/* The icon sits inside the field rather than beside it, so the rail
          keeps one column and the input keeps its full width. */}
      <div className="relative">
        <span className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-fg-muted">
          <Icon name="search" size={14} />
        </span>
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search every page…"
          className="ring-focus w-full rounded-xl border border-border bg-bg py-2 pl-8 pr-2.5 text-xs text-fg placeholder:text-fg-subtle outline-none transition-colors focus:border-accent focus:bg-panel"
        />
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto">
        {hits ? (
          <div className="flex flex-col">
            <div className="px-2 py-1 text-[10px] font-semibold uppercase tracking-[0.08em] text-fg-muted">
              {hits.length} result{hits.length === 1 ? "" : "s"} across all spaces
            </div>
            {hits.map((h) => (
              <button
                key={h.id}
                onClick={() => navigate(`/knowledge/${h.id}`)}
                className="ring-focus truncate rounded-lg px-2 py-1.5 text-left text-sm text-fg transition-colors hover:bg-panel-2"
              >
                {h.icon || "▦"} {h.title}
              </button>
            ))}
          </div>
        ) : (
          <PageTree pages={pages} activeId={pageId} onCreateChild={createPage} />
        )}
      </div>

      <div className="flex flex-col gap-1.5 border-t border-border pt-2">
        <Button
          variant="primary"
          className="w-full"
          onClick={() => createPage(null)}
          icon={<Icon name="plus" size={13} strokeWidth={2.5} />}
        >
          New page
        </Button>
        <Button
          className="w-full"
          onClick={() => setGenerating(true)}
          icon={<Icon name="sparkle" size={13} />}
        >
          Ask an agent to write one
        </Button>
      </div>
    </div>
  );

  const context: KnowledgeContext = {
    workspaceId: active.id,
    pages,
    spaceId,
    reload,
  };

  return (
    <div
      className={
        narrow
          ? "flex h-full min-h-0 flex-col"
          : "grid h-full min-h-0 grid-cols-[260px_minmax(0,1fr)]"
      }
    >
      {narrow ? (
        <>
          <button
            onClick={() => setRailOpen((v) => !v)}
            className="flex shrink-0 items-center gap-2 border-b border-border bg-panel px-3 py-2.5 text-left text-sm font-medium text-fg"
          >
            <Icon name="knowledge" size={15} />
            Pages
          </button>
          {railOpen && <div className="max-h-64 shrink-0 overflow-hidden">{rail}</div>}
        </>
      ) : (
        rail
      )}
      <main className="min-h-0 min-w-0 overflow-hidden">
        <Outlet context={context} />
      </main>

      {generating && (
        <GenerateModal
          workspaceId={active.id}
          defaultProjectId={spaceId}
          onClose={() => setGenerating(false)}
          onStarted={(id) => {
            setGenerating(false);
            reload();
            if (id) navigate(`/knowledge/${id}`);
          }}
        />
      )}
    </div>
  );
}
