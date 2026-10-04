import { useEffect, useRef, useState } from "react";
import { api, type CloneProgress } from "../lib/api";
import { readStored, writeStored } from "../lib/storage";
import { FolderBrowserModal } from "./FolderBrowserModal";
import { Dialog } from "./ui/Dialog";
import { Button } from "./ui/Button";
import { Field, Input } from "./ui/Field";

/**
 * Where the last clone went.
 *
 * People keep their code in one place, so asking again every time is asking a
 * question already answered. Same idea as the knowledge base remembering which
 * space you were in. If the folder has since gone, the server refuses and the
 * picker is one click away.
 */
const LAST_PARENT = "eren.clone.parent";

/**
 * Clone a repository from GitHub into a new project.
 *
 * Polls rather than waits, because a clone of any size takes longer than a
 * request should — the same reason the preview panel polls a build. The poll
 * only runs while a clone is in flight.
 *
 * The one-time cancel matters: a killed clone leaves half a repository on disk,
 * so the server writes into a hidden temporary folder and moves it into place
 * only on success. Cancelling removes it.
 */
export function CloneRepoModal({
  workspaceId,
  onClose,
  onCloned,
}: {
  workspaceId: string;
  onClose: () => void;
  onCloned: (projectId: string) => void;
}) {
  const [repo, setRepo] = useState("");
  const [name, setName] = useState("");
  const [parent, setParent] = useState<string | null>(
    () => readStored(LAST_PARENT),
  );
  const [browsing, setBrowsing] = useState(false);
  const [id, setId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const cloneId = useRef<string | null>(null);

  // The default location, asked of the server rather than guessed: it is
  // `EREN_BROWSE_ROOT` or `$HOME`, and only the server knows which.
  useEffect(() => {
    if (parent) return;
    api
      .fsList()
      .then((l) => setParent(l.path))
      .catch(() => {});
  }, [parent]);

  // Only while one is running.
  useEffect(() => {
    if (!id) return;
    const tick = () =>
      api
        .cloneStatus(id)
        .then((p: CloneProgress) => {
          if (p.state === "done") {
            setId(null);
            cloneId.current = null;
            onCloned(p.projectId);
          } else if (p.state === "failed") {
            setId(null);
            cloneId.current = null;
            setBusy(false);
            setError(p.reason);
          }
        })
        .catch(() => {});
    tick();
    const t = setInterval(tick, 2000);
    return () => clearInterval(t);
  }, [id, onCloned]);

  // What the server would call the folder if nothing is typed — the last
  // segment of the repository, which is what `gh repo clone` uses.
  const defaultName = repo
    .trim()
    .replace(/\.git$/, "")
    .replace(/\/+$/, "")
    .split("/")
    .filter(Boolean)
    .pop()
    ?.split(":")
    .pop() ?? "";

  const start = async () => {
    if (!repo.trim()) {
      setError("Paste a repository — owner/repo, or its URL.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const started = await api.cloneRepo(
        workspaceId,
        repo.trim(),
        parent ?? undefined,
        name.trim() || undefined,
      );
      if (parent) writeStored(LAST_PARENT, parent);
      cloneId.current = started.id;
      setId(started.id);
    } catch (e) {
      setBusy(false);
      setError(String(e).replace(/^Error:\s*/, ""));
    }
  };

  const cancel = async () => {
    if (cloneId.current) await api.cancelClone(cloneId.current).catch(() => {});
    onClose();
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && !busy && onClose()}
      title="Clone from GitHub"
      description={
        <>
          Cloned with your own <code className="font-mono">gh</code> login — Eren holds no
          credential and never asks for one.
        </>
      }
      width={512}
      footer={
        <>
          <Button variant="ghost" onClick={cancel}>
            {busy ? "Stop and discard" : "Cancel"}
          </Button>
          <Button variant="primary" onClick={start} disabled={busy}>
            {busy ? "Cloning…" : "Clone"}
          </Button>
        </>
      }
    >
      <Field label="Repository">
        {(fid) => (
          <Input
            id={fid}
            autoFocus
            value={repo}
            onChange={(e) => setRepo(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !busy) start();
            }}
            disabled={busy}
            placeholder="owner/repo, or https://github.com/owner/repo"
          />
        )}
      </Field>

      <div className="mt-3">
        <span className="mb-1.5 block text-xs font-medium text-fg">Location</span>
        <div className="flex items-center gap-2">
          {/* Shown rather than assumed. It used to be the folder Eren
              browses from, silently — so the only way to find out where a
              repository had gone was to go looking for it. */}
          <span className="min-w-0 flex-1 truncate rounded-md border border-border bg-panel-2 px-2.5 py-1.5 font-mono text-xs text-fg-muted">
            {parent ?? "…"}
          </span>
          <Button onClick={() => setBrowsing(true)} disabled={busy}>
            Change
          </Button>
        </div>
      </div>

      <Field
        className="mt-3"
        label={
          <>
            Folder name <span className="font-normal text-fg-muted">— optional</span>
          </>
        }
      >
        {(fid) => (
          <Input
            id={fid}
            value={name}
            onChange={(e) => setName(e.target.value)}
            disabled={busy}
            placeholder={defaultName || "the repository's own name"}
          />
        )}
      </Field>

      {/* The whole answer to "where will this end up", in one line, before
          anything is downloaded. */}
      {parent && (name.trim() || defaultName) && (
        <p className="mt-2 truncate font-mono text-[11px] text-fg-muted">
          → {parent.replace(/\/$/, "")}/{name.trim() || defaultName}
        </p>
      )}

      {error && (
        <div className="mt-3 rounded-lg bg-danger-subtle px-3 py-2 text-[11px] leading-relaxed text-danger-fg">
          {error}
        </div>
      )}

      {id && (
        <div className="mt-3 flex items-center gap-2 text-xs text-fg-muted">
          <span className="size-1.5 animate-pulse rounded-full bg-accent" />
          Cloning… this can take a while for a large repository.
        </div>
      )}

      {browsing && (
        <FolderBrowserModal
          start={parent ?? undefined}
          title="Where should it be cloned?"
          confirmLabel="Clone here"
          // Choosing a place to put a clone initialises nothing — the clone
          // brings its own repository with it.
          initialisesGit={false}
          onClose={() => setBrowsing(false)}
          onPick={async (path) => {
            setParent(path);
            setBrowsing(false);
          }}
        />
      )}
    </Dialog>
  );
}
