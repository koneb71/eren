import { describe, expect, it } from "vitest";
import { coerceSkillDrafts } from "./skillDrafts";

describe("coerceSkillDrafts", () => {
  it("keeps well-formed drafts as they are", () => {
    const d = { name: "migrations", description: "d", instructions: "i", must_not: "m" };
    expect(coerceSkillDrafts([d])).toEqual([d]);
  });

  it("drops anything without a non-empty string name", () => {
    expect(
      coerceSkillDrafts([
        { description: "no name" },
        { name: 42 },
        { name: "   " },
        null,
        "a string",
        ["an", "array"],
        { name: "kept" },
      ]).map((d) => d.name),
    ).toEqual(["kept"]);
  });

  it("makes every other field a string or nothing", () => {
    const [d] = coerceSkillDrafts([
      { name: "x", description: 7, instructions: ["one", "two"], must_not: { not: "text" } },
    ]);
    expect(d).toEqual({
      name: "x",
      description: "7",
      instructions: "one\ntwo",
      must_not: undefined,
    });
  });

  it("answers an empty list for something that is not a list", () => {
    expect(coerceSkillDrafts(undefined)).toEqual([]);
    expect(coerceSkillDrafts({ name: "x" })).toEqual([]);
  });
});
