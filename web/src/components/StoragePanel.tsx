import { useCallback, useEffect, useState } from "react";
import { api, ProjectStorage } from "../lib/api";
import { size } from "../lib/bytes";

/**
 * What this project is holding, and what can be given back.
 *
 * One place, because the parts were scattered and so the total was invisible:
 * checkouts were a line in the Files tab, preview images a line in the Previews
 * tab, and the per-run leftovers were nowhere at all. That is how 2.9 GB of
 * worktrees went unnoticed until somebody measured the directory by hand.
 *
 * Every reclaim here is explicit and every refusal is explained. Nothing on
 * this page removes anything on a timer — the boot sweeps do that, and they
 * only touch what nothing can reach.
 */
export function StoragePanel({ projectId }: { projectId: string }) {
  const [held, setHeld] = useState<ProjectStorage | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);

  const load = useCallback(() => {
    api.storage(projectId).then(setHeld).catch(() => setHeld(null));
  }, [projectId]);
  useEffect(load, [load]);

  if (!held) return null;

  const act = async (key: string, fn: () => Promise<string>) => {
    setBusy(key);
    setNote(null);
    try {
      setNote(await fn());
      load();
    } catch (e) {
      setNote(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="h-full overflow-y-auto p-6">
      <div className="mb-4">
        <h2 className="text-base font-semibold">Storage</h2>
        <p className="mt-0.5 max-w-xl text-xs text-fg-muted">
          What this project is holding on disk. Everything here is safe to give
          back — anything that is not, stays, and says why.
        </p>
      </div>

      <div className="mb-5 text-2xl font-bold">{size(held.total)}</div>

      <Section
        title="Checkouts"
        subtitle="One per card that has run. A card's own copy of the repository, kept so its diff can be reviewed."
        bytes={held.checkouts.bytes}
        count={held.checkouts.count}
        action={
          held.checkouts.reclaimable > 0
            ? {
                label: `reclaim ${held.checkouts.reclaimable} · ${size(held.checkouts.reclaimableBytes)}`,
                busy: busy === "checkouts",
                run: () =>
                  act("checkouts", async () => {
                    const r = await api.reclaimWorktrees(projectId);
                    const kept = r.kept.length
                      ? ` · kept ${r.kept.length} (${r.kept[0].why})`
                      : "";
                    return `Freed ${size(r.bytes)} from ${r.released.length}${kept}`;
                  }),
              }
            : undefined
        }
      >
        {held.checkouts.items.map((c) => (
          <Row
            key={c.branch}
            name={c.branch.replace(/^Eren\//, "")}
            bytes={c.bytes}
            why={c.keptBecause}
          />
        ))}
      </Section>

      <Section
        title="Preview images"
        subtitle="Built so a branch can be run and clicked through. Kept after a preview stops, so waking it takes seconds rather than a rebuild."
        bytes={held.previews.bytes}
        count={held.previews.items.length}
        // Docker cannot attribute an image to a project without asking per
        // image, so this figure is every project's. Saying so beats a
        // per-project number that is really a global one.
        note="across every project"
        action={
          held.previews.reclaimable > 0
            ? {
                label: `reclaim ${held.previews.reclaimable}`,
                busy: busy === "previews",
                run: () =>
                  act("previews", async () => {
                    const r = await api.reclaimPreviewDisk();
                    return `Released ${r.reclaimed} image${r.reclaimed === 1 ? "" : "s"} — the next wake rebuilds`;
                  }),
              }
            : undefined
        }
      >
        {held.previews.items.map((p) => (
          <Row
            key={p.id}
            name={p.title ?? "main"}
            why={
              p.status === "running"
                ? "it is running"
                : p.imageKept
                  ? null
                  : "its image is already gone"
            }
          />
        ))}
      </Section>

      {/* A footer, not a section with a dead button. Saying "11 MB, and you
          cannot have it back" as a row with a greyed-out control reads as
          broken; saying why it is kept reads as a decision. */}
      <div className="mt-6 border-t border-border pt-4 text-[11px] leading-relaxed text-fg-muted">
        <span className="font-medium text-fg">Kept on purpose.</span> This
        project's run history — {held.history.events.toLocaleString()} events,
        about {size(held.history.bytes)} — is what a reconnecting page replays
        from, so nothing trims it unless <code>EREN_EVENT_RETENTION_DAYS</code> is
        set, which removes the transcripts of runs finished more than that many
        days ago (the run, its cost and its summary stay). Deleting a card takes
        its own history with it.
        <div className="mt-1.5">
          Per-run leftovers — generated MCP configs, engine prompt files, preview
          build logs — are swept at every start, once their run is finished.
        </div>
      </div>

      {note && (
        <div className="mt-3 rounded-lg bg-panel-2 px-3 py-2 text-[11px] text-fg-muted">{note}</div>
      )}
    </div>
  );
}

function Section({
  title,
  subtitle,
  bytes,
  count,
  note,
  action,
  children,
}: {
  title: string;
  subtitle: string;
  bytes: number;
  count: number;
  note?: string;
  action?: { label: string; busy: boolean; run: () => void };
  children: React.ReactNode;
}) {
  return (
    <div className="card-shadow mb-4 max-w-2xl rounded-xl border border-border bg-panel p-4">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <div className="min-w-0">
          <div className="text-sm font-medium">{title}</div>
          <p className="mt-0.5 text-[11px] text-fg-muted">{subtitle}</p>
        </div>
        {/* `ml-auto` as well as `justify-between`: a long subtitle wraps this
            block onto its own flex line, where `justify-between` has nothing to
            push it against and the figure drifts to the middle of the card. */}
        <div className="ml-auto shrink-0 text-right">
          <div className="text-sm font-semibold">{size(bytes)}</div>
          <div className="text-[11px] text-fg-muted">
            {count} item{count === 1 ? "" : "s"}
            {note && ` · ${note}`}
          </div>
        </div>
      </div>
      {count > 0 && <div className="mt-3 flex flex-col gap-1">{children}</div>}
      {action && (
        <button
          onClick={action.run}
          disabled={action.busy}
          className="mt-3 text-[11px] text-accent-fg underline disabled:opacity-50"
        >
          {action.busy ? "reclaiming…" : action.label}
        </button>
      )}
    </div>
  );
}

function Row({
  name,
  bytes,
  why,
}: {
  name: string;
  bytes?: number;
  why: string | null | undefined;
}) {
  return (
    <div className="flex items-baseline gap-2 text-[11px]">
      <span className="min-w-0 flex-1 truncate">{name}</span>
      {/* The reason comes before the size: a person scanning this wants to know
          why something is staying, not how big it is. */}
      {why && <span className="shrink-0 text-fg-muted/80">{why}</span>}
      {bytes != null && (
        <span className="w-16 shrink-0 text-right tabular-nums text-fg-muted">{size(bytes)}</span>
      )}
    </div>
  );
}
