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
 *               inventory line, and inventing a rationale for 103 of them is
 *               exactly how the previous list rotted. `UNTRIAGED` below is the
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
export const UNTRIAGED = 103;

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
 * TRUE PIXEL-FREE POPULATION = PIXEL_FREE (115 topics / 131 sites)
 *                            + NO_OP_CASES (9 topics / 14 sites)
 *                            = 124 topics / 145 sites,
 * out of 294 topics / 395 production emit calls — 42.2% of the topics the daemon
 * emits reach no pixel, not the 39.1% that counting only PIXEL_FREE reports.
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
  "agent.reroute": { bucket: "untriaged", sites: "router.rs:3432" },
  "aperture.command": { bucket: "untriaged", sites: "router.rs:594" },
  "aperture.configured": { bucket: "untriaged", sites: "main.rs:2883" },
  "aperture.loop_started": { bucket: "untriaged", sites: "main.rs:3617" },
  "app.autostart_failed": { bucket: "untriaged", sites: "main.rs:3520" },
  "app.proxy_denied": {
    bucket: "diagnostic",
    sites: "fetchproxy.rs:611 fetchproxy.rs:644 genproxy.rs:337 genproxy.rs:367",
    why:
      "A sandboxed app's fetch/generate call was refused at the broker. The app learns the stable reason; the operator's surface is the warn! next to each emit. Four sites, all already carrying the marker before this sweep.",
  },
  "app.result": { bucket: "untriaged", sites: "apps.rs:2822" },
  "app.tool_invoked": { bucket: "untriaged", sites: "anthropic.rs:2101" },
  "audio.capture_stopped": {
    bucket: "diagnostic",
    sites: "audio.rs:579",
    why:
      "Capture DIED mid-run, which deliberately drops the sender and ends main for a launchd restart. The HUD's real signal is the socket closing a moment later; this frame is the reason, on the live stream.",
  },
  "audio.sound": { bucket: "untriaged", sites: "router.rs:8470" },
  "audit.anchor": { bucket: "untriaged", sites: "audit.rs:860" },
  "babel.interpret": { bucket: "untriaged", sites: "anthropic.rs:9588" },
  "barge_in": {
    bucket: "diagnostic",
    sites: "audio.rs:443",
    why:
      "The barge itself is already a pixel - the reply is cut and the core state leaves speaking. This frame carries only the triggering rms, for tuning the detector. A toast per barge-in would fire on ordinary interruption.",
  },
  "calibrate.report": { bucket: "untriaged", sites: "calibrate.rs:526" },
  "command.auth_failed": { bucket: "untriaged", sites: "command.rs:859" },
  "command.channel_up": { bucket: "untriaged", sites: "main.rs:3150" },
  "command.denied": { bucket: "untriaged", sites: "command.rs:839 command.rs:846 command.rs:872" },
  "daemon.ready": { bucket: "untriaged", sites: "main.rs:3450" },
  "design_voice.created": { bucket: "untriaged", sites: "main.rs:5692" },
  "design_voice.failed": { bucket: "untriaged", sites: "main.rs:5697" },
  "design_voice.no_key": { bucket: "untriaged", sites: "main.rs:5675" },
  "dls.status": { bucket: "untriaged", sites: "main.rs:3006" },
  "egress.beacon": { bucket: "untriaged", sites: "egress_beacon.rs:675" },
  "egress.newhost": { bucket: "untriaged", sites: "egress_beacon.rs:649" },
  "egress.refused": { bucket: "untriaged", sites: "anthropic.rs:7219 anthropic.rs:7654 anthropic.rs:7688 anthropic.rs:14147" },
  "enclave.status": { bucket: "untriaged", sites: "main.rs:2958" },
  "envlock.build_refused": { bucket: "untriaged", sites: "envlock.rs:641" },
  "envlock.built": { bucket: "untriaged", sites: "envlock.rs:673" },
  "envlock.verify": { bucket: "untriaged", sites: "envlock.rs:534" },
  "forge.aborted": { bucket: "untriaged", sites: "forge.rs:1317" },
  "forge.dismissed": { bucket: "untriaged", sites: "command.rs:1932" },
  "forge.drafting": { bucket: "untriaged", sites: "forge.rs:686" },
  "forge.suppressed": { bucket: "untriaged", sites: "forge.rs:1244" },
  "forge_gap.blocked": { bucket: "untriaged", sites: "forge_gap.rs:325 forge_gap.rs:350" },
  "forge_gap.detected": { bucket: "untriaged", sites: "forge_gap.rs:370" },
  "hyde": {
    bucket: "diagnostic",
    sites: "docsearch.rs:2259",
    why:
      "A retrieval-internal trace. The HUD shows the RESULT via docsearch.searched; which of the two query vectors found it is an eval question answered off the live stream.",
  },
  "inference.degraded": { bucket: "untriaged", sites: "inference.rs:1004" },
  "inference.health": { bucket: "untriaged", sites: "inference.rs:989" },
  "inference.recovered": { bucket: "untriaged", sites: "inference.rs:1011" },
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
  "lumen.action": { bucket: "untriaged", sites: "router.rs:4128" },
  "lumen.configured": { bucket: "untriaged", sites: "main.rs:2867" },
  "lumen.read": { bucket: "untriaged", sites: "router.rs:4106" },
  "music.failed": { bucket: "untriaged", sites: "main.rs:5408" },
  "music.generated": { bucket: "untriaged", sites: "main.rs:5397" },
  "music.intent": { bucket: "untriaged", sites: "router.rs:1044" },
  "music.no_key": { bucket: "untriaged", sites: "main.rs:5389" },
  "notebook.retention": { bucket: "untriaged", sites: "main.rs:1087" },
  "optimize.trace_corrected": { bucket: "untriaged", sites: "main.rs:4643" },
  "pasteboard.command": { bucket: "untriaged", sites: "router.rs:713" },
  "pasteboard.configured": { bucket: "untriaged", sites: "main.rs:2874" },
  "pasteboard.loop_started": { bucket: "untriaged", sites: "main.rs:3600" },
  "policy.parked_unattended": { bucket: "untriaged", sites: "anthropic.rs:7520" },
  "policy.user_set": { bucket: "untriaged", sites: "router.rs:338" },
  "privacy.transient_screen_read": { bucket: "untriaged", sites: "main.rs:4521" },
  "pronunciation.created": { bucket: "untriaged", sites: "main.rs:5745" },
  "pronunciation.failed": { bucket: "untriaged", sites: "main.rs:5750" },
  "pronunciation.no_key": { bucket: "untriaged", sites: "main.rs:5730" },
  "realm.blocked": { bucket: "untriaged", sites: "anthropic.rs:11778" },
  "realm.verdict": { bucket: "untriaged", sites: "anthropic.rs:11805" },
  "registry.install_gate": { bucket: "untriaged", sites: "registry.rs:911" },
  "registry.status": { bucket: "untriaged", sites: "registry.rs:734" },
  "registry.verdict": { bucket: "untriaged", sites: "registry.rs:841" },
  "rollcall.completed": { bucket: "untriaged", sites: "router.rs:2271" },
  "rollcall.interrupted": { bucket: "untriaged", sites: "router.rs:2226" },
  "rollcall.started": { bucket: "untriaged", sites: "router.rs:2212" },
  "runbook.blocked": { bucket: "untriaged", sites: "router.rs:2842" },
  "runbook.plan": { bucket: "untriaged", sites: "runbook.rs:961" },
  "runbook.run": { bucket: "untriaged", sites: "runbook.rs:1190" },
  "runbook.status": { bucket: "untriaged", sites: "main.rs:2900" },
  "screen_context.loop_armed": { bucket: "untriaged", sites: "main.rs:3582" },
  "screen_context.loop_started": { bucket: "untriaged", sites: "main.rs:3567" },
  "security.exposure": { bucket: "untriaged", sites: "exposure.rs:419 exposure.rs:424" },
  "security.interception": { bucket: "untriaged", sites: "interception.rs:889" },
  "security.persistence": { bucket: "untriaged", sites: "persistence.rs:1275" },
  "security.triage": { bucket: "untriaged", sites: "triage.rs:701" },
  "selector.clarify": { bucket: "untriaged", sites: "router.rs:1397" },
  "selector.mode": { bucket: "untriaged", sites: "router.rs:1409" },
  "selector.standing_proposed": { bucket: "untriaged", sites: "router.rs:2744" },
  "sfx.cue.cached": { bucket: "untriaged", sites: "main.rs:5465" },
  "sfx.cue.disabled": { bucket: "untriaged", sites: "main.rs:5471" },
  "sfx.cue.failed": { bucket: "untriaged", sites: "main.rs:5482" },
  "sfx.cue.generated": { bucket: "untriaged", sites: "main.rs:5460" },
  "sfx.failed": { bucket: "untriaged", sites: "main.rs:5343" },
  "sfx.generated": { bucket: "untriaged", sites: "main.rs:5338" },
  "sfx.no_key": { bucket: "untriaged", sites: "main.rs:5327" },
  "snapshot.anchor": {
    bucket: "diagnostic",
    sites: "snapshot.rs:238 snapshot.rs:252",
    why:
      "A restore point taken BEFORE a consequential step. The step is what the operator is watching and has its own confirm-gate surface; the anchor is the record that makes 'undo that' nameable, read back through journal.rs.",
  },
  "standing.cancelled": { bucket: "untriaged", sites: "anthropic.rs:13006" },
  "standing.created": { bucket: "untriaged", sites: "anthropic.rs:12757" },
  "standing.tripwire": { bucket: "untriaged", sites: "main.rs:1911" },
  "standing.tripwire_armed": { bucket: "untriaged", sites: "standing.rs:640" },
  "threshold.confirm_refused": { bucket: "untriaged", sites: "anthropic.rs:8056" },
  "threshold.fast_path_refused": { bucket: "untriaged", sites: "router.rs:308" },
  "threshold.guest": { bucket: "untriaged", sites: "threshold.rs:397" },
  "threshold.local_refused": { bucket: "untriaged", sites: "router.rs:3697" },
  "threshold.tool_refused": { bucket: "untriaged", sites: "anthropic.rs:7718" },
  "undo.armed": { bucket: "untriaged", sites: "router.rs:3121" },
  "utterance.self_echo": { bucket: "untriaged", sites: "main.rs:3914" },
  "utterance.stale": { bucket: "untriaged", sites: "main.rs:3812" },
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
  "voice.whisper_command": { bucket: "untriaged", sites: "router.rs:402" },
  "voiceclone.cancelled": { bucket: "untriaged", sites: "main.rs:5191" },
  "voiceclone.cloned": { bucket: "untriaged", sites: "main.rs:5222" },
  "voiceclone.failed": { bucket: "untriaged", sites: "main.rs:5232" },
  "voiceclone.forgot": { bucket: "untriaged", sites: "main.rs:5251" },
  "voiceclone.no_key": { bucket: "untriaged", sites: "main.rs:5201" },
  "voiceclone.proposed": { bucket: "untriaged", sites: "main.rs:5283" },
  "voiceid.denied": { bucket: "untriaged", sites: "anthropic.rs:7247 anthropic.rs:7801 anthropic.rs:8030 anthropic.rs:12831 router.rs:283" },
  "voiceid.enroll_refused": { bucket: "untriaged", sites: "main.rs:5053" },
  "voiceid.forget_refused": { bucket: "untriaged", sites: "main.rs:5014" },};
