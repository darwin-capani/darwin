/**
 * THE MEASURED LIST of daemon topics that reach no `applyEnvelope` case.
 *
 * `pixel-free-emits.test.ts` scrapes `daemon/src/**` and `hud/src/core/state.ts`
 * and requires the set difference to EQUAL this file, key for key. It is a
 * checked-in measurement, not a wish: `sites` is generated from the scrape and is
 * there so a reader can go and look, and the test prints the whole list on every
 * run so the next sweep reads it instead of re-deriving it (the last hand
 * derivation was wrong in both directions).
 *
 * BUCKETS
 *   diagnostic  A deliberate operator-stream-only frame. Requires `why`, AND a
 *               `PIXEL-FREE(diagnostic):` comment within 8 lines above EVERY one
 *               of its emit sites — the claim has to be made where the emit is,
 *               or the next person reading audio.rs cannot know the silence was
 *               chosen. The test enforces both directions of that.
 *   untriaged   Enumerated, not yet decided. NO `why`: an entry here is an
 *               inventory line, and inventing a rationale for a hundred of them
 *               in one sitting is exactly how the previous list rotted. `UNTRIAGED` below is the
 *               scoreboard and the test pins it to the measured count, so this
 *               number can only be moved deliberately — and it should only ever
 *               go DOWN.
 *
 * There is no `dead` bucket: "stop emitting" is not a state a topic can sit in,
 * it is a deletion. A topic that should not be emitted leaves this file entirely.
 */

export type Bucket = "diagnostic" | "untriaged";

export interface LedgerEntry {
  bucket: Bucket;
  /** Where it is emitted, `file:line`, from the scrape. Informational. */
  sites: string;
  /** Required for `diagnostic`; absent for `untriaged` by design. */
  why?: string;
}

/**
 * How many topics are enumerated but undecided. THIS NUMBER MAY ONLY GO DOWN.
 * It is pinned to the measured count, so a new pixel-free emit cannot be parked
 * in the backlog without editing it — which is a visible, arguable act in review.
 */
export const UNTRIAGED = 0;

/**
 * THE OTHER SILENCE — topics that DO have an `applyEnvelope` case, whose case is
 * exactly `return s;`.
 *
 * That is byte-for-byte what the `default` arm does, so these reach no pixel
 * either. They are not in `PIXEL_FREE` (which is defined as "no case at all"),
 * and without this second list they are not counted anywhere — which left the
 * gate with a hole big enough to walk the backlog through: MEASURED, adding
 * `case "audit.anchor": return s;`, deleting its ledger line and setting
 * UNTRIAGED to 102 kept every one of the gate's tests green while adding exactly
 * zero pixels. `audit.anchor` fires when the live audit chain diverges from its
 * Keychain witness.
 *
 * Each of the nine below is a deliberate, commented decision in state.ts, and
 * they stay. The point of pinning them is that the TENTH has to be argued for at
 * the moment it is written, in a diff a reviewer can see — the same contract the
 * `diagnostic` bucket imposes on the daemon side.
 *
 * TRUE PIXEL-FREE POPULATION = PIXEL_FREE (114 topics / 130 sites)
 *                            + NO_OP_CASES (9 topics / 14 sites)
 *                            = 123 topics / 144 sites,
 * out of 294 topics / 395 production emit calls — 41.8% of the topics the daemon
 * emits reach no pixel, not the 38.8% that counting only PIXEL_FREE reports.
 */
export const NO_OP_CASES: string[] = [
  "app.auth_failed",
  "app.log",
  "intent.handled",
  "macro.blocked",
  "macro.recording_started",
  "mission.blocked",
  "opener.orphaned",
  "opener.played",
  "vad.segment_capped",
];

export const PIXEL_FREE: Record<string, LedgerEntry> = {
  "action.deduped": {
    bucket: "diagnostic",
    sites: "anthropic.rs:2819",
    why:
      "The dedup is a correctness guarantee, not news: the user asked for one action and got one, and the action itself already rendered via action.executed. Showing the suppression would report a bug the user does not have.",
  },
  "agent.reroute": {
    bucket: "diagnostic",
    sites: "router.rs:3432",
    why:
      "Paired with a warn! 'tool outside agent allowlist; re-routing to the owning agent' log that is the operator surface; this frame is the stream copy of a benign allowlist reroute to the owning agent.",
  },
  "aperture.command": {
    bucket: "diagnostic",
    sites: "router.rs:594",
    why:
      "Secret-free: the verb and the gate only, never the already-redacted app/title text; the command is acted on and its result returned to the user, so this frame is the operator-stream record of an aperture voice command.",
  },
  "aperture.configured": {
    bucket: "diagnostic",
    sites: "main.rs:2883",
    why:
      "The activity-timeline enabled-gate plus retention/interval bounds; the live rendered pixel is aperture.status and darwin.toml's [aperture] section is authoritative, so this once-at-startup frame is only the operator-stream snapshot.",
  },
  "aperture.loop_started": {
    bucket: "diagnostic",
    sites: "main.rs:3617",
    why:
      "Paired with an info! 'started aperture activity-timeline poll loop' log that is the real surface; the rendered pixel for the feature is aperture.status, so this startup lifecycle frame is a stream/log breadcrumb.",
  },
  "app.autostart_failed": {
    bucket: "diagnostic",
    sites: "main.rs:3520",
    why:
      "Paired with a warn! 'autostart skipped' log carrying the app name and error; that log is the operator surface for a failed [apps].autostart entry, so this frame is the stream copy of the same failure.",
  },
  "app.proxy_denied": {
    bucket: "diagnostic",
    sites: "fetchproxy.rs:611 fetchproxy.rs:644 genproxy.rs:337 genproxy.rs:367",
    why:
      "A sandboxed app's fetch/generate call was refused at the broker. The app learns the stable reason; the operator's surface is the warn! next to each emit. Four sites, all already carrying the marker before this sweep.",
  },
  "app.result": {
    bucket: "diagnostic",
    sites: "apps.rs:2822",
    why:
      "A breadcrumb that a micro-app tool answered; neither the HUD nor audit consumes it (pinned by silent-drops.test.ts) and the error arm warns of a result for no live request, so it is operator-stream only.",
  },
  "app.tool_invoked": {
    bucket: "diagnostic",
    sites: "anthropic.rs:2101",
    why:
      "A per-app-tool-call breadcrumb carrying the app, the tool name, whether it succeeded, and its latency; it is pure observability of the micro-app runtime, so the operator stream is the right surface.",
  },
  "audio.capture_stopped": {
    bucket: "diagnostic",
    sites: "audio.rs:579",
    why:
      "Capture DIED mid-run, which deliberately drops the sender and ends main for a launchd restart. The HUD's real signal is the socket closing a moment later; this frame is the reason, on the live stream.",
  },
  "audio.sound": {
    bucket: "diagnostic",
    sites: "router.rs:8470",
    why:
      "Records only that the on-device sound classification ran; it carries no audio, no clip samples, and no path, and the actual labels ride the async vision.sound relay, so this frame is the operator-stream record that classify ran.",
  },
  "audit.anchor": {
    bucket: "diagnostic",
    sites: "audit.rs:860",
    why:
      "Fires ONCE at startup, before any HUD has connected, and is not a sticky announce - a reducer case for it would render nothing in the deployed configuration. The verdict is cached and folded onto audit.snapshot under `anchor`, which the AuditPanel draws as its EXTERNAL ANCHOR row; that is the pixel. This frame is the operator's full copy.",
  },
  "babel.interpret": {
    bucket: "diagnostic",
    sites: "anthropic.rs:9588",
    why:
      "Deliberate observability of whether a real rendering was produced versus an honest could-not-translate line; it records the translate turn's outcome, so the operator stream is the right surface, not a pixel.",
  },
  "barge_in": {
    bucket: "diagnostic",
    sites: "audio.rs:443",
    why:
      "The barge itself is already a pixel - the reply is cut and the core state leaves speaking. This frame carries only the triggering rms, for tuning the detector. A toast per barge-in would fire on ordinary interruption.",
  },
  "calibrate.report": {
    bucket: "diagnostic",
    sites: "calibrate.rs:526",
    why:
      "An internal reliability-curve report used for calibration evaluation; it is diagnostic tuning data rather than an operator decision, so the operator stream and eval store are the right surface.",
  },
  "command.auth_failed": {
    bucket: "diagnostic",
    sites: "command.rs:859",
    why:
      "The command socket is 0600 inside a 0700 dir and the token sits in a 0600 file in that same dir, so any principal that can reach the socket could have read the token and passed. A failure is a stale/broken client, not a crossed boundary - and not the HUD, which re-reads the token per round-trip.",
  },
  "command.channel_up": {
    bucket: "diagnostic",
    sites: "main.rs:3150",
    why:
      "The deliberate observability signal for command-channel bring-up: the socket path and whether the out-of-band 0600 token handoff succeeded (never the token), so it is an operator-stream security breadcrumb, not a pixel.",
  },
  "command.denied": {
    bucket: "diagnostic",
    sites: "command.rs:839 command.rs:846 command.rs:872",
    why:
      "unknown_command and oversized fire PRE-AUTH, and the first carries the caller's raw `cmd` string bounded only by the 8 KiB line cap - rendering it would give an unauthenticated local client a writable line in the owner's HUD. rate_limited is post-auth and the caller is already told. Every arm refused something and there is no owner decision in it.",
  },
  "daemon.ready": {
    bucket: "diagnostic",
    sites: "main.rs:3450",
    why:
      "The aggregated startup-readiness snapshot; it is paired with an info! 'daemon ready' log carrying inference reachability and is documented in docs/BRINGUP.md, so the log is the operator surface for a down inference server before a turn is lost.",
  },
  "design_voice.created": {
    bucket: "diagnostic",
    sites: "main.rs:5692",
    why:
      "The success is returned to the user in the tool reply; this frame carries only the agent slot (never the voice id or the key) as the operator-stream record of a cloud voice-design op.",
  },
  "design_voice.failed": {
    bucket: "diagnostic",
    sites: "main.rs:5697",
    why:
      "Paired with a warn! 'design-voice: design_voice op failed' log, and the failure is returned to the user in the tool reply; this frame carries only the agent slot as the operator-stream record.",
  },
  "design_voice.no_key": {
    bucket: "diagnostic",
    sites: "main.rs:5675",
    why:
      "The missing-ElevenLabs-key outcome is returned to the user verbatim in the tool reply telling them to add a key in Settings; this frame carries only the agent slot (never the voice id or key) as the operator-stream record.",
  },
  "dls.status": {
    bucket: "diagnostic",
    sites: "main.rs:3006",
    why:
      "The config-assistant DLS enabled-gate and loopback port; it is strictly read-only and darwin.toml's [dls] section is authoritative, so this once-at-startup config echo is only the operator-stream snapshot.",
  },
  "egress.newhost": {
    bucket: "diagnostic",
    sites: "egress_beacon.rs:649",
    why:
      "The old defect is FIXED (the baseline persists in state/egress_baseline.db and a cold store's first sample seeds silently, so a restart no longer re-alerts on known talkers) — what keeps this off the HUD now is the OWNER, not a bug: hosts are bare IPs (lsof -nP) and ordinary browsing mints new (process, IP) pairs continuously (measured: 3 in one 45s window, all browser-owned), so a rendered row would be 'browser -> fresh CDN IP' at the 5-minute debounce floor, up to 288/day. Nobody acts on that; the rendered surface for egress is the beacon alert, which names a behaviour instead of an inventory delta.",
  },
  "egress.refused": {
    bucket: "diagnostic",
    sites: "anthropic.rs:7219 anthropic.rs:7654 anthropic.rs:7688 anthropic.rs:14147",
    why:
      "Three of the four sites RETURN the refusal as the tool result the model relays, so the owner is told in the answer, in the turn it happened. The fourth (sage_research) is not relayed - research.rs skips a failed source - and that silence is still right: the refused URL came from a search engine, not from the owner, and the SSRF guard declining it is not their decision.",
  },
  "enclave.status": {
    bucket: "diagnostic",
    sites: "main.rs:2958",
    why:
      "The hardware-bound key-custody posture; the rendered pixel is security.status, whose panel already states whether the master key resolved, so this frame is the operator-stream copy rather than a second indicator saying the same thing.",
  },
  "envlock.build_refused": {
    bucket: "diagnostic",
    sites: "envlock.rs:641",
    why:
      "Paired with a warn! 'envlock: refusing a non-user-originated env_build (egress gate)' log and the refusal message is returned to the caller, so the log and reply are the surfaces and this frame is the operator-stream record.",
  },
  "envlock.built": {
    bucket: "diagnostic",
    sites: "envlock.rs:673",
    why:
      "Paired with an info! 'envlock: closure materialized + verified' log carrying the app and closure hash; that log is the operator surface, so this frame is the stream copy of a completed reproducible build.",
  },
  "envlock.verify": {
    bucket: "diagnostic",
    sites: "envlock.rs:534",
    why:
      "An env-lock pin verify verdict emitted on the app launch path and a no-op for an unpinned app; it is a security attestation for the operator stream, so the stream is the right surface, not a pixel.",
  },
  "forge.aborted": {
    bucket: "diagnostic",
    sites: "forge.rs:1317",
    why:
      "A forge draft aborted at a named stage; it sits beside the reject/quarantine warn! path and any change would land on the changeq surface, so this ts/stage frame is the operator-stream lifecycle trace.",
  },
  "forge.dismissed": {
    bucket: "diagnostic",
    sites: "command.rs:1932",
    why:
      "Returns 'Dismissed the forge proposal {ts}.' to the user in the reply noting it was never deployed and apply stays a script, so the reply delivers it and this ts-only frame is the operator-stream record.",
  },
  "forge.drafting": {
    bucket: "diagnostic",
    sites: "forge.rs:686",
    why:
      "A forge draft-attempt lifecycle frame; any resulting change lands on the rendered changeq review surface and apply stays a gated script, so this goal/ts frame is the operator-stream lifecycle trace.",
  },
  "forge.suppressed": {
    bucket: "diagnostic",
    sites: "forge.rs:1244",
    why:
      "Fires when the forge pipeline is fully inert because forge.enabled is false, so nothing is drafted, staged, or proposed; this reason-only frame is the operator-stream trace that the pipeline was suppressed.",
  },
  "forge_gap.blocked": {
    bucket: "diagnostic",
    sites: "forge_gap.rs:325 forge_gap.rs:350",
    why:
      "Emitted once-ish for visibility while a detected gap is suppressed (a burst is in flight or a human has not cleared the pending), and the eventual proposal lands on the rendered changeq surface, so this is a stream trace.",
  },
  "forge_gap.detected": {
    bucket: "diagnostic",
    sites: "forge_gap.rs:370",
    why:
      "Fires when a capability gap is detected and a goal is synthesized; the resulting forge proposal lands on the rendered changeq review surface, so this frame is the operator-stream trace of the detection.",
  },
  "hyde": {
    bucket: "diagnostic",
    sites: "docsearch.rs:2259",
    why:
      "A retrieval-internal trace. The HUD shows the RESULT via docsearch.searched; which of the two query vectors found it is an eval question answered off the live stream.",
  },
  "inference.degraded": {
    bucket: "diagnostic",
    sites: "inference.rs:1004",
    why:
      "Paired with a warn! 'inference server became UNREACHABLE, running degraded' log and documented in docs/BRINGUP.md; that log is the operator surface for a down server, so this one-shot edge frame is the stream copy.",
  },
  "inference.health": {
    bucket: "diagnostic",
    sites: "inference.rs:989",
    why:
      "The background inference-liveness snapshot published on a cadence and documented in docs/BRINGUP.md; a down server is surfaced through the degraded/recovered edge and the log, so this cadence frame is the operator-stream copy.",
  },
  "inference.recovered": {
    bucket: "diagnostic",
    sites: "inference.rs:1011",
    why:
      "Paired with an info! 'inference server is reachable again, degraded mode cleared' log and documented in docs/BRINGUP.md; that log is the operator surface, so this one-shot edge frame is the stream copy.",
  },
  "introspect.es": {
    bucket: "diagnostic",
    sites: "es.rs:133 es.rs:138",
    why:
      "The ES client's own liveness. What it FINDS does reach pixels (introspect.security_event and introspect.anomaly both have cases); whether the seam started is a build/entitlement fact for the log.",
  },
  "journal.recorded": {
    bucket: "diagnostic",
    sites: "journal.rs:311",
    why:
      "The reversible journal's own append. The HUD already renders the ACTION (action.executed) and the undo affordance (undo.armed); a frame per action would be one ticker row per row already shown.",
  },
  "lockdown.panic": {
    bucket: "diagnostic",
    sites: "router.rs:135",
    why:
      "The LOCK is already a pixel: lockdown::panic emits a sticky lockdown.status{locked:true} that drives the LockdownChip. This frame adds only the channel (voice vs the HUD verb).",
  },
  "lockdown.unlock": {
    bucket: "diagnostic",
    sites: "router.rs:168",
    why:
      "Twin of lockdown.panic - lockdown::unlock emits lockdown.status{locked:false}, which is what the chip reads. This frame adds only the channel.",
  },
  "lumen.action": {
    bucket: "diagnostic",
    sites: "router.rs:4128",
    why:
      "Secret-free: only the control count, the selected control, and the refusal class; the resolved voice action is performed and acknowledged, so this frame is the operator-stream record of a lumen control action.",
  },
  "lumen.configured": {
    bucket: "diagnostic",
    sites: "main.rs:2867",
    why:
      "The lumen narration opt-in gate; darwin.toml's [lumen] section is the authoritative surface for whether it is on, so this once-at-startup config echo is only the operator-stream snapshot and reaches no separate lumen pixel.",
  },
  "lumen.read": {
    bucket: "diagnostic",
    sites: "router.rs:4106",
    why:
      "Content-free: it only forwards and acknowledges a screen-read op to the Vision app, which carries any content over its own relay, so this frame is the operator-stream record that a read was forwarded.",
  },
  "music.failed": {
    bucket: "diagnostic",
    sites: "main.rs:5408",
    why:
      "Paired with a warn! 'compose-music: compose_music op failed' log, and the failure is returned to the user in the tool reply; this empty-payload frame is only the operator-stream record.",
  },
  "music.generated": {
    bucket: "diagnostic",
    sites: "main.rs:5397",
    why:
      "The composed track is actually played for the user and the reply confirms it, so the audible output is the surface; this empty-payload frame is only the operator-stream record that a compose op ran.",
  },
  "music.intent": {
    bucket: "diagnostic",
    sites: "router.rs:1044",
    why:
      "Fires when generation begins; the audible track and the tool reply (and music.generated/music.failed) are the surfaces the user actually experiences, so this empty-payload start breadcrumb is operator-stream only.",
  },
  "music.no_key": {
    bucket: "diagnostic",
    sites: "main.rs:5389",
    why:
      "The missing-ElevenLabs-key outcome is returned to the user in the tool reply telling them to add a key; there is no on-device fallback, so this empty-payload frame is only the operator-stream record.",
  },
  "notebook.retention": {
    bucket: "diagnostic",
    sites: "main.rs:1087",
    why:
      "Paired with an info! 'research-notebook retention pass evicted oldest entries' log carrying the count; that log is the operator surface for a maintenance eviction, so this frame is the stream copy.",
  },
  "optimize.trace_corrected": {
    bucket: "diagnostic",
    sites: "main.rs:4643",
    why:
      "Labels a prior turn's trace as CorrectedNextTurn for the offline optimize evaluation; it is internal training-signal bookkeeping written to the trace store, so the operator stream and store are the right surface.",
  },
  "pasteboard.command": {
    bucket: "diagnostic",
    sites: "router.rs:713",
    why:
      "Secret-free: the verb and the gate only, never the already-redacted clip text; the command is acted on and its result returned to the user, so this frame is the operator-stream record of a pasteboard voice command.",
  },
  "pasteboard.configured": {
    bucket: "diagnostic",
    sites: "main.rs:2874",
    why:
      "The semantic-pasteboard enabled-gate plus retention/cadence bounds; the live rendered pixel is pasteboard.status and darwin.toml's [pasteboard] section is authoritative, so this once-at-startup frame is only the operator-stream snapshot.",
  },
  "pasteboard.loop_started": {
    bucket: "diagnostic",
    sites: "main.rs:3600",
    why:
      "Paired with an info! 'started semantic-pasteboard poll loop' log that is the real surface; the rendered pixel for the feature is pasteboard.status, so this startup lifecycle frame is a stream/log breadcrumb.",
  },
  "policy.parked_unattended": {
    bucket: "diagnostic",
    sites: "anthropic.rs:7520",
    why:
      "Paired with a warn! 'policy: Always downgraded to a park, unattended autonomous run' log; the park itself is the acted-on behavior an operator sees, so this frame is the operator-stream record of the downgrade.",
  },
  "policy.user_set": {
    bucket: "diagnostic",
    sites: "router.rs:338",
    why:
      "A spoken policy command that is applied (itself refusing if the layer is off or the ceiling would be exceeded) and an honest acknowledgement is spoken back to the user, so this frame is the operator-stream record.",
  },
  "privacy.transient_screen_read": {
    bucket: "diagnostic",
    sites: "main.rs:4521",
    why:
      "Records the privacy-preserving default that this screen-read turn was transient and was not persisted (persisted:false); the safe path needs no owner action, so it is an operator-stream privacy record.",
  },
  "pronunciation.created": {
    bucket: "diagnostic",
    sites: "main.rs:5745",
    why:
      "The success is returned to the user in the tool reply; this frame carries no dictionary/version ids and no key, so it is only the operator-stream record that a pronunciation dictionary was created.",
  },
  "pronunciation.failed": {
    bucket: "diagnostic",
    sites: "main.rs:5750",
    why:
      "Paired with a warn! 'create-pronunciation: create_pronunciation op failed' log, and the failure is returned to the user in the tool reply; this empty-payload frame is only the operator-stream record.",
  },
  "pronunciation.no_key": {
    bucket: "diagnostic",
    sites: "main.rs:5730",
    why:
      "The missing-ElevenLabs-key outcome is returned to the user in the tool reply telling them to add a key; this empty-payload frame is only the operator-stream record of a cloud pronunciation op.",
  },
  "realm.blocked": {
    bucket: "diagnostic",
    sites: "anthropic.rs:11778",
    why:
      "The realm subsystem is inert because it is disabled or lockdown is engaged, so the call returns None and does nothing; this reason-only frame is the operator-stream trace that the guarded path was a no-op.",
  },
  "realm.verdict": {
    bucket: "diagnostic",
    sites: "anthropic.rs:11805",
    why:
      "The realm verdict is written to realm_verdict.md on the proposal card, which is the rendered surface the owner reviews, so this frame is only the operator-stream copy of a decision already on the card.",
  },
  "registry.install_gate": {
    bucket: "diagnostic",
    sites: "registry.rs:911",
    why:
      "A plugin install-gate decision for a candidate app; it is a security admission record returned to its caller as the gate value, so the operator stream is the right surface for the frame, not a pixel.",
  },
  "registry.status": {
    bucket: "diagnostic",
    sites: "registry.rs:734",
    why:
      "The plugin-registry verification policy (whether verify and rebuild-match are required, and the signer count); it is paired with a tracing::info! and is secret-free, so the operator stream and log are the surface.",
  },
  "registry.verdict": {
    bucket: "diagnostic",
    sites: "registry.rs:841",
    why:
      "A plugin-admission verdict carrying the plugin id, the admission decision, and the signer key id; it is a security attestation, so the operator stream is the right surface for the record, not a pixel.",
  },
  "rollcall.completed": {
    bucket: "diagnostic",
    sites: "router.rs:2271",
    why:
      "Roll-call is a spoken feature in which each agent introduces itself aloud, so the agents' spoken intros are the surface and this frame is only the operator-stream record that the run completed.",
  },
  "rollcall.interrupted": {
    bucket: "diagnostic",
    sites: "router.rs:2226",
    why:
      "Paired with an info! 'roll-call interrupted; stopping after N agents' log, and roll-call is a spoken feature, so the speech and log are the surfaces and this frame is the operator-stream record.",
  },
  "rollcall.started": {
    bucket: "diagnostic",
    sites: "router.rs:2212",
    why:
      "Roll-call is a spoken feature in which each agent introduces itself aloud, so the speech the user hears is the surface and this frame is only the operator-stream record that a roll-call run began.",
  },
  "runbook.blocked": {
    bucket: "diagnostic",
    sites: "router.rs:2842",
    why:
      "Returns the honest 'Runbooks are off' refusal to the user in the reply when the subsystem is disabled, so the refusal is already delivered and this reason-only frame is the operator-stream record.",
  },
  "runbook.plan": {
    bucket: "diagnostic",
    sites: "runbook.rs:961",
    why:
      "A runbook DAG plan carrying step ids and capability names but never input/output values; the runbook subsystem is benign-only and parks every consequential step, so the operator stream is the right surface.",
  },
  "runbook.run": {
    bucket: "diagnostic",
    sites: "runbook.rs:1190",
    why:
      "A runbook per-step run report carrying capability names and per-step outcomes but never any input/output value; it is a benign-only automation record, so the operator stream is the right surface, not a pixel.",
  },
  "runbook.status": {
    bucket: "diagnostic",
    sites: "main.rs:2900",
    why:
      "The benign-only automation-DAG master gate and step bound; the subsystem ships OFF and darwin.toml's [runbook] section is the honest surface for whether it is on, so this once-at-startup frame is only the operator-stream snapshot.",
  },
  "screen_context.loop_armed": {
    bucket: "diagnostic",
    sites: "main.rs:3582",
    why:
      "Paired with an info! 'screen-context loop armed; it starts when Vision connects' log that is the real surface; the loop starts on a later Vision reconnect, so this startup lifecycle frame is a stream/log breadcrumb.",
  },
  "screen_context.loop_started": {
    bucket: "diagnostic",
    sites: "main.rs:3567",
    why:
      "Paired with an info! 'started continuous screen-context loop (#42)' log that is the real surface; the rendered pixel for screen activity is screen_context.watching, so this startup lifecycle frame is a stream/log breadcrumb.",
  },
  "security.exposure": {
    bucket: "diagnostic",
    sites: "exposure.rs:419 exposure.rs:424",
    why:
      "The owner's half is the one-line summary the same tick caches, which posture::scanner_notes folds onto posture.snapshot and the PostureDashboardPanel draws under AMBIENT SCANNERS. What stays here is the per-socket table - an inventory, not a decision. The error arm reports a failed netstat read without clobbering the last honest summary.",
  },
  "security.interception": {
    bucket: "diagnostic",
    sites: "interception.rs:889",
    why:
      "Same fold as its two scanner siblings: the cached one-liner rides posture.snapshot into the PostureDashboardPanel's AMBIENT SCANNERS block, which is how a rogue trusted root CA reaches a person without being asked for. This frame is the full per-surface finding table for the operator's stream.",
  },
  "security.persistence": {
    bucket: "diagnostic",
    sites: "persistence.rs:1275",
    why:
      "Same fold as its two scanner siblings: the cached one-liner rides posture.snapshot into the AMBIENT SCANNERS block, so a new or unsigned autostart item reaches the owner unasked. This frame is the full per-surface inventory plus the skip list, for the operator's stream.",
  },
  "security.triage": {
    bucket: "diagnostic",
    sites: "triage.rs:701",
    why:
      "capture is USER-INVOKED and its TriageSummary is rendered straight back to the caller by anthropic::render_triage_summary - the owner asked and already has the answer in the reply. A HUD copy would echo a question they just asked. This frame records where the bundle landed.",
  },
  "selector.clarify": {
    bucket: "diagnostic",
    sites: "router.rs:1397",
    why:
      "A genuine-ambiguity clarification question that is voiced to the user by the orchestrator while establishing, querying, or firing nothing, so the spoken question is the surface and this frame is the operator-stream record.",
  },
  "selector.mode": {
    bucket: "diagnostic",
    sites: "router.rs:1409",
    why:
      "Records the routing mode the selector chose for the turn (a capability mode versus a plain route); it is an internal routing decision, so the operator stream is the right surface, not a pixel.",
  },
  "selector.standing_proposed": {
    bucket: "diagnostic",
    sites: "router.rs:2744",
    why:
      "Fires when a standing mission is proposed and parked for the spoken-yes replay; the proposal response is spoken back to the user, so this frame is the operator-stream record of the proposal.",
  },
  "sfx.cue.cached": {
    bucket: "diagnostic",
    sites: "main.rs:5465",
    why:
      "The cue was served from cache with no cloud call and its path is returned to the caller, so the audible cue is the surface; this cue-name-only frame is the operator-stream record.",
  },
  "sfx.cue.disabled": {
    bucket: "diagnostic",
    sites: "main.rs:5471",
    why:
      "The honest 'cue tier is off, turn on [voice].cloud_sfx' message is returned to the user in the reply; this cue-name-only frame is only the operator-stream record of an attempt while disabled.",
  },
  "sfx.cue.failed": {
    bucket: "diagnostic",
    sites: "main.rs:5482",
    why:
      "Paired with a warn! 'sfx-cue: play_cue generation failed' log, and the error message is returned to the caller; this cue-name-only frame is only the operator-stream record.",
  },
  "sfx.cue.generated": {
    bucket: "diagnostic",
    sites: "main.rs:5460",
    why:
      "The cue is played and its path returned to the caller, so the audible cue is the surface; this frame carries only the cue name as the operator-stream record that a fresh cue was generated.",
  },
  "sfx.failed": {
    bucket: "diagnostic",
    sites: "main.rs:5343",
    why:
      "Paired with a warn! 'sound-effect: sound_effect op failed' log, and the failure is returned to the user in the tool reply; this empty-payload frame is only the operator-stream record.",
  },
  "sfx.generated": {
    bucket: "diagnostic",
    sites: "main.rs:5338",
    why:
      "The generated sound-effect path is returned to the caller and played, so the audible output is the surface; this empty-payload frame is only the operator-stream record that a sound-effect op ran.",
  },
  "sfx.no_key": {
    bucket: "diagnostic",
    sites: "main.rs:5327",
    why:
      "The missing-ElevenLabs-key outcome is returned to the user in the tool reply telling them to add a key; there is no on-device fallback, so this empty-payload frame is only the operator-stream record.",
  },
  "snapshot.anchor": {
    bucket: "diagnostic",
    sites: "snapshot.rs:238 snapshot.rs:252",
    why:
      "A restore point taken BEFORE a consequential step. The step is what the operator is watching and has its own confirm-gate surface; the anchor is the record that makes 'undo that' nameable, read back through journal.rs.",
  },
  "standing.cancelled": {
    bucket: "diagnostic",
    sites: "anthropic.rs:13006",
    why:
      "Returns 'Cancelled standing mission {id}.' to the user in the reply, and cancellation is reversible so it is not gated; this id-only frame is the operator-stream record of the cancellation.",
  },
  "standing.created": {
    bucket: "diagnostic",
    sites: "anthropic.rs:12757",
    why:
      "The spoken acknowledgement 'Standing mission established' is returned to the user in the tool reply; there is no HUD card (pinned by silent-drops.test.ts), so this frame is the operator-stream and journal record.",
  },
  "standing.tripwire": {
    bucket: "diagnostic",
    sites: "main.rs:1911",
    why:
      "A mission that fired from a tripwire rather than a clock; the HUD sees the mission's own identical run frames either way, so this frame is kept for the operator stream and the journal, where the WHY is the whole value.",
  },
  "standing.tripwire_armed": {
    bucket: "diagnostic",
    sites: "standing.rs:640",
    why:
      "The user is told at the moment that matters, by the confirmation they just gave and the spoken ack this call returns into; there is no case for it, so this frame is the operator-stream and journal record.",
  },
  "threshold.confirm_refused": {
    bucket: "diagnostic",
    sites: "anthropic.rs:8056",
    why:
      "Paired with a warn! 'THRESHOLD: refusing to replay a parked action for a guest' log; refusing simply drops the parked action so nothing fires and the owner can re-initiate, making this the operator-stream security record.",
  },
  "threshold.fast_path_refused": {
    bucket: "diagnostic",
    sites: "router.rs:308",
    why:
      "A guest-turn fast-path refusal; the honest refusal is voiced back to the speaker and on the owner path this is a no-op, so this category-only frame is the operator-stream security record of a denied guest fast-path.",
  },
  "threshold.guest": {
    bucket: "diagnostic",
    sites: "threshold.rs:397",
    why:
      "The guest posture is enforced by the gates themselves and stated in the turn's own reply; a per-turn 'a guest is speaking' banner is a surface to add on purpose, not by wiring up this stray frame, so it stays the operator-stream record.",
  },
  "threshold.local_refused": {
    bucket: "diagnostic",
    sites: "router.rs:3697",
    why:
      "A guest-turn refusal of a local intent outside the read-only guest scope; the honest refusal is returned to the speaker and on the owner path this is a no-op, so this frame is the operator-stream security record.",
  },
  "threshold.tool_refused": {
    bucket: "diagnostic",
    sites: "anthropic.rs:7718",
    why:
      "Paired with a warn! 'THRESHOLD: refusing a tool outside the guest read-only scope' log; the call is refused and the turn's reply states it, so this frame is the operator-stream security record of the denied tool.",
  },
  "undo.armed": {
    bucket: "diagnostic",
    sites: "router.rs:3121",
    why:
      "Fires when 'undo that' arms and executes the derived inverse; the user hears the spoken outcome and there is no undo chip to render, so this tool/agent/seq frame is the operator-stream record of an undo.",
  },
  "utterance.self_echo": {
    bucket: "diagnostic",
    sites: "main.rs:3914",
    why:
      "Paired with an info! 'dropping implausible/self-echo transcript before route' log carrying the text; that log is the operator surface for a dropped self-echo, so this frame is the stream copy.",
  },
  "utterance.stale": {
    bucket: "diagnostic",
    sites: "main.rs:3812",
    why:
      "Paired with an info! 'utterance waited out a long in-flight turn; discarding as stale' log carrying the wait; that log is the operator surface for a discarded stale utterance, so this frame is the stream copy.",
  },
  "vad.backend_fallback": {
    bucket: "diagnostic",
    sites: "vad.rs:299",
    why:
      "Twin of vad.backend_live. The RMS gate is a working fallback, not a degradation the operator must act on, and it self-heals on the next weights retry.",
  },
  "vad.backend_live": {
    bucket: "diagnostic",
    sites: "vad.rs:280",
    why:
      "Which VAD decides speech frames is a startup/tuning fact, not an operator decision, and it flips silently mid-run as the weights land. The info! beside it is the surface that gets read.",
  },
  "voice.whisper_command": {
    bucket: "diagnostic",
    sites: "router.rs:402",
    why:
      "A whisper-mode toggle applied only after the owner voice-id all-scope gate so a bystander cannot flip it; the toggle takes effect and is acknowledged, so this frame is the operator-stream record.",
  },
  "voiceclone.cancelled": {
    bucket: "diagnostic",
    sites: "main.rs:5191",
    why:
      "Fail-safe cancellation when the confirmation is not a clear yes, so the audio never leaves the device and the reply tells the user; this frame carries only the agent slot as the operator-stream record.",
  },
  "voiceclone.cloned": {
    bucket: "diagnostic",
    sites: "main.rs:5222",
    why:
      "The clone result is returned to the user in the tool reply; this frame carries only the agent slot (never the voice id or key) as the operator-stream record of a completed clone.",
  },
  "voiceclone.failed": {
    bucket: "diagnostic",
    sites: "main.rs:5232",
    why:
      "Paired with a warn! 'voice-clone: clone_voice failed' log, and the failure is returned to the user in the reply; this frame carries only the agent slot as the operator-stream record.",
  },
  "voiceclone.forgot": {
    bucket: "diagnostic",
    sites: "main.rs:5251",
    why:
      "The forget outcome is returned to the user in the reply; this frame carries only whether a clone had existed (had_clone) as the operator-stream record that the stored clone was removed.",
  },
  "voiceclone.no_key": {
    bucket: "diagnostic",
    sites: "main.rs:5201",
    why:
      "With no ElevenLabs key the clone fails cleanly and the outcome is returned to the user; this frame carries only the agent slot (never the sample or key) as the operator-stream record.",
  },
  "voiceclone.proposed": {
    bucket: "diagnostic",
    sites: "main.rs:5283",
    why:
      "A spoken consent prompt is returned to the user asking to confirm the clone; this frame carries only the agent slot as the operator-stream record that a clone was proposed and is pending consent.",
  },
  "voiceid.denied": {
    bucket: "diagnostic",
    sites: "anthropic.rs:7247 anthropic.rs:7801 anthropic.rs:8030 anthropic.rs:12831 router.rs:283",
    why:
      "An owner-scope gate refusal for an unrecognized speaker; the turn's reply states the denial and the gate is additive (never permits what other gates block), so this phase-only frame is the operator-stream security record.",
  },
  "voiceid.enroll_refused": {
    bucket: "diagnostic",
    sites: "main.rs:5053",
    why:
      "Re-enrolling the owner voiceprint is refused for an unverified speaker so a bystander cannot take over the owner identity; the reply states the refusal, so this frame is the operator-stream security record.",
  },
  "voiceid.forget_refused": {
    bucket: "diagnostic",
    sites: "main.rs:5014",
    why:
      "A 'forget my voice' is refused for an unverified speaker so the profile cannot be deleted to defeat the threshold un-scope rail; the reply states the refusal, so this frame is the operator-stream security record.",
  },};
