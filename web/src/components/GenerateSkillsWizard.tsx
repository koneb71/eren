import { useState } from "react";
import { api, SkillDraft, Tier } from "../lib/api";
import { useAuth } from "../lib/auth";
import { TierPicker } from "./TierPicker";
import { Dialog } from "./ui/Dialog";
import { Button } from "./ui/Button";
import { Field, Input, Switch, Textarea } from "./ui/Field";

type Phase = "describe" | "generating" | "review";

/**
 * Draft skills from a description, then read, edit and save each one.
 *
 * Nothing is saved by generating: every draft goes through the same create a
 * hand-written skill does, so its name is checked against the `@` namespace and
 * its text against the secret check like any other. A skill is applied only
 * where it is named, so a draft that is too broad costs nothing until somebody
 * names it — but it is also useless, which is why the prompt asks for narrow.
 */
export function GenerateSkillsWizard({
  workspaceId,
  onClose,
  onSaved,
}: {
  workspaceId: string;
  onClose: () => void;
  onSaved: () => void;
}) {
  const { user } = useAuth();
  const [phase, setPhase] = useState<Phase>("describe");
  const [description, setDescription] = useState("");
  const [tier, setTier] = useState<Tier>("medium");
  const [drafts, setDrafts] = useState<SkillDraft[]>([]);
  const [saved, setSaved] = useState<Set<number>>(new Set());
  const [personal, setPersonal] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const generate = async () => {
    if (!description.trim()) return;
    setPhase("generating");
    setError(null);
    try {
      const r = await api.generateSkills(description.trim(), undefined, tier);
      setDrafts(r.drafts);
      setSaved(new Set());
      setPhase("review");
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
      setPhase("describe");
    }
  };

  const saveDraft = async (index: number) => {
    const d = drafts[index];
    setError(null);
    try {
      await api.createSkill({
        workspace_id: workspaceId,
        personal,
        name: d.name.trim(),
        description: d.description ?? "",
        instructions: d.instructions ?? "",
        must_not: d.must_not ?? "",
      });
      setSaved((prev) => new Set(prev).add(index));
      onSaved();
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    }
  };

  const editDraft = (index: number, patch: Partial<SkillDraft>) =>
    setDrafts((prev) => prev.map((d, i) => (i === index ? { ...d, ...patch } : d)));

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onClose()}
      width={672}
      dismissible={phase !== "review" || saved.size === drafts.length}
      title="Generate skills with AI"
      description="Runs on your own CLI login. Drafts are yours to edit — nothing is saved until you say so."
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            {phase === "review" ? "Done" : "Cancel"}
          </Button>
          {phase === "describe" && (
            <Button variant="primary" onClick={() => void generate()} disabled={!description.trim()}>
              Generate
            </Button>
          )}
          {phase === "review" && <Button onClick={() => setPhase("describe")}>Regenerate</Button>}
        </>
      }
    >
      {phase === "describe" && (
        <div className="flex flex-col gap-3">
          <Textarea
            autoFocus
            aria-label="What the skills are for"
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            rows={4}
            placeholder="Describe the job… e.g. “how we write and review database migrations: reversible, tested against a copy of production data”"
          />
          <label className="flex items-center gap-2 text-xs text-fg-muted">
            Written by
            <TierPicker value={tier} onChange={setTier} />
          </label>
          {error && <p className="rounded-lg bg-danger-subtle px-3 py-2 text-xs text-danger-fg">{error}</p>}
        </div>
      )}

      {phase === "generating" && (
        <div className="flex flex-col items-center gap-3 py-12" role="status">
          <div className="size-8 animate-spin rounded-full border-2 border-accent border-t-transparent" />
          <div className="text-sm text-fg-muted">Writing your skills… this is one model request.</div>
        </div>
      )}

      {phase === "review" && (
        <div className="flex flex-col gap-4">
          <div className="flex items-start gap-3 rounded-lg border border-border bg-panel-2 px-3 py-2.5">
            <Switch checked={personal} onChange={setPersonal} label="Save as personal skills" />
            <div className="min-w-0">
              <div className="text-[13px] text-fg">Save as personal skills</div>
              <div className="text-xs leading-relaxed text-fg-muted">
                {user ? "Yours" : "The machine's"}, offered in every workspace
                {user ? " you have" : ""} — not only this one.
              </div>
            </div>
          </div>
          {drafts.map((d, i) => (
            <div key={i} className="rounded-xl border border-border p-4">
              <div className="grid gap-3">
                <Field label="Name" hint="What you write after @ to use it.">
                  {(id) => (
                    <Input
                      id={id}
                      className="font-mono"
                      value={d.name}
                      onChange={(e) => editDraft(i, { name: e.target.value })}
                      disabled={saved.has(i)}
                    />
                  )}
                </Field>
                <Field label="When to use it">
                  {(id) => (
                    <Input
                      id={id}
                      value={d.description ?? ""}
                      onChange={(e) => editDraft(i, { description: e.target.value })}
                      disabled={saved.has(i)}
                    />
                  )}
                </Field>
                <Field label="How it's done">
                  {(id) => (
                    <Textarea
                      id={id}
                      rows={6}
                      className="font-mono text-[12px]"
                      value={d.instructions ?? ""}
                      onChange={(e) => editDraft(i, { instructions: e.target.value })}
                      disabled={saved.has(i)}
                    />
                  )}
                </Field>
                <Field label="Never">
                  {(id) => (
                    <Textarea
                      id={id}
                      rows={2}
                      className="font-mono text-[12px]"
                      value={d.must_not ?? ""}
                      onChange={(e) => editDraft(i, { must_not: e.target.value })}
                      disabled={saved.has(i)}
                    />
                  )}
                </Field>
              </div>
              <div className="mt-3 flex justify-end">
                {saved.has(i) ? (
                  <span className="text-sm text-success-fg">Saved</span>
                ) : (
                  <Button variant="primary" size="sm" onClick={() => void saveDraft(i)} disabled={!d.name.trim()}>
                    Save skill
                  </Button>
                )}
              </div>
            </div>
          ))}
          {error && <p className="rounded-lg bg-danger-subtle px-3 py-2 text-xs text-danger-fg">{error}</p>}
        </div>
      )}
    </Dialog>
  );
}
