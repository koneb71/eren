import { useEffect, useState } from "react";
import { api, Effort, EffortSettings, EngineModels, LocalHosts, LocalModel, ModelSettings, PermissionMode, PermissionSettings, Tier } from "../lib/api";
import { EffortPicker } from "../components/EffortPicker";
import { PreviewSettings } from "../components/PreviewSettings";
import { AttentionSettings } from "../components/AttentionSettings";
import { UnattendedSettings } from "../components/UnattendedSettings";
import { RulesSettings } from "../components/RulesSettings";
import { Page, PageHead } from "../components/ui/Surface";
import { Icon } from "../components/ui/Icon";
import { Button } from "../components/ui/Button";
import { tappable } from "../lib/motion";

/**
 * Machine-wide settings. Today: which model each complexity tier runs.
 *
 * Tiers are how everything else in the app expresses "how hard is this" — a
 * task's tier, an agent's tier, a workflow step's tier. Which model answers
 * that depends on your plan and your appetite for spend, so it belongs to
 * you rather than to the code.
 */
const TIERS: { key: Tier; label: string; when: string }[] = [
  { key: "easy", label: "Easy", when: "Mechanical, well-specified work." },
  { key: "medium", label: "Medium", when: "Typical feature and bugfix work." },
  { key: "complex", label: "Complex", when: "Architecture, judging, gnarly debugging." },
];

/** `{engine_id: {easy, medium, complex}}` — what the Save button sends. */
type Draft = Record<string, Record<Tier, string>>;
/** The same shape for how hard each tier thinks. Null means inherit. */
type EffortDraft = Record<string, Record<Tier, Effort | null>>;

const asDraft = (s: ModelSettings): Draft =>
  Object.fromEntries(s.engines.map((e) => [e.id, { ...e.tiers }]));

const NO_EFFORTS: Record<Tier, Effort | null> = {
  easy: null,
  medium: null,
  complex: null,
};

const asEffortDraft = (s: ModelSettings): EffortDraft =>
  Object.fromEntries(s.engines.map((e) => [e.id, { ...NO_EFFORTS, ...e.efforts }]));

export default function SettingsPage() {
  // Probed once: a runtime started after this page opened is a refresh
  // away, and polling a port that is usually not there is noise.
  const [localModels, setLocalModels] = useState<LocalModel[]>([]);
  const [localHosts, setLocalHosts] = useState<LocalHosts | null>(null);
  const [localError, setLocalError] = useState<string | null>(null);
  useEffect(() => {
    api
      .localModels()
      .then((r) => {
        setLocalModels(r.models);
        setLocalHosts(r.hosts);
      })
      .catch(() => {});
  }, []);

  const saveLocalHost = async (patch: Partial<LocalHosts>) => {
    setLocalError(null);
    try {
      const r = await api.setLocalHosts(patch);
      setLocalModels(r.models);
      setLocalHosts(r.hosts);
    } catch (e) {
      setLocalError(String(e).replace(/^Error:\s*/, ""));
    }
  };
  const [settings, setSettings] = useState<ModelSettings | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [effortDraft, setEffortDraft] = useState<EffortDraft | null>(null);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [perms, setPerms] = useState<PermissionSettings | null>(null);
  const [effort, setEffort] = useState<EffortSettings | null>(null);
  /**
   * The running binary is older than this page.
   *
   * Not a hypothetical: the dashboard is served from disk, so a server left
   * running from before a release serves the new page and then answers its
   * calls with an API that has never heard of them. Worth naming, because the
   * alternative is a Thinking section with one lonely option and a Save button
   * that discards what you just set.
   */
  const [stale, setStale] = useState(false);

  useEffect(() => {
    api.permissionSettings().then(setPerms).catch(() => {});
    api.effortSettings().then(setEffort).catch(() => setStale(true));
  }, []);

  useEffect(() => {
    api
      .modelSettings()
      .then((s) => {
        setSettings(s);
        setDraft(asDraft(s));
        setEffortDraft(asEffortDraft(s));
      })
      .catch((e) => setError(String(e)));
  }, []);

  const dirty =
    !!draft &&
    !!settings &&
    settings.engines.some((e) =>
      TIERS.some(
        (t) =>
          draft[e.id]?.[t.key] !== e.tiers[t.key] ||
          (effortDraft?.[e.id]?.[t.key] ?? null) !== (e.efforts?.[t.key] ?? null),
      ),
    );

  const save = async () => {
    if (!draft) return;
    setBusy(true);
    setError(null);
    try {
      await api.setModelSettings(draft, effortDraft ?? {});
      const fresh = await api.modelSettings();
      setSettings(fresh);
      setDraft(asDraft(fresh));
      setEffortDraft(asEffortDraft(fresh));
      setSaved(true);
      // Labels elsewhere in the app come from a provider loaded at startup,
      // so a reload is the honest way to make every chip agree at once.
      setTimeout(() => window.location.reload(), 700);
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Page>
      <PageHead
        title="Settings"
        subtitle="Which model runs at each complexity tier. Tasks, agents and workflow steps all pick a tier — this is what those tiers mean."
      />

      <RulesSettings />

      <h2 className="mt-7 text-sm font-semibold uppercase tracking-wider text-fg-muted">
        Permissions
      </h2>
      <p className="mt-1 max-w-xl text-sm text-fg-muted">
        How much new work is allowed to do before it stops to ask you.
      </p>
      <div className="mt-3 max-w-2xl space-y-2">
        {perms?.modes.map((m) => (
          <label
            key={m.id}
            className={`flex cursor-pointer items-start gap-2.5 rounded-xl border p-3 ${
              perms.defaultMode === m.id ? "border-accent bg-accent/5" : "border-border bg-panel"
            }`}
          >
            <input
              type="radio"
              name="permission-mode"
              checked={perms.defaultMode === m.id}
              onChange={async () => {
                setPerms({ ...perms, defaultMode: m.id });
                await api.setDefaultPermissionMode(m.id as PermissionMode);
              }}
              className="mt-0.5 accent-[var(--color-accent)]"
            />
            <span className="min-w-0">
              <span className="block text-sm font-medium">{m.label}</span>
              <span className="mt-0.5 block text-xs text-fg-muted">{m.blurb}</span>
            </span>
          </label>
        ))}
        <p className="text-[11px] text-fg-muted">
          Applies to cards created from now on. "Don't ask" also needs the project
          itself to opt in — that switch is on the project, next to its name.
        </p>
        {!!perms?.agentsOverriding && (
          <div className="rounded-xl border border-warning/40 bg-warning-subtle p-3 text-xs text-warning-fg">
            <span className="font-semibold">
              {perms.agentsOverriding} agent{perms.agentsOverriding === 1 ? "" : "s"} set
              their own permission mode
            </span>{" "}
            — a card's agent overrides the setting above, so those runs will keep
            asking whatever you choose here.
            <Button
              size="xs"
              onClick={async () => {
                await api.applyPermissionsToAgents();
                setPerms(await api.permissionSettings());
              }}
              className="ml-2"
            >
              Make them follow this setting
            </Button>
          </div>
        )}
      </div>

      <h2 className="mt-8 text-sm font-semibold uppercase tracking-wider text-fg-muted">
        Thinking
      </h2>
      <p className="mt-1 max-w-xl text-sm text-fg-muted">
        How hard the model works before it answers. Separate from which model —
        the same model can think for a second or for several minutes, and the
        second one costs a great deal more.
      </p>
      <div className="mt-3 max-w-2xl space-y-2">
        {stale && <StaleServer />}
        {!stale && (
          <EffortChoice
            checked={effort?.defaultEffort == null}
            label="Leave it to the CLI"
            blurb="Whatever claude or opencode does on its own. This is what Eren ships with."
            onPick={async () => {
              setEffort((e) => (e ? { ...e, defaultEffort: null } : e));
              await api.setDefaultEffort(null);
            }}
          />
        )}
        {effort?.levels.map((l) => (
          <EffortChoice
            key={l.id}
            checked={effort.defaultEffort === l.id}
            label={l.label}
            blurb={l.blurb}
            onPick={async () => {
              setEffort({ ...effort, defaultEffort: l.id });
              await api.setDefaultEffort(l.id);
            }}
          />
        ))}
        {!stale && (
          <p className="text-[11px] text-fg-muted">
            The fallback, for any tier that doesn't set one of its own below. A
            card or its agent still outranks both. Resolved when a run starts
            rather than when a card is made, so raising this reaches work
            already sitting in the backlog.
          </p>
        )}
        {!!effort?.agentsOverriding && (
          <div className="rounded-xl border border-warning/40 bg-warning-subtle p-3 text-xs text-warning-fg">
            <span className="font-semibold">
              {effort.agentsOverriding} agent
              {effort.agentsOverriding === 1 ? "" : "s"} set their own
            </span>{" "}
            — an agent's budget outranks this, the same way its permission preset
            does. Change those on the agent itself.
          </div>
        )}
      </div>

      {/* Where to look for models this machine serves. Not engines — see
          eren_core::local_models — so it sits with the other settings
          rather than beside the engine list, which would imply otherwise. */}
      <section className="mt-8">
        <h2 className="text-sm font-semibold">Local model runtimes</h2>
        <p className="mt-1 max-w-2xl text-xs leading-relaxed text-fg-muted">
          Ollama and LM Studio serve models on this machine. They are not engines — they hold
          no tools — but an engine that fronts them, like OpenCode, can use their models, and
          what they have pulled is offered in the model fields above. Leave a box empty for the
          default port.
        </p>
        {localError && (
          <div className="mt-2 max-w-lg rounded-lg bg-danger-subtle px-3 py-2 text-xs text-danger-fg">
            {localError}
          </div>
        )}
        <div className="mt-3 flex flex-wrap gap-4">
          {(["ollama", "lmstudio"] as const).map((k) => (
            <label key={k} className="block">
              <span className="mb-1 block text-[11px] font-semibold uppercase tracking-wide text-fg-muted">
                {k === "ollama" ? "Ollama" : "LM Studio"}
              </span>
              <input
                defaultValue={localHosts?.[k] ?? ""}
                key={localHosts?.[k] ?? k}
                placeholder={k === "ollama" ? "http://127.0.0.1:11434" : "http://127.0.0.1:1234"}
                spellCheck={false}
                // On blur rather than on every keystroke: each save probes the
                // address, and probing a half-typed URL is noise.
                onBlur={(e) => {
                  const v = e.target.value.trim();
                  if (v !== (localHosts?.[k] ?? "")) void saveLocalHost({ [k]: v });
                }}
                className="w-64 rounded-lg border border-border bg-panel px-2.5 py-2 font-mono text-xs"
              />
            </label>
          ))}
        </div>
        <p className="mt-2 text-[11px] text-fg-muted">
          {localModels.length > 0
            ? `${localModels.length} model${localModels.length === 1 ? "" : "s"} found: ${localModels
                .map((m) => m.id)
                .slice(0, 4)
                .join(", ")}${localModels.length > 4 ? "…" : ""}`
            : "Nothing answering at those addresses — which is fine if you do not run either."}
        </p>
      </section>

      <PreviewSettings />
      <AttentionSettings />
      <UnattendedSettings />

      <h2 className="mt-8 text-sm font-semibold uppercase tracking-wider text-fg-muted">
        Models
      </h2>
      <p className="mt-1 max-w-2xl text-sm text-fg-muted">
        A tier means a different model on each engine — "medium" can't name one
        model globally, since OpenCode has never heard of{" "}
        <code className="text-[11px]">opus</code>. Claude Code&apos;s &ldquo;latest&rdquo;
        choices are aliases the installed CLI resolves to its newest model of that family,
        so new models arrive when the CLI is updated.
      </p>
      <div className="mt-3 max-w-2xl space-y-5">
        {settings?.engines.map((engine) => (
          <div key={engine.id}>
            <div className="mb-2 flex flex-wrap items-baseline gap-2">
              <span className="text-sm font-semibold">{engine.label}</span>
              {!!engine.providers.length && (
                <span className="text-[11px] text-fg-muted">
                  signed in with {engine.providers.map((p) => p.name).join(", ")}
                </span>
              )}
            </div>
            <div className="space-y-3">
              {TIERS.map((tier) => (
                <div
                  key={tier.key}
                  className="card-shadow rounded-xl border border-border bg-panel p-4"
                >
                  <div className="flex flex-wrap items-center justify-between gap-3">
                    <div className="min-w-0 flex-1">
                      <div className="text-sm font-semibold">{tier.label}</div>
                      <div className="mt-0.5 text-xs text-fg-muted">{tier.when}</div>
                    </div>
                    {/* Kept on one line: a tier reads as a single choice, and
                        having the budget wrap under the model on some rows and
                        not others made them look like different controls. */}
                    <div className="flex shrink-0 items-center gap-2">
                      <TierField
                        engine={engine}
                        localModels={localModels}
                        value={draft?.[engine.id]?.[tier.key] ?? ""}
                        onChange={(v) =>
                          setDraft((d) =>
                            d
                              ? { ...d, [engine.id]: { ...d[engine.id], [tier.key]: v } }
                              : d,
                          )
                        }
                      />
                      {/* Beside the model rather than in its own section: a
                          tier answers both questions at once, and "Complex"
                          meaning Opus at low effort is a choice worth being
                          able to see in one glance. */}
                      <EffortPicker
                        value={effortDraft?.[engine.id]?.[tier.key] ?? null}
                        inherited={effort?.defaultEffort ?? null}
                        // Not merely useless against an older server — it would
                        // take the change, enable Save, and drop it.
                        disabled={stale}
                        onChange={(v) =>
                          setEffortDraft((d) =>
                            d
                              ? { ...d, [engine.id]: { ...d[engine.id], [tier.key]: v } }
                              : d,
                          )
                        }
                      />
                    </div>
                  </div>
                  {draft && (
                    <div className="mt-2 text-[11px] text-fg-muted">
                      {engine.choices.find(
                        (c) => c.id === draft[engine.id]?.[tier.key],
                      )?.blurb ??
                        (engine.fixedCatalog
                          ? null
                          : "Any provider/model id this engine can reach.")}
                      {draft[engine.id]?.[tier.key] !== engine.defaults[tier.key] && (
                        <span className="ml-1 opacity-70">
                          (default:{" "}
                          {engine.choices.find((c) => c.id === engine.defaults[tier.key])
                            ?.label ?? engine.defaults[tier.key]}
                          )
                        </span>
                      )}
                    </div>
                  )}
                </div>
              ))}
            </div>
          </div>
        ))}
      </div>

      {error && (
        <div className="mt-3 max-w-2xl rounded-lg bg-danger-subtle px-3 py-2 text-xs text-danger-fg">
          {error}
        </div>
      )}

      <div className="mt-4 flex max-w-2xl items-center gap-3">
        <Button variant="primary" onClick={save} disabled={!dirty || busy}>
          {busy ? "Saving…" : saved && !dirty ? "Saved" : "Save"}
        </Button>
        {settings && (
          <Button
            variant="ghost"
            size="xs"
            onClick={() =>
              setDraft(
                Object.fromEntries(
                  settings.engines.map((e) => [e.id, { ...e.defaults }]),
                ),
              )
            }
          >
            Reset to defaults
          </Button>
        )}
        <span className="text-[11px] text-fg-muted">
          Runs already in flight keep the model they started with.
        </span>
      </div>
    </Page>
  );
}

/**
 * A fixed catalog gets a picker; everything else gets free text.
 *
 * OpenCode fronts 75+ providers plus local models, so a dropdown would be
 * both wrong within a week and unable to express `ollama/qwen3-coder`. The
 * server still validates the `provider/model` shape, which catches the one
 * mistake people actually make: pasting a Claude id into this field.
 */
/**
 * Said once, where the missing controls would have been.
 *
 * Deliberately not a toast or a console warning: the failure mode this
 * replaces is a page that looks like it works and quietly drops what you set.
 */
function StaleServer() {
  return (
    <div className="rounded-xl border border-warning/40 bg-warning-subtle p-3 text-xs text-warning-fg">
      <span className="font-semibold">
        This server is older than this page.
      </span>{" "}
      It was started before thinking budgets existed, so it can't store one —
      the dashboard is served from disk, which is why the two can disagree.
      Restart Eren and reload.
    </div>
  );
}

/** One radio in the thinking list. Same shape as a permission mode. */
function EffortChoice({
  checked,
  label,
  blurb,
  onPick,
}: {
  checked: boolean;
  label: string;
  blurb: string;
  onPick: () => void;
}) {
  return (
    <label
      className={`flex cursor-pointer items-start gap-2.5 rounded-xl border p-3 ${
        checked ? "border-accent bg-accent/5" : "border-border bg-panel"
      }`}
    >
      <input
        type="radio"
        name="default-effort"
        checked={checked}
        onChange={onPick}
        className="mt-0.5 accent-[var(--color-accent)]"
      />
      <span className="min-w-0">
        <span className="block text-sm font-medium">{label}</span>
        <span className="mt-0.5 block text-xs text-fg-muted">{blurb}</span>
      </span>
    </label>
  );
}

function TierField({
  engine,
  value,
  onChange,
  localModels,
}: {
  engine: EngineModels;
  value: string;
  onChange: (v: string) => void;
  /** Models an on-machine runtime has pulled. Suggestions only — they are
   *  ordinary provider/model ids for whichever engine fronts them. */
  localModels: LocalModel[];
}) {
  if (engine.fixedCatalog) {
    return (
      <select
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="min-w-52 rounded-lg border border-border bg-panel px-2.5 py-2 text-sm"
      >
        {engine.choices.map((c) => (
          <option key={c.id} value={c.id}>
            {c.label}
          </option>
        ))}
      </select>
    );
  }
  // A datalist rather than a select: the CLI's list is what this machine can
  // reach today, but a local model it doesn't enumerate is still legitimate.
  const listId = `models-${engine.id}`;
  return (
    <div className="flex flex-col items-end gap-1">
      <input
        value={value}
        list={listId}
        onChange={(e) => onChange(e.target.value)}
        spellCheck={false}
        placeholder="provider/model"
        className="min-w-64 rounded-lg border border-border bg-panel px-2.5 py-2 font-mono text-xs"
      />
      <datalist id={listId}>
        {engine.available.map((id) => (
          <option key={id} value={id} />
        ))}
        {/* What Ollama and LM Studio have pulled, offered beside what the CLI
            reported. They are not engines — they are providers this one
            fronts — so they belong in the same list rather than a section of
            their own. */}
        {localModels.map((m) => (
          <option key={m.id} value={m.id} label={`${m.provider} · on this machine`} />
        ))}
      </datalist>
      {!engine.available.includes(value) &&
        !!engine.available.length &&
        // A local model is legitimately absent from the CLI's list, so
        // warning about it would be telling the user off for the thing this
        // feature exists to make easy.
        !localModels.some((m) => m.id === value) && (
          <span className="text-[10px] text-warning-fg">
            not in this install's model list
          </span>
        )}
    </div>
  );
}
