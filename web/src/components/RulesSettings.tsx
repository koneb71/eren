import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import { useAuth } from "../lib/auth";
import { Button } from "./ui/Button";
import { Textarea } from "./ui/Field";
import { toast } from "./ui/Toast";

/**
 * Your rules for agents, written into each new repository project.
 *
 * Saved per person (yours alone with accounts on). When a project is added or
 * cloned, Eren writes them as AGENTS.md plus a CLAUDE.md that imports it, and
 * commits both — after that they belong to the repository, so editing them
 * here changes projects made from now on, not the ones that exist.
 */
export function RulesSettings() {
  const { user } = useAuth();
  const [text, setText] = useState<string | null>(null);
  const [saved, setSaved] = useState("");
  const [max, setMax] = useState(20000);
  const [busy, setBusy] = useState(false);
  // Rules that could not be read are not empty rules. An editor shown empty
  // would invite saving over what is there without ever seeing it, so until
  // a load succeeds there is the reason and a retry, and nothing to save.
  const [loadError, setLoadError] = useState<string | null>(null);

  const load = useCallback(() => {
    setLoadError(null);
    api
      .rules()
      .then((r) => {
        setText(r.text);
        setSaved(r.text);
        setMax(r.maxChars);
      })
      .catch((e) => setLoadError(String(e).replace(/^Error:\s*/, "")));
  }, []);
  useEffect(load, [load]);

  if (loadError !== null) {
    return (
      <section className="mt-7 max-w-2xl rounded-xl border border-border bg-panel p-4">
        <h2 className="text-sm font-semibold text-fg">{user ? "Your rules" : "Rules"}</h2>
        <p className="mt-2 rounded-lg bg-danger-subtle px-3 py-2 text-xs text-danger-fg">
          Your rules could not be loaded, so they cannot be edited here yet: {loadError}
        </p>
        <div className="mt-2 flex justify-end">
          <Button size="sm" onClick={load}>
            Try again
          </Button>
        </div>
      </section>
    );
  }
  if (text === null) return null;
  const dirty = text !== saved;

  const save = async () => {
    setBusy(true);
    try {
      await api.saveRules(text);
      setSaved(text);
      toast("Rules saved", { tone: "success", body: "New projects start with them." });
    } catch (e) {
      toast("Not saved", { tone: "danger", body: String(e).replace(/^Error:\s*/, "") });
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="mt-7 max-w-2xl rounded-xl border border-border bg-panel p-4">
      <h2 className="text-sm font-semibold text-fg">{user ? "Your rules" : "Rules"}</h2>
      <p className="mt-0.5 text-xs leading-relaxed text-fg-muted">
        How you want agents to work, written once. Every new repository project — added or cloned — starts with them as{" "}
        <code className="font-mono text-[11px]">AGENTS.md</code>, plus a{" "}
        <code className="font-mono text-[11px]">CLAUDE.md</code> that imports it, committed. From then on they are the
        project&apos;s files: editing them here changes projects made later, not ones that exist. A repository that
        already has its own <code className="font-mono text-[11px]">AGENTS.md</code> is left alone.
      </p>
      <Textarea
        className="mt-3 min-h-[160px] font-mono text-[12px]"
        aria-label={user ? "Your rules" : "Rules"}
        placeholder={"- Keep changes small and focused.\n- Run the tests before saying you are done.\n- Never commit secrets."}
        value={text}
        maxLength={max}
        onChange={(e) => setText(e.target.value)}
      />
      <div className="mt-2 flex items-center justify-between gap-3">
        <span className="tabular text-[11px] text-fg-subtle">
          {text.length.toLocaleString()} / {max.toLocaleString()}
        </span>
        <Button size="sm" variant="primary" disabled={!dirty || busy} onClick={() => void save()}>
          {busy ? "Saving…" : "Save rules"}
        </Button>
      </div>
    </section>
  );
}
