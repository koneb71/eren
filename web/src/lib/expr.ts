/**
 * The expression language, in the browser.
 *
 * Used for display-only decisions — chiefly `show_if`, which decides whether a
 * button appears. Anything authoritative (computed columns, defaults, the
 * arguments an action step runs with) is evaluated on the server by
 * `crates/eren-core/src/apps/expr.rs`, and this must never be the thing that
 * decides what gets stored.
 *
 * Two implementations of one language drift. The defence is
 * `crates/eren-core/src/apps/expr_cases.json` — the specification both sides
 * read, and what `expr.test.ts` runs. A case added there fails on whichever
 * side has not caught up.
 *
 * Kept deliberately close to the Rust in shape and in naming, so the two can be
 * read side by side when a case disagrees.
 */

export type Val = null | boolean | number | string;
export type Record_ = Record<string, Val>;

export class ExprError extends Error {}

/** How deep an expression may nest, matching MAX_DEPTH in expr.rs. */
const MAX_DEPTH = 32;

export const FUNCTIONS = [
  "now",
  "today",
  "len",
  "lower",
  "upper",
  "round",
  "abs",
  "coalesce",
  "concat",
] as const;

// ── Tokens ──────────────────────────────────────────────────────────────────

type Tok =
  | { t: "num"; v: number }
  | { t: "str"; v: string }
  | { t: "name"; v: string }
  | { t: "op"; v: string }
  | { t: "(" }
  | { t: ")" }
  | { t: "," };

const TWO = ["==", "!=", "<=", ">=", "&&", "||"];
const ONE = ["+", "-", "*", "/", "%", "<", ">", "!"];

function lex(src: string): Tok[] {
  const out: Tok[] = [];
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    if (/\s/.test(c)) {
      i++;
      continue;
    }
    if (c === "(") { out.push({ t: "(" }); i++; continue; }
    if (c === ")") { out.push({ t: ")" }); i++; continue; }
    if (c === ",") { out.push({ t: "," }); i++; continue; }

    if (c === "'" || c === '"') {
      const quote = c;
      i++;
      let s = "";
      for (;;) {
        if (i >= src.length) throw new ExprError("a quote was opened and never closed");
        if (src[i] === "\\") {
          const next = src[i + 1];
          if (next === undefined) throw new ExprError("a quote was opened and never closed");
          s += next === "n" ? "\n" : next === "t" ? "\t" : next;
          i += 2;
          continue;
        }
        if (src[i] === quote) { i++; break; }
        s += src[i];
        i++;
      }
      out.push({ t: "str", v: s });
      continue;
    }

    if (c >= "0" && c <= "9") {
      const start = i;
      while (i < src.length && src[i] >= "0" && src[i] <= "9") i++;
      if (src[i] === "." && src[i + 1] >= "0" && src[i + 1] <= "9") {
        i++;
        while (i < src.length && src[i] >= "0" && src[i] <= "9") i++;
      }
      out.push({ t: "num", v: Number(src.slice(start, i)) });
      continue;
    }

    // A dot lives inside a name so `record.field` is one token. Not property
    // access — there are no objects — just a spelling that reads naturally.
    if (/[A-Za-z_]/.test(c)) {
      const start = i;
      while (i < src.length && /[A-Za-z0-9_.]/.test(src[i])) i++;
      out.push({ t: "name", v: src.slice(start, i) });
      continue;
    }

    const two = src.slice(i, i + 2);
    if (TWO.includes(two)) { out.push({ t: "op", v: two }); i += 2; continue; }
    if (ONE.includes(c)) { out.push({ t: "op", v: c }); i++; continue; }
    // The mistake worth naming: it is what everyone writes the first time.
    if (c === "=") throw new ExprError("use == to compare, not =");
    throw new ExprError(`"${c}" does not mean anything here`);
  }
  return out;
}

// ── Syntax ──────────────────────────────────────────────────────────────────

export type Ast =
  | { k: "lit"; v: Val }
  | { k: "field"; name: string }
  | { k: "unary"; op: string; a: Ast }
  | { k: "binary"; op: string; a: Ast; b: Ast }
  | { k: "call"; name: string; args: Ast[] };

const PRECEDENCE: Record<string, number> = {
  "||": 1, "&&": 2,
  "==": 3, "!=": 3,
  "<": 4, "<=": 4, ">": 4, ">=": 4,
  "+": 5, "-": 5,
  "*": 6, "/": 6, "%": 6,
};

export function parse(src: string): Ast {
  const toks = lex(src);
  if (toks.length === 0) throw new ExprError("this expression is empty");
  let at = 0;
  const peek = () => toks[at];

  function expr(min: number, depth: number): Ast {
    if (depth > MAX_DEPTH) throw new ExprError("this expression is nested too deeply");
    let left = unary(depth);
    for (;;) {
      const tok = peek();
      if (!tok || tok.t !== "op") break;
      const bp = PRECEDENCE[tok.v];
      if (bp === undefined || bp < min) break;
      at++;
      left = { k: "binary", op: tok.v, a: left, b: expr(bp + 1, depth + 1) };
    }
    return left;
  }

  function unary(depth: number): Ast {
    if (depth > MAX_DEPTH) throw new ExprError("this expression is nested too deeply");
    const tok = peek();
    if (tok && tok.t === "op" && (tok.v === "-" || tok.v === "!")) {
      at++;
      return { k: "unary", op: tok.v, a: unary(depth + 1) };
    }
    return atom(depth);
  }

  function atom(depth: number): Ast {
    const tok = peek();
    if (!tok) throw new ExprError("the expression stops before it says anything");
    at++;
    if (tok.t === "num") return { k: "lit", v: tok.v };
    if (tok.t === "str") return { k: "lit", v: tok.v };
    if (tok.t === "(") {
      const inner = expr(0, depth + 1);
      if (peek()?.t !== ")") throw new ExprError("a bracket was opened and never closed");
      at++;
      return inner;
    }
    if (tok.t === ")") throw new ExprError("there is a ) with nothing to close");
    if (tok.t === ",") throw new ExprError("there is a , outside a function call");
    if (tok.t === "op") throw new ExprError(`"${tok.v}" needs something before it`);

    if (tok.v === "true") return { k: "lit", v: true };
    if (tok.v === "false") return { k: "lit", v: false };
    if (tok.v === "null") return { k: "lit", v: null };
    if (peek()?.t === "(") {
      at++;
      const args: Ast[] = [];
      if (peek()?.t !== ")") {
        for (;;) {
          args.push(expr(0, depth + 1));
          if (peek()?.t !== ",") break;
          at++;
        }
      }
      if (peek()?.t !== ")") throw new ExprError(`${tok.v}( was opened and never closed`);
      at++;
      return { k: "call", name: tok.v, args };
    }
    return { k: "field", name: tok.v };
  }

  const ast = expr(0, 0);
  if (at !== toks.length) throw new ExprError("there is more here than one expression");
  return ast;
}

/** Every field an expression reads, once each. */
export function fieldsUsed(ast: Ast): string[] {
  const out: string[] = [];
  const walk = (n: Ast) => {
    switch (n.k) {
      case "field": {
        const bare = n.name.startsWith("record.") ? n.name.slice(7) : n.name;
        if (!out.includes(bare)) out.push(bare);
        break;
      }
      case "unary": walk(n.a); break;
      case "binary": walk(n.a); walk(n.b); break;
      case "call": n.args.forEach(walk); break;
    }
  };
  walk(ast);
  return out;
}

// ── Evaluation ──────────────────────────────────────────────────────────────

/**
 * Null, false, zero and the empty string are false. Chosen to match what
 * someone writing `show_if: "category"` means, which is "when there is one".
 */
export function truthy(v: Val): boolean {
  if (v === null) return false;
  if (typeof v === "boolean") return v;
  if (typeof v === "number") return v !== 0;
  return v.length > 0;
}

function text(v: Val): string {
  if (v === null) return "";
  if (typeof v === "number") {
    return Number.isInteger(v) && Math.abs(v) < 1e15 ? String(v) : String(v);
  }
  return String(v);
}

function typeName(v: Val): string {
  if (v === null) return "nothing";
  if (typeof v === "boolean") return "a true/false";
  if (typeof v === "number") return "a number";
  return "text";
}

export function evaluate(ast: Ast, record: Record_, now: string): Val {
  switch (ast.k) {
    case "lit":
      return ast.v;
    case "field": {
      const bare = ast.name.startsWith("record.") ? ast.name.slice(7) : ast.name;
      // Absent is null, not an error: a record is often half-filled, and
      // `category == ''` should say "not yet" rather than blow up.
      // Own properties only: `in` also sees what every object inherits, so a
      // field called `constructor` or `toString` read back a function.
      return Object.hasOwn(record, bare) ? record[bare] : null;
    }
    case "unary": {
      const a = evaluate(ast.a, record, now);
      if (ast.op === "!") return !truthy(a);
      if (typeof a !== "number") throw new ExprError(`cannot negate ${typeName(a)}`);
      return -a;
    }
    case "binary": {
      // Short-circuit before the other side is touched.
      if (ast.op === "&&") {
        return truthy(evaluate(ast.a, record, now)) && truthy(evaluate(ast.b, record, now));
      }
      if (ast.op === "||") {
        return truthy(evaluate(ast.a, record, now)) || truthy(evaluate(ast.b, record, now));
      }
      return binary(ast.op, evaluate(ast.a, record, now), evaluate(ast.b, record, now));
    }
    case "call":
      return call(ast.name, ast.args.map((a) => evaluate(a, record, now)), now);
  }
}

function binary(op: string, l: Val, r: Val): Val {
  // Equality does not coerce: "1" is not 1. Anything else would make
  // `status == 0` quietly true for an empty status.
  if (op === "==") return l === r;
  if (op === "!=") return l !== r;

  // The one place types need not match, because it is what everyone means by
  // `first + ' ' + last`.
  if (op === "+" && (typeof l === "string" || typeof r === "string")) {
    return text(l) + text(r);
  }

  if (typeof l === "string" && typeof r === "string") {
    switch (op) {
      case "<": return l < r;
      case "<=": return l <= r;
      case ">": return l > r;
      case ">=": return l >= r;
      default: throw new ExprError(`"${op}" does not work on text`);
    }
  }

  if (typeof l !== "number" || typeof r !== "number") {
    // Null propagates: a row with no amount yet has no total yet.
    if (l === null || r === null) return null;
    throw new ExprError(`cannot use "${op}" between ${typeName(l)} and ${typeName(r)}`);
  }

  switch (op) {
    case "+": return l + r;
    case "-": return l - r;
    case "*": return l * r;
    // Null rather than Infinity: a divisor of zero is missing data, and a row
    // showing nothing beats a row showing Infinity.
    case "/": return r === 0 ? null : l / r;
    case "%": return r === 0 ? null : l % r;
    case "<": return l < r;
    case "<=": return l <= r;
    case ">": return l > r;
    case ">=": return l >= r;
    default: throw new ExprError(`"${op}" is not an operator`);
  }
}

function call(name: string, args: Val[], now: string): Val {
  const num = (i: number): number => {
    const v = args[i];
    if (typeof v !== "number") {
      throw new ExprError(
        v === undefined ? `${name} needs another argument` : `${name} wants a number, not ${typeName(v)}`,
      );
    }
    return v;
  };
  switch (name) {
    case "now":
      return now;
    case "today":
      return now.split("T")[0] ?? now;
    case "len": {
      const v = args[0];
      if (v === undefined || v === null) return 0;
      if (typeof v !== "string") throw new ExprError(`len wants text, not ${typeName(v)}`);
      return [...v].length;
    }
    case "lower":
      return text(args[0] ?? null).toLowerCase();
    case "upper":
      return text(args[0] ?? null).toUpperCase();
    case "abs":
      return Math.abs(num(0));
    case "round": {
      // As the Rust does it: places clamped to 0..=10 and then truncated
      // (`as i32`, which also makes NaN 0), and halves rounded away from zero
      // (`f64::round`) — Math.round rounds them up, so -2.5 became -2.
      const places = args.length > 1 ? num(1) : 0;
      const whole = Math.trunc(Math.min(Math.max(places, 0), 10)) || 0;
      const f = Math.pow(10, whole);
      const x = num(0) * f;
      return (Math.sign(x) * Math.round(Math.abs(x))) / f;
    }
    case "coalesce":
      return args.find((v) => v !== null) ?? null;
    case "concat":
      return args.map(text).join("");
    default:
      throw new ExprError(
        `there is no function called ${name} — there is ${FUNCTIONS.join(", ")}`,
      );
  }
}

/** Parse and evaluate in one go. */
export function run(src: string, record: Record_, now: string): Val {
  return evaluate(parse(src), record, now);
}

/**
 * Evaluate a `show_if`, treating a broken one as "show it".
 *
 * A button that vanishes because its condition has a typo is a feature that
 * looks absent rather than broken, and the person who could fix it never finds
 * out. Showing it means the failure surfaces when it is clicked.
 */
export function showIf(src: string | null | undefined, record: Record_, now: string): boolean {
  if (!src) return true;
  try {
    return truthy(run(src, record, now));
  } catch {
    return true;
  }
}
