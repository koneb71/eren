import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { api, type PullRequestState } from "../lib/api";
import { PublishModal } from "./PublishModal";
import { Button } from "./ui/Button";
import { prSummary, prTone, shouldPoll, syncedLabel } from "../lib/pullRequest";
import { safeHttpUrl } from "../lib/url";

/**
 * Finishing a card as a pull request, and what became of it.
 *
 * Modelled on `PreviewPanel`: it fetches its own state, gates itself on a
 * capability it reports rather than a request that fails, and polls **only
 * while something is in flight**. A merged pull request never changes again,
 * and a card nobody is looking at should not cost a `gh` process per interval.
 *
 * The button is deliberately one button. Pressing it on a card that already
 * has a pull request pushes the follow-up commits and re-reads it, which is
 * what "update my pull request" means — GitHub updates the request itself from
 * the branch, so there is nothing to re-open.
 */
export function PullRequestPanel({
  taskId,
  projectId,
  onPublished,
}: {
  taskId: string;
  /** Only needed to offer publishing; the panel works without it. */
  projectId?: string;
  onPublished?: () => void;
}) {
  const [state, setState] = useState<PullRequestState | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** The branch and the remote disagree; force is the second, explicit click. */
  const [needsForce, setNeedsForce] = useState(false);
  const [publishing, setPublishing] = useState(false);

  const load = useCallback(
    () =>
      api
        .pullRequest(taskId)
        .then(setState)
        .catch(() => {}),
    [taskId],
  );

  useEffect(() => {
    load();
  }, [load]);

  // Only while checks are actually running.
  useEffect(() => {
    if (!shouldPoll(state?.pr ?? null)) return;
    const t = setInterval(() => {
      api
        .refreshPullRequest(taskId)
        .then(() => load())
        .catch(() => {});
    }, 15_000);
    return () => clearInterval(t);
  }, [state?.pr, taskId, load]);

  const act = async (force: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await api.openPullRequest(taskId, force);
      setNeedsForce(false);
      await load();
    } catch (e) {
      const message = String(e).replace(/^Error:\s*/, "");
      setError(message);
      // The refusal names its own remedy, so the button for it appears only
      // when that is the refusal.
      setNeedsForce(message.includes("overwrite"));
    } finally {
      setBusy(false);
    }
  };

  const refresh = async () => {
    setBusy(true);
    try {
      await api.refreshPullRequest(taskId);
      await load();
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  };

  if (!state) return null;
  const { pr, canOpen, refusal } = state;

  return (
    <div className="space-y-1.5">
      <div className="flex flex-wrap items-center gap-2 text-xs">
        {canOpen && (
          <Button variant="secondary" size="sm" onClick={() => act(false)} disabled={busy}>
            {busy
              ? "Working…"
              : pr
                ? "Update pull request"
                : "Open pull request"}
          </Button>
        )}

        {pr && (
          <>
            <span className={`size-1.5 rounded-full ${prTone(pr).dot}`} />
            <a
              href={safeHttpUrl(pr.url)}
              target="_blank"
              rel="noreferrer"
              className="font-medium text-accent-fg hover:underline"
            >
              #{pr.number}
            </a>
            <span className={prTone(pr).text}>{prSummary(pr)}</span>
            <button
              onClick={refresh}
              disabled={busy}
              className="text-fg-muted hover:text-fg disabled:opacity-50"
              // The age is the honest part: this is what gh said when asked,
              // not what is true right now.
              title="Ask GitHub again"
            >
              · {syncedLabel(pr.syncedAt, Date.now())}
            </button>
          </>
        )}
      </div>

      {/* A card that cannot do this says why, quietly, instead of showing a
          button that would only fail. */}
      {!canOpen && refusal && (
        <p className="text-[11px] text-fg-muted">
          {refusal}
          {refusal.includes("Connections") && (
            <>
              {" "}
              <Link to="/connections" className="text-accent-fg hover:underline">
                Open Connections
              </Link>
            </>
          )}
          {/* The one refusal that used to be permanent. "No origin remote" is
              not a fact about the world — it is a step nobody had been given a
              way to take, so every GitHub feature stayed dark on a project
              that started life on this disk. */}
          {projectId && refusal.includes("no GitHub `origin` remote") && (
            <>
              {" "}
              <Button variant="link" size="xs" onClick={() => setPublishing(true)}>
                Publish it to GitHub
              </Button>
            </>
          )}
        </p>
      )}

      {publishing && projectId && (
        <PublishModal
          projectId={projectId}
          onClose={() => setPublishing(false)}
          onDone={() => {
            setPublishing(false);
            load();
            onPublished?.();
          }}
        />
      )}

      {error && (
        <div className="rounded-lg bg-danger-subtle px-3 py-2 text-[11px] leading-relaxed text-danger-fg">
          {error}
          {needsForce && (
            <div className="mt-1.5">
              <Button variant="danger" size="xs" onClick={() => act(true)} disabled={busy}>
                Push anyway, replacing what is there
              </Button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
