import { describe, expect, it } from "vitest";
import { load } from "js-yaml";
import {
  depthOf,
  emitWorkflow,
  layoutSteps,
  parseWorkflow,
  removeStep,
  renameStep,
  uniqueStepId,
  wouldCycle,
  workflowChanged,
} from "./workflowGraph";

const DEBATE = `name: fix-hard-bug
description: Attempts in parallel, then a judge
on:
  schedule: "0 3 * * *"
defaults:
  engine: claude-code
  permission_mode: auto_edit
steps:
  - id: triage
    model: easy
    prompt: |
      Reproduce the bug.
      Summarize the root cause.
  - id: attempt
    needs: [triage]
    model: medium
    strategy: { parallel: 3, isolated_worktrees: true }
    prompt: |
      Fix it: {{ steps.triage.output }}
  - id: judge
    needs: [attempt]
    agent: Reviewer
    session: continue
    prompt: |
      Pick the best fix.
`;

describe("parsing", () => {
  it("reads meta, steps, and strategy", () => {
    const { meta, steps } = parseWorkflow(DEBATE);
    expect(meta).toEqual({
      name: "fix-hard-bug",
      description: "Attempts in parallel, then a judge",
      schedule: "0 3 * * *",
      engine: "claude-code",
      permissionMode: "auto_edit",
    });
    expect(steps.map((s) => s.id)).toEqual(["triage", "attempt", "judge"]);
    expect(steps[1]).toMatchObject({
      needs: ["triage"],
      model: "medium",
      parallel: 3,
      isolatedWorktrees: true,
    });
    expect(steps[2]).toMatchObject({ agent: "Reviewer", session: "continue" });
    expect(steps[0].prompt).toBe("Reproduce the bug.\nSummarize the root cause.\n");
  });

  it("survives malformed input instead of throwing", () => {
    expect(parseWorkflow("").steps).toEqual([]);
    expect(parseWorkflow("name: x").steps).toEqual([]);
    // A step missing its id still gets one so the canvas can render it.
    expect(parseWorkflow("name: x\nsteps:\n  - prompt: hi\n").steps[0].id).toBe("step_1");
  });
});

describe("round-tripping", () => {
  it("re-emits YAML that parses back to the same graph", () => {
    const first = parseWorkflow(DEBATE);
    const yaml = emitWorkflow(first.meta, first.steps);
    const second = parseWorkflow(yaml);
    expect(second.meta).toEqual(first.meta);
    expect(second.steps).toEqual(first.steps);
  });

  it("emits prompts as readable block scalars", () => {
    const yaml = emitWorkflow({ name: "t" }, [
      { id: "a", prompt: "line one\nline two", needs: [] },
    ]);
    expect(yaml).toContain("    prompt: |\n      line one\n      line two");
  });

  it("keeps defaults.model, in its place among the defaults", () => {
    const source = DEBATE.replace(
      "  engine: claude-code\n",
      "  engine: claude-code\n  model: complex\n",
    );
    const first = parseWorkflow(source);
    expect(first.meta.model).toBe("complex");
    const yaml = emitWorkflow(first.meta, first.steps);
    expect(yaml).toContain(
      "defaults:\n  engine: claude-code\n  model: complex\n  permission_mode: auto_edit\n",
    );
    expect(parseWorkflow(yaml).meta).toEqual(first.meta);
  });

  it("keeps a prompt whose first line is indented more than the rest", () => {
    const prompt = "  indented first\nthen flush\n";
    const yaml = emitWorkflow({ name: "t" }, [{ id: "a", prompt, needs: [] }]);
    // Must be valid YAML, and say exactly what was typed.
    const doc = load(yaml) as { steps: { prompt: string }[] };
    expect(doc.steps[0].prompt).toBe(prompt);
    expect(parseWorkflow(yaml).steps[0].prompt).toBe(prompt);
  });

  it("keeps the leading spaces of a prompt indented throughout", () => {
    const prompt = "    code()\n    more()\n";
    const yaml = emitWorkflow({ name: "t" }, [{ id: "a", prompt, needs: [] }]);
    expect(yaml).toContain("    prompt: |2\n");
    expect(parseWorkflow(yaml).steps[0].prompt).toBe(prompt);
    // And stays stable through another save.
    const again = parseWorkflow(yaml);
    expect(emitWorkflow(again.meta, again.steps)).toBe(yaml);
  });

  it("keeps a prompt that opens with a line of spaces", () => {
    const prompt = "   \nbody\n";
    const yaml = emitWorkflow({ name: "t" }, [{ id: "a", prompt, needs: [] }]);
    expect(parseWorkflow(yaml).steps[0].prompt).toBe(prompt);
  });

  it("omits optional fields rather than writing empty ones", () => {
    const yaml = emitWorkflow({ name: "t" }, [{ id: "a", prompt: "x", needs: [] }]);
    expect(yaml).not.toContain("needs:");
    expect(yaml).not.toContain("strategy:");
    expect(yaml).not.toContain("agent:");
    expect(yaml).not.toContain("description:");
  });

  it("quotes values that YAML would otherwise misread", () => {
    const yaml = emitWorkflow({ name: "no" }, [{ id: "a", prompt: "x", needs: [] }]);
    // Bare `no` would parse as boolean false.
    expect(parseWorkflow(yaml).meta.name).toBe("no");

    const colon = emitWorkflow({ name: "a: b", description: "* starts risky" }, [
      { id: "s", prompt: "p", needs: [] },
    ]);
    expect(parseWorkflow(colon).meta.name).toBe("a: b");
    expect(parseWorkflow(colon).meta.description).toBe("* starts risky");
  });
});

describe("layout", () => {
  it("places dependents to the right of their dependencies", () => {
    const { steps } = parseWorkflow(DEBATE);
    const depths = depthOf(steps);
    expect(depths.get("triage")).toBe(0);
    expect(depths.get("attempt")).toBe(1);
    expect(depths.get("judge")).toBe(2);

    const positions = layoutSteps(steps);
    expect(positions.triage.x).toBeLessThan(positions.attempt.x);
    expect(positions.attempt.x).toBeLessThan(positions.judge.x);
  });

  it("stacks independent steps in the same column", () => {
    const { steps } = parseWorkflow(
      "name: t\nsteps:\n  - id: a\n    prompt: x\n  - id: b\n    prompt: y\n",
    );
    const positions = layoutSteps(steps);
    expect(positions.a.x).toBe(positions.b.x);
    expect(positions.a.y).not.toBe(positions.b.y);
  });

  it("honors saved positions over auto-layout", () => {
    const { steps } = parseWorkflow(DEBATE);
    const positions = layoutSteps(steps, { judge: { x: 5, y: 7 } });
    expect(positions.judge).toEqual({ x: 5, y: 7 });
    expect(positions.triage).not.toEqual({ x: 5, y: 7 });
  });
});

describe("cycle prevention", () => {
  const { steps } = parseWorkflow(DEBATE);

  it("refuses self-links and back-edges", () => {
    expect(wouldCycle(steps, "triage", "triage")).toBe(true);
    // judge already depends on triage transitively, so triage→judge is fine
    // but judge→triage would close the loop.
    expect(wouldCycle(steps, "judge", "triage")).toBe(true);
    expect(wouldCycle(steps, "attempt", "triage")).toBe(true);
  });

  it("allows genuinely new edges", () => {
    expect(wouldCycle(steps, "triage", "judge")).toBe(false);
  });
});

describe("editing helpers", () => {
  it("keeps generated ids unique and executor-safe", () => {
    const steps = parseWorkflow(DEBATE).steps;
    expect(uniqueStepId(steps, "New Step!")).toBe("new_step");
    expect(uniqueStepId(steps, "triage")).toBe("triage_2");
  });

  it("repoints dependencies when a step is renamed", () => {
    const renamed = renameStep(parseWorkflow(DEBATE).steps, "triage", "diagnose");
    expect(renamed[0].id).toBe("diagnose");
    expect(renamed[1].needs).toEqual(["diagnose"]);
  });

  it("drops dangling references when a step is deleted", () => {
    const pruned = removeStep(parseWorkflow(DEBATE).steps, "triage");
    expect(pruned.map((s) => s.id)).toEqual(["attempt", "judge"]);
    expect(pruned[0].needs).toEqual([]);
  });
});

describe("workflowChanged", () => {
  const { meta, steps } = parseWorkflow(DEBATE);
  const yaml = emitWorkflow(meta, steps);
  const saved = { yaml, layout: {} };

  it("is clean when nothing was touched", () => {
    expect(workflowChanged(saved, { yaml, steps, positions: {} })).toBe(false);
  });
  it("is clean when a drag commits the auto-laid positions unchanged", () => {
    expect(workflowChanged(saved, { yaml, steps, positions: layoutSteps(steps) })).toBe(false);
  });
  it("sees a moved step", () => {
    expect(workflowChanged(saved, { yaml, steps, positions: { triage: { x: 999, y: 0 } } })).toBe(true);
  });
  it("sees a step moved back to where it was saved as clean", () => {
    const layout = { triage: { x: 5, y: 7 } };
    expect(workflowChanged({ yaml, layout }, { yaml, steps, positions: { triage: { x: 5, y: 7 } } })).toBe(false);
  });
  it("sees an edit to the workflow itself", () => {
    const edited = emitWorkflow({ ...meta, name: "renamed" }, steps);
    expect(workflowChanged(saved, { yaml: edited, steps, positions: {} })).toBe(true);
  });
});
