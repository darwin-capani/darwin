import { useCallback, useState } from "react";
import type { MemoryState } from "../core/state";
import { agentProfile } from "../core/agents";
import { inTauri } from "../tauri/bridge";
import { sendCommand } from "../tauri/command";
import Frame from "./Frame";

/**
 * MEMORY // EPISODIC TIMELINE + WHAT DARWIN KNOWS ABOUT YOU.
 *
 * The HUD-side surface for the episodic store (Core-A, daemon/src/episodic.rs)
 * and the user model (Core-B, daemon/src/user_model.rs). It is TELEMETRY-FED and
 * READ-MOSTLY: the daemon emits only ACTIVITY — episodic.recorded (per turn),
 * user_model.consolidated[_failed], and memory.retention — never the episode
 * bodies or the profile entries. That is the privacy line, kept deliberately:
 *
 *   - OBSERVED, NOT CLAIRVOYANT — every entry comes from a real recorded turn;
 *     the panel shows THAT DARWIN remembered, with the agent + time, never a
 *     fabricated memory. Gated-out turns (transient screen-read, abandoned turn,
 *     voice-id UNVERIFIED, store off) are shown honestly as "not kept".
 *   - LOCAL + VOICE-INSPECTED — the redacted utterance/summary and the profile
 *     entries live in the daemon's SQLite and are recalled by VOICE
 *     (episodic_recall / user_model_query: "what do you know about me"). The HUD
 *     never streams or persists them.
 *   - BOUNDED, NOT "REMEMBERS EVERYTHING" — the retention pass evicts oldest at
 *     the cap; the eviction count is surfaced as the proof.
 *   - FORGETTABLE — the FORGET control clears the WHOLE user model via the
 *     daemon's user_model_forget tool (sent as a bounded `ask` command; the
 *     daemon is the trust boundary). The model can be WRONG and is correctable.
 *
 * No data is persisted client-side beyond the daemon's own store.
 */

/** Memory's accent hue — borrow Mnemosyne's (the semantic-memory agent that owns
 *  recall) from the static roster, falling back to a calm indigo. Drives the
 *  panel accent; RED stays reserved for the stale/alert affordance only. */
const MEMORY_HUE = agentProfile("mnemosyne")?.hue ?? 280;

/** The exact natural-language phrase that triggers the daemon's
 *  user_model_forget tool (it clears the whole user-model tier and honestly
 *  reports the count). Routed through the bounded `ask` command — the HUD never
 *  forgets anything itself; the daemon is the trust boundary. */
const FORGET_USER_MODEL_PHRASE = "Forget everything you know about me — clear my whole user model.";

/** Strip the agent namespace prefix to a short scope label (e.g. "agent.darwin"
 *  -> "DARWIN", "agent.mnemosyne" -> "MNEMOSYNE"). The shared scope reads as
 *  "SHARED". An empty/odd value still renders rather than dropping. */
function scopeLabel(agent: string): string {
  if (agent.length === 0) return "SHARED";
  const bare = agent.startsWith("agent.") ? agent.slice("agent.".length) : agent;
  if (bare === "darwin") return "SHARED";
  return bare.toUpperCase();
}

/** Render an envelope rfc3339 ts as a compact local HH:MM:SS, or "" if it does
 *  not parse (never throw on an odd ts). */
function clock(ts: string): string {
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

export default function MemoryPanel({
  memory,
  beliefCount = 0,
}: {
  memory: MemoryState;
  /** How many beliefs the LIVE user-model profile holds, from the MIRROR
   *  snapshot (`mirror.belief` action "snapshot", built by the daemon from
   *  user_model::snapshot). App passes `state.mirror?.beliefs.length`.
   *
   *  WHAT WENT WRONG: this panel used to infer "a profile exists" purely from
   *  `memory.userModelEntries`, which is set ONLY by `user_model.consolidated`
   *  — an event the reflection task emits at most once per ~20h
   *  (reflect.rs STALENESS_SECS), that is NOT in telemetry.rs's sticky
   *  replay-on-connect set, and that therefore never arrives on a fresh HUD
   *  start. The mirror snapshot IS retained and IS replayed on connect. So on
   *  every HUD reload, MIRROR listed the beliefs DARWIN holds while MEMORY said
   *  "NOTHING OBSERVED YET" and disabled the FORGET control with the false
   *  tooltip "nothing observed to forget yet" — for up to ~20 hours, precisely
   *  when a user who had just read their profile would reach for it. */
  beliefCount?: number;
}) {
  const shell = inTauri();
  const { timeline } = memory;
  const kept = memory.recordedCount;
  const total = memory.recordedCount + memory.gatedCount;

  // Header tag: the live "kept of seen" ratio once any turn has been observed,
  // else an honest resting state.
  const tag = total > 0 ? `${kept}/${total} KEPT` : "OBSERVED ONLY";

  const style = { ["--memory-hue" as string]: String(MEMORY_HUE) };

  return (
    <Frame className="memory" title="MEMORY // EPISODIC + USER MODEL" tag={tag}>
      <div className="mem-body" style={style}>
        {/* Honesty banner — the whole contract in one line. */}
        <div className="mem-honesty">
          <span className="mem-honesty-dot" aria-hidden="true" />
          <span className="mem-honesty-text">OBSERVED · LOCAL · BOUNDED · FORGETTABLE</span>
          <span className="mem-honesty-sub">
            built from real turns · redacted · recalled by voice · not clairvoyant
          </span>
        </div>

        {/* ---- EPISODIC TIMELINE — newest-first episode-store outcomes. ---- */}
        <div className="mem-section">
          <div className="mem-section-head">
            <span className="mem-section-label">EPISODIC TIMELINE</span>
            <span className="mem-section-note">recent turns · newest first</span>
          </div>

          {timeline.length === 0 ? (
            <div className="mem-empty">
              NO EPISODES OBSERVED YET — a completed turn appears here once recorded
            </div>
          ) : (
            <div className="mem-timeline">
              {timeline.map((e) => (
                <div key={e.seq} className={`mem-ep ${e.recorded ? "kept" : "gated"}`}>
                  <span className="mem-ep-dot" aria-hidden="true" />
                  <span className="mem-ep-time">{clock(e.ts) || "—"}</span>
                  <span className="mem-ep-scope">{scopeLabel(e.agent)}</span>
                  <span className="mem-ep-state">
                    {e.recorded ? "RECORDED" : "NOT KEPT"}
                  </span>
                </div>
              ))}
            </div>
          )}

          <div className="mem-timeline-foot">
            Each row is THAT a turn was remembered — the redacted utterance stays
            local in the daemon and is recalled by voice (&ldquo;recall when
            we&hellip;&rdquo;). &ldquo;NOT KEPT&rdquo; turns were gated out: a
            transient screen-read, an empty/abandoned turn, an unverified speaker,
            or the store off.
          </div>
        </div>

        {/* ---- WHAT DARWIN KNOWS ABOUT YOU — the user-model inspector. ---- */}
        <div className="mem-section">
          <div className="mem-section-head">
            <span className="mem-section-label">WHAT DARWIN KNOWS ABOUT YOU</span>
            <span className="mem-section-note">observed profile · not certain</span>
          </div>

          {beliefCount === 0 && memory.userModelEntries === null ? (
            <div className="mem-empty">
              NOTHING OBSERVED YET — the profile compounds from repeated signals
              across your turns; one-off mentions are not kept
            </div>
          ) : (
            <div className="mem-um">
              <div className="mem-um-stat">
                {/* The PROFILE SIZE, from the live MIRROR snapshot. NOT
                    `userModelEntries` — that is `entries_written`, how many
                    entries the LAST consolidation pass wrote, which counts only
                    the groups this pass's inputs produced (the most recent 200
                    shared-tier episodes + stored facts). Entries written by an
                    earlier pass whose source episodes have scrolled out of that
                    window stay in the profile and are never re-counted, so
                    printing it under "WHAT DARWIN KNOWS ABOUT YOU" under-reported
                    what DARWIN actually stores — and read "0 OBSERVED ENTRIES"
                    after a quiet cycle while MIRROR listed the beliefs. */}
                <span className="mem-um-num">{beliefCount}</span>
                <span className="mem-um-unit">
                  OBSERVED {beliefCount === 1 ? "ENTRY" : "ENTRIES"}
                </span>
              </div>
              <div className="mem-um-meta">
                {memory.userModelEntries !== null ? (
                  <span className="mem-um-written">
                    {memory.userModelEntries} WRITTEN LAST PASS
                  </span>
                ) : null}
                {memory.userModelConsolidatedAt ? (
                  <span className="mem-um-when">
                    CONSOLIDATED {clock(memory.userModelConsolidatedAt) || "—"}
                  </span>
                ) : null}
                {memory.userModelStale ? (
                  <span className="mem-um-stale">LAST PASS FAILED — MAY BE STALE</span>
                ) : null}
              </div>
            </div>
          )}

          <div className="mem-um-foot">
            Preferences, patterns, recurring topics and style — each entry carries
            HOW MANY TIMES it was observed and the episodes/facts it came from. The
            profile lives local in the daemon; ask{" "}
            <b>&ldquo;what do you know about me&rdquo;</b> and DARWIN reads it back
            WITH its provenance. It can be WRONG — say{" "}
            <b>&ldquo;that&rsquo;s wrong, &hellip;&rdquo;</b> to correct an entry.
          </div>

          {/* The FORGET gate keys off the LIVE profile (the replayed MIRROR
              snapshot), not off the ~20-hourly consolidation activity event —
              otherwise the control is dead after every HUD start on a daemon
              that already holds a profile. */}
          <ForgetControl
            shell={shell}
            hasModel={beliefCount > 0 || memory.userModelEntries !== null}
          />
        </div>

        {/* ---- RETENTION — the bounded evict-oldest proof. ---- */}
        <div className="mem-retention">
          <span className="mem-retention-label">RETENTION</span>
          {memory.lastEvictedEpisodes === null ? (
            <span className="mem-retention-text">
              bounded · evict-oldest at the cap — not &ldquo;remembers everything&rdquo;
            </span>
          ) : (
            <span className="mem-retention-text">
              last pass evicted {memory.lastEvictedEpisodes}{" "}
              {memory.lastEvictedEpisodes === 1 ? "episode" : "episodes"}
              {memory.lastRetentionAt ? ` @ ${clock(memory.lastRetentionAt)}` : ""} ·
              bounded evict-oldest
            </span>
          )}
        </div>
      </div>
    </Frame>
  );
}

/** The FORGET control — clears the WHOLE user model via the daemon's
 *  user_model_forget tool, sent as a bounded `ask` command (the daemon is the
 *  trust boundary; the HUD never forgets anything itself). Two-step (arm ->
 *  confirm) so a single stray click cannot wipe the profile. Reports the
 *  daemon's own honest reply. Disabled outside the desktop shell (no command
 *  channel in a plain browser) and when there is nothing observed to forget. */
function ForgetControl({ shell, hasModel }: { shell: boolean; hasModel: boolean }) {
  const [armed, setArmed] = useState(false);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<string | null>(null);

  const forget = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    setResult(null);
    try {
      const reply = await sendCommand({ cmd: "ask", text: FORGET_USER_MODEL_PHRASE });
      // The daemon's user_model_forget tool reports how many entries it cleared;
      // surface its own prose, never a fabricated success. A failed/declined
      // command shows its honest error.
      setResult(reply.ok ? reply.reply ?? "Cleared." : reply.error ?? "Could not forget.");
    } catch {
      setResult("command failed");
    } finally {
      setBusy(false);
      setArmed(false);
    }
  }, [busy]);

  const disabled = !shell || busy || !hasModel;

  return (
    <div className="mem-forget">
      {!armed ? (
        <button
          type="button"
          className="mem-forget-btn"
          onClick={() => setArmed(true)}
          disabled={disabled}
          title={
            !shell
              ? "available in the DARWIN desktop app"
              : !hasModel
                ? "nothing observed to forget yet"
                : "clear the whole observed user model"
          }
        >
          FORGET WHAT YOU KNOW ABOUT ME
        </button>
      ) : (
        <div className="mem-forget-confirm">
          <span className="mem-forget-q">Clear the whole observed profile?</span>
          <button
            type="button"
            className="mem-forget-yes"
            onClick={() => void forget()}
            disabled={busy}
          >
            {busy ? "FORGETTING…" : "YES, FORGET"}
          </button>
          <button
            type="button"
            className="mem-forget-no"
            onClick={() => setArmed(false)}
            disabled={busy}
          >
            CANCEL
          </button>
        </div>
      )}
      {result ? <div className="mem-forget-result">{result}</div> : null}
      <div className="mem-forget-note">
        Clears ONLY the user-model profile (not your facts, the world model, or
        episodes — each has its own forget path). You are always in control.
      </div>
    </div>
  );
}
