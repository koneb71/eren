import { useCallback, useEffect, useState } from "react";
import { api, DockerStatus, previewUrl, TaskPreview } from "../lib/api";
import { Button } from "./ui/Button";
import { safeHttpUrl } from "../lib/url";

/**
 * Start this card's preview, and reach it.
 *
 * One line, because the drawer is a column of controls and this is one of
 * them. Everything about previews as a *set* — what else is running, what the
 * disk costs, comparing against main, the project's Dockerfile — lives in the
 * project's Previews tab, where they can be seen together. This is only the
 * button for the card you happen to have open.
 *
 * Polls only while building. A running preview has nothing further to report
 * to this page, and the container never talks to Eren at all.
 */
export function PreviewPanel({
  taskId,
  projectId,
  onOpenPreviews,
}: {
  taskId: string;
  projectId: string;
  /** Take me to the tab that owns all of this. */
  onOpenPreviews?: () => void;
}) {
  const [preview, setPreview] = useState<TaskPreview | null>(null);
  const [docker, setDocker] = useState<DockerStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(
    () =>
      api
        .taskPreview(taskId)
        .then((r) => setPreview(r.preview))
        .catch(() => {}),
    [taskId],
  );

  useEffect(() => {
    setPreview(null);
    setError(null);
    refresh();
    api.dockerStatus().then(setDocker).catch(() => {});
  }, [taskId, refresh]);

  const building = preview?.status === "building";
  useEffect(() => {
    if (!building) return;
    const t = setInterval(refresh, 2000);
    return () => clearInterval(t);
  }, [building, refresh]);

  const start = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await api.startPreview(taskId);
      setPreview(r.preview);
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  };

  // Nothing to offer and nothing to explain: don't take up the row.
  if (docker && !docker.installed && !preview) return null;

  const live = preview?.status === "running";
  const url = preview && live ? previewUrl(preview) : null;

  return (
    <div className="mt-3 flex flex-wrap items-center gap-2 text-[11px]">
      <span className="font-semibold uppercase tracking-wide text-fg-muted">
        Preview
      </span>

      {building && <span className="text-fg-muted">building…</span>}

      {live && url && (
        <>
          <a
            href={safeHttpUrl(url)}
            target="_blank"
            rel="noreferrer"
            className="font-medium text-accent-fg hover:underline"
          >
            {url.replace(/^https?:\/\//, "")}
          </a>
          {preview!.stale && (
            <span className="rounded bg-warning-subtle px-1 text-warning-fg">
              built before the latest run
            </span>
          )}
        </>
      )}

      {!live && !building && (
        <Button
          size="xs"
          onClick={start}
          disabled={busy || (!!docker && !docker.usable)}
          className="font-normal!"
        >
          {preview?.canWake ? "Wake it" : "Build & run"}
        </Button>
      )}

      {/* Where the rest of it lives. Said out loud rather than left to be
          discovered, since this row is deliberately not the whole feature. */}
      {onOpenPreviews && (
        <Button
          variant="link"
          size="xs"
          onClick={onOpenPreviews}
          className="font-normal! text-fg-muted! hover:text-fg!"
        >
          all previews
        </Button>
      )}

      {error && (
        <span className="rounded bg-danger-subtle px-1.5 py-0.5 text-danger-fg">
          {error}
          {/* The one failure with a way out, and the tab is where the fix is. */}
          {error.includes("no Dockerfile") && onOpenPreviews && (
            <>
              {" "}
              <button onClick={onOpenPreviews} className="underline">
                write one
              </button>
            </>
          )}
        </span>
      )}
    </div>
  );
}
