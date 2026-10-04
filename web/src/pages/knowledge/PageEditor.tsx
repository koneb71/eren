import { useCallback, useEffect, useRef, useState } from "react";
import { useNavigate, useOutletContext, useParams } from "react-router-dom";
import { Article, api, ConflictError, RevisionDiff } from "../../lib/api";
import { RichTextEditor } from "../../components/kb/RichTextEditor";
import { IconPicker } from "../../components/kb/IconPicker";
import { DiffBody } from "../../components/kb/PendingRevisionBanner";
import { KnowledgeContext } from "./KnowledgeLayout";
import { readStored, removeStored, writeStored } from "../../lib/storage";
import { Button } from "../../components/ui/Button";

/**
 * Editing a page.
 *
 * No Save button. Typing is saving — that is the ceremony a wiki removes, and
 * the one people notice. What replaces the button is a save chip that tells you
 * the truth, including the one state a Save button never had a way to express:
 * somebody else changed this page while you were typing.
 *
 * Every save carries the revision it started from. If that no longer matches,
 * the server refuses rather than overwriting, and you get a diff and a choice.
 */
type SaveState =
  | { kind: "idle" }
  | { kind: "saving" }
  | { kind: "saved" }
  // `kept`: whether the draft also made it into storage, so it survives a
  // reload. Storage can refuse; the conflict is still shown either way.
  | { kind: "conflict"; diff: RevisionDiff | null; kept: boolean }
  | { kind: "error"; message: string };

const DEBOUNCE_MS = 1500;
const DRAFT_KEY = (id: string) => `eren.kb.draft.${id}`;

export default function PageEditor() {
  const { pageId } = useParams();
  const navigate = useNavigate();
  const { workspaceId, reload } = useOutletContext<KnowledgeContext>();

  const [page, setPage] = useState<Article | null>(null);
  const [title, setTitle] = useState("");
  const [html, setHtml] = useState("");
  const [assetIds, setAssetIds] = useState<string[]>([]);
  const [save, setSave] = useState<SaveState>({ kind: "idle" });

  // Where the page stood when this edit began. A save that doesn't carry it is
  // a save that can silently overwrite whatever arrived in the meantime.
  //
  // Two numbers, because they answer different questions. `baseSeq` is the
  // revision to diff against when showing what changed. `baseVersion` is the
  // guard — and it has to be a separate counter, because rapid autosaves
  // coalesce into the revision they are extending, leaving `currentSeq` exactly
  // where it was. Guarding on the revision number let a second tab match it and
  // overwrite the first with no conflict at all.
  const baseSeq = useRef<number>(0);
  const baseVersion = useRef<number>(0);
  const dirty = useRef(false);

  useEffect(() => {
    if (!pageId) return;
    api
      .article(pageId)
      .then((p) => {
      setPage(p);
      setTitle(p.title);
      baseSeq.current = p.currentSeq;
      baseVersion.current = p.bodyVersion;
      // A draft left by a refused save is restored rather than lost — a
      // conflict must never cost someone their typing.
      const stashed = readStored(DRAFT_KEY(p.id));
      setHtml(stashed ?? p.contentHtml ?? "");
        if (stashed) setSave({ kind: "conflict", diff: null, kept: true });
      })
      // Otherwise a page that fails to load sits on "Loading…" with no way to
      // tell whether it is slow, gone, or broken.
      .catch((e) =>
        setSave({ kind: "error", message: String(e).replace(/^Error:\s*/, "") }),
      );
  }, [pageId]);

  const persist = useCallback(async () => {
    if (!pageId || !dirty.current) return;
    dirty.current = false;
    setSave({ kind: "saving" });
    try {
      const updated = await api.updateArticle(pageId, {
        title: title.trim() || "Untitled",
        content_html: html,
        base_seq: baseSeq.current,
        base_version: baseVersion.current,
        asset_ids: assetIds,
      });
      baseSeq.current = updated.currentSeq;
      baseVersion.current = updated.bodyVersion;
      removeStored(DRAFT_KEY(pageId));
      setSave({ kind: "saved" });
      reload();
    } catch (e) {
      if (e instanceof ConflictError) {
        // Keep the work somewhere it survives a reload, then show what
        // actually changed underneath rather than a bare "try again".
        const kept = writeStored(DRAFT_KEY(pageId), html);
        const fresh = await api.article(pageId).catch(() => null);
        const diff =
          fresh && fresh.currentSeq > baseSeq.current
            ? await api
                .revisionDiff(pageId, fresh.currentSeq, baseSeq.current)
                .catch(() => null)
            : null;
        setSave({ kind: "conflict", diff, kept });
        return;
      }
      setSave({ kind: "error", message: String(e).replace(/^Error:\s*/, "") });
    }
  }, [pageId, title, html, assetIds, reload]);

  // Autosave on a debounce. Long enough not to write on every keystroke, short
  // enough that closing the tab rarely loses anything.
  useEffect(() => {
    if (!dirty.current) return;
    const t = setTimeout(persist, DEBOUNCE_MS);
    return () => clearTimeout(t);
  }, [title, html, persist]);

  // Leaving the editor commits immediately rather than waiting out the timer.
  //
  // Read through a ref with an EMPTY dependency list, and both halves of that
  // matter. Listing `persist` here made this cleanup run on every keystroke —
  // React tears an effect down when its dependencies change, not only on
  // unmount. That fired a save per character with the *previous* render's
  // content, and then cleared `dirty`, so the real unmount found nothing to do
  // and the last thing you typed was never written at all.
  const latest = useRef(persist);
  latest.current = persist;
  useEffect(() => () => void latest.current(), []);

  if (!page) {
    return save.kind === "error" ? (
      <div className="p-8 text-sm text-danger-fg">
        This page could not be loaded: {save.message}
      </div>
    ) : (
      <div className="p-8 text-sm text-fg-muted">Loading…</div>
    );
  }

  return (
    <div className="h-full overflow-y-auto">
      <div className="mx-auto max-w-4xl px-6 py-6 lg:px-10">
        <div className="mb-3 flex items-center justify-between gap-3">
          <Button variant="ghost" size="xs" onClick={() => navigate(`/knowledge/${page.id}`)}>
            ← Done
          </Button>
          <SaveChip state={save} onReload={() => window.location.reload()} />
        </div>

        {save.kind === "conflict" && (
          <div className="mb-4 rounded-xl border border-warning/40 bg-warning-subtle p-4">
            <div className="text-sm font-semibold text-warning-fg">
              This page changed while you were editing
            </div>
            <p className="mt-1 text-xs text-fg">
              {save.kept
                ? "Your version is safe — it is still in the editor below and will survive a reload. "
                : "Your version is still in the editor below, but this browser would not keep a copy, so it will not survive a reload. "}
              {save.diff
                ? "Here is what changed underneath it."
                : // No diff when the other save folded into the revision this
                  // one started from: coalescing rewrites that revision in
                  // place, so the text it used to hold is genuinely gone and
                  // there is nothing honest to show. Better to say so than to
                  // render an empty box under a promise.
                  "Their change went into the same revision yours started from, so there is no before-and-after to show."}
            </p>
            {save.diff && (
              <div className="mt-3 max-h-64 overflow-auto rounded-lg border border-border bg-panel">
                <DiffBody unified={save.diff.diff} />
              </div>
            )}
            <div className="mt-3 flex flex-wrap gap-2">
              <Button
                variant="primary"
                size="sm"
                onClick={async () => {
                  // Take the newer revision as the base and re-save on top —
                  // deliberate, and the only thing that discards their edit.
                  const fresh = await api.article(page.id);
                  baseSeq.current = fresh.currentSeq;
                  baseVersion.current = fresh.bodyVersion;
                  dirty.current = true;
                  await persist();
                }}
              >
                Keep mine
              </Button>
              <Button
                size="sm"
                onClick={() => {
                  removeStored(DRAFT_KEY(page.id));
                  window.location.reload();
                }}
              >
                Take theirs
              </Button>
            </div>
          </div>
        )}

        <div className="mb-3 flex items-start gap-2">
          <IconPicker
            value={page.icon}
            onChange={async (icon) => {
              await api.updateArticle(page.id, { icon });
              setPage({ ...page, icon });
              reload();
            }}
          />
          <input
            value={title}
            onChange={(e) => {
              dirty.current = true;
              setTitle(e.target.value);
            }}
            placeholder="Untitled"
            className="min-w-0 flex-1 border-0 bg-transparent text-3xl font-bold tracking-tight outline-none text-fg placeholder:text-fg-subtle"
          />
        </div>

        <RichTextEditor
          value={html}
          onChange={(next) => {
            dirty.current = true;
            setHtml(next);
          }}
          workspaceId={workspaceId}
          onAssetUploaded={(id) => setAssetIds((prev) => [...prev, id])}
        />
      </div>
    </div>
  );
}

function SaveChip({ state, onReload }: { state: SaveState; onReload: () => void }) {
  switch (state.kind) {
    case "saving":
      return <span className="text-xs text-fg-muted">Saving…</span>;
    case "saved":
      return <span className="text-xs text-fg-muted">Saved</span>;
    case "conflict":
      return (
        <button onClick={onReload} className="ring-focus rounded text-xs font-medium text-warning-fg hover:underline">
          Changed elsewhere
        </button>
      );
    case "error":
      return <span className="text-xs text-danger-fg">{state.message}</span>;
    default:
      return <span className="text-xs text-fg-muted">Typing saves automatically</span>;
  }
}
