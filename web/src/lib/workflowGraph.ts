/**
 * Bidirectional bridge between workflow YAML and the visual canvas.
 *
 * YAML stays the source of truth — the Rust executor parses it and users
 * commit it to `.eren/workflows/`. The canvas is a view: we parse YAML
 * into nodes/edges, and re-emit YAML when the graph is edited.
 */
import { load } from "js-yaml";

export interface StepData {
  id: string;
  prompt: string;
  /** ids of steps that must finish first (edge sources) */
  needs: string[];
  agent?: string;
  /** tier name ("easy"|"medium"|"complex") or a literal model id */
  model?: string;
  /** Overrides `defaults.engine` for this step alone. */
  engine?: string;
  session?: "fresh" | "continue";
  parallel?: number;
  isolatedWorktrees?: boolean;
}

export interface WorkflowMeta {
  name: string;
  description?: string;
  schedule?: string;
  engine?: string;
  /** `defaults.model`: tier name or literal model id for every step that names none. */
  model?: string;
  permissionMode?: string;
}

export interface ParsedWorkflow {
  meta: WorkflowMeta;
  steps: StepData[];
}

export interface Position {
  x: number;
  y: number;
}

const asString = (v: unknown): string | undefined =>
  typeof v === "string" && v.trim() ? v : undefined;

/** Never throws: the canvas has to render *something* while you type, and
 *  half-written YAML is the normal state of an editor. */
export function parseWorkflow(yaml: string): ParsedWorkflow {
  let parsed: unknown;
  try {
    parsed = load(yaml);
  } catch {
    parsed = null;
  }
  const raw = (parsed ?? {}) as Record<string, any>;
  const defaults = (raw.defaults ?? {}) as Record<string, any>;
  const steps: StepData[] = Array.isArray(raw.steps)
    ? raw.steps.map((s: Record<string, any>, i: number) => ({
        id: asString(s?.id) ?? `step_${i + 1}`,
        prompt: typeof s?.prompt === "string" ? s.prompt : "",
        needs: Array.isArray(s?.needs) ? s.needs.filter((n: unknown) => typeof n === "string") : [],
        agent: asString(s?.agent),
        model: asString(s?.model),
        engine: asString(s?.engine),
        session: s?.session === "continue" ? "continue" : undefined,
        parallel: typeof s?.strategy?.parallel === "number" ? s.strategy.parallel : undefined,
        isolatedWorktrees:
          s?.strategy?.isolated_worktrees === true || s?.strategy?.isolatedWorktrees === true,
      }))
    : [];

  return {
    meta: {
      name: asString(raw.name) ?? "untitled",
      description: asString(raw.description),
      schedule: asString(raw.on?.schedule),
      engine: asString(defaults.engine),
      model: asString(defaults.model),
      permissionMode: asString(defaults.permission_mode ?? defaults.permissionMode),
    },
    steps,
  };
}

/** Quote only when YAML would otherwise misread the value. */
function scalar(value: string): string {
  const risky = /^[\s>|&*!%@`{}[\]#-]|:\s|\s#|^$|^(true|false|null|yes|no|on|off|~)$/i;
  return risky.test(value) || value !== value.trim()
    ? JSON.stringify(value)
    : value;
}

/**
 * Emit canonical YAML. Hand-rolled rather than a generic dumper so prompts
 * stay readable as block scalars and the field order is stable — which
 * keeps git diffs small when only one step changes.
 */
export function emitWorkflow(meta: WorkflowMeta, steps: StepData[]): string {
  const lines: string[] = [`name: ${scalar(meta.name)}`];
  if (meta.description) lines.push(`description: ${scalar(meta.description)}`);
  if (meta.schedule) lines.push("on:", `  schedule: ${JSON.stringify(meta.schedule)}`);

  const defaults: string[] = [];
  if (meta.engine) defaults.push(`  engine: ${meta.engine}`);
  if (meta.model) defaults.push(`  model: ${scalar(meta.model)}`);
  if (meta.permissionMode) defaults.push(`  permission_mode: ${meta.permissionMode}`);
  if (defaults.length) lines.push("defaults:", ...defaults);

  lines.push("steps:");
  for (const step of steps) {
    lines.push(`  - id: ${step.id}`);
    if (step.needs.length) lines.push(`    needs: [${step.needs.join(", ")}]`);
    if (step.agent) lines.push(`    agent: ${scalar(step.agent)}`);
    if (step.model) lines.push(`    model: ${step.model}`);
    if (step.engine) lines.push(`    engine: ${step.engine}`);
    if (step.session === "continue") lines.push("    session: continue");
    const parallel = step.parallel ?? 1;
    if (parallel > 1 || step.isolatedWorktrees) {
      const parts = [`parallel: ${parallel}`];
      if (step.isolatedWorktrees) parts.push("isolated_worktrees: true");
      lines.push(`    strategy: { ${parts.join(", ")} }`);
    }
    // Prompt last: it's the tallest field, so diffs stay readable.
    //
    // A block scalar guesses its indentation from its first non-empty line,
    // so a prompt that opens indented (a code sample, a quoted list) would
    // lose those spaces — or, if a later line is indented less, be invalid
    // YAML. Then the indentation is said outright: `|2` is two past the step
    // mapping's four, the six every line below is written at.
    const body = (step.prompt || "").split("\n");
    lines.push(body.some((l) => l.startsWith(" ")) ? "    prompt: |2" : "    prompt: |");
    for (const line of body) {
      lines.push(line ? `      ${line}` : "");
    }
  }
  return lines.join("\n") + "\n";
}

/** Longest path from a root, so dependents always sit right of their deps. */
export function depthOf(steps: StepData[]): Map<string, number> {
  const byId = new Map(steps.map((s) => [s.id, s]));
  const memo = new Map<string, number>();
  const visiting = new Set<string>();

  const depth = (id: string): number => {
    const cached = memo.get(id);
    if (cached !== undefined) return cached;
    if (visiting.has(id)) return 0; // malformed cycle: don't hang
    visiting.add(id);
    const step = byId.get(id);
    const value =
      !step || step.needs.length === 0
        ? 0
        : Math.max(...step.needs.map((n) => (byId.has(n) ? depth(n) + 1 : 0)));
    visiting.delete(id);
    memo.set(id, value);
    return value;
  };

  return new Map(steps.map((s) => [s.id, depth(s.id)]));
}

const COLUMN = 280;
const ROW = 150;

/** Auto-arrange by dependency depth; saved positions win where present. */
export function layoutSteps(
  steps: StepData[],
  saved: Record<string, Position> = {},
): Record<string, Position> {
  const depths = depthOf(steps);
  const perColumn = new Map<number, number>();
  const positions: Record<string, Position> = {};

  for (const step of steps) {
    const depth = depths.get(step.id) ?? 0;
    const row = perColumn.get(depth) ?? 0;
    perColumn.set(depth, row + 1);
    positions[step.id] = saved[step.id] ?? { x: depth * COLUMN, y: row * ROW };
  }
  return positions;
}

/**
 * Whether the editor holds work its saved workflow does not: different YAML
 * (both sides as `emitWorkflow` writes it, so formatting is not an edit), or a
 * step that now sits somewhere else. Positions are compared as laid out, since
 * the canvas commits every node after a drag — including the ones still where
 * the auto-layout put them.
 */
export function workflowChanged(
  saved: { yaml: string; layout: Record<string, Position> },
  draft: { yaml: string; steps: StepData[]; positions: Record<string, Position> },
): boolean {
  if (draft.yaml !== saved.yaml) return true;
  const was = layoutSteps(draft.steps, saved.layout);
  const now = layoutSteps(draft.steps, draft.positions);
  return draft.steps.some((s) => was[s.id].x !== now[s.id].x || was[s.id].y !== now[s.id].y);
}

/** Every step `from` transitively depends on. */
function ancestors(steps: StepData[], from: string): Set<string> {
  const byId = new Map(steps.map((s) => [s.id, s]));
  const seen = new Set<string>();
  const stack = [...(byId.get(from)?.needs ?? [])];
  while (stack.length) {
    const id = stack.pop()!;
    if (seen.has(id)) continue;
    seen.add(id);
    stack.push(...(byId.get(id)?.needs ?? []));
  }
  return seen;
}

/**
 * Would connecting `source → target` (target gains a dependency on source)
 * close a loop? Checked before the edge is added, so the canvas can refuse
 * it rather than saving a workflow the executor will reject.
 */
export function wouldCycle(steps: StepData[], source: string, target: string): boolean {
  if (source === target) return true;
  return ancestors(steps, source).has(target);
}

/** A step id that is unique and safe for the executor (`[A-Za-z0-9_-]`). */
export function uniqueStepId(steps: StepData[], base = "step"): string {
  const slug = base.toLowerCase().replace(/[^a-z0-9_-]+/g, "_").replace(/^_+|_+$/g, "") || "step";
  const taken = new Set(steps.map((s) => s.id));
  if (!taken.has(slug)) return slug;
  for (let i = 2; ; i++) {
    const candidate = `${slug}_${i}`;
    if (!taken.has(candidate)) return candidate;
  }
}

/** Rename a step and repoint every `needs` reference at it. */
export function renameStep(steps: StepData[], from: string, to: string): StepData[] {
  return steps.map((s) => ({
    ...s,
    id: s.id === from ? to : s.id,
    needs: s.needs.map((n) => (n === from ? to : n)),
  }));
}

/** Remove a step and drop dangling references to it. */
export function removeStep(steps: StepData[], id: string): StepData[] {
  return steps
    .filter((s) => s.id !== id)
    .map((s) => ({ ...s, needs: s.needs.filter((n) => n !== id) }));
}
