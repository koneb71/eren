import type { SkillDraft } from "./api";

/**
 * Generated drafts, made safe to render.
 *
 * The server hands the model's JSON through as it came, and a model does not
 * always write the shape it was asked for: a draft with no name, a name that
 * is a number, instructions as a list of lines. The review screen calls
 * `.trim()` on these, so anything that is not a draft with a name is dropped
 * here, where the drafts arrive, and every other field becomes a string or
 * nothing.
 */
export function coerceSkillDrafts(raw: unknown): SkillDraft[] {
  if (!Array.isArray(raw)) return [];
  const out: SkillDraft[] = [];
  for (const d of raw) {
    if (!d || typeof d !== "object" || Array.isArray(d)) continue;
    const r = d as Record<string, unknown>;
    if (typeof r.name !== "string" || !r.name.trim()) continue;
    out.push({
      name: r.name,
      description: text(r.description),
      instructions: text(r.instructions),
      must_not: text(r.must_not),
    });
  }
  return out;
}

function text(v: unknown): string | undefined {
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  if (Array.isArray(v) && v.every((x) => typeof x === "string")) return v.join("\n");
  return undefined;
}
