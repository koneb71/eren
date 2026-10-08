import { HistoryButton } from "./RevisionsPanel";
import { useEffect, useState } from "react";
import { api, AttentionSettingsValue, AttentionEvent } from "../lib/api";
import { Button } from "./ui/Button";

/**
 * How long Eren waits for you, and how it reaches you while it waits.
 *
 * One panel, because from where you sit they are one question. Everything else
 * Eren has is a browser notification from an open tab, which only helps if
 * you are at the machine with the dashboard still up — and a run that takes
 * forty minutes and asks one question at minute three is exactly the case
 * where you are not.
 */
const EVENTS: { id: AttentionEvent; label: string; hint: string }[] = [
  { id: "permission", label: "A run needs permission", hint: "the one that stops work until you answer" },
  { id: "plan", label: "A plan needs review", hint: "a plan-first card, waiting on you" },
  { id: "rate_limited", label: "Rate limited", hint: "it will resume on its own; this just tells you" },
  { id: "budget_warning", label: "A budget is nearly spent", hint: "past its warning line, before anything is held" },
  { id: "question", label: "An agent asked you something", hint: "the card waits for your answer in the inbox" },
  { id: "decision", label: "An agent proposed a decision", hint: "only you can approve it, in the inbox" },
  { id: "review", label: "An agent review needs you", hint: "its rounds are spent, or the reviewer gave no verdict" },
  { id: "stalled", label: "A run stalled", hint: "nothing was running it any more, or it went silent past your limit" },
  { id: "over_budget", label: "A budget is spent", hint: "what it covers holds until its window turns" },
  { id: "routine", label: "A routine delivered", hint: "it ran on its schedule; the result is waiting" },
  { id: "unblocked", label: "A card can start", hint: "the card it was waiting on landed" },
  { id: "finished", label: "A run finished", hint: "off by default — it fires on every card" },
];

/** Ready-made commands, so the first one is a paste rather than a project. */
const EXAMPLES: { os: string; command: string }[] = [
  { os: "Linux", command: 'notify-send "$EREN_TITLE" "$EREN_BODY"' },
  { os: "macOS", command: `osascript -e "display notification \\"$EREN_BODY\\" with title \\"$EREN_TITLE\\""` },
  { os: "Windows", command: 'powershell -c "[console]::beep(800,400)"' },
  { os: "Phone", command: 'curl -s -d "$EREN_BODY" -H "Title: $EREN_TITLE" ntfy.sh/your-topic' },
];

export function AttentionSettings() {
  const [v, setV] = useState<AttentionSettingsValue | null>(null);
  const [available, setAvailable] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [warning, setWarning] = useState<string | null>(null);

  const [loads, setLoads] = useState(0);
  useEffect(() => {
    api
      .attentionSettings()
      .then(setV)
      // An older server has no such route; the panel removes itself rather
      // than sitting there broken. Same guard PreviewSettings uses.
      .catch(() => setAvailable(false));
  }, [loads]);

  if (!available || !v) return null;

  const save = async (patch: Partial<AttentionSettingsValue>) => {
    setBusy(true);
    setError(null);
    try {
      // The command is sent only when this person may see it: an absent
      // field leaves the server's copy as it is.
      const { command, ...rest } = { ...v, ...patch };
      const saved = await api.setAttentionSettings(
        command === null ? rest : { ...rest, command },
      );
      setV(saved);
      setWarning(saved.warning ?? null);
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  };

  const toggle = (id: AttentionEvent) =>
    save({
      events: v.events.includes(id) ? v.events.filter((e) => e !== id) : [...v.events, id],
    });

  return (
    <section className="mt-8 max-w-2xl">
      <div className="flex items-center justify-between gap-2">
        <h2 className="text-sm font-semibold">When a run needs you</h2>
        <HistoryButton kind="attention" id="attention" onRestored={() => setLoads((n) => n + 1)} />
      </div>
      <p className="mt-1 text-xs leading-relaxed text-fg-muted">
        A run that stops to ask something releases its place in the queue, so the rest of the
        board keeps moving. It waits for you — and unlike before, if the wait runs out it is{" "}
        <span className="font-medium text-fg">stopped rather than told you said no</span>.
      </p>

      <div className="mt-4">
        <label className="text-[11px] font-semibold uppercase tracking-wide text-fg-muted">
          Wait for me
        </label>
        <div className="mt-1.5 flex flex-wrap gap-1.5">
          {[
            { secs: 3600, label: "1 hour" },
            { secs: 8 * 3600, label: "8 hours" },
            { secs: 24 * 3600, label: "1 day" },
            { secs: 0, label: "Indefinitely" },
          ].map((o) => (
            <button
              key={o.secs}
              disabled={busy}
              onClick={() => save({ waitSecs: o.secs })}
              className={`ring-focus rounded-lg border px-2.5 py-1.5 text-xs disabled:opacity-50 ${
                v.waitSecs === o.secs ? "border-accent text-accent-fg" : "border-border text-fg-muted"
              }`}
            >
              {o.label}
            </button>
          ))}
        </div>
        <p className="mt-1 text-[11px] text-fg-muted/80">
          {v.waitSecs === 0
            ? "It will hold the card until you answer. A waiting run costs nothing but a worktree — it is not using a queue slot."
            : "After that it stops the run and says nobody answered, which is not the same as you refusing."}
        </p>
      </div>

      <label className="mt-5 flex cursor-pointer items-start gap-2 text-sm">
        <input
          type="checkbox"
          checked={v.enabled}
          disabled={busy}
          onChange={(e) => save({ enabled: e.target.checked })}
          className="mt-0.5 accent-[var(--color-accent)]"
        />
        <span className="min-w-0">
          <span className="block font-medium">Run a command to tell me</span>
          <span className="block text-xs text-fg-muted">
            Anything your shell can do — a desktop notification, a beep, a push to your phone.
            This works with the dashboard closed, which browser notifications do not.
          </span>
        </span>
      </label>

      {v.enabled && (
        <div className="mt-3 rounded-xl border border-border bg-bg p-3">
          <input
            defaultValue={v.command ?? ""}
            disabled={busy || v.command === null}
            onBlur={(e) => e.target.value !== (v.command ?? "") && save({ command: e.target.value })}
            placeholder={EXAMPLES[0].command}
            className="w-full rounded-lg border border-border bg-panel px-2 py-1.5 font-mono text-xs outline-none focus:border-accent"
          />
          <div className="mt-2 flex flex-wrap gap-1.5">
            {EXAMPLES.map((ex) => (
              <Button
                key={ex.os}
                size="xs"
                disabled={busy}
                onClick={() => save({ command: ex.command })}
                title={ex.command}
              >
                {ex.os}
              </Button>
            ))}
          </div>

          <div className="mt-3 text-[11px] text-fg-muted">
            Your command is run as one argument, so nothing from a card can become part of it.
            The details arrive as environment variables:
          </div>
          <div className="mt-1 flex flex-wrap gap-1">
            {v.envNames.map((n) => (
              <code key={n} className="rounded bg-panel-2 px-1.5 py-0.5 font-mono text-[10px]">
                {n}
              </code>
            ))}
          </div>
          {/* Stated rather than left to be discovered, because the absence is
              deliberate and someone will otherwise go looking for it. */}
          <p className="mt-1.5 text-[11px] leading-relaxed text-fg-muted/80">
            What the tool was going to do is <span className="font-medium">not</span> among them.
            A command can forward anywhere, and a Bash input or a file edit carries your code.
            Open the dashboard to see that before you answer.
          </p>

          <div className="mt-3">
            <div className="text-[11px] font-semibold uppercase tracking-wide text-fg-muted">
              Tell me about
            </div>
            {EVENTS.map((e) => (
              <label key={e.id} className="mt-1.5 flex cursor-pointer items-start gap-2 text-xs">
                <input
                  type="checkbox"
                  checked={v.events.includes(e.id)}
                  disabled={busy}
                  onChange={() => toggle(e.id)}
                  className="mt-0.5 accent-[var(--color-accent)]"
                />
                <span className="min-w-0">
                  <span className="block">{e.label}</span>
                  <span className="block text-[11px] text-fg-muted">{e.hint}</span>
                </span>
              </label>
            ))}
          </div>
        </div>
      )}

      {warning && (
        <div className="mt-3 whitespace-pre-wrap rounded-lg bg-warning-subtle px-3 py-2 text-[11px] leading-relaxed text-warning-fg">
          {warning}
        </div>
      )}
      {error && (
        <div className="mt-3 rounded-lg bg-danger-subtle px-3 py-2 text-[11px] text-danger-fg">{error}</div>
      )}
    </section>
  );
}
