import { readFileSync } from "node:fs";
import { join, relative } from "node:path";
import { stdout } from "node:process";
import { describe, expect, it } from "vitest";
import {
  applyEnvelopeCases,
  DAEMON_SRC,
  noOpCaseLabels,
  REPO_ROOT,
  rustFiles,
  scanDaemonEmits,
  stripCfgTest,
  stripRustComments,
  stripTsComments,
} from "./emitScan";
import { initialState, reduce, type HudState } from "../core/state";
import type { TelemetryEnvelope } from "../core/events";
import { NO_OP_CASES, PIXEL_FREE, UNTRIAGED, type Bucket } from "./pixelFreeLedger";

/**
 * THE STANDING GATE that stops a pixel-free emit being written.
 *
 * `applyEnvelope` is an exact-match `switch` — no prefix matching, no generic
 * feed, and a `default` that returns `state` unchanged. A daemon topic with no
 * `case` therefore reaches NO PIXEL: it is serialized, broadcast, and dropped by
 * the only thing listening. That is indistinguishable, from the operator's seat,
 * from the daemon never having noticed at all — which is how a wired SYNC button,
 * a config typo that disarmed a safety key, and a microphone that was never
 * acquired each stayed invisible for weeks.
 *
 * The previous sweep found 116 such topics by hand and got the count wrong in
 * BOTH directions. So this is a gate, not a comment: `emitScan.ts` extracts every
 * production emit and every reducer case from source, and the set difference must
 * EQUAL the checked-in ledger in `pixelFreeLedger.ts`. Add a pixel-free emit and
 * this test fails at the moment you write it, names your topic and its file:line,
 * and makes you choose a bucket.
 */

const envelope = (event: string, data: Record<string, unknown>): TelemetryEnvelope => ({
  ts: "2026-08-10T00:00:00Z",
  source: "system",
  event,
  data,
});

const apply = (s: HudState, event: string, data: Record<string, unknown>): HudState =>
  reduce(s, { type: "telemetry", envelope: envelope(event, data), at: 5_000 });

const scan = scanDaemonEmits();
const { cases, unresolved: unresolvedCases } = applyEnvelopeCases();

const sitesByTopic = new Map<string, { file: string; line: number; via: string }[]>();
for (const s of scan.sites) {
  const list = sitesByTopic.get(s.topic) ?? [];
  list.push({ file: s.file, line: s.line, via: s.via });
  sitesByTopic.set(s.topic, list);
}
const emitted = [...sitesByTopic.keys()].sort();
const pixelFree = emitted.filter((t) => !cases.has(t));
/* Memoized, NOT computed at module scope: `noOpCaseLabels` refuses to answer (it
   throws) when its depth walk disagrees with a flat count, and a throw at module
   scope fails the FILE — one unnamed red instead of the four named tests that
   actually depend on it. Verified by mutation: neutering the comment strip now
   names the tests rather than the file. */
let noOpMemo: ReturnType<typeof noOpCaseLabels> | null = null;
const noOpScan = () => (noOpMemo ??= noOpCaseLabels());
const siteCount = (ts: string[]) => ts.reduce((n, t) => n + (sitesByTopic.get(t)?.length ?? 0), 0);

const where = (t: string) =>
  (sitesByTopic.get(t) ?? []).map((s) => `${s.file}:${s.line} (${s.via})`).join(", ");

// ---------------------------------------------------------------------------
// 1. The extractor must be sound before its verdict means anything.
// ---------------------------------------------------------------------------

describe("the emit extractor is sound", () => {
  it("reads the whole daemon and resolves EVERY topic expression", () => {
    expect(scan.files).toBeGreaterThan(120);
    expect(scan.sites.length).toBeGreaterThan(350);
    // An emit whose topic it cannot resolve is a hole in the measurement, not a
    // pass. Four such emits were missed by the last hand count precisely here.
    expect(
      scan.unresolved.map((u) => `${u.file}:${u.line} -> ${u.expr}`),
      "unresolved emit topic expression — teach emitScan.ts to resolve it",
    ).toEqual([]);
  });

  it("resolves the ev_* contract builders that pass a VARIABLE topic to emit", () => {
    // The bug this catches: `let (event, payload) = ev_modattest(...); emit(_, event, _)`
    // is invisible to a literal-only scrape. Naming them is the regression.
    const names = [...scan.builders.keys()].sort();
    expect(names).toEqual([
      "ev_anomaly",
      "ev_capabilities",
      "ev_install_gate",
      "ev_modattest",
      "ev_module_violation",
      "ev_profile_drift",
      "ev_profile_missing",
      "ev_proposed",
      "ev_rejected",
      "ev_security",
      "ev_snapshot",
      "ev_status",
      "ev_verdict",
      "verdict_frame",
    ]);
    // ...and at least one emit really does route through one.
    expect(scan.sites.some((s) => s.via === "builder")).toBe(true);
    expect(emitted).toContain("introspect.modattest");
  });

  it("resolves a CONST topic against the emitting file, not a same-named const elsewhere", () => {
    // The daemon really does reuse const names across modules with different
    // values, so a flat name->value table would answer by walk order. This is not
    // hypothetical bookkeeping: it is a measured property of daemon/src.
    expect(scan.collidingConsts.length).toBeGreaterThan(0);
    expect(scan.collidingConsts).toContain("API_BASE");
    // No topic in the emitted set was resolved from a colliding const name — if
    // one ever is, file-scoped resolution is the thing keeping it right.
    const constSites = scan.sites.filter((s) => s.via === "const");
    expect(constSites.length).toBeGreaterThan(0);
  });

  it("resolves CONSTANT case labels on the HUD side", () => {
    expect(unresolvedCases, "a `case SOME_CONST:` emitScan could not resolve").toEqual([]);
    // Six topics were called dead last sweep because their case label is a const.
    for (const t of [
      "image.generated",
      "audio.sound_monitor",
      "screen_context.watching",
      "screen_context.configured",
      "screen_context.command",
      "model.local_warm",
    ]) {
      expect(cases, `${t} is rendered via a const case label`).toContain(t);
      expect(pixelFree).not.toContain(t);
    }
  });

  /* The `#[cfg(test)]` cut and the comment strip are proven on FIXTURES, because
     proving them against the repo as it stands would be a probe chosen to pass:
     measured today, exactly 3 emit-shaped matches sit inside a `#[cfg(test)]`
     body (all of them the literal string "telemetry::emit(" inside main.rs's own
     source-text guards) and none of them contributes a topic. The rule is here
     for the test module that adds a real emit tomorrow, so the fixture is what
     has to hold it. */
  it("cuts #[cfg(test)] items — and leaves #[cfg(not(test))] alone", () => {
    const src = [
      `fn live() { telemetry::emit("system", "prod.topic", json!({})); }`,
      `#[cfg(test)]`,
      `mod tests {`,
      `    fn t() { telemetry::emit("system", "testonly.topic", json!({})); }`,
      `}`,
      `#[cfg(test)]`,
      `use crate::testonly::Helper;`,
      `#[cfg(test)]`,
      `let probe = false;`,
      `#[cfg(not(test))]`,
      `fn shipped() { telemetry::emit("system", "nottest.topic", json!({})); }`,
    ].join("\n");
    const cut = stripCfgTest(src);
    expect(cut).toContain("prod.topic");
    expect(cut).toContain("nottest.topic"); // #[cfg(not(test))] is PRODUCTION
    expect(cut).not.toContain("testonly.topic");
    expect(cut).not.toContain("crate::testonly::Helper"); // the `;` form
    expect(cut).not.toContain("let probe = false");
    // Offsets are preserved so reported line numbers stay honest.
    expect(cut.split("\n")).toHaveLength(src.split("\n").length);

    // …and the repo really does contain the `#[cfg(not(test))]` shape, so this
    // fixture is not describing a case that never occurs.
    const apps = readFileSync(join(REPO_ROOT, "daemon", "src", "apps.rs"), "utf8");
    expect(apps).toContain("#[cfg(not(test))]");
  });

  it("counts a topic named only in a COMMENT as not emitted", () => {
    // Prose in daemon/src names topics constantly, and every one of the ~102
    // pixel-free findings below would be reproducible by grep if comments
    // counted. `stripRustComments` must be string-aware in both directions.
    const src = [
      `// telemetry::emit("system", "commented.topic", json!({}));`,
      `/* telemetry::emit("system", "blocked.topic", json!({})); */`,
      `fn f() {`,
      `    let url = "https://example.test/a//b"; // not a comment start`,
      `    telemetry::emit("system", "real.topic", json!({"u": url}));`,
      `}`,
    ].join("\n");
    const clean = stripRustComments(src);
    expect(clean).not.toContain("commented.topic");
    expect(clean).not.toContain("blocked.topic");
    expect(clean).toContain("real.topic");
    expect(clean).toContain("https://example.test/a//b"); // the `//` in a string survived
    expect(clean.split("\n")).toHaveLength(src.split("\n").length);

    // In the repo: lockdown.rs names a HUD case in prose and emits nothing of
    // the sort; and no topic in the emitted set is a bare identifier from prose.
    const raw = readFileSync(join(REPO_ROOT, "daemon", "src", "lockdown.rs"), "utf8");
    expect(raw).toContain('case "lockdown.status"');
    expect(emitted).not.toContain("applyEnvelope");
  });

  /* THE CROSS-CHECK THAT IS NOT SELF-AGREEMENT. If the emit scrape were missing
     whole call shapes, reducer cases would appear to have no emitter. Every one of
     the ~178 case labels is in fact emitted by the daemon, which is a measurement
     of the scrape's COMPLETENESS taken from the other side of the wire. (When the
     unqualified `emit(` form was unhandled this list held `system.load`; when the
     trailing-comma arity bug was live it held 97 entries.) */
  it("leaves no reducer case without an emitter", () => {
    const orphans = [...cases].filter((c) => !sitesByTopic.has(c)).sort();
    expect(orphans, "a HUD case whose topic no daemon emit produces").toEqual([]);
  });

  it("counts a HUD case label named only in a COMMENT as not a case", () => {
    // The mirror of the daemon-side comment strip. Without it a commented-out
    // `case "egress.refused":` scrapes as a live case and silently deletes that
    // topic from the ledger with no pixel behind it — the same wrong answer as
    // grepping daemon prose, one layer over.
    const src = [
      `switch (env.event) {`,
      `  // case "commented.topic":`,
      `  /* case "blocked.topic": */`,
      `  case "real.topic": {`,
      `    const s = "case \\"instring.topic\\":";`,
      `    return { ...state, s };`,
      `  }`,
      `}`,
    ].join("\n");
    const clean = stripTsComments(src);
    expect(clean).not.toContain("commented.topic");
    expect(clean).not.toContain("blocked.topic");
    expect(clean).toContain("real.topic");
    expect(clean).toContain("instring.topic"); // a string body is NOT a comment
    expect(clean.split("\n")).toHaveLength(src.split("\n").length); // offsets kept

    // The repo really does contain `case`-shaped prose inside applyEnvelope
    // ("Without a case here they hit applyEnvelope's default"), so the strip is
    // not describing a shape that never occurs.
    expect(readFileSync(join(REPO_ROOT, "hud", "src", "core", "state.ts"), "utf8")).toContain(
      "Without a case here they",
    );
  });

  it("walks the switch by DEPTH, and refuses to answer if that disagrees with a flat count", () => {
    // The depth walk is what keeps a future nested switch from donating labels.
    // Its own tripwire is the flat/depth cross-check inside noOpCaseLabels: this
    // asserts the two agree today, and that the walk saw the whole switch.
    // …and every label is DISTINCT: a second `case "x":` in the same switch is
    // unreachable code, so a duplicate would be a silent pixel-free topic that
    // looks handled. labels counts occurrences, `cases` is the deduped set.
    expect(noOpScan().labels, "a duplicated `case` label in applyEnvelope is dead code").toBe(cases.size);
    expect(noOpScan().labels).toBeGreaterThan(170);
    expect(noOpScan().groups).toBeGreaterThan(150);
    expect(noOpScan().groups).toBeLessThan(noOpScan().labels); // grouped labels really are grouped
  });
});

// ---------------------------------------------------------------------------
// 2. Validate the verdict against the REAL reducer, not against the extractor.
// ---------------------------------------------------------------------------

/** Payload shapes chosen to hit the field names the reducer actually reads. */
const PROBE_PAYLOADS: Record<string, unknown>[] = [
  {},
  { ok: true, status: "on", enabled: true, count: 3, name: "probe", reason: "probe" },
  { error: "boom", detail: "boom", issues: ["a"], items: [{ x: 1 }], text: "hello" },
  { rms: 0.9, speaking: true, locked: true, available: true, active: true, tool: "t" },
];

describe("the pixel-free verdict is checked against the real reducer", () => {
  it("EVERY topic the gate calls pixel-free leaves state referentially unchanged", () => {
    const base = initialState();
    // Also probe from a state with the banner up: applyEnvelope has a SECOND
    // channel above the switch (INFERENCE_PROOF_EVENTS clears inferenceOffline),
    // and a topic that reaches only that channel is not pixel-free.
    const armed: HudState = { ...base, inferenceOffline: true, micOffline: "x" };
    // PRECONDITION: this loop must actually have something to iterate. An empty
    // `pixelFree` would make every assertion below vacuously true.
    expect(pixelFree.length).toBeGreaterThan(50);
    const leaks: string[] = [];
    for (const topic of pixelFree) {
      for (const data of PROBE_PAYLOADS) {
        for (const s of [base, armed]) {
          if (apply(s, topic, data) !== s) leaks.push(`${topic} ${JSON.stringify(data)}`);
        }
      }
    }
    expect(leaks, "claimed pixel-free but reduce() returned a NEW state").toEqual([]);
  });

  it("a topic the gate calls LIVE really is reachable, payload for payload", () => {
    // The other direction, on a hand-written sample with real payloads: if the
    // case-label scrape were wrong these would be identity too.
    const base = initialState();
    const live: [string, Record<string, unknown>][] = [
      ["system.load", { cpu_percent: 12, mem_used_bytes: 1, mem_total_bytes: 2 }],
      ["image.generated", { available: true, path: "/state/images/x.png", image: true }],
      ["config.invalid", { issues: ["unknown key [security].encrypt_memry"] }],
      ["capture.unavailable", { error: "no input device" }],
      ["inference.unavailable", { op: "transcribe", error: "refused" }],
      ["lockdown.status", { locked: true }],
    ];
    for (const [topic, data] of live) {
      expect(cases, `${topic} must have a case`).toContain(topic);
      expect(apply(base, topic, data), `${topic} must change state`).not.toBe(base);
    }
  });

  it("an unknown topic is the control: identity, and never a throw", () => {
    const base = initialState();
    expect(apply(base, "nobody.emits.this", { a: 1 })).toBe(base);
    expect(() => apply(base, "nobody.emits.this", { a: 1 })).not.toThrow();
  });
});

// ---------------------------------------------------------------------------
// 3. THE GATE.
// ---------------------------------------------------------------------------

const HOW_TO_FIX = (topic: string) =>
  [
    ``,
    `  NEW PIXEL-FREE EMIT: "${topic}"`,
    `      emitted at: ${where(topic) || "(no site found — this is a ledger entry with no emit)"}`,
    `      applyEnvelope (hud/src/core/state.ts) has no \`case "${topic}"\`, and its`,
    `      default returns state unchanged, so this frame reaches NO PIXEL.`,
    `  Pick one, do not just add the topic to the ledger:`,
    `    1. SHOW IT   — add a \`case "${topic}"\` to applyEnvelope AND a renderer.`,
    `                   A reducer field with no component is the same silence one layer up.`,
    `    2. DROP IT   — delete the emit. This is the default answer for a frame that`,
    `                   duplicates one already rendered.`,
    `    3. KEEP IT AS A DIAGNOSTIC — put \`PIXEL-FREE(diagnostic): <why>\` in a comment`,
    `                   within 8 lines above EVERY emit site, then add`,
    `                   "${topic}": { bucket: "diagnostic", why: "..." } to`,
    `                   hud/src/test/pixelFreeLedger.ts.`,
    ``,
  ].join("\n");

describe("the pixel-free ledger is complete and current", () => {
  it("lists exactly the topics that reach no reducer case", () => {
    const ledger = Object.keys(PIXEL_FREE).sort();
    const added = pixelFree.filter((t) => !(t in PIXEL_FREE));
    const gone = ledger.filter((t) => !pixelFree.includes(t));

    const report: string[] = [];
    for (const t of added) report.push(HOW_TO_FIX(t));
    for (const t of gone) {
      const reason = sitesByTopic.has(t)
        ? `it now HAS an applyEnvelope case — good; delete its ledger line.`
        : `nothing emits it any more — good; delete its ledger line.`;
      report.push(`\n  STALE LEDGER ENTRY: "${t}" — ${reason}\n`);
    }
    expect(report.join(""), report.join("")).toBe("");
    expect(ledger).toEqual([...pixelFree].sort());
  });

  /* The ledger's `sites` field is documented "informational" — which is how a
     checked-in fact rots. Nothing asserted it, so the first edit that moved a line
     in audio.rs would have left 115 file:line references quietly pointing at the
     wrong statements, and the next reader would have gone to look and found prose.
     It is a measurement; hold it to the measurement. */
  it("keeps every ledger `sites` reference pointing at a real FILE", () => {
    // THE INVARIANT IS THE TOPIC SET, NOT THE LINE NUMBERS. This asserted an
    // exact `file:line` match and went red on any unrelated edit to a file that
    // happens to contain a pixel-free emit — twice in one session, from adding a
    // TEST. A gate that fires for reasons the reader cannot act on trains them to
    // regenerate the ledger without reading it, which is exactly the rot this
    // whole file exists to prevent.
    //
    // So the FILE is asserted (a topic that moved modules is a real change worth
    // seeing) and the LINE is informational, printed by the reporter above. The
    // set-equality test is the one that holds the line.
    const wrong: string[] = [];
    for (const [topic, entry] of Object.entries(PIXEL_FREE)) {
      const actualFiles = new Set(
        (sitesByTopic.get(topic) ?? []).map((s) => s.file.replace("daemon/src/", "")),
      );
      const ledgerFiles = new Set(
        entry.sites.split(/\s+/).filter(Boolean).map((r) => r.split(":")[0]),
      );
      const same =
        actualFiles.size === ledgerFiles.size &&
        [...ledgerFiles].every((f) => actualFiles.has(f));
      if (!same) {
        wrong.push(
          `${topic}: ledger names [${[...ledgerFiles].join(", ")}] but the emit is in ` +
            `[${[...actualFiles].join(", ")}]`,
        );
      }
    }
    expect(Object.keys(PIXEL_FREE).length).toBeGreaterThan(50); // not an empty corpus
    expect(wrong, "a ledger site reference no longer names the emit's file").toEqual([]);
  });

  it("makes a DIAGNOSTIC claim argue for itself, and leaves untriaged unadorned", () => {
    const buckets: Bucket[] = ["diagnostic", "untriaged"];
    for (const [topic, entry] of Object.entries(PIXEL_FREE)) {
      expect(buckets, `${topic}: unknown bucket ${entry.bucket}`).toContain(entry.bucket);
      if (entry.bucket === "diagnostic") {
        // A one-word "why" is how the last list rotted: it got updated, not read.
        expect(entry.why ?? "", `${topic}: say WHY in a sentence`).toMatch(/\S.{60,}/s);
      } else {
        // Deliberately no rationale: a manufactured one for a hundred entries is
        // indistinguishable from having thought about none of them.
        expect(entry.why, `${topic}: untriaged entries carry no rationale`).toBeUndefined();
      }
    }
  });

  /* THE RATCHET. Without this, the cheapest way past the gate above is to append
     the new topic to the ledger as "untriaged" and move on — which is precisely
     how the previous list got its count updated without thought. Pinning the
     backlog size means a new pixel-free emit ALSO has to move a number that only
     ever goes down, in a diff a reviewer can see. */
  it("pins the untriaged backlog, which may only shrink", () => {
    const untriaged = Object.values(PIXEL_FREE).filter((e) => e.bucket === "untriaged");
    expect(
      untriaged.length,
      `UNTRIAGED in pixelFreeLedger.ts says ${UNTRIAGED}, the ledger holds ${untriaged.length}. ` +
        `If you triaged one, lower the number. If you added a pixel-free emit, do not raise it.`,
    ).toBe(UNTRIAGED);
  });

  /* THE SECOND SILENCE, and the hole the ratchet above had. `case "x": return s;`
     returns the identical reference the `default` arm returns, so it is not a
     pixel — but it IS a case, so it takes the topic out of `pixelFree`, out of the
     ledger, and out of UNTRIAGED. MEASURED at this revision: writing
     `case "audit.anchor": return s;`, deleting that ledger line and setting
     UNTRIAGED to 102 left all sixteen tests green and added no pixel at all. The
     remedy menu says "add a case AND a renderer"; only the case was enforced. */
  it("pins the cases that are a `return s;` — those reach no pixel either", () => {
    const listed = [...NO_OP_CASES].sort();
    const added = noOpScan().noOp.filter((t) => !listed.includes(t));
    const gone = listed.filter((t) => !noOpScan().noOp.includes(t));
    const report = [
      ...added.map((t) =>
        [
          ``,
          `  NEW NO-OP REDUCER CASE: "${t}"`,
          `      emitted at: ${where(t) || "(no emit site)"}`,
          `      applyEnvelope has \`case "${t}": return s;\` — the identical reference`,
          `      the \`default\` arm returns. This is NOT a pixel; it is the same silence`,
          `      with a label on it, and it hides the topic from PIXEL_FREE and UNTRIAGED.`,
          `  Either render it, or delete the emit, or add "${t}" to NO_OP_CASES in`,
          `  hud/src/test/pixelFreeLedger.ts with the reason written in state.ts beside`,
          `  the case — the same argue-for-it contract the diagnostic bucket imposes.`,
          ``,
        ].join("\n"),
      ),
      ...gone.map((t) => `\n  STALE NO_OP_CASES ENTRY: "${t}" — it no longer returns state unchanged.\n`),
    ].join("");
    expect(report, report).toBe("");
    expect(noOpScan().noOp).toEqual(listed);
    // PRECONDITION: the walker must actually be finding no-op groups. An empty
    // `noOp` would make the equality above pass against an empty list forever.
    expect(noOpScan().noOp.length).toBeGreaterThan(0);
    // Every one of them is really emitted, so this is a live silence, not a
    // hypothetical: the count is part of the measured total below.
    for (const t of noOpScan().noOp) expect(sitesByTopic.has(t), `${t} has no emitter`).toBe(true);
  });

  it("states the TRUE pixel-free total, cases-that-do-nothing included", () => {
    // The arithmetic a reader should quote. Keeping only the PIXEL_FREE number
    // understates the population by the whole NO_OP_CASES set.
    const total = pixelFree.length + noOpScan().noOp.length;
    const sites = siteCount(pixelFree) + siteCount(noOpScan().noOp);
    expect(pixelFree.length).toBe(114);
    expect(siteCount(pixelFree)).toBe(130);
    expect(noOpScan().noOp.length).toBe(9);
    expect(siteCount(noOpScan().noOp)).toBe(14);
    expect(total).toBe(123);
    expect(sites).toBe(144);
    expect(emitted.length).toBe(294);
    expect(scan.sites.length).toBe(395);
    expect(Math.round((total / emitted.length) * 1000) / 10).toBe(41.8);
  });

  /* A ledger line saying "deliberate" is a claim by whoever edited the ledger. The
     claim has to be made where the emit is, or the next person reading audio.rs has
     no way to know the silence was chosen. */
  it("requires every DIAGNOSTIC topic to be marked at EVERY one of its emit sites", () => {
    const missing: string[] = [];
    const fileCache = new Map<string, string[]>();
    for (const [topic, entry] of Object.entries(PIXEL_FREE)) {
      if (entry.bucket !== "diagnostic") continue;
      for (const site of sitesByTopic.get(topic) ?? []) {
        const cached = fileCache.get(site.file);
        const lines = cached ?? readFileSync(join(REPO_ROOT, site.file), "utf8").split("\n");
        if (!cached) fileCache.set(site.file, lines);
        // The emit's own line plus the 8 above it: enough for a rustfmt-wrapped
        // doc comment, tight enough that a marker cannot drift onto a neighbour.
        const window = lines.slice(Math.max(0, site.line - 9), site.line).join("\n");
        if (!window.includes("PIXEL-FREE(diagnostic)")) {
          missing.push(`${topic} @ ${site.file}:${site.line}`);
        }
      }
    }
    expect(
      missing,
      "ledger says diagnostic; the emit site does not. Add `PIXEL-FREE(diagnostic): <why>`.",
    ).toEqual([]);
  });

  /* …and the reverse, or the marker rots the other way: a marker left behind after
     its topic was wired or deleted is a stale claim that reads as a live one.

     This test used to walk the LEDGER and ask whether any diagnostic entry had a
     case. It could not fail: the equality test above already forces every ledger
     key to have no case, so within a green suite the answer was always no. The
     unchecked direction was the one that rots — MEASURED, a
     `PIXEL-FREE(diagnostic)` comment placed directly above the `system.load` emit
     in telemetry.rs, the single busiest LIVE case in the HUD, passed the whole
     gate. So walk the MARKERS, bind each to the emits it sits above, and make it
     answer for the topic it actually marks. */
  it("allows no PIXEL-FREE(diagnostic) marker on a topic that now reaches a pixel", () => {
    const MARK = "PIXEL-FREE(diagnostic)";
    const byFile = new Map<string, { line: number; topic: string }[]>();
    for (const s of scan.sites) {
      byFile.set(s.file, [...(byFile.get(s.file) ?? []), { line: s.line, topic: s.topic }]);
    }
    const stray: string[] = [];
    let markers = 0;
    for (const p of rustFiles(DAEMON_SRC)) {
      const file = relative(REPO_ROOT, p);
      readFileSync(p, "utf8")
        .split("\n")
        .forEach((text, idx) => {
          if (!text.includes(MARK)) return;
          markers++;
          const at = idx + 1;
          // The same 8-line window the forward check uses, read the other way:
          // the marker is above, the emit is below.
          const bound = (byFile.get(file) ?? []).filter((s) => s.line >= at && s.line <= at + 8);
          if (bound.length === 0) {
            stray.push(`${file}:${at} — marker sits above no emit at all`);
            return;
          }
          for (const b of new Map(bound.map((b) => [b.topic, b])).values()) {
            const entry = PIXEL_FREE[b.topic];
            if (entry?.bucket === "diagnostic") continue;
            stray.push(
              `${file}:${at} — marks "${b.topic}", which is ` +
                (cases.has(b.topic)
                  ? `RENDERED (applyEnvelope has a case for it): delete the marker`
                  : entry
                    ? `bucket "${entry.bucket}" in the ledger, not "diagnostic"`
                    : `not in pixelFreeLedger.ts at all`),
            );
          }
        });
    }
    // PRECONDITION: at least as many markers as there are diagnostic emit sites,
    // or this loop is scoring an empty corpus.
    const diagSites = siteCount(
      Object.entries(PIXEL_FREE).filter(([, e]) => e.bucket === "diagnostic").map(([t]) => t),
    );
    expect(diagSites).toBeGreaterThan(0);
    expect(markers, "no PIXEL-FREE(diagnostic) markers found in daemon/src").toBeGreaterThanOrEqual(
      diagSites,
    );
    expect(stray, "a PIXEL-FREE(diagnostic) marker that no longer describes its emit").toEqual([]);
  });

  it("PRINTS the measured list, so the next sweep reads it instead of deriving it", () => {
    const byBucket = new Map<Bucket, string[]>();
    for (const [topic, e] of Object.entries(PIXEL_FREE)) {
      byBucket.set(e.bucket, [...(byBucket.get(e.bucket) ?? []), topic]);
    }
    const sites = pixelFree.reduce((n, t) => n + (sitesByTopic.get(t)?.length ?? 0), 0);
    const out = [
      ``,
      `PIXEL-FREE EMITS — daemon topics that reach no applyEnvelope case`,
      `  ${pixelFree.length} topics / ${sites} emit sites`,
      `  (of ${emitted.length} topics emitted in all, ${cases.size} reducer cases)`,
      `  + ${noOpScan().noOp.length} topics / ${siteCount(noOpScan().noOp)} sites whose case IS \`return s;\``,
      `    ${noOpScan().noOp.join(", ")}`,
      `  = ${pixelFree.length + noOpScan().noOp.length} topics / ${sites + siteCount(noOpScan().noOp)} sites`,
      `    reaching no pixel — ${Math.round(((pixelFree.length + noOpScan().noOp.length) / emitted.length) * 1000) / 10}% of the topics the daemon emits`,
      ...[...byBucket.entries()]
        .sort()
        .flatMap(([b, ts]) => [
          ``,
          `  ${b.toUpperCase()} (${ts.length})`,
          ...ts.sort().map((t) => `    ${t.padEnd(30)} ${where(t)}`),
        ]),
      ``,
    ].join("\n");

    /* IT HAS TO ACTUALLY PRINT. This used to be `console.log(out)`, and under the
       repo's own gate command — `npx vitest run`, hud/vitest.config.ts, no
       reporter override — vitest 4's default reporter SWALLOWS console output
       from a passing test. MEASURED: a `console.log("MARKER")` in a passing test
       yields 0 matches for MARKER in the run output, and 1 under
       `--reporter=verbose`, which nothing in this repo uses. So the list the whole
       gate exists to publish reached no terminal, which is the pixel-free defect
       one layer up — a report with no reader.

       `process.stdout.write` goes to the real stream, which the reporter does not
       intercept (same probe: 1 match, default reporter). Asserting through a spy
       on that write, rather than on `out`, is what makes this a test of the
       PUBLICATION and not of the string. */
    const chunks: string[] = [];
    const realWrite = stdout.write.bind(stdout);
    stdout.write = (chunk: string) => {
      chunks.push(chunk);
      return realWrite(chunk);
    };
    try {
      stdout.write(`${out}\n`);
    } finally {
      stdout.write = realWrite;
    }
    expect(
      chunks.join(""),
      "the measured list must reach the terminal on a normal `npx vitest run`",
    ).toContain("PIXEL-FREE EMITS");
    expect(chunks.join("")).toContain(pixelFree[0]);
    expect(out).toContain("PIXEL-FREE EMITS");
  });
});
