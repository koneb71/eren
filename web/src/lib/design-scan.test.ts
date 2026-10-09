import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

// The design system holds only while nothing works around it. Every colour on
// screen comes from a token in index.css — which is what makes dark mode one
// block of variables rather than a second set of classes — and every overlay
// comes from the kit, which is what gives it a focus trap, Escape and a label.
// A screen that hard-codes `bg-white` or rolls its own scrim looks fine in the
// review it was written for and breaks in the theme nobody checked, so this
// reads the source and refuses the shortcuts.

// Read as text through Vite's own glob, so the check covers exactly the files
// the app is built from. Tests are left out: they quote what they forbid.
const RAW = import.meta.glob(["../**/*.ts", "../**/*.tsx", "!../**/*.test.ts", "!../**/*.test.tsx"], {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;

const FILES = Object.entries(RAW).map(([path, text]) => ({
  // Keys are relative to this file; name each from src/, as people do.
  path: new URL(path, "file:///src/lib/").pathname.replace(/^\/src\//, ""),
  text,
}));

/** Every match of `pattern` outside `allowed`, as "file:line  match". */
function offences(pattern: RegExp, allowed: (path: string) => boolean): string[] {
  return FILES.filter((f) => !allowed(f.path)).flatMap((f) =>
    f.text.split("\n").flatMap((line, i) =>
      [...line.matchAll(pattern)].map((m) => `${f.path}:${i + 1}  ${m[0]}`),
    ),
  );
}

// The places allowed to do what screens may not.
const KIT = (p: string) => p.startsWith("components/ui/");
const SHELL = (p: string) => p.startsWith("components/shell/") || p === "AppShell.tsx";
// Code surfaces (Monaco, xterm, the editor island) have themes of their own
// that tokens cannot reach, and agent colours are data a person picks.
const COLOUR_DATA = (p: string) => p === "theme/editorThemes.ts" || p === "lib/swatches.ts";

const PALETTE =
  /\b(?:bg|text|border|ring|ring-offset|from|to|via|fill|stroke|outline|divide|placeholder|accent|caret|decoration|shadow)-(?:slate|gray|zinc|neutral|stone|red|orange|amber|yellow|lime|green|emerald|teal|cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose)-\d{2,3}\b/g;
const HEX = /#(?:[0-9a-fA-F]{6}|[0-9a-fA-F]{3})\b/g;
const ABSOLUTE = /\b(?:bg|text|border|from|to|via|ring|fill|stroke)-(?:white|black)\b/g;
const SCRIM = /fixed inset-0/g;

describe("design scan", () => {
  it("reads the whole tree", () => {
    // A path mistake would make every check below pass on nothing.
    expect(FILES.length).toBeGreaterThan(150);
    expect(FILES.some((f) => f.path === "AppShell.tsx")).toBe(true);
  });

  it("uses tokens, never Tailwind's palette", () => {
    expect(offences(PALETTE, () => false)).toEqual([]);
  });

  it("keeps raw hex to the code-surface themes and agent swatches", () => {
    expect(offences(HEX, COLOUR_DATA)).toEqual([]);
  });

  it("never paints white or black, which are only right in one theme", () => {
    expect(offences(ABSOLUTE, KIT)).toEqual([]);
  });

  it("leaves overlays to the kit, which traps focus and closes on Escape", () => {
    expect(offences(SCRIM, (p) => KIT(p) || SHELL(p))).toEqual([]);
  });

  it("catches each thing it forbids", () => {
    // The patterns themselves, against one example each and one near miss.
    const hit = (re: RegExp, s: string) => [...s.matchAll(re)].length > 0;
    expect(hit(PALETTE, 'className="bg-gray-50"')).toBe(true);
    expect(hit(PALETTE, 'className="bg-panel-2 text-fg-muted"')).toBe(false);
    expect(hit(HEX, 'style={{ color: "#9ca3af" }}')).toBe(true);
    expect(hit(HEX, "issue #1234")).toBe(false);
    expect(hit(ABSOLUTE, "text-white")).toBe(true);
    expect(hit(ABSOLUTE, "text-whitespace")).toBe(false);
    expect(hit(SCRIM, '<div className="fixed inset-0 z-10" />')).toBe(true);
  });
});

// The dashboard's content-security policy allows exactly the inline scripts
// the served page carries, by hash (`crates/eren-server/src/csp.rs`). One is
// there on purpose — the theme before first paint. A second one, or an
// inline handler, would be blocked by the browser and fail silently, so the
// page is held to what the policy was written for.
describe("index.html stays within the content-security policy", () => {
  const html = readFileSync(new URL("../../index.html", import.meta.url), "utf8");

  it("has exactly one inline script, the theme block", () => {
    const inline = [...html.matchAll(/<script(?![^>]*\bsrc=)[^>]*>/g)];
    expect(inline).toHaveLength(1);
    expect(html).toContain('localStorage.getItem("eren.theme")');
  });

  it("has no inline event handlers or javascript: URLs", () => {
    expect(html.match(/\son[a-z]+=/i)).toBeNull();
    expect(html).not.toMatch(/javascript:/i);
  });
});
