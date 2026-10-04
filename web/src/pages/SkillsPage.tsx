import { useCallback, useEffect, useRef, useState } from "react";
import { Project, Skill, SkillInstall, api } from "../lib/api";
import { useWorkspace } from "../lib/workspace";
import { Card, Empty, Item, Page, PageHead, Stagger, TintIcon } from "../components/ui/Surface";
import { Icon } from "../components/ui/Icon";
import { Button } from "../components/ui/Button";
import { Dialog } from "../components/ui/Dialog";

/**
 * Skills: a named way of doing something, smaller than an agent.
 *
 * An agent is *who* does the work. A skill is *how* one job is done here — the
 * release checklist, how migrations get written, what a bug report must
 * contain. They compose: pick both on a card.
 *
 * A skill applies only when you name it, never because something matched its
 * description. That is the whole reason it cannot go wrong quietly: if a run
 * behaves oddly, the cause is a name you typed, not an invisible list of
 * everything switched on.
 */
export default function SkillsPage() {
  const { active } = useWorkspace();
  const [skills, setSkills] = useState<Skill[]>([]);
  const [editing, setEditing] = useState<Skill | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [installing, setInstalling] = useState(false);

  const load = useCallback(() => {
    if (!active) return;
    api.skills(active.id).then((r) => setSkills(r.skills)).catch(() => {});
  }, [active]);
  useEffect(load, [load]);

  const add = async (personal = false) => {
    if (!active) return;
    setError(null);
    try {
      const s = await api.createSkill({
        workspace_id: active.id,
        personal,
        name: `new-skill-${skills.length + 1}`,
        description: "",
        instructions: "",
        must_not: "",
      });
      load();
      setEditing(s);
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    }
  };

  return (
    <Page>
      <PageHead
        title="Skills"
        subtitle={
          <>
            A named way of doing something. An agent is <em>who</em> does the work; a skill is{" "}
            <em>how</em> this kind of job is done here — and they compose. A skill applies only
            when you name it: <code className="rounded bg-panel-2 px-1 font-mono text-[11px]">@its-name</code>{" "}
            in chat, or picked on a card. Never because it looked relevant, which is what stops a
            stale one steering a request that never mentioned it.
          </>
        }
        actions={
          <div className="flex shrink-0 items-center gap-2">
          <Button variant="secondary" size="md" onClick={() => setInstalling(true)}>
            Add from a registry
          </Button>
          <Button
            variant="secondary"
            size="md"
            onClick={() => void add(true)}
            title="A skill of your own, offered in every workspace you have"
          >
            New personal skill
          </Button>
          <Button
            variant="primary"
            size="md"
            onClick={() => void add()}
            icon={<Icon name="plus" size={15} strokeWidth={2.5} />}
          >
            New skill
          </Button>
          </div>
        }
      />

      {error && (
        <div className="mb-4 max-w-xl rounded-xl bg-danger-subtle px-3.5 py-2.5 text-xs text-danger-fg">
          {error}
        </div>
      )}

      <Stagger className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {skills.map((s) => (
          <Item key={s.id}>
            <Card onClick={() => setEditing(s)} className="h-full p-4">
              <div className="flex items-center gap-2.5">
                <TintIcon tint={s.enabled ? "amber" : "slate"} size={34}>
                  <Icon name="skills" size={16} />
                </TintIcon>
                <div className="flex min-w-0 items-baseline gap-2">
                  <span className="min-w-0 truncate font-mono text-sm font-semibold">
                    @{s.name}
                  </span>
                  {!s.enabled && (
                    <span className="shrink-0 rounded-full bg-panel-2 px-2 py-0.5 text-[10px] text-fg-muted">
                      off
                    </span>
                  )}
                  {s.personal && (
                    <span
                      className="shrink-0 rounded-full bg-accent-subtle px-2 py-0.5 text-[10px] text-accent-fg"
                      title="Yours: offered in every workspace you have"
                    >
                      personal
                    </span>
                  )}
                </div>
              </div>
              <p className="mt-3 line-clamp-2 text-xs leading-relaxed text-fg-muted">
                {s.description || "no description yet"}
              </p>
              {s.sourceRepo && (
                <p className="mt-2 truncate font-mono text-[10px] text-fg-muted">
                  mirrors {s.sourceRepo}
                </p>
              )}
              {s.mustNot.trim() && (
                <p className="mt-2 line-clamp-1 rounded-lg bg-warning-subtle px-2 py-1 text-[11px] text-warning-fg">
                  won't: {s.mustNot}
                </p>
              )}
            </Card>
          </Item>
        ))}
        {skills.length === 0 && (
          <div className="sm:col-span-2 lg:col-span-3">
            <Empty
              icon={<Icon name="skills" size={28} />}
              title="No skills yet"
              hint={'A good first one is narrow — "how we write a migration", not "how we code".'}
            />
          </div>
        )}
      </Stagger>

      {installing && active && (
        <InstallFromRegistry
          workspaceId={active.id}
          onClose={() => setInstalling(false)}
          onInstalled={load}
        />
      )}

      {editing && (
        <SkillEditor
          skill={editing}
          onClose={() => setEditing(null)}
          onChanged={(s) => {
            setEditing(s);
            load();
          }}
          onDeleted={() => {
            setEditing(null);
            load();
          }}
        />
      )}
    </Page>
  );
}

function SkillEditor({
  skill,
  onClose,
  onChanged,
  onDeleted,
}: {
  skill: Skill;
  onClose: () => void;
  onChanged: (s: Skill) => void;
  onDeleted: () => void;
}) {
  const [draft, setDraft] = useState(skill);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [trying, setTrying] = useState(false);
  const [tryPrompt, setTryPrompt] = useState("");
  const [result, setResult] = useState<{ output: string; prompt: string } | null>(null);

  const dirty =
    draft.name !== skill.name ||
    draft.description !== skill.description ||
    draft.instructions !== skill.instructions ||
    draft.mustNot !== skill.mustNot;
  // A close request saves an unsaved edit first, but only once per edit: a
  // save the server refuses leaves `dirty` set, and every later close would
  // otherwise just repeat it and never close.
  const closeSaved = useRef(false);
  useEffect(() => {
    closeSaved.current = false;
  }, [draft]);

  const save = async (patch: Partial<Skill> = {}) => {
    setBusy(true);
    setError(null);
    try {
      const next = { ...draft, ...patch };
      const saved = await api.updateSkill(skill.id, {
        name: next.name,
        description: next.description,
        instructions: next.instructions,
        must_not: next.mustNot,
        enabled: next.enabled,
      });
      setDraft(saved);
      onChanged(saved);
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  };

  const runTry = async () => {
    setTrying(true);
    setError(null);
    setResult(null);
    try {
      setResult(await api.trySkill(skill.id, tryPrompt));
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setTrying(false);
    }
  };

  return (
    <Dialog
      open
      // Closing mid-save would drop the answer on the floor, as the scrim click
      // did before. Fields save on blur, and Escape or a click outside closes
      // without one — so an unsaved edit is saved first, and only the next
      // request closes, the way the scrim click used to save and stay open —
      // whether or not that save was accepted.
      onOpenChange={(o) => {
        if (o || busy) return;
        if (dirty && !closeSaved.current) {
          closeSaved.current = true;
          save();
          return;
        }
        onClose();
      }}
      title="Skill"
      width={672}
      footer={
        <>
          <Button variant="ghost" size="sm" onClick={onClose}>
            Done
          </Button>
          <Button
            variant="danger"
            size="sm"
            className="ml-auto"
            onClick={async () => {
              await api.deleteSkill(skill.id);
              onDeleted();
            }}
          >
            Delete
          </Button>
        </>
      }
    >
        <div className="flex items-start justify-end gap-3">
          <label className="flex shrink-0 items-center gap-2 text-xs">
            <input
              type="checkbox"
              checked={draft.enabled}
              disabled={busy}
              onChange={(e) => save({ enabled: e.target.checked })}
            />
            <span className={draft.enabled ? "text-fg" : "text-fg-muted"}>
              {draft.enabled ? "In use" : "Off"}
            </span>
          </label>
        </div>

        <Field label="Name" hint="What you type after @. Shares one namespace with your agents.">
          <input
            value={draft.name}
            onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            onBlur={() => draft.name !== skill.name && save()}
            disabled={busy}
            className="w-full rounded-lg border border-border bg-bg px-2 py-1.5 font-mono text-sm outline-none focus:border-accent"
          />
        </Field>

        <Field label="When to use it" hint="One line. This is all you see in the picker.">
          <input
            value={draft.description}
            onChange={(e) => setDraft({ ...draft, description: e.target.value })}
            onBlur={() => save()}
            disabled={busy}
            placeholder="how we cut a release"
            className="w-full rounded-lg border border-border bg-bg px-2 py-1.5 text-sm outline-none focus:border-accent"
          />
        </Field>

        <Field label="How to do it">
          <textarea
            value={draft.instructions}
            onChange={(e) => setDraft({ ...draft, instructions: e.target.value })}
            onBlur={() => save()}
            disabled={busy}
            rows={8}
            placeholder={"Name the steps, in order.\nSay what the finished thing looks like.\nSay when to stop and ask."}
            className="w-full resize-y rounded-lg border border-border bg-bg p-2 font-mono text-xs leading-relaxed outline-none focus:border-accent"
          />
        </Field>

        {/* Its own field rather than a paragraph in the one above, because
            "explicit about what it should not do" is the part free text always
            omits — a box asks the question. */}
        <Field
          label="What it must not do"
          hint="Kept separate and put last in the prompt, where it is read rather than skimmed."
        >
          <textarea
            value={draft.mustNot}
            onChange={(e) => setDraft({ ...draft, mustNot: e.target.value })}
            onBlur={() => save()}
            disabled={busy}
            rows={3}
            placeholder="never force-push; never edit files outside src/"
            className="w-full resize-y rounded-lg border border-border bg-bg p-2 font-mono text-xs leading-relaxed outline-none focus:border-accent"
          />
        </Field>

        <div className="mt-5 rounded-xl border border-border bg-bg p-3">
          <div className="text-xs font-medium">Try it</div>
          <p className="mt-0.5 text-[11px] leading-relaxed text-fg-muted">
            Runs the skill against one harmless prompt, with{" "}
            <span className="font-medium text-fg">no tools, no repository and no worktree</span> —
            so whatever it says to do, there is nothing here to do it to. This tells you how
            the skill reads, not what it would do to your files.
          </p>
          <div className="mt-2 flex gap-2">
            <input
              value={tryPrompt}
              onChange={(e) => setTryPrompt(e.target.value)}
              placeholder="Describe what you would do for: bump the version to 2.1"
              className="min-w-0 flex-1 rounded-lg border border-border bg-panel px-2 py-1.5 text-xs outline-none focus:border-accent"
            />
            <Button
              variant="secondary"
              size="sm"
              onClick={runTry}
              disabled={trying || !tryPrompt.trim()}
            >
              {trying ? "Trying…" : "Try it"}
            </Button>
          </div>
          {result && (
            <div className="mt-3">
              <pre className="max-h-56 overflow-y-auto whitespace-pre-wrap rounded-lg bg-panel p-2 text-[11px] leading-relaxed">
                {result.output}
              </pre>
              <details className="mt-1.5">
                {/* Half of what a test tells you is whether the skill says what
                    you thought it said. */}
                <summary className="cursor-pointer text-[11px] text-fg-muted">
                  what it was actually sent
                </summary>
                <pre className="mt-1 max-h-56 overflow-y-auto whitespace-pre-wrap rounded-lg bg-panel p-2 font-mono text-[10px] text-fg-muted">
                  {result.prompt}
                </pre>
              </details>
            </div>
          )}
        </div>

        {error && (
          <div className="mt-3 whitespace-pre-wrap rounded-lg bg-danger-subtle px-3 py-2 text-[11px] leading-relaxed text-danger-fg">
            {error}
          </div>
        )}

        <p className="mt-3 text-[11px] leading-relaxed text-fg-muted">
          <span className="font-medium text-fg">No secrets here.</span> This text goes into
          a prompt, so a save containing something key-shaped is refused.
        </p>
    </Dialog>
  );
}

function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="mt-4">
      <span className="mb-1 block text-[11px] font-semibold uppercase tracking-wide text-fg-muted">
        {label}
      </span>
      {children}
      {hint && <p className="mt-1 text-[11px] text-fg-muted/80">{hint}</p>}
    </div>
  );
}

/**
 * Install Agent Skills from a registry into a project.
 *
 * Project-shaped even though the library is workspace-shaped, and the form
 * says why: the files land in one checkout, and that checkout is what an agent
 * reads. The library row that follows is a mirror, reachable from every
 * project — which is the bit worth stating, because "install into project X"
 * and "now @name works everywhere" are surprising together until you know the
 * folder and the row are two different things.
 */
function InstallFromRegistry({
  workspaceId,
  onClose,
  onInstalled,
}: {
  workspaceId: string;
  onClose: () => void;
  onInstalled: () => void;
}) {
  const [projects, setProjects] = useState<Project[]>([]);
  const [projectId, setProjectId] = useState("");
  const [reference, setReference] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<SkillInstall | null>(null);

  useEffect(() => {
    api
      .projects(workspaceId)
      // A document space has no agents to install a skill for, and the server
      // refuses one — so it is not offered.
      .then((r) => {
        const repos = r.projects.filter((p) => p.kind !== "space");
        setProjects(repos);
        setProjectId((id) => id || repos[0]?.id || "");
      })
      .catch(() => {});
  }, [workspaceId]);

  const install = async () => {
    setBusy(true);
    setError(null);
    setResult(null);
    try {
      const out = await api.skillInstall(projectId, reference.trim());
      setResult(out);
      onInstalled();
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      // An install in flight is not abandoned by a stray click or Escape.
      onOpenChange={(o) => !o && !busy && onClose()}
      title="Add skills from a registry"
      description={
        <>
          Installs an Agent Skill with <code className="font-mono">npx skills</code>. The files
          land in the project you pick and are committed, which is what lets a card's worktree
          see them; each skill is also mirrored into this library so you can{" "}
          <code className="font-mono">@name</code> it anywhere.
        </>
      }
      width={512}
      footer={
        <>
          {busy && (
            <span className="mr-auto text-[11px] text-fg-muted">
              fetching the package, then the repository — this takes a moment
            </span>
          )}
          <Button variant="secondary" size="sm" onClick={onClose} disabled={busy}>
            {result ? "Done" : "Cancel"}
          </Button>
          <Button
            variant="primary"
            size="sm"
            onClick={install}
            disabled={busy || !projectId || !reference.trim()}
          >
            {busy ? "Installing…" : result ? "Install another" : "Install"}
          </Button>
        </>
      }
    >
        <label className="block">
          <span className="mb-1 block text-[11px] font-semibold uppercase tracking-wide text-fg-muted">
            Install into
          </span>
          <select
            value={projectId}
            onChange={(e) => setProjectId(e.target.value)}
            className="w-full rounded-lg border border-border bg-panel px-2.5 py-1.5 text-sm outline-none focus:border-accent"
          >
            {projects.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </label>

        <label className="mt-3 block">
          <span className="mb-1 block text-[11px] font-semibold uppercase tracking-wide text-fg-muted">
            Repository
          </span>
          <input
            value={reference}
            onChange={(e) => setReference(e.target.value)}
            placeholder="vercel-labs/agent-skills"
            className="w-full rounded-lg border border-border bg-panel px-2.5 py-1.5 font-mono text-sm outline-none focus:border-accent"
          />
          <span className="mt-1 block text-[11px] text-fg-muted">
            owner/repo, or a link from github.com or skills.sh.
          </span>
        </label>

        <p className="mt-3 rounded-lg bg-warning-subtle px-2.5 py-2 text-[11px] leading-relaxed text-warning-fg">
          A skill is instructions, and sometimes scripts, written by somebody else — and it runs
          with whatever permissions you give the agent. Read one before you rely on it; what it
          brought with it is listed below once it lands.
        </p>

        {error && (
          <div className="mt-3 rounded-lg bg-danger-subtle px-2.5 py-2 text-xs text-danger-fg">{error}</div>
        )}

        {result && (
          <div className="mt-3 space-y-2">
            <p className="text-xs">
              Installed {result.skills.length}{" "}
              {result.skills.length === 1 ? "skill" : "skills"}
              {result.committed ? " and committed them." : " — but the commit did not happen, so a card's worktree will not see them yet."}
            </p>
            {result.skills.map((s) => (
              <div key={s.name} className="rounded-lg border border-border p-2.5">
                <div className="font-mono text-xs font-semibold">@{s.name}</div>
                <p className="mt-0.5 line-clamp-2 text-[11px] text-fg-muted">{s.description}</p>
                {s.bundled.length > 0 && (
                  <p className="mt-1 text-[11px] text-warning-fg">
                    ships {s.bundled.length} file{s.bundled.length === 1 ? "" : "s"}:{" "}
                    <span className="font-mono">{s.bundled.slice(0, 4).join(", ")}</span>
                    {s.bundled.length > 4 && ` and ${s.bundled.length - 4} more`}
                  </p>
                )}
                {s.mirrorError && (
                  <p className="mt-1 text-[11px] text-danger">{s.mirrorError}</p>
                )}
              </div>
            ))}
          </div>
        )}

    </Dialog>
  );
}
