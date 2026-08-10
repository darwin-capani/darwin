/**
 * Scrape the daemon's telemetry emit sites out of `daemon/src/**\/*.rs`, and the
 * HUD's `applyEnvelope` case labels out of `core/state.ts`, so a test can assert
 * that the set of topics the daemon emits WITH NO REDUCER CASE is exactly a
 * checked-in list.
 *
 * Why a scraper and not a hand count: the hand count was wrong in BOTH directions
 * (six live topics called dead because their case label is a CONSTANT; four emits
 * missed because the topic reaches `emit` as a VARIABLE out of an `ev_*` contract
 * builder). Every rule below exists because a naive version of it produced a wrong
 * answer:
 *
 *  - COMMENTS ARE STRIPPED. `daemon/src/**` is heavily commented and the comments
 *    name topics constantly ("without this case ... `standing.tripwire_armed`").
 *    A `grep` for a topic hits prose; only real call syntax counts.
 *  - `#[cfg(test)]` ITEMS ARE CUT. Test modules emit freely; a test-only emit is
 *    not a production pixel-free emit. (`#[cfg(not(test))]` is production and is
 *    deliberately NOT matched.)
 *  - CONSTANT TOPICS ARE RESOLVED on both sides — `emit("system", EV_MODATTEST,…)`
 *    and `case IMAGE_GENERATED_EVENT:` are the same string once resolved.
 *  - VARIABLE TOPICS ARE RESOLVED through the contract builders: a builder is any
 *    fn returning `(&'static str, …)` or `Option<(&'static str, …)>`; the topics
 *    it can return are the string/const heads of the tuples in its body. The emit
 *    site binds them with `let (event, payload) = …` / `if let Some((event, data))`.
 *
 * The extractor is NOT trusted on its own say-so (that would be a guard that
 * self-matches): `pixel-free-emits.test.ts` feeds every extracted topic through the
 * real `reduce` and requires the pixel-free ones to leave state referentially
 * unchanged and the live ones to be reachable case labels.
 */

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = fileURLToPath(new URL(".", import.meta.url));
export const REPO_ROOT = join(HERE, "..", "..", "..");
export const DAEMON_SRC = join(REPO_ROOT, "daemon", "src");
export const STATE_TS = join(REPO_ROOT, "hud", "src", "core", "state.ts");
export const EVENTS_TS = join(REPO_ROOT, "hud", "src", "core", "events.ts");

export interface EmitSite {
  /** Topic string, fully resolved through consts and contract builders. */
  topic: string;
  /** Repo-relative path of the emitting file. */
  file: string;
  /** 1-indexed line of the `telemetry::emit(` call. */
  line: number;
  /** How the topic was written at the call site. */
  via: "literal" | "const" | "builder";
  /** For `via === "builder"`, the builder that produced it. */
  builder?: string;
}

// ---------------------------------------------------------------------------
// Rust lexing: blank out comments, keep offsets (so line numbers stay honest).
// ---------------------------------------------------------------------------

/**
 * Replace every comment byte with a space, preserving newlines and total length.
 * String-aware: `"// not a comment"` and `r#"…"#` survive intact, so a topic
 * literal is never eaten and a `//` inside a URL literal never blanks the rest
 * of the line.
 */
export function stripRustComments(src: string): string {
  const out = src.split("");
  let i = 0;
  const n = src.length;
  const blank = (from: number, to: number) => {
    for (let k = from; k < to && k < n; k++) if (out[k] !== "\n") out[k] = " ";
  };
  while (i < n) {
    const c = src[i];
    // raw string: r"…" / r#"…"# / br#"…"#
    if ((c === "r" || (c === "b" && src[i + 1] === "r")) && /[^A-Za-z0-9_]/.test(src[i - 1] ?? " ")) {
      let j = c === "r" ? i + 1 : i + 2;
      let hashes = 0;
      while (src[j] === "#") {
        hashes++;
        j++;
      }
      if (src[j] === '"') {
        const close = '"' + "#".repeat(hashes);
        const end = src.indexOf(close, j + 1);
        i = end === -1 ? n : end + close.length;
        continue;
      }
    }
    if (c === '"') {
      let j = i + 1;
      while (j < n) {
        if (src[j] === "\\") j += 2;
        else if (src[j] === '"') break;
        else j++;
      }
      i = j + 1;
      continue;
    }
    if (c === "'") {
      // char literal or lifetime. `'a` (lifetime) has no closing quote.
      if (src[i + 1] === "\\") {
        const end = src.indexOf("'", i + 2);
        i = end === -1 ? n : end + 1;
        continue;
      }
      if (src[i + 2] === "'") {
        i += 3;
        continue;
      }
      i += 1;
      continue;
    }
    if (c === "/" && src[i + 1] === "/") {
      let j = src.indexOf("\n", i);
      if (j === -1) j = n;
      blank(i, j);
      i = j;
      continue;
    }
    if (c === "/" && src[i + 1] === "*") {
      let depth = 1;
      let j = i + 2;
      while (j < n && depth > 0) {
        if (src[j] === "/" && src[j + 1] === "*") {
          depth++;
          j += 2;
        } else if (src[j] === "*" && src[j + 1] === "/") {
          depth--;
          j += 2;
        } else j++;
      }
      blank(i, j);
      i = j;
      continue;
    }
    i++;
  }
  return out.join("");
}

/** Index just past the `}` matching the `{` at or after `from`; -1 if none. */
function matchBrace(src: string, openIdx: number): number {
  let depth = 0;
  for (let i = openIdx; i < src.length; i++) {
    if (src[i] === "{") depth++;
    else if (src[i] === "}") {
      depth--;
      if (depth === 0) return i + 1;
    }
  }
  return -1;
}

/**
 * Blank out every `#[cfg(test)]`-gated item. Handles `mod tests { … }`,
 * `#[cfg(test)] fn helper() { … }` and `#[cfg(test)] use …;` (no braces).
 * Matches the EXACT attribute so `#[cfg(not(test))]` — which is production code —
 * is left alone.
 */
export function stripCfgTest(src: string): string {
  const ATTR = "#[cfg(test)]";
  let out = src;
  for (;;) {
    const at = out.indexOf(ATTR);
    if (at === -1) break;
    // Walk forward to the item body: first `{` wins, unless a `;` comes first.
    let i = at + ATTR.length;
    let end = -1;
    let depthParen = 0;
    for (; i < out.length; i++) {
      const c = out[i];
      if (c === "(" || c === "<") depthParen++;
      else if (c === ")" || c === ">") depthParen = Math.max(0, depthParen - 1);
      else if (c === ";" && depthParen === 0) {
        end = i + 1;
        break;
      } else if (c === "{" && depthParen === 0) {
        end = matchBrace(out, i);
        break;
      }
    }
    if (end === -1) end = out.length;
    const chars = out.split("");
    for (let k = at; k < end; k++) if (chars[k] !== "\n") chars[k] = " ";
    out = chars.join("");
  }
  return out;
}

export function rustFiles(dir: string): string[] {
  const acc: string[] = [];
  const walk = (d: string) => {
    for (const name of readdirSync(d).sort()) {
      const p = join(d, name);
      if (statSync(p).isDirectory()) walk(p);
      else if (name.endsWith(".rs")) acc.push(p);
    }
  };
  walk(dir);
  return acc;
}

// ---------------------------------------------------------------------------
// Resolution tables
// ---------------------------------------------------------------------------

const CONST_STR =
  /(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+([A-Z][A-Z0-9_]*)\s*:\s*&(?:'static\s+)?str\s*=\s*"([^"\\]*)"\s*;/g;

/**
 * `NAME -> "value"` for every `const NAME: &str = "…"` in the daemon, both
 * per-file and flattened.
 *
 * Per-file matters: the daemon defines the SAME const name with DIFFERENT values
 * in different modules (measured: API_BASE has 7 values, FAKE_CLIENT_ID has 7,
 * MARKER has 2). None of those is a topic today, but a flat table resolves a
 * collision by whichever file was walked last, which is a silent wrong answer
 * waiting for the first module that names its topic const something generic.
 * Resolution therefore prefers the emitting file's own definition.
 */
export function collectRustConsts(cleaned: Map<string, string>): {
  byFile: Map<string, Map<string, string>>;
  global: Map<string, string>;
} {
  const byFile = new Map<string, Map<string, string>>();
  const global = new Map<string, string>();
  for (const [file, text] of cleaned) {
    const local = new Map<string, string>();
    for (const m of text.matchAll(CONST_STR)) {
      local.set(m[1], m[2]);
      global.set(m[1], m[2]);
    }
    byFile.set(file, local);
  }
  return { byFile, global };
}

/** Signature of a fn whose return type carries `(&'static str, …)`. */
const BUILDER_SIG = /\bfn\s+([a-z_][A-Za-z0-9_]*)\s*\(/g;

/**
 * The telemetry-contract return type, and ONLY that: `(&'static str, Value)` or
 * `Option<(&'static str, Value)>`. Deliberately narrow — a looser "contains
 * `(&'static str,`" test also swept up `netstat_command`, `launch_dirs`,
 * `verdict_label` and `describe_confined_path`, whose first tuple slots are
 * `/usr/sbin/netstat`, `LaunchDaemon`, `admitted` and `system`. None of those is
 * a topic, and any of them could have been mis-attached to an emit whose topic
 * variable happened to be named `event` (trap (g), a guard matching too widely).
 */
const CONTRACT_RET = /^(Option<)?\(\s*&'static\s+str\s*,\s*(?:serde_json::)?Value\s*\)(>)?$/;

/**
 * Every telemetry-contract builder: a fn returning `(&'static str, Value)` or
 * `Option<(&'static str, Value)>`, mapped to the topics its body can return.
 */
export function collectBuilders(
  cleaned: Map<string, string>,
  consts: { byFile: Map<string, Map<string, string>>; global: Map<string, string> },
): Map<string, string[]> {
  const builders = new Map<string, string[]>();
  for (const [file, text] of cleaned) {
    const local = consts.byFile.get(file);
    for (const m of text.matchAll(BUILDER_SIG)) {
      const name = m[1];
      // Params may span lines; find the `)` that closes the arg list, then the
      // `->` return type up to the body `{`.
      let i = m.index! + m[0].length - 1;
      let depth = 0;
      for (; i < text.length; i++) {
        if (text[i] === "(") depth++;
        else if (text[i] === ")") {
          depth--;
          if (depth === 0) break;
        }
      }
      const bodyOpen = text.indexOf("{", i);
      if (bodyOpen === -1) continue;
      const ret = text.slice(i + 1, bodyOpen);
      if (!/->/.test(ret)) continue;
      const retType = ret.slice(ret.indexOf("->") + 2).replace(/\s+/g, " ").trim();
      if (!CONTRACT_RET.test(retType)) continue;
      const bodyEnd = matchBrace(text, bodyOpen);
      const body = text.slice(bodyOpen, bodyEnd === -1 ? text.length : bodyEnd);
      const topics = new Set<string>();
      // Tuple heads: `("topic",` or `(CONST,` — including across a newline.
      for (const t of body.matchAll(/\(\s*"([^"\\]+)"\s*,/g)) topics.add(t[1]);
      for (const t of body.matchAll(/\(\s*([A-Z][A-Z0-9_]*)\s*,/g)) {
        const v = local?.get(t[1]) ?? consts.global.get(t[1]);
        if (v) topics.add(v);
      }
      if (topics.size) builders.set(name, [...topics].sort());
    }
  }
  return builders;
}

// ---------------------------------------------------------------------------
// Emit sites
// ---------------------------------------------------------------------------

/**
 * Any call to `telemetry::emit`. The bare `emit(` alternative is NOT optional
 * decoration: `telemetry.rs` calls its own `emit` unqualified, and that call is
 * `system.load` — the HUD's single busiest reducer case. A qualified-only regex
 * reported it as a `case` with no emitter, which is the same blind spot in
 * mirror image.
 *
 * Only ONE match is not a call: `fn emit(source, event, data)`, the definition
 * itself, excluded by the `fn` guard below. The arity check excludes nothing
 * today — MEASURED: of the 396 `emit(`-shaped tokens left after the comment and
 * `#[cfg(test)]` cuts, 396 parse to exactly 3 top-level arguments, so 395 calls
 * survive both guards. (This comment used to name "1-arg `emit` helpers in
 * artifact.rs / router.rs / anthropic.rs" as the thing arity excludes; there are
 * none — those files' helpers are `emit_peek` / `emit_agent_active` /
 * `emit_payloads`, which the `emit\s*\(` regex never matches. The arity check
 * stays because a helper with a different arity is a plausible next edit, but it
 * is a guard against tomorrow, not a description of today.)
 */
const EMIT = /(?<![A-Za-z0-9_])(?:(?:crate::)?telemetry::)?emit\s*\(/g;

/** `crate::foo::BAR` names another module's const; a bare `BAR` names this one's. */
const qualified = (expr: string) => expr.includes("::");

/** Split a call's argument text on TOP-LEVEL commas. */
function splitArgs(text: string): string[] {
  const args: string[] = [];
  let depth = 0;
  let start = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === "(" || c === "[" || c === "{") depth++;
    else if (c === ")" || c === "]" || c === "}") depth--;
    else if (c === "," && depth === 0) {
      args.push(text.slice(start, i).trim());
      start = i + 1;
    }
  }
  args.push(text.slice(start).trim());
  // Rust allows a trailing comma, and most of the multi-line emits use one; an
  // unfiltered split reports arity 4 and an arity check then drops the call.
  if (args.length > 1 && args[args.length - 1] === "") args.pop();
  return args;
}

/**
 * Resolve a variable topic (`event`, `ev`, …) to the builder that bound it.
 * Looks backwards from the call for the nearest binding of that identifier in a
 * tuple destructure whose right-hand side is a call: `let (event, payload) = …`,
 * `if let Some((event, data)) = …`, `let (event, payload) = match … {}`.
 */
function resolveVarTopic(
  text: string,
  callIdx: number,
  varName: string,
  builders: Map<string, string[]>,
): { topics: string[]; builder: string } | null {
  // Bound the search to the ENCLOSING TOP-LEVEL ITEM. `\n}` at column 0 closes a
  // top-level fn/impl; without this bound the scan would happily reach back into
  // a previous function and attach ITS builder to this emit — a binding that does
  // not dominate the call is not a binding.
  const head = text.slice(0, callIdx);
  const itemStart = head.lastIndexOf("\n}");
  const before = itemStart === -1 ? head : head.slice(itemStart + 1);

  // `let (event, payload) = …;` and `if let Some((event, data)) = … {`. Both
  // terminators are needed: the `if let` form has no `;`, and cutting a `match`
  // RHS at its first `{` loses the arms, so try the `;` form as a fallback.
  const pattern = (term: string) =>
    new RegExp(
      String.raw`(?:let|if\s+let|while\s+let)\s+(?:Some\s*\(\s*)?\(\s*(?:mut\s+)?` +
        varName +
        String.raw`\s*,[^)]*\)\s*\)?\s*=\s*([\s\S]{0,600}?)` +
        term,
      "g",
    );
  for (const term of ["[;{]", ";"]) {
    const re = pattern(term);
    let last: RegExpExecArray | null = null;
    for (let m = re.exec(before); m; m = re.exec(before)) last = m;
    if (!last) continue;
    // Last function-call head in the RHS path: `crate::introspect::ev_security(`.
    const calls = [...last[1].matchAll(/([a-z_][A-Za-z0-9_]*)\s*\(/g)].map((c) => c[1]);
    for (const c of calls) {
      const topics = builders.get(c);
      if (topics) return { topics, builder: c };
    }
  }
  return null;
}

export interface ScanResult {
  sites: EmitSite[];
  /** Emit sites whose topic could not be resolved — must be EMPTY. */
  unresolved: { file: string; line: number; expr: string }[];
  builders: Map<string, string[]>;
  consts: Map<string, string>;
  /** Const names defined more than once with DIFFERENT values, for the record. */
  collidingConsts: string[];
  files: number;
}

export function scanDaemonEmits(daemonSrc = DAEMON_SRC): ScanResult {
  const cleaned = new Map<string, string>();
  for (const p of rustFiles(daemonSrc)) {
    cleaned.set(relative(REPO_ROOT, p), stripCfgTest(stripRustComments(readFileSync(p, "utf8"))));
  }
  const consts = collectRustConsts(cleaned);
  const builders = collectBuilders(cleaned, consts);
  const seen = new Map<string, Set<string>>();
  for (const local of consts.byFile.values()) {
    for (const [k, v] of local) seen.set(k, (seen.get(k) ?? new Set()).add(v));
  }
  const collidingConsts = [...seen].filter(([, v]) => v.size > 1).map(([k]) => k).sort();

  const sites: EmitSite[] = [];
  const unresolved: ScanResult["unresolved"] = [];

  for (const [file, text] of cleaned) {
    const lineOf = (idx: number) => text.slice(0, idx).split("\n").length;
    for (const m of text.matchAll(EMIT)) {
      const open = m.index! + m[0].length - 1;
      const close = (() => {
        let depth = 0;
        for (let i = open; i < text.length; i++) {
          if (text[i] === "(") depth++;
          else if (text[i] === ")") {
            depth--;
            if (depth === 0) return i;
          }
        }
        return -1;
      })();
      if (close === -1) continue;
      const args = splitArgs(text.slice(open + 1, close));
      // `telemetry::emit` is (source, event, data). Anything else with this name
      // is a different function: artifact.rs/router.rs/anthropic.rs each have a
      // 1-arg `emit`, and `fn emit(source: &str, …)` is the definition itself.
      if (args.length !== 3) continue;
      if (/\bfn\s+$/.test(text.slice(Math.max(0, m.index! - 16), m.index!))) continue;
      const expr = args[1].trim();
      const line = lineOf(m.index!);

      const lit = /^"([^"\\]*)"$/.exec(expr);
      if (lit) {
        sites.push({ topic: lit[1], file, line, via: "literal" });
        continue;
      }
      const constName = /^(?:[A-Za-z0-9_]+::)*([A-Z][A-Z0-9_]*)$/.exec(expr);
      if (constName) {
        // The emitting file's own definition wins over a same-named const in
        // some other module (see collectRustConsts).
        const local = qualified(expr) ? undefined : consts.byFile.get(file)?.get(constName[1]);
        const v = local ?? consts.global.get(constName[1]);
        if (v !== undefined) {
          sites.push({ topic: v, file, line, via: "const" });
          continue;
        }
      }
      const varName = /^([a-z_][A-Za-z0-9_]*)$/.exec(expr);
      if (varName) {
        const r = resolveVarTopic(text, m.index!, varName[1], builders);
        if (r) {
          for (const t of r.topics) {
            sites.push({ topic: t, file, line, via: "builder", builder: r.builder });
          }
          continue;
        }
      }
      unresolved.push({ file, line, expr });
    }
  }
  return { sites, unresolved, builders, consts: consts.global, collidingConsts, files: cleaned.size };
}

// ---------------------------------------------------------------------------
// HUD side: applyEnvelope case labels
// ---------------------------------------------------------------------------

/** `NAME -> "value"` for exported string consts in `core/events.ts`. */
export function collectTsConsts(text: string): Map<string, string> {
  const table = new Map<string, string>();
  for (const m of text.matchAll(
    /(?:export\s+)?const\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?::\s*[^=]+)?=\s*"([^"\\]*)"\s*;/g,
  )) {
    table.set(m[1], m[2]);
  }
  return table;
}

/**
 * Blank every TS comment, preserving offsets and every string/template literal.
 * The daemon side has stripped comments from the start (prose names topics
 * constantly); the HUD side did not, and the asymmetry is the same defect one
 * layer over: a commented-out `case "egress.refused":` would be scraped as a
 * live case, silently deleting that topic from the pixel-free ledger with no
 * pixel behind it. Measured today the repo has no such line — 179 labels before
 * the strip, 179 after — so this changes no answer; it removes a way to get a
 * wrong one.
 */
export function stripTsComments(src: string): string {
  const out = src.split("");
  const n = src.length;
  const blank = (from: number, to: number) => {
    for (let k = from; k < to && k < n; k++) if (out[k] !== "\n") out[k] = " ";
  };
  let i = 0;
  while (i < n) {
    const c = src[i];
    if (c === '"' || c === "'" || c === "`") {
      let j = i + 1;
      while (j < n) {
        if (src[j] === "\\") j += 2;
        else if (src[j] === c) break;
        else j++;
      }
      i = j + 1;
      continue;
    }
    if (c === "/" && src[i + 1] === "/") {
      let j = src.indexOf("\n", i);
      if (j === -1) j = n;
      blank(i, j);
      i = j;
      continue;
    }
    if (c === "/" && src[i + 1] === "*") {
      let j = src.indexOf("*/", i + 2);
      j = j === -1 ? n : j + 2;
      blank(i, j);
      i = j;
      continue;
    }
    i++;
  }
  return out.join("");
}

/** Blank string/template BODIES too, so a `}` in a literal cannot move depth. */
function maskTsStrings(src: string): string {
  const out = src.split("");
  const n = src.length;
  let i = 0;
  while (i < n) {
    const c = src[i];
    if (c === '"' || c === "'" || c === "`") {
      let j = i + 1;
      while (j < n) {
        if (src[j] === "\\") j += 2;
        else if (src[j] === c) break;
        else j++;
      }
      for (let k = i + 1; k < j && k < n; k++) if (out[k] !== "\n") out[k] = " ";
      i = j + 1;
      continue;
    }
    i++;
  }
  return out.join("");
}

/**
 * Every topic `applyEnvelope` has a `case` for, constants resolved. Scoped to the
 * function body so a `case` in some other switch in state.ts cannot smuggle a
 * topic in (that would be trap (g): a guard matching too widely).
 */
export function applyEnvelopeCases(
  stateSrc = readFileSync(STATE_TS, "utf8"),
  eventsSrc = readFileSync(EVENTS_TS, "utf8"),
): { cases: Set<string>; unresolved: string[] } {
  const fnAt = stateSrc.indexOf("function applyEnvelope(");
  if (fnAt === -1) throw new Error("applyEnvelope not found in core/state.ts");
  const open = stateSrc.indexOf("{", fnAt);
  const end = matchBrace(stateSrc, open);
  const body = stripTsComments(stateSrc.slice(open, end === -1 ? stateSrc.length : end));
  const tsConsts = new Map([
    ...collectTsConsts(eventsSrc),
    ...collectTsConsts(stateSrc),
  ]);
  const cases = new Set<string>();
  const unresolved: string[] = [];
  for (const m of body.matchAll(/\bcase\s+("([^"\\]*)"|[A-Za-z_][A-Za-z0-9_]*)\s*:/g)) {
    if (m[2] !== undefined) {
      cases.add(m[2]);
      continue;
    }
    const v = tsConsts.get(m[1]);
    if (v !== undefined) cases.add(v);
    else unresolved.push(m[1]);
  }
  return { cases, unresolved };
}

/**
 * THE OTHER SILENCE: `case "x": return s;`.
 *
 * A case label whose whole body is `return s;` does EXACTLY what the `default`
 * arm does — it returns the identical state reference, so it reaches no pixel.
 * "Has a reducer case" is therefore not the same claim as "reaches a pixel", and
 * the difference is not academic: with only the case-label check in place, the
 * cheapest way past this gate is to write `case "audit.anchor": return s;`, drop
 * the ledger line and lower UNTRIAGED by one. That was MEASURED — the full gate
 * stayed green, on a topic whose emit means the live audit chain diverged from
 * its Keychain witness, and the backlog number a reviewer is meant to watch went
 * DOWN while nothing was shown.
 *
 * So these labels are enumerated too, and pinned separately. They are legitimate
 * — each one carries a comment saying why the HUD ignores the frame — but they
 * are a decision, and a new one has to be argued for rather than typed.
 *
 * Walked by BRACE DEPTH inside `switch (env.event)`, not by a flat regex: a
 * nested switch would otherwise donate its labels to this one (trap (g)). The
 * depth-0 label count is cross-checked against the flat count and disagreement
 * THROWS rather than answering — that is the tripwire for a nested switch, for a
 * mis-masked literal, and for the regex-literal case this masker deliberately
 * does not try to parse (measured: the switch body contains no `/…/` literal).
 */
export function noOpCaseLabels(
  stateSrc = readFileSync(STATE_TS, "utf8"),
  eventsSrc = readFileSync(EVENTS_TS, "utf8"),
): { noOp: string[]; labels: number; groups: number } {
  const fnAt = stateSrc.indexOf("function applyEnvelope(");
  if (fnAt === -1) throw new Error("applyEnvelope not found in core/state.ts");
  const swAt = stateSrc.indexOf("switch (env.event) {", fnAt);
  if (swAt === -1) throw new Error("applyEnvelope's `switch (env.event) {` not found");
  const open = stateSrc.indexOf("{", swAt);
  const end = matchBrace(stateSrc, open);
  if (end === -1) throw new Error("applyEnvelope's switch body is unbalanced");
  const text = stripTsComments(stateSrc.slice(open + 1, end - 1));
  const masked = maskTsStrings(text);

  const word = (at: number, w: string) =>
    masked.startsWith(w, at) && !/[A-Za-z0-9_$]/.test(masked[at - 1] ?? " ");

  const labels: { start: number; colon: number }[] = [];
  let defaultAt = -1;
  let depth = 0;
  for (let i = 0; i < masked.length; i++) {
    const c = masked[i];
    if (c === "{" || c === "(" || c === "[") depth++;
    else if (c === "}" || c === ")" || c === "]") depth--;
    else if (depth === 0 && word(i, "case") && /\s/.test(masked[i + 4] ?? "")) {
      const colon = masked.indexOf(":", i);
      if (colon === -1) throw new Error("unterminated `case` label in applyEnvelope");
      labels.push({ start: i + 4, colon });
      i = colon;
    } else if (depth === 0 && defaultAt === -1 && word(i, "default")) {
      defaultAt = i;
    }
  }
  const flat = [...masked.matchAll(/\bcase\s+("[^"\\]*"|[A-Za-z_][A-Za-z0-9_]*)\s*:/g)].length;
  if (flat !== labels.length) {
    throw new Error(
      `applyEnvelope: ${flat} flat \`case\` labels but ${labels.length} at switch depth 0 — ` +
        `a nested switch, a regex literal, or an unbalanced literal. Teach emitScan.ts about it ` +
        `rather than letting either count stand.`,
    );
  }

  const tsConsts = new Map([...collectTsConsts(eventsSrc), ...collectTsConsts(stateSrc)]);
  const label = (l: { start: number; colon: number }) => {
    const raw = text.slice(l.start, l.colon).trim();
    const lit = /^"([^"\\]*)"$/.exec(raw);
    if (lit) return lit[1];
    const v = tsConsts.get(raw);
    if (v === undefined) throw new Error(`applyEnvelope: unresolved case label \`${raw}\``);
    return v;
  };

  const noOp: string[] = [];
  let groups = 0;
  for (let i = 0; i < labels.length; ) {
    let j = i + 1;
    while (j < labels.length && text.slice(labels[j - 1].colon + 1, labels[j].start - 4).trim() === "") j++;
    groups++;
    const after = labels[j - 1].colon + 1;
    const until = j < labels.length ? labels[j].start - 4 : defaultAt === -1 ? masked.length : defaultAt;
    const body = masked.slice(after, until).replace(/\s+/g, " ").trim();
    if (body === "return s;" || body === "{ return s; }") {
      for (let k = i; k < j; k++) noOp.push(label(labels[k]));
    }
    i = j;
  }
  return { noOp: noOp.sort(), labels: labels.length, groups };
}
