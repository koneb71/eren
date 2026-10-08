import { useEffect, useState } from "react";
import { api, type GitHubIssue } from "../lib/api";
import { Dialog } from "./ui/Dialog";
import { Button } from "./ui/Button";
import { safeHttpUrl } from "../lib/url";

/**
 * Turn GitHub issues into board cards.
 *
 * Nothing is ticked to begin with and there is no "import all", and that is
 * the security control rather than a UI preference. An issue body becomes an
 * agent's prompt, and on a public repository anyone on the internet wrote it —
 * so the person choosing sees the text first, one issue at a time.
 *
 * Bodies render as **plain text**, never through the markdown component. An
 * issue must not be able to put a tracking pixel or a dressed-up link into the
 * dashboard, which is a separate exposure from the prompt itself.
 */
export function ImportIssuesModal({
  projectId,
  onClose,
  onImported,
}: {
  projectId: string;
  onClose: () => void;
  onImported: () => void;
}) {
  const [repo, setRepo] = useState<string | null>(null);
  const [issues, setIssues] = useState<GitHubIssue[]>([]);
  const [publicRepo, setPublicRepo] = useState(true);
  const [refusal, setRefusal] = useState<string | null>(null);
  const [chosen, setChosen] = useState<Set<number>>(new Set());
  const [open, setOpen] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .githubIssues(projectId)
      .then((r) => {
        setRepo(r.repo);
        setIssues(r.issues);
        setPublicRepo(r.public ?? true);
        setRefusal(r.refusal);
      })
      .catch((e) => setError(String(e).replace(/^Error:\s*/, "")))
      .finally(() => setLoading(false));
  }, [projectId]);

  const toggle = (n: number) =>
    setChosen((prev) => {
      const next = new Set(prev);
      if (next.has(n)) next.delete(n);
      else next.add(n);
      return next;
    });

  const importChosen = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.importIssues(projectId, [...chosen]);
      onImported();
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
      setBusy(false);
    }
  };

  const importable = issues.filter((i) => !i.importedAs);

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={
        <>
          Import issues{repo && <span className="ml-2 font-mono text-xs font-normal text-fg-muted">{repo}</span>}
        </>
      }
      width={768}
      footer={
        // The error sits above the buttons, outside the scrolling list, so a
        // long list of issues never hides why an import failed.
        <div className="flex w-full min-w-0 flex-col gap-2">
          {error && (
            <div className="rounded-lg bg-danger-subtle px-3 py-2 text-[11px] text-danger-fg">{error}</div>
          )}
          <div className="flex items-center justify-end gap-2">
            {importable.length > 0 && (
              <span className="mr-auto text-[11px] text-fg-muted">
                {importable.length} not yet imported
              </span>
            )}
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button variant="primary" onClick={importChosen} disabled={busy || chosen.size === 0}>
              {busy
                ? "Importing…"
                : chosen.size === 0
                  ? "Choose issues to import"
                  : `Import ${chosen.size} as ${chosen.size === 1 ? "a card" : "cards"}`}
            </Button>
          </div>
        </div>
      }
    >
      {/* Said before anything is ticked, because it changes what "read this
          first" means. */}
      {publicRepo && !refusal && (
        <p className="rounded-lg bg-warning-subtle px-3 py-2 text-[11px] leading-relaxed text-warning-fg">
          Anyone on the internet can open an issue on a public repository, and an
          imported issue becomes an agent&rsquo;s instructions. Read each one before you
          tick it. Imported cards always land in Backlog and never start on their own.
        </p>
      )}

      {loading && <p className="mt-4 text-xs text-fg-muted">Asking GitHub…</p>}
      {refusal && <p className="mt-4 text-xs text-fg-muted">{refusal}</p>}
      {!loading && !refusal && issues.length === 0 && (
        <p className="mt-4 text-xs text-fg-muted">No open issues.</p>
      )}

      <div className="mt-3 space-y-1">
        {issues.map((issue) => {
          const done = Boolean(issue.importedAs);
          return (
            <div
              key={issue.number}
              className={`rounded-lg border px-3 py-2 ${
                done ? "border-border bg-panel-2 opacity-60" : "border-border"
              }`}
            >
              <div className="flex items-start gap-2">
                <input
                  type="checkbox"
                  className="mt-1 accent-[var(--color-accent)]"
                  checked={chosen.has(issue.number)}
                  disabled={done || busy}
                  onChange={() => toggle(issue.number)}
                />
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-baseline gap-x-2 text-sm">
                    <span className="font-mono text-xs text-fg-muted">#{issue.number}</span>
                    {/* Plain text. React escapes it; nothing renders it as markup. */}
                    <span className="min-w-0 break-words font-medium">{issue.title}</span>
                    {issue.author && (
                      <span className="text-[11px] text-fg-muted">by @{issue.author}</span>
                    )}
                    {done && <span className="text-[11px] text-fg-muted">· already a card</span>}
                  </div>
                  {issue.labels.length > 0 && (
                    <div className="mt-1 flex flex-wrap gap-1">
                      {issue.labels.map((l) => (
                        <span
                          key={l}
                          className="rounded-full bg-panel-2 px-2 py-0.5 text-[10px] text-fg-muted"
                        >
                          {l}
                        </span>
                      ))}
                    </div>
                  )}
                  <Button
                    variant="link"
                    size="xs"
                    onClick={() => setOpen(open === issue.number ? null : issue.number)}
                    className="mt-1 font-normal!"
                  >
                    {open === issue.number ? "Hide" : "Read"} what it says
                  </Button>
                  {open === issue.number && (
                    // Monospace, plain, scroll-capped: this is the text that
                    // becomes a prompt, shown as text.
                    <pre className="mt-1 max-h-56 overflow-auto whitespace-pre-wrap rounded-lg bg-bg p-2 font-mono text-[11px] leading-relaxed text-fg-muted">
                      {issue.body || "(no description)"}
                    </pre>
                  )}
                </div>
                <a
                  href={safeHttpUrl(issue.url)}
                  target="_blank"
                  rel="noreferrer"
                  className="shrink-0 text-[11px] text-fg-muted hover:text-fg"
                >
                  open ↗
                </a>
              </div>
            </div>
          );
        })}
      </div>
    </Dialog>
  );
}
