use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::Result;
use serde_json::json;
use tracing::{error, info, warn};

use std::sync::Arc;

use crate::actions;
use crate::agents::{Agent, AgentRegistry};
use crate::anthropic;
use crate::apps::{self, AppRegistry};
use crate::config::Config;
use crate::inference::{Classification, DescribeOutcome, GenerateOutcome, InferenceClient};
use crate::memory::Memory;
use crate::speech;
use crate::telemetry;

/// Exchange pairs sent as chat history with every LLM-voiced reply.
const HISTORY_EXCHANGES: usize = 6;
/// Recent DARWIN replies passed as the anti-repeat `avoid` list on the cloud
/// conversation path. Opus 4.8 takes no temperature/top_p/top_k, so the only
/// lever against a greeting collapsing to one fixed reply is changing the
/// prompt per call — and the most recent few replies are what a repeated bare
/// "Hi DARWIN" would otherwise echo, so they are exactly what to dodge.
const AVOID_RECENT_REPLIES: usize = 4;
/// Most-recent facts injected into every LLM-voiced reply.
const FACTS_LIMIT: usize = 12;
/// Token budget for locally generated replies (persona keeps them short).
const GENERATE_MAX_TOKENS: u32 = 200;
/// Data note for the local degrade path when cloud completion fails: the
/// local model must still answer fully, not announce reduced capability.
const CLOUD_DEGRADE_NOTE: &str = "Cloud uplink unavailable - answer fully and directly from your own local knowledge; do not mention the uplink unless the user asks why an answer is brief.";

pub struct RouteOutcome {
    pub routed_to: &'static str,
    pub response: String,
    /// The agent that handled this request (Darwin-Prime delegation). Owns
    /// the persona/voice the reply was spoken in and the memory namespace the
    /// exchange is recorded under; main uses it for namespaced bookkeeping and
    /// the HUD already saw it via the agent.active telemetry route() emitted.
    pub agent: String,
    /// The handling agent's memory namespace ("agent.<name>"); main tags the
    /// recorded transcript/exchange with it so recall stays per-agent.
    pub namespace: String,
    /// Set when route() already spoke the reply (the streamed converse
    /// path); main then skips speech::speak and uses these timings.
    pub spoken: Option<SpokenReply>,
}

/// Timings for a reply that was spoken inside route() via converse.
pub struct SpokenReply {
    /// route() entry -> the server's done event (contract item 6).
    pub route_ms: u64,
    pub report: speech::SpeakReport,
}

/// What a local handler produces: verified data, not final prose. When
/// llm_voice is set the LLM phrases the reply in persona around `data`;
/// the raw data string itself is the spoken fallback if generation fails.
struct HandlerOutput {
    data: String,
    llm_voice: bool,
}

/// Contract policy: cloud iff complexity == "heavy" OR confidence below the
/// configured threshold; everything else is handled locally. Local
/// llm_voice replies are generated AND spoken here in one streamed converse
/// call (`started` is the utterance-pickup instant for first_audio timing);
/// cloud replies still come back as text for main to speak. `reply` is the
/// session main opened at utterance receipt (possibly already carrying the
/// instant opener) — every spoken path appends to it. `brief` is the
/// proactive first-contact brief when this utterance ended an away gap:
/// verified data assembled daemon-side, appended to the converse data so
/// the persona phrases it. It rides ONLY the LLM-voiced local path — never
/// a verbatim-spoken handler reply, and not the cloud tool loop (per the
/// proactive contract the brief is converse data).
#[allow(clippy::too_many_arguments)]
pub async fn route(
    class: &Classification,
    text: &str,
    cfg: &Config,
    memory: &Memory,
    infer: &mut InferenceClient,
    started: Instant,
    reply: &mut speech::ReplySession,
    brief: Option<&str>,
    app_registry: &Arc<AppRegistry>,
    agents: &AgentRegistry,
    cloud_reachable: bool,
    root: &Path,
) -> Result<RouteOutcome> {
    let route_entry = Instant::now();

    // VAULT MODE ("go dark", vault.rs) + THRESHOLD GUEST (guest = local-only) — SEAM 1
    // of 2. Fold BOTH an active vault AND a guest turn into THIS turn's cloud
    // reachability ONCE, at entry, so EVERY downstream cloud decision that consults
    // `cloud_reachable` (the conversation brain, the roster answer, capability routing,
    // agent selection) deterministically sees NO cloud this turn and stays on the local
    // MLX brain. RESTRICT-ONLY + COMPOSABLE (`guest OR vault -> local`): each
    // `deny_cloud` can only turn a reachable cloud UNREACHABLE, never the reverse, so
    // with BOTH off this is byte-for-byte today's `cloud_reachable` (the owner still
    // uses the cloud by default). GUEST rationale: a bystander's turn must never reach
    // the owner's PAID cloud — that would append an obol spend row + bump the owner's
    // daily budget (a durable, owner-readable trace) and egress the guest's turn under
    // the owner's key. The actuating tool-loop gate (which does not consult
    // reachability) is closed separately at SEAM 2 below.
    let cloud_reachable = crate::threshold::deny_cloud(crate::vault::deny_cloud(cloud_reachable));

    // PANIC / LOCKDOWN (task #12) — THE emergency stop, honored BEFORE anything
    // else, even mid-confirmation / mid-anything. Any panic phrase ("panic",
    // "lockdown", "stop everything", "kill switch", "shut it all down") engages
    // the emergency stop NOW: it sets the process-global flag (so every master
    // gate reads OFF from this instant — consequential, proactive, MCP, standing,
    // heal, forge, the mic), DROPS any parked confirmation, PERSISTS a marker (so
    // a restart re-enters lockdown), audits the event, and speaks an honest
    // confirmation. This runs FIRST so even a parked outward action awaiting a
    // spoken yes is killed rather than confirmed. It is the SPOKEN twin of the
    // HUD `Command::Panic` verb. HONEST: it stops FUTURE actions + the mic and
    // persists; it cannot undo an action already executed.
    if crate::lockdown::is_panic_intent(text) {
        let msg = crate::lockdown::panic().await;
        // End any capture ALREADY RUNNING. `send_op`'s gate stops a new one from
        // starting; the Vision app is a separate process, so one in flight keeps
        // going until told otherwise. The HUD panic path sends the same stops, so
        // the two surfaces behave identically — a panic that closed the lens only
        // when spoken would be the same split this codebase keeps producing.
        crate::apps::stop_all_captures(app_registry).await;
        telemetry::emit("system", "lockdown.panic", json!({"via": "voice"}));
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        return Ok(RouteOutcome {
            routed_to: "local",
            response: msg.to_string(),
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // UNLOCK (task #12) — the explicit, deliberate USER resume ("unlock" / "resume
    // normal" / "end lockdown"). This is the SPOKEN twin of the HUD
    // `Command::Unlock` verb and, together with it, the ONLY path to
    // `lockdown::unlock` — there is NO route from the model tool loop, an MCP
    // server, or injected/agent text. It clears the flag (every gate returns to
    // its CONFIGURED value — lockdown was an overlay, nothing was clobbered) and
    // removes the marker (the next restart comes up normal).
    //
    // THIS ARM CANNOT LIFT A LIVE LOCKDOWN, and it used to claim it could. A
    // panic suppresses the microphone (`audio.rs`: mic_capture_suppressed drops
    // every chunk ahead of the VAD), so while locked no utterance is ever
    // produced and `route` is never called. What this arm actually handles is an
    // unlock spoken when NOTHING is locked — which `lockdown::unlock` now answers
    // honestly instead of announcing a lift that did not happen.
    //
    // Recovery from a live lockdown is the HUD `Command::Unlock` verb, which is
    // what PANIC_CONFIRMATION now tells the user.
    if crate::lockdown::is_unlock_intent(text) {
        let msg = crate::lockdown::unlock().await;
        telemetry::emit("system", "lockdown.unlock", json!({"via": "voice"}));
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        return Ok(RouteOutcome {
            routed_to: "local",
            response: msg.to_string(),
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // CROSS-TURN SPOKEN CONFIRMATION GATE (round F). When a consequential action
    // is awaiting a spoken human "yes" (parked last turn by execute_tool while
    // the master switch is on), THIS utterance is first read as a reply to that
    // pending — BEFORE any classifier/cloud/conversation routing — exactly like
    // the roll-call pre-check. The classifier never sees a "confirm"/"cancel" in
    // this state, so the parked action's fate is decided deterministically here:
    //   * Affirm  -> REPLAY the EXACT parked {tool,input} in Execute mode (the
    //                only thing in the whole system that fires a real action),
    //                speak the result, clear the slot.
    //   * Deny    -> clear the slot, acknowledge ("Cancelled.").
    //   * Unrelated -> clear the slot (so a stray later command can NEVER be
    //                mistaken as confirming the stale action) and fall through to
    //                route THIS utterance normally.
    // A pending older than the TTL is treated as already gone (take_live drops
    // it). The slot also shares the barge / roll-call-cancel lifecycle
    // (speech::clear_barge_in -> confirm::clear), so an interrupted turn never
    // leaves an action armed.
    if let Some(pending) = crate::confirm::take_live(Instant::now()) {
        // Decision is a PURE function of (utterance, pending): Affirm -> replay
        // the EXACT parked action; Deny -> a spoken cancel; Unrelated -> drop and
        // fall through. The slot was already emptied by take_live, so whatever
        // happens the stale action can never later be confirmed.
        let namespace = pending.agent.clone();
        let tool = pending.tool.clone();
        match crate::confirm::resolve_reply(pending, text, |t| {
            format!("Cancelled. I won't {}.", action_phrase(t))
        }) {
            crate::confirm::Resolution::Replay(pending) => {
                // The human said yes. Replay in Execute mode: SAME tool+input,
                // re-checking the parked agent's allowlist AND the master switch
                // (replay_confirmed_action enforces both). Nothing is re-derived
                // from this utterance — only what was previewed can fire.
                let (outcome, is_error) =
                    anthropic::replay_confirmed_action(&pending, memory).await;
                let agent = agent_for_namespace(agents, &namespace);
                emit_agent_active(agent);
                // PLAN-APPLY: a replay can RE-PARK instead of executing when the
                // action's state drifted since its plan was shown (plan.rs). In that
                // case a FRESH pending now sits in the slot (and the re-park already
                // published its own `confirm.parked` + drift `plan.diff`), so we must
                // NOT emit `confirm.affirmed` — that HUD event clears the just-shown
                // diff and would blank the panel for an action still awaiting confirm.
                // The action resolved (executed/errored) iff the slot is now EMPTY.
                let reparked = crate::confirm::peek_pending(Instant::now()).is_some();
                if !reparked {
                    telemetry::emit(
                        "system",
                        "confirm.affirmed",
                        json!({"tool": tool, "is_error": is_error}),
                    );
                }
                return Ok(RouteOutcome {
                    routed_to: "local",
                    response: outcome,
                    agent: agent.name.clone(),
                    namespace,
                    spoken: None,
                });
            }
            crate::confirm::Resolution::Cancelled(ack) => {
                let agent = agent_for_namespace(agents, &namespace);
                emit_agent_active(agent);
                telemetry::emit("system", "confirm.denied", json!({"tool": tool}));
                // A DENIAL IS A DECISION, and the audit log recorded only the park.
                // "The operator was asked and said no" is exactly the kind of fact a
                // hash-chained record exists to hold — and its absence is worse than
                // an absent execution record, because a reader cannot distinguish a
                // refused action from one that was never answered at all.
                crate::audit::record_global(
                    &namespace, &tool, &tool,
                    crate::policy::Decision::Ask, crate::audit::Outcome::Denied,
                ).await;
                return Ok(RouteOutcome {
                    routed_to: "local",
                    response: ack,
                    agent: agent.name.clone(),
                    namespace,
                    spoken: None,
                });
            }
            crate::confirm::Resolution::PassThrough => {
                // Neither yes nor no: the user moved on. The slot is already
                // cleared, so the stale action can never later be confirmed. Fall
                // through and route THIS utterance normally. No tacked-on note —
                // the normal reply to the new utterance follows immediately;
                // telemetry records the drop for the HUD/audit.
                telemetry::emit("system", "confirm.dropped_unrelated", json!({"tool": tool}));
            }
        }
    }

    // VOICE-ID STRICT SCOPE (round G): under [voice_id].gate_scope = "all", an
    // unrecognized speaker is blocked from EVERY command, not just outward ones.
    // This runs AFTER the confirmation pre-check (so a parked action's own
    // voice-gated replay still resolves) but BEFORE any other routing, so an
    // unverified bystander gets nothing under the strict posture. Under the
    // DEFAULT "consequential" scope `allow_noncly()` is always true and this is a
    // no-op; with voice-id off/unenrolled the gate is OFF and it is a no-op too —
    // the consequential-only layer in execute_tool/replay still applies as the
    // common case. ADDITIVE: never permits anything the other gates would block.
    if !crate::voiceid::current_turn_gate().allow_noncly() {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        telemetry::emit("system", "voiceid.denied", json!({"phase": "all_scope"}));
        return Ok(RouteOutcome {
            routed_to: "local",
            response: crate::voiceid::unrecognized_refusal(),
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // THRESHOLD — GUEST MODE fast-path gate. A guest scope confines a bystander to
    // plain conversation, translation, and non-personal status. EVERY specialized
    // route() fast path below BYPASSES the tool-loop + recall gates and either READS
    // the owner's personal data (activity / screen / clipboard / notebooks / reports
    // / lifelog / rewind / decision traces / user-model / vision describe) or takes a
    // consequential / owner-CONTROL action (policy / model-swap / voice-mode / vault
    // / macros / undo / charts / artifacts / music / images / designs / audio). None
    // is safe for a bystander, so a guest turn that would trigger one is REFUSED
    // HERE — before it can fire, with NO read and NO write. A guest-safe turn
    // (conversation / translation / status) matches none of these anchored
    // classifiers and flows through to the already guest-gated conversational path.
    // On the owner path (no scope installed) this is a no-op and routing is
    // byte-for-byte today's.
    if crate::threshold::is_guest_turn() {
        if let Some(category) = guest_denied_fast_path(text, cfg) {
            telemetry::emit("local", "threshold.fast_path_refused", json!({"category": category}));
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            return Ok(RouteOutcome {
                routed_to: "local",
                response: format!(
                    "I can't do that in guest mode — that would use the owner's {category}, \
                     which is off-limits to a guest. I can talk, translate, and give \
                     non-personal status. The owner can do it."
                ),
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // PER-ACTION POLICY VOICE COMMAND (consequential gate control): the user
    // spoken "always allow the <tool> action" / "never allow the <tool> action" /
    // "always ask before the <tool> action". CONSERVATIVELY anchored (only the
    // exact phrase shapes classify — a sentence that merely mentions a tool or
    // "allow" does NOT trigger; `policy::classify_policy_command` rejects every
    // near-miss). This is the SPOKEN twin of the HUD policy editor + the
    // authenticated-local `policy` command verb — the SAME USER-SET-ONLY write
    // path, never the model tool loop. It runs AFTER the owner voice-id all-scope
    // gate above (so an unrecognized bystander cannot set a policy) and BEFORE any
    // normal routing, so a policy utterance NEVER falls through to the model. On a
    // hit we apply the rule (which itself refuses if the layer is disabled or the
    // master ceiling would be exceeded at evaluate time) and SPEAK an honest ack.
    if let Some(ack) = crate::policy::handle_user_policy_text(text) {
        telemetry::emit("system", "policy.user_set", json!({"via": "voice"}));
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        return Ok(RouteOutcome {
            routed_to: "local",
            response: ack,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // MODEL-SWAP VOICE COMMAND (model-tier control): "use the powerful model" /
    // "go offline" / "fast mode" / "auto". CONSERVATIVELY detected (anchored on
    // imperative model-control phrasing — a sentence that merely mentions
    // "fast"/"offline" does NOT trigger) and handled BEFORE any normal routing,
    // like roll-call/agent-query. This is MODEL-ONLY: it installs/clears the
    // process-global tier override that resolve_tier later reads; it changes NO
    // safety gate (the consequential confirmation gate, the allow_consequential
    // master switch, the owner voice-id gate, and the per-agent allowlist are
    // untouched). It runs AFTER the voice-id all-scope gate above, so an
    // unrecognized bystander cannot re-aim the model. On a hit we set the
    // override, emit model.swap telemetry, and SPEAK a short honest ack, then
    // return so it never falls through to a normal answer.
    if let Some(intent) = crate::model_tier::classify_model_swap(text) {
        crate::model_tier::set_override(intent.to_override());
        telemetry::emit(
            "system",
            "model.swap",
            json!({
                "intent": intent.as_str(),
                // The override now in force after the swap: a tier string for a
                // manual pick, or null for Auto (override cleared -> config default).
                "override": crate::model_tier::current_override().map(|t| t.as_str()),
                "manual": intent != crate::model_tier::ModelSwapIntent::Auto,
            }),
        );
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        return Ok(RouteOutcome {
            routed_to: "local",
            response: intent.ack().to_string(),
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // WHISPER / DISCREET MODE VOICE COMMAND (#34): "whisper mode" / "speak quietly" /
    // "be discreet" engage; "back to normal" / "speak normally" / "out loud" disengage.
    // CONSERVATIVELY anchored (prosody::parse_whisper_command matches only a small
    // phrase set, OFF-phrases taking precedence so a "normal" utterance never reads as
    // "on") and handled BEFORE normal routing so a whisper toggle never falls through
    // to the model. This is DELIVERY-ONLY: it sets the process-global whisper state
    // that the speak path reads (mirroring the model-swap override above); it changes
    // NO safety gate — the consequential confirmation gate, the allow_consequential
    // master switch, the owner voice-id gate, lockdown and per-action policy are all
    // untouched, and a required confirmation is NEVER softened/silenced (apply_whisper
    // guards it). The [voice].whisper master switch (ON by default; delivery-only)
    // gates a stray command: apply_command_global honours it, so with the feature off the
    // global stays OFF and this whole arm is a no-op toggle. Runs AFTER the owner
    // voice-id all-scope gate, so an unrecognized bystander cannot flip it.
    if let Some(cmd) = crate::prosody::parse_whisper_command(text) {
        let now_on = crate::prosody::apply_command_global(cfg, cmd);
        telemetry::emit(
            "system",
            "voice.whisper_command",
            json!({
                // The command parsed, and the state now in force after honouring the
                // master switch (with [voice].whisper OFF this is always false).
                "command": match cmd {
                    crate::prosody::WhisperCommand::On => "on",
                    crate::prosody::WhisperCommand::Off => "off",
                },
                "whisper_on": now_on,
                "enabled": cfg.voice.whisper,
            }),
        );
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        // Honest ack: confirm the new delivery state, or — when the feature is off —
        // say so plainly rather than pretending to have engaged it.
        let ack = if !cfg.voice.whisper {
            "Discreet mode isn't enabled, sir.".to_string()
        } else if now_on {
            "Speaking discreetly, sir.".to_string()
        } else {
            "Back to my normal voice, sir.".to_string()
        };
        return Ok(RouteOutcome {
            routed_to: "local",
            response: ack,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // VAULT MODE VOICE COMMAND ("go dark", vault.rs): "go dark" / "vault mode on" /
    // "vault mode off" / "come back online". CONSERVATIVELY anchored
    // (vault::classify_vault_command matches only the imperative phrase set — an
    // ordinary sentence that merely mentions "vault" never triggers, OFF taking
    // precedence over ON) and handled BEFORE normal routing so a vault toggle never
    // falls through to the model. This flips the process-global vault mode that the
    // two cloud-decision seams above read; it changes NO safety gate (the
    // consequential confirmation gate, the owner voice-id gate, lockdown, and
    // per-action policy are all untouched) and is NOTHING CONSEQUENTIAL — it only
    // TIGHTENS (removes cloud access + forces CUSTOMS to the maximal trim), never
    // adds an outward action. Runs AFTER the owner voice-id all-scope gate, so an
    // unrecognized bystander cannot flip it. On a hit we set the mode, emit the
    // secret-free `vault.status` frame, and SPEAK an HONEST ack, then return.
    if let Some(cmd) = crate::vault::classify_vault_command(text) {
        let now_on = crate::vault::set(matches!(cmd, crate::vault::VaultCommand::On));
        telemetry::emit("system", "vault.status", crate::vault::status_frame(now_on));
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        return Ok(RouteOutcome {
            routed_to: "local",
            response: crate::vault::ack(now_on).to_string(),
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // MACRO RECORD/REPLAY VOICE COMMANDS (#27): "record a macro called X" /
    // "stop recording" / "list macros" / "forget macro X". CONSERVATIVELY anchored
    // (macros::classify_macro_command matches only explicit phrasings — an ordinary
    // sentence that merely mentions "macro" never triggers) and handled BEFORE normal
    // routing so a macro control utterance never falls through to the model. The
    // REPLAY command is handled one level up (in the turn loop), where it re-runs
    // each recorded command through the FULL classify->route->gate pipeline FRESH;
    // here we handle the non-replay control verbs. ON by default ([macros].enabled):
    // with it off these report the subsystem is off and record/persist nothing — no
    // store accrues. Recording captures only the UTTERANCE + intent name, redacted at
    // persist time (macros.rs), so a secret is never stored; and recording NEVER
    // changes a gate — a captured command still runs (and re-gates) normally.
    if let Some(cmd) = crate::macros::classify_macro_command(text) {
        // Replay is driven by the turn loop (it needs to re-classify+route each
        // step); ignore it here so it falls through to the loop's replay check.
        if !matches!(cmd, crate::macros::MacroCommand::Replay { .. }) {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            let response = handle_macro_command(cmd, cfg, memory).await;
            return Ok(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // RUNBOOK VOICE COMMANDS (runbook.rs): "plan the runbook X" (PURE, read-only —
    // render the typed DAG + which steps will PARK) / "run the runbook X" (execute —
    // re-issue EACH step FRESH through the live tool gate, ONE AT A TIME). CONSERVATIVELY
    // anchored (runbook::classify_runbook_command fires ONLY on the explicit "plan/run
    // the runbook <name>" shapes whose name normalizes to a SAFE, CONFINED file stem — an
    // ordinary sentence that merely mentions "runbook", or a path-shaped name, never
    // triggers) and handled BEFORE normal routing so a runbook utterance never falls
    // through to the model. SHIPS OFF ([runbook].enabled=false): with it off both verbs
    // report the subsystem is off and NOTHING plans or runs. SAFETY: `run` carries NO
    // authority — it mirrors the macro-replay dispatch, re-issuing each step through the
    // SAME anthropic::execute_tool + gate a live tool call takes, so a consequential step
    // PARKS FRESH for a spoken confirm (single slot, never batched, never pre-approved);
    // a parked step produces no value, so its `${ref}` consumer BLOCKS rather than run on
    // a fabricated one. Runs AFTER the owner voice-id all-scope gate, like the macro arm.
    if let Some(cmd) = crate::runbook::classify_runbook_command(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let response = handle_runbook_command(cmd, cfg, memory, prime, root).await;
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // ONE-WORD UNDO (F2): "undo that" / "revert that" / "what can you undo".
    // CONSERVATIVELY anchored (journal::classify_undo_command fires only on
    // explicit undo phrasings — a sentence merely mentioning "undo" never
    // triggers, and a QUESTION about undo answers instead of arming). Handled
    // AFTER the confirmation pre-check above, so while an action is PARKED an
    // "undo" reply is consumed there as a Deny (retract the un-executed action)
    // and never reaches here; this arm therefore only ever undoes EXECUTED
    // actions from the journal. SAFETY: arming an undo hands the derived
    // inverse to anthropic::execute_tool — the SAME entry point a live tool
    // call uses — so the inverse gets the identical voice-id check, faithful
    // dry-run preview, policy layer, master-switch ceiling, and single-slot
    // spoken-confirm park. Undo executes NOTHING itself and grants nothing a
    // spoken command would not. Runs after the owner voice-id all-scope gate,
    // like the macro arm above.
    if let Some(cmd) = crate::journal::classify_undo_command(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let response = handle_undo_command(cmd, memory).await;
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // APERTURE VOICE COMMANDS (aperture.rs): the on-device activity timeline —
    // "what did I do this morning" / "what was I working on around 3pm" (RECALL) and
    // "forget my activity timeline" (FORGET). Handled BEFORE the screen-context block
    // below and CONSERVATIVELY anchored: classify_aperture_intent only fires on a
    // recall cue that ALSO carries a resolvable TIME WINDOW ("this morning", "around
    // 3pm", "the last hour", "today") OR an explicit "activity"/"timeline" word. That
    // is exactly how it COEXISTS with the recent screen-context recall: a bare "what
    // was I working on" (no window, no timeline word) falls through here and is
    // handled by screen_context below. READ-ONLY: RECALL summarizes the BOUNDED,
    // PII-REDACTED timeline (app + window title + duration — NEVER screen pixels); an
    // off / un-fed timeline is an HONEST "no activity recorded", never fabricated.
    // FORGET wipes the in-RAM ring. SHIPS OFF ([aperture].enabled=false) — with it
    // off nothing was ever recorded. Runs after the owner voice-id all-scope gate, so
    // an unrecognized bystander cannot recall or wipe the owner's activity timeline.
    if let Some(intent) = crate::aperture::classify_aperture_intent(text, &chrono::Local::now()) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let (verb, response) = if !cfg.aperture.enabled {
            // OFF (the shipped default): nothing was ever recorded. Honest, never a
            // fabricated timeline and never a claim the feature is running.
            (
                "off",
                "The activity timeline is off, sir — I'm not recording what you work on. \
                 Enable [aperture] and I'll keep a private, on-device record of which app \
                 you're in and its window title (never your screen) so I can tell you what \
                 you were working on."
                    .to_string(),
            )
        } else {
            match intent {
                crate::aperture::ApertureIntent::Recall(query) => {
                    // Summarize the redacted timeline for the constructed query
                    // (window + optional subject). Honest-empty on an un-fed ring.
                    ("recall", crate::aperture::global_render_recall(&query, 6))
                }
                crate::aperture::ApertureIntent::Forget => {
                    let cleared = crate::aperture::global_clear();
                    let ack = if cleared {
                        "Done, sir — I've wiped your activity timeline.".to_string()
                    } else {
                        "There was no activity timeline to forget, sir.".to_string()
                    };
                    ("forget", ack)
                }
            }
        };
        // SECRET-FREE telemetry: the verb + the gate only — never the recalled
        // (already-redacted) app/title text.
        telemetry::emit(
            "system",
            "aperture.command",
            json!({ "verb": verb, "enabled": cfg.aperture.enabled }),
        );
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // CONTINUOUS SCREEN CONTEXT VOICE COMMANDS (#42): "what was I working on" /
    // "recall my screen context" (RECALL) and "forget my screen context" (FORGET).
    // CONSERVATIVELY anchored (screen_context::classify_screen_context_intent
    // requires the explicit "screen context" phrase or the narrow "what was I
    // working on" recall cue, so an ordinary sentence — and crucially the one-shot
    // OCR `read.screen` phrasings "read my screen" / "what's on my screen" — never
    // reaches here) and handled BEFORE normal routing so a screen-context utterance
    // never falls through to the model. READ-ONLY: RECALL renders the BOUNDED,
    // REDACTED recent context from the in-RAM ring (an empty/un-fed ring is an
    // HONEST "no recent screen context", never fabricated); FORGET wipes the ring.
    // The recalled text is kept TRANSIENT by the main.rs gate (is_screen_read unions
    // the recall here) so it never seeds lifelong memory / optimizer traces, exactly
    // like the one-shot screen read. The CONTINUOUS capture loop that FEEDS the ring
    // ships ON ([screen_context].enabled) but is INERT WITHOUT Screen-Recording TCC
    // consent — these voice commands only READ/CLEAR the ring, they never start a capture. Runs after the
    // owner voice-id all-scope gate, so an unrecognized bystander cannot recall the
    // owner's screen context or wipe it.
    if let Some(intent) = crate::screen_context::classify_screen_context_intent(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let (verb, response) = match intent {
            crate::screen_context::ScreenContextIntent::Recall { subject } => {
                // Bounded redacted recall — read-only; honest-empty on an un-fed
                // ring. A named subject narrows to matching entries (never invents
                // a match); a bare recall renders the recent context.
                let rendered = match subject {
                    Some(s) => crate::screen_context::global_render_recall_matching(&s, 10),
                    None => crate::screen_context::global_render_recall(10),
                };
                ("recall", rendered)
            }
            crate::screen_context::ScreenContextIntent::Forget => {
                let cleared = crate::screen_context::global_clear();
                let ack = if cleared {
                    "Done, sir — I've wiped your recent screen context.".to_string()
                } else {
                    "There was no screen context to forget, sir.".to_string()
                };
                ("forget", ack)
            }
        };
        // SECRET-FREE telemetry: the verb only — never the recalled redacted text
        // (which is transient + already redacted, but is not echoed to telemetry).
        telemetry::emit(
            "system",
            "screen_context.command",
            json!({ "verb": verb, "enabled": cfg.screen_context.enabled }),
        );
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // SEMANTIC PASTEBOARD VOICE COMMANDS (pasteboard.rs): "what did I copy about
    // the lease" / "recall my clipboard" (RECALL) and "forget my clipboard"
    // (FORGET). CONSERVATIVELY anchored (classify_pasteboard_intent requires an
    // explicit clipboard/"copied" reference plus a recall/forget cue, so an
    // ordinary sentence — and crucially an imperative "copy X to my clipboard"
    // (which is the confirm-gated pasteboard_put tool, NOT a recall) — never reaches
    // here). Handled BEFORE normal routing so a pasteboard utterance never falls
    // through to the model. READ-ONLY: RECALL ranks the BOUNDED, PII-REDACTED clip
    // ring by MEANING via the recall.rs path (an off / empty ring is an HONEST
    // "nothing copied yet", never fabricated); FORGET wipes the ring. SHIPS OFF
    // ([pasteboard].enabled=false) — with it off nothing was ever captured, so
    // recall/forget honestly report an empty history. Runs after the owner voice-id
    // all-scope gate, so an unrecognized bystander cannot recall or wipe the owner's
    // clipboard history.
    if let Some(intent) = crate::pasteboard::classify_pasteboard_intent(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let (verb, response) = if !cfg.pasteboard.enabled {
            // OFF (the shipped default): nothing was ever captured. Honest, not a
            // fabricated recall — and never a claim the feature is running.
            (
                "off",
                "The semantic pasteboard is off, sir — I'm not capturing your clipboard. \
                 Enable [pasteboard] and I'll start remembering what you copy, on-device."
                    .to_string(),
            )
        } else {
            match intent {
                crate::pasteboard::PasteboardIntent::Recall { subject } => {
                    // Rank the redacted clip ring by meaning; a named subject
                    // narrows the query, a bare recall ranks against the whole
                    // utterance. Honest-empty on an off / un-fed ring.
                    let query = subject.as_deref().unwrap_or(text);
                    ("recall", crate::pasteboard::global_render_recall(query, 10))
                }
                crate::pasteboard::PasteboardIntent::Forget => {
                    let cleared = crate::pasteboard::global_clear();
                    let ack = if cleared {
                        "Done, sir — I've wiped your clipboard history.".to_string()
                    } else {
                        "There was no clipboard history to forget, sir.".to_string()
                    };
                    ("forget", ack)
                }
            }
        };
        // SECRET-FREE telemetry: the verb + the gate only — never the recalled
        // (already-redacted) clip text.
        telemetry::emit(
            "system",
            "pasteboard.command",
            json!({ "verb": verb, "enabled": cfg.pasteboard.enabled }),
        );
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // RESEARCH NOTEBOOK VOICE COMMAND (#19): "save this research" / "show my
    // research notebook on X" / "what have I researched" / "forget my research on
    // X". CONSERVATIVELY anchored (classify_notebook_intent requires an explicit
    // notebook/"my research" cue, so an ordinary "research the competitors" still
    // routes to SAGE's live run, never here). Handled BEFORE normal routing so a
    // notebook utterance never falls through to the model. READ/PROPOSE-ONLY: it
    // persists a run that ALREADY happened (the real last SAGE run, with its real
    // grounded citations — never a fabricated source) and reads runs that were
    // really saved; it speaks, but acts/reaches nothing outward. AGENT-SCOPED: the
    // notebook store is scoped to the active agent's namespace (own + shared
    // orchestrator). On a bare save with no recent run it honestly says so and
    // saves NOTHING. Voiced by the orchestrator (the conversational tier that owns
    // the user's saved research). Runs after the owner voice-id all-scope gate, so
    // an unrecognized bystander cannot touch the notebooks.
    if let Some(intent) = crate::notebook::classify_notebook_intent(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let outcome = crate::notebook::dispatch(memory, &prime.namespace, intent)
            .await
            .unwrap_or_else(|e| crate::notebook::NotebookOutcome {
                reply: format!("I couldn't reach your research notebooks just now, sir — {e}."),
                verb: "error",
                card: None,
            });
        // Enriched, SECRET-FREE telemetry: the verb plus the rendered CARD the HUD
        // renders — the topic, a bounded snippet of the already-redacted synthesis,
        // and the run's REAL fetched-source citations (id + title + url), exactly the
        // persisted/grounded ones (never a fabricated source, never raw content).
        // When there's no content to surface (save_none/forget_none/error) the card
        // is absent and only the verb rides.
        let card_json = outcome.card.as_ref().map(|c| {
            json!({
                "verb": c.verb,
                "topic": c.topic,
                "snippet": c.snippet,
                "run_count": c.run_count,
                "citations": c
                    .citations
                    .iter()
                    .map(|cit| json!({
                        "source_id": cit.source_id,
                        "title": cit.title,
                        "url": cit.url,
                    }))
                    .collect::<Vec<_>>(),
            })
        });
        telemetry::emit(
            "system",
            "notebook.card",
            json!({"verb": outcome.verb, "card": card_json}),
        );
        return Ok(RouteOutcome {
            routed_to: "local",
            response: outcome.reply,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // PRECOG // WHAT-IF (simulate.rs): "what would you do if I said X". CONSERVATIVELY
    // anchored (simulate::extract_hypothetical fires ONLY on the high-precision
    // "what would you do if I said/asked/told you to X" framing and requires a
    // non-empty tail — a bare "simulate ..." is CASSANDRA's forecast vocabulary and
    // is NOT claimed here). GATED by [precog].enabled (ships ON; read-only) — when
    // off this falls through to ordinary routing (the query is just another
    // question). READ-ONLY by CONSTRUCTION: the classify below is a read-only label
    // of the HYPOTHETICAL (it fires nothing), and simulate() runs the SAME pipeline
    // the live turn would — classify -> selector -> agent -> tier -> gate projection
    // -> reversibility — UP TO but NEVER THROUGH the confirmation gate. The simulate
    // path holds NO actuator / memory-write / inference handle (SimContext carries
    // only read views), so it cannot fire an action even a benign one. It emits the
    // PlannedOutcome as a `precog.plan` frame + speaks a summary that honestly
    // reports a real run WOULD park (PRECOG never satisfies a gate itself). Runs
    // after the owner voice-id all-scope gate, like the other command cues.
    if cfg.precog.enabled {
        if let Some(hypothetical) = crate::simulate::extract_hypothetical(text) {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            // Classify the HYPOTHETICAL (read-only — it labels text, it fires
            // nothing); on any classifier error fall back to the safe "unknown"
            // view (a low-confidence plain conversation), exactly the live degrade.
            let predicted = match infer.classify(&hypothetical).await {
                Ok(c) => crate::simulate::PredictedIntent {
                    intent: c.intent,
                    confidence: c.confidence,
                    complexity: c.complexity,
                },
                Err(e) => {
                    warn!("precog: classify of the hypothetical failed ({e}); using safe default");
                    crate::simulate::PredictedIntent::unknown()
                }
            };
            // The read-only context: shared roster + read-only config + the SAME
            // pure lexical scorer the live routing uses + the current tier override
            // + this turn's cloud reachability. No actuator / memory / brain handle.
            let ctx = crate::simulate::SimContext {
                agents,
                cfg,
                scorer: &crate::agents::LexicalAgentScorer,
                override_tier: crate::model_tier::current_override(),
                cloud_reachable,
            };
            let plan = crate::simulate::simulate(&hypothetical, &predicted, &ctx);
            // SECRET-FREE telemetry: only the pipeline decisions + the (already
            // user-spoken) hypothetical ride the wire — nothing ran, so there is no
            // fact/memory/tool-output to leak. The frame PINS executed=false /
            // satisfied_a_gate=false so the HUD copy is grounded in the contract.
            telemetry::emit("local", "precog.plan", plan.telemetry(&hypothetical));
            let response = plan.spoken_summary(&hypothetical);
            return Ok(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // REPORT GENERATION VOICE COMMAND (#40): "generate a report on X" / "write me a
    // report about X". CONSERVATIVELY anchored (classify_report_intent requires
    // "report" + an explicit build verb + a topic, so a question about an existing
    // report and an ordinary "research X" never trip it). GATED by [report].enabled
    // (ships ON; read-only, safe to enable) — when off the op declines honestly and
    // reads nothing. READ-ONLY: it pulls the agent-scoped, already-cited
    // notebook runs on the topic and folds them into a BOUNDED markdown report under
    // research.rs's cite discipline (every citation a REAL source ref an input claim
    // carried; an uncited run contributes nothing; no citable source -> an
    // honest-empty report) — it never fetches, never calls a model, never persists.
    // Voiced by the orchestrator (the tier that owns the user's saved research).
    // Runs after the owner voice-id all-scope gate, so an unrecognized bystander
    // cannot read the notebooks. Only entered when the flag is on, so it adds no
    // surface by default.
    if cfg.report.enabled {
        if let Some(intent) = crate::report::classify_report_intent(text) {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            let report_cfg = crate::report::ReportConfig { enabled: cfg.report.enabled };
            let outcome = crate::report::dispatch(memory, &prime.namespace, intent, &report_cfg)
                .await
                .unwrap_or_else(|e| crate::report::ReportOutcome {
                    markdown: format!("I couldn't assemble that report just now, sir — {e}."),
                    verb: "error",
                    report: None,
                });
            // Structured telemetry: the verb plus the report's title, section
            // headings, the count of REAL citations, and the honest-empty flag — all
            // derived from the already-cited material (never a fabricated source).
            let report_json = outcome.report.as_ref().map(|r| {
                json!({
                    "title": r.title,
                    "empty": r.empty,
                    "section_count": r.sections.len(),
                    "headings": r.sections.iter().map(|s| s.heading.clone()).collect::<Vec<_>>(),
                    "citation_count": r.all_citations.len(),
                    "citations": r
                        .all_citations
                        .iter()
                        .map(|c| json!({"id": c.id, "title": c.title, "url": c.url}))
                        .collect::<Vec<_>>(),
                })
            });
            telemetry::emit(
                "system",
                "report.built",
                json!({"verb": outcome.verb, "report": report_json}),
            );
            // ARTIFACT REGISTRY: register a REAL (non-empty) built report so the
            // peek surface can surface it. Provenance is HONEST — the real producing
            // agent (prime) + the report's REAL citations (each a source ref an input
            // claim carried, never fabricated); an empty report is not registered
            // (nothing was produced). The registry is in-memory + on-device; this
            // opens no surface.
            if let Some(r) = outcome.report.as_ref() {
                if !r.empty {
                    let citations = r
                        .all_citations
                        .iter()
                        .filter_map(|c| crate::artifact::Citation::new(c.title.clone(), c.url.clone()))
                        .collect::<Vec<_>>();
                    crate::artifact::register(
                        crate::artifact::ArtifactKind::Report,
                        r.title.clone(),
                        prime.name.clone(),
                        citations,
                        format!(
                            "{} section{}, {} citation{}",
                            r.sections.len(),
                            if r.sections.len() == 1 { "" } else { "s" },
                            r.all_citations.len(),
                            if r.all_citations.len() == 1 { "" } else { "s" },
                        ),
                    );
                }
            }
            return Ok(RouteOutcome {
                routed_to: "local",
                response: outcome.markdown,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // CHART VOICE COMMAND (#41): "chart this" / "plot the system load" / "graph the
    // cpu". CONSERVATIVELY anchored (classify_chart_intent requires a chart/plot/
    // graph verb + a chartable subject, so an ordinary "what's the cpu" never trips
    // it). GATED by [chart].enabled (ships ON — a neutral presentation act, safe to
    // enable outright) — when off the op declines and emits nothing. NEUTRAL presentation: it
    // serializes a ChartSpec from the latest REAL system snapshot the telemetry bus
    // already publishes (the EXACT cpu/mem values — no interpolation, no invented
    // point; no snapshot -> an honest-empty chart) and fire-and-forget emits it as a
    // `chart.data` envelope the HUD plots exactly. It changes no gate, takes no
    // action, reaches no network. Only entered when the flag is on (it ships on),
    // and emitting is a pure presentation act with no safety surface.
    if cfg.chart.enabled {
        if let Some(_intent) = crate::chart::classify_chart_intent(text) {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            // The data path: the latest REAL system snapshot -> a ChartSpec of the
            // exact metrics (honest-empty when no reading is available yet).
            let spec = crate::chart::chart_from_snapshot(telemetry::latest_snapshot());
            crate::chart::emit_chart(&spec);
            // ARTIFACT REGISTRY: register a REAL (non-empty) chart. A chart of live
            // system metrics genuinely cites nothing, so it is registered UNCITED —
            // honest, never dressed up with a fabricated source. In-memory +
            // on-device; opens no surface.
            if !spec.is_empty() {
                let points: usize = spec.series.iter().map(|s| s.points.len()).sum();
                crate::artifact::register(
                    crate::artifact::ArtifactKind::Chart,
                    spec.title.clone(),
                    prime.name.clone(),
                    Vec::new(), // live system metrics carry no citation -> UNCITED
                    format!(
                        "{} series, {} point{}",
                        spec.series.len(),
                        points,
                        if points == 1 { "" } else { "s" },
                    ),
                );
            }
            let response = if spec.is_empty() {
                "I don't have a system reading to chart yet, sir — give me a moment and ask again."
                    .to_string()
            } else {
                "Charting the current system load for you, sir — it's on the HUD.".to_string()
            };
            return Ok(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // ARTIFACT PEEK VOICE COMMAND (artifact.rs): "what did you just do" / "peek".
    // CONSERVATIVELY anchored (classify_peek_intent requires an explicit peek cue or
    // a "what did you just <produce>" recall phrase, so an ordinary "what did you
    // say" never trips it). GATED by [artifact].enabled (ships ON, armed-by-default)
    // — when off this arm is skipped and the utterance routes normally. READ-ONLY:
    // it reads the MOST RECENT artifact the producers registered back out of the
    // in-memory, on-device registry and fire-and-forget emits it as an
    // `artifact.peek` frame the HUD's QuickLook overlay renders — with HONEST
    // provenance (the real producing agent + real citations, or UNCITED). It changes
    // no gate, takes no action, reaches no network. An empty registry is answered
    // honestly ("nothing to peek yet"), never a fabricated artifact.
    if cfg.artifact.enabled && crate::artifact::classify_peek_intent(text) {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let response = match crate::artifact::peek_and_emit(None) {
            Some(artifact) => artifact.summary(),
            None => crate::artifact::empty_reply(),
        };
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // COMPOSE-MUSIC VOICE COMMAND (Phase-2 flagship "DARWIN, compose an 8-bit happy
    // birthday"). CONSERVATIVELY anchored (classify_music_intent requires an explicit
    // music-CREATION verb + a musical anchor, so "play some jazz" and "what's the
    // time" never trip it). MIRRORS the chart arm's shape: gated, handled BEFORE
    // normal model routing so a creation utterance never falls through to the model.
    // GATED by [voice].cloud_music (the music-generation tier switch) — when OFF this
    // arm is skipped entirely and the utterance routes normally (we never claim to
    // compose with the tier off). When ON but there's NO ElevenLabs key (or we're
    // offline), the spawned trigger_compose_music honestly NO-OPS — nothing is
    // fabricated and no track plays; the "composing now" ack is then mildly optimistic
    // but never a lie about a produced track. The composition runs FIRE-AND-FORGET on
    // a Send-safe per-call client (compose_music_for_command's dedicated thread +
    // current-thread runtime), and Part-1 plays the finished WAV on the SEPARATE music
    // sink — so this route returns the Jerome-voiced ack IMMEDIATELY without blocking
    // on the 30 s–10 min generation. The el_key is read ONLY inside the trigger.
    if cfg.voice.cloud_music {
        if let Some(prompt) = classify_music_intent(text) {
            // JEROME — "Leisure + DJ": the agent that owns music/entertainment.
            // Fall back to the orchestrator if the roster lacks it, but WARN so a
            // missing specialist is visible (a silent fallback would route music
            // to the wrong namespace/voice without any signal to the operator).
            let jerome = match agents.get("jerome") {
                Some(a) => a,
                None => {
                    warn!("router: agents.toml has no 'jerome' (music specialist); routing music via the orchestrator");
                    agents.orchestrator()
                }
            };
            emit_agent_active(jerome);
            telemetry::emit("system", "music.intent", json!({}));
            // Fire-and-forget the (genuinely non-Send) generation on its own thread,
            // reusing the command channel's Send-safe wrapper. Part 1 plays the track
            // when it finishes; failures stay inside the trigger (honest no-op).
            let cfg_owned = cfg.clone();
            let root_owned = root.to_path_buf();
            let sock = infer.socket_path().to_path_buf();
            tokio::spawn(async move {
                let _ = crate::compose_music_for_command(
                    cfg_owned,
                    prompt,
                    None,
                    root_owned,
                    sock,
                )
                .await;
            });
            return Ok(RouteOutcome {
                routed_to: "local",
                response: "Composing your track now, sir — I'll have it ready in a moment."
                    .to_string(),
                agent: jerome.name.clone(),
                namespace: jerome.namespace.clone(),
                spoken: None,
            });
        }
    }

    // LIFE-LOG DIGEST VOICE COMMAND (#20): "what did I do this week" / "show my
    // life log" / "what did I do today". CONSERVATIVELY anchored
    // (classify_lifelog_intent requires an explicit own-activity cue, so an
    // ordinary "what's the weather today" never trips it). Handled BEFORE normal
    // routing so a life-log utterance never falls through to the model. READ-ONLY:
    // it SUMMARIZES real recorded episodes — an empty/sparse window renders an
    // honest empty/sparse digest, never a fabricated event. AGENT-SCOPED: the
    // digest is built over the active agent's recall scope (own + shared
    // orchestrator), so it can never show another agent's episodes. Voiced by the
    // orchestrator (the user's main interaction tier). Runs after the owner
    // voice-id all-scope gate, so an unrecognized bystander cannot read the log.
    if let Some(intent) = crate::lifelog::classify_lifelog_intent(text) {
        // [lifelog].enabled — the MASTER SWITCH, honoured here.
        //
        // It was declared in KNOWN_KEYS, defaulted, typo-validated, autocompleted by
        // the DLS and documented as "with it false the digest intent returns an honest
        // 'the life log is off'" — and nothing outside config.rs ever read it. An
        // operator who set it false got no warning (the parser accepts it happily) and
        // a full digest anyway: the one posture where the switch matters was the one it
        // did not have.
        if !cfg.lifelog.enabled {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            return Ok(RouteOutcome {
                routed_to: "local",
                response: "The life log is off, sir — [lifelog].enabled is false in \
                           config, so I won't build a digest."
                    .to_string(),
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        let crate::lifelog::LifeLogIntent::Digest(period) = intent;
        // The spoken reply comes from the unchanged dispatch; the enriched card is
        // built from the SAME agent-scoped, bounded digest read so the HUD can render
        // content. No logic change to lifelog.rs — this reuses its public surface.
        let reply = crate::lifelog::dispatch(memory, &prime.namespace, intent).await;
        let digest = crate::lifelog::build_digest(memory, &prime.namespace, period).await;
        let card = crate::lifelog::build_card(&digest);
        // Enriched, SECRET-FREE telemetry: the period plus the digest's
        // already-redacted content — the rendered digest text, the REAL episode
        // count, and the bounded themes / topics / recent summaries. Every field is
        // the episodic store's already-redacted output (a secret was stripped before
        // write), never raw, never fabricated; an empty window rides empty:true.
        telemetry::emit(
            "system",
            "lifelog.digest",
            json!({
                "period": card.period,
                "empty": card.empty,
                "episode_count": card.episode_count,
                "digest_text": card.digest_text,
                "themes": card.themes,
                "topics": card.topics,
                "recent_summaries": card.recent_summaries,
            }),
        );
        return Ok(RouteOutcome {
            routed_to: "local",
            response: reply,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // SESSION REWIND (F12): "what happened at 2pm" / "rewind the last hour" /
    // "walk me through this morning". REVIEW-ONLY time travel: reconstructs a
    // bounded timeline of the window from the RECORDED stores — episodes (the
    // redacted, gated turn record; deliberately NOT raw transcripts, which keep
    // what the episodic privacy gate excludes) and the audit log's redacted
    // consequential-action entries — narrates a digest, and emits the timeline
    // for the HUD step-through. It NEVER re-executes anything (that is macro
    // replay's job, and it re-gates). Runs AFTER the lifelog arm so lifelog
    // keeps its own-activity phrasing ("what did I do", "my day"); the rewind
    // classifier requires an explicit gate + time qualifier and never matches
    // macro-replay verbs. Reads stay fail-open (an empty window is never an
    // error) — but a FAILED read is DISCLOSED, never narrated as a clean
    // "nothing happened" (that would fabricate absence).
    if let Some(window) =
        crate::rewind::classify_rewind_intent(text, chrono::Local::now().fixed_offset())
    {
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        // Both reads are WINDOW-SCOPED (a depth-only read would silently miss
        // an old window's rows and narrate a false absence) and share one cap;
        // a saturated read flips counts_floor so the counts are disclosed as
        // "at least N", never presented as exact.
        const REWIND_READ_CAP: usize = 200;
        let mut reads_failed = false;
        // Episodes over the shared/orchestrator scope (the lifelog precedent).
        let episodes = match memory
            .episodes_around(&prime.namespace, &window.from_utc, &window.to_utc, REWIND_READ_CAP)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "rewind: episode read failed; disclosing a partial record");
                reads_failed = true;
                Vec::new()
            }
        };
        // Audit entries via the windowed read — both sides UTC RFC3339, so the
        // lexical compare is exact. A MISSING log (audit off) is honestly "no
        // actions"; a FAILED read on a present log is disclosed.
        let actions: Vec<crate::audit::AuditEntry> = match crate::audit::global() {
            Some((_enabled, log)) => {
                match log.between(&window.from_utc, &window.to_utc, REWIND_READ_CAP).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = %e, "rewind: audit read failed; disclosing a partial record");
                        reads_failed = true;
                        Vec::new()
                    }
                }
            }
            None => Vec::new(),
        };
        let counts_floor =
            episodes.len() >= REWIND_READ_CAP || actions.len() >= REWIND_READ_CAP;
        let rewind = crate::rewind::build_timeline(&window, &episodes, &actions, counts_floor);
        let mut payload = crate::rewind::payload(&rewind);
        if reads_failed {
            payload["reads_failed"] = serde_json::json!(true);
        }
        telemetry::emit("system", "session.rewind", payload);
        let mut response = crate::rewind::render_spoken(&rewind);
        if reads_failed {
            response.push_str(
                " One caveat, sir — part of the record was unreadable just now, so this view may be incomplete.",
            );
        }
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // CAUSA (causal decision-trace explainer, explain.rs): "why did you do that" /
    // "why <Agent>" narrates the ordered, REDACTED decision trace of the relevant
    // recent turn (recorded at the END of run_pipeline). GATED by [explain].enabled
    // (ships ON); when off, the question simply falls through to the model. Placed
    // right after rewind so the review-family verbs stay together and after the
    // higher-priority control arms (panic/unlock/confirm/undo/…). REVIEW-ONLY: it
    // re-executes nothing — it explains what ALREADY happened, from records the
    // daemon already holds, and returns an HONEST EMPTY (never a fabricated
    // rationale) when there is no trace for the ask.
    if cfg.explain.enabled {
        if let Some(query) = crate::explain::classify_explain_intent(text) {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            let trace = crate::explain::lookup(&query);
            telemetry::emit("system", "causa.trace", crate::explain::payload(&query, trace.as_ref()));
            let response = crate::explain::render_spoken(&query, trace.as_ref());
            return Ok(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // MIRROR (belief-audit + contest over the SELF-MODEL, user_model.rs): "why do you
    // think I prefer X" surfaces the STORED observation, provenance, and observed-count
    // for that belief (never a fabricated reason); "that's wrong about X" DROPS the
    // belief AND writes a suppression tombstone so the consolidation pass never
    // re-derives it. GATED by [mirror].enabled (ships ON, read-only/reduce-only surface).
    // Placed right after CAUSA so the two "explain" families stay together — MIRROR's
    // cues are SELF-MODEL-specific ("why do you THINK I…"), distinct from CAUSA's
    // turn-decision asks, so it never steals a "why did you do that". REDUCE-ONLY:
    // explain reads the shared tier; contest only ever removes/suppresses a shared
    // `user.model.*` belief and is structurally unable to touch a private agent.* note.
    // Emits the secret-free `mirror.belief` telemetry frame.
    if cfg.mirror.enabled {
        if let Some(intent) = crate::user_model::classify_mirror_intent(text) {
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            let response = match intent {
                crate::user_model::MirrorIntent::Explain(subject) => {
                    let explanation = crate::user_model::explain_belief(memory, &subject)
                        .await
                        .unwrap_or(crate::user_model::Explanation {
                            asked: subject.clone(),
                            entries: Vec::new(),
                        });
                    crate::user_model::emit_belief_frame(
                        memory,
                        "explain",
                        &subject,
                        explanation.found(),
                    )
                    .await;
                    explanation.text()
                }
                crate::user_model::MirrorIntent::Contest(subject) => {
                    let contest = crate::user_model::contest_belief(memory, &subject)
                        .await
                        .unwrap_or_default();
                    crate::user_model::emit_belief_frame(
                        memory,
                        "contest",
                        &subject,
                        contest.any(),
                    )
                    .await;
                    contest.text(&subject)
                }
                crate::user_model::MirrorIntent::Clear(subject) => {
                    // The tombstone is user-clearable: lifting a prior contest lets
                    // the consolidation pass learn the belief afresh.
                    let cleared = crate::user_model::clear_suppression(memory, &subject)
                        .await
                        .unwrap_or(0);
                    crate::user_model::emit_belief_frame(
                        memory,
                        "clear",
                        &subject,
                        cleared > 0,
                    )
                    .await;
                    if cleared > 0 {
                        "Done, sir — I have lifted that suppression; I may learn it again \
                         if I keep observing it.".to_string()
                    } else {
                        format!(
                            "There was no suppression on \"{}\" to lift, sir.",
                            subject.trim()
                        )
                    }
                }
            };
            return Ok(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
    }

    // Roll-call (item 3, the reel centerpiece): "introduce the team" / "roll
    // call" / "assemble" -> each agent speaks its one-line self-introduction in
    // ITS OWN voice, in order, emitting agent.active per agent so the HUD
    // highlights them in turn and the core color cycles. Checked before any
    // routing so it never lands on the classifier/cloud. Interruptible.
    if crate::agents::is_roll_call(text) {
        let (response, report) = roll_call(agents, infer, reply, started, root, cfg).await;
        return Ok(RouteOutcome {
            routed_to: "local",
            response,
            agent: agents.orchestrator().name.clone(),
            namespace: agents.orchestrator().namespace.clone(),
            spoken: Some(SpokenReply {
                route_ms: route_entry.elapsed().as_millis() as u64,
                report,
            }),
        });
    }

    // Agent-ROSTER query ("list my agents" / "who are my agents" / "what's the
    // constellation"): answered DETERMINISTICALLY from the live registry, BEFORE
    // acting on the classification. The classifier has been observed to misroute
    // these to the local model, where — with no roster in its context — it
    // HALLUCINATES agents that do not exist and leaks unrelated facts. Here the
    // answer always comes from the real registry: cloud-reachable, DARWIN phrases
    // the true roster in persona (grounded — persona.txt forbids inventing agents
    // not in it); offline / on a cloud error, a plain spoken list (still the real
    // team). The constellation is named accurately or not at all, never invented.
    if crate::agents::is_agent_query(text) {
        let (response, routed_to) =
            answer_agent_roster(text, agents, memory, cfg, cloud_reachable).await;
        let prime = agents.orchestrator();
        emit_agent_active(prime);
        return Ok(RouteOutcome {
            routed_to,
            response,
            agent: prime.name.clone(),
            namespace: prime.namespace.clone(),
            spoken: None,
        });
    }

    // CAPABILITY SELECTOR (the "extremely smart" glue): from the natural request,
    // decide WHICH CAPABILITY to engage BEFORE agent selection — so the user never
    // manages modes. This is a HIGHER-LEVEL dispatch than agent routing (which
    // picks WHICH AGENT); it picks WHICH MODE: a plain one-shot answer, a read of
    // the shared World Model, a fold of a stated fact INTO the World Model, a
    // complex multi-step mission NOW (FURY), or the SETUP of a recurring standing
    // mission. Deterministic cues run first; a pure semantic fallback (the same
    // LexicalAgentScorer the smart agent routing uses) only PROMOTES out of the
    // safe one-shot default on a strong, unambiguous signal.
    //
    // THE TWO RAILS are enforced inside classify_mode + here:
    //   * Rail 1 (clarify / safe-default, never guess into autonomy): a mere
    //     semantic lean toward a standing mission NEVER silently establishes it —
    //     it returns a one-line CLARIFY ("every day, or just once?") which we speak
    //     and stop. A low-confidence / ambiguous turn falls back to one_shot. Only
    //     a HARD recurring cue routes straight to the (still gated) standing setup.
    //   * Rail 2 (no silent autonomy): the standing mode only PROPOSES — it routes
    //     to standing_create, which PARKS behind the cross-turn confirmation gate
    //     (and the armed-by-default master switch, which still requires a fresh
    //     per-action confirm). world_update writes ONLY the
    //     shared user.world.* tier, never a consequential external action.
    //
    // one_shot falls THROUGH to the existing pipeline unchanged (so current fast
    // cue routing + all routing tests are untouched). The user can always be
    // explicit and override the selector with a plain phrasing.
    match crate::selector::classify_mode(text, &crate::agents::LexicalAgentScorer) {
        crate::selector::Selection::Route(crate::selector::Mode::OneShot) => {
            // Default: the normal pipeline below handles it (unchanged).
        }
        crate::selector::Selection::Clarify(question) => {
            // RAIL 1: genuinely ambiguous between a safe one-shot and arming
            // autonomy — ask, never guess. Voiced by the orchestrator; nothing is
            // established, queried, or fired. The next turn's explicit answer
            // routes deterministically (hard cue or plain one-shot).
            telemetry::emit("local", "selector.clarify", json!({"question": question}));
            let prime = agents.orchestrator();
            emit_agent_active(prime);
            return Ok(RouteOutcome {
                routed_to: "local",
                response: question,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            });
        }
        crate::selector::Selection::Route(mode) => {
            telemetry::emit("local", "selector.mode", json!({"mode": mode.as_str()}));
            // THRESHOLD — GUEST MODE: the capability modes (World read/fold, a NOW
            // multi-step mission, a standing-mission setup) either READ the owner's
            // shared World Model or take a CONSEQUENTIAL/autonomous action. A guest
            // reaches none of them — skip the capability dispatch and fall through to
            // the (guest-gated) conversational path, which safely answers without any
            // owner data or tool. Owner path: byte-for-byte today's.
            if !crate::threshold::is_guest_turn() {
                if let Some(outcome) =
                    route_capability(mode, text, memory, agents, cloud_reachable).await
                {
                    return Ok(outcome);
                }
            }
            // A capability that declined (e.g. nothing to read) degrades to the
            // normal pipeline below rather than going silent.
        }
    }

    // A HOIST OF THE FOUR NON-CAPTURE APP GATES WAS MEASURED HERE AND REFUSED.
    //
    // The four gates that forward a structured op or render on-device (silicon,
    // nexus, markforge, genimage) sit — like the other four — BELOW the two
    // cloud early-returns below, so on the shipped
    // `[router].conversation_route = "cloud_heavy"` an utterance the on-device
    // classifier labels "conversation" never reaches them and the SAME utterance
    // offline actuates. `recall_probe::CLOUD_PREEMPTED` measures that as 28.2% of
    // the recall fixture, and giving those four FIRST REFUSAL instead would make
    // 13.9% of it reachable.
    //
    // IT WAS NOT TAKEN, AND THE REASON IS A MEASUREMENT, NOT A PREFERENCE.
    // Suppressing the two returns for the turns those gates claim gives them
    // first refusal on everything that currently reaches cloud conversation —
    // which is only safe if all four classifiers are precise enough to be FIRST.
    // Four of them are not. Sentences constructed against the branches' own
    // trigger vocabulary, each ordinary English, each of which the gates claim
    // TODAY (they are live on the offline / vault / guest path, and a hoist would
    // make them live on the shipped path too):
    //
    //   markforge LAUNCH   `mentions_mark_forge` still admits the bare nouns "the
    //                      simulation" / "the sandbox", so with one of four
    //                      ordinary verbs it opens the engine —
    //                      "they start the simulation training for new nurses
    //                      next month", "show me the sandbox where the kids play
    //                      at the park", "I need to open the sandbox account
    //                      before the demo tomorrow". `names_mark_forge` (added
    //                      for the reset branch) is the fix this branch needs.
    //   markforge STEP     `physics_ctx` accepts the bare whole words "world" /
    //                      "physics" / "frames", so any sentence carrying one plus
    //                      step/advance/pause/freeze/hold/halt advances or freezes
    //                      the world — "she took a step into a whole new world
    //                      after graduation", "hold on, the world is not ending
    //                      today", "pause the video during the physics lecture".
    //   markforge GRAVITY  the ordinary English idiom "the gravity of <X>" plus
    //                      any of ten set-ish verbs plus an ordinary target word
    //                      writes the world's gravity vector — "put the gravity of
    //                      the situation aside, nothing about this is normal",
    //                      "we should change how gravity is taught in earth
    //                      science", "turn to the gravity chapter and read about
    //                      the moon".
    //   markforge SPAWN    `spawn_context` also takes `mentions_mark_forge`, so a
    //                      child's sandbox spawns a rigid body — "throw the ball
    //                      into the sandbox at the playground".
    //   silicon   LAUNCH   `mentions_silicon_canvas` admits the bare "the
    //                      schematic" and this branch's verbs are still
    //                      `contains` — "show the schematic to the electrician
    //                      when he arrives", "the schematic showed a startling
    //                      amount of detail".
    //   genimage           the remaining hole is grammatical PERSON, not
    //                      vocabulary: a request to DARWIN is an IMPERATIVE, and
    //                      present-tense narration reuses the base form in the
    //                      same verb-object shape — "we draw a picture of the
    //                      family every christmas", "you cannot paint a picture
    //                      with only one color", "we make art with the kids on
    //                      saturdays". Verb POSITION (utterance-initial modulo a
    //                      bounded politeness prefix) is the missing rule; verb
    //                      FORM alone does not separate them.
    //
    // So the ORDER of operations is: harden those six branches, re-prove each
    // against sentences written for the NEW rule, and only then hoist. Hoisting
    // first buys 13.9% reachability by re-opening the defect class this campaign
    // spent the most effort closing — 317 of 1,897 ordinary utterances captured by
    // app gates, a tornado-watch question that turned the camera on. The gain is
    // real and it is not worth that.
    //
    // The OTHER four (describe / sound / lumen / vision) are a separate question
    // and a harder one: each actuates a camera, a screen read, a mic clip or a UI
    // actuation, so hoisting them changes WHEN capture can fire. That is a consent
    // decision for the owner, named here and not taken.

    let needs_deep_reasoning = class.complexity == "heavy";
    // VAULT MODE ("go dark") + THRESHOLD GUEST (guest = local-only) — SEAM 2 of 2. The
    // actuating tool-loop gate does NOT consult `cloud_reachable` (it would otherwise
    // try the cloud and degrade on the resolve_api_key error), so close it here at the
    // decision itself: an active vault OR a guest turn forces `to_cloud` false, so a
    // heavy / low-confidence turn never reaches the cloud tool loop and instead stays
    // on the local path (or honestly degrades offline). RESTRICT-ONLY + COMPOSABLE via
    // the same `deny_cloud` gates as SEAM 1 — each can only turn a cloud decision OFF;
    // with BOTH off this is exactly `wants_cloud(class, cfg)`. GUEST: this is the
    // second half of forcing a bystander local — no cloud tool loop, so no obol spend
    // and no owner-key egress on a guest turn.
    let to_cloud = crate::threshold::deny_cloud(crate::vault::deny_cloud(wants_cloud(class, cfg)));
    // RC-6: a turn that is cloud-bound ONLY because the classifier was unsure
    // (low confidence on a conversation intent — the CLASSIFY_FALLBACK shape a
    // garbled echo produces) must NOT reach the actuating cloud tool loop. An
    // uncertain transcript could otherwise open URLs / launch apps. Such turns
    // take the NO-TOOLS persona completion instead, so an unsure transcript can
    // talk but never act. Confident heavy ACTION intents are unaffected.
    let actuating_cloud = to_cloud && !is_uncertain_fallback(class, cfg);

    // Darwin-Prime delegation: pick the handling agent BEFORE acting, then
    // resolve the tool this turn will actually invoke and enforce the agent's
    // allowlist — an out-of-domain match is handed to the tool's real owner so
    // isolation holds (no agent acts through another agent's exclusive tool).
    // The final selection is announced as agent.active so the HUD highlights
    // it and the core color shifts to its hue.
    let agent = select_agent(agents, &class.intent, text, cloud_reachable);
    emit_agent_active(agent);
    // OBOL: note the handling agent so a cloud spend row this turn attributes cost
    // to it (a secret-free agent NAME, never an utterance). No-op accounting seam.
    crate::obol::note_active_agent(&agent.name);

    if actuating_cloud {
        let model = cloud_model(needs_deep_reasoning, cfg);
        telemetry::emit(
            "cloud",
            "route.cloud",
            json!({
                "intent": class.intent,
                "confidence": class.confidence,
                "model": model,
                "deep_reasoning": needs_deep_reasoning,
            }),
        );
        // Bookkeeping must never kill a response (a busy darwin.db would
        // otherwise leave the user with dead air).
        if let Err(e) = memory.record_event("cloud", "route.cloud", text).await {
            warn!(error = %e, "failed to record cloud route event");
        }
        // Tool-use loop: the cloud model can ACT (open apps, search files,
        // set volume, remember facts) before phrasing its spoken answer, so
        // any phrasing of a request routed here still gets things done. Recall
        // is namespaced to the active agent (own namespace + shared facts) so
        // the cloud reply respects constellation isolation like the local one.
        // PROACTIVE RAG: rank the scoped facts by relevance to THIS request and
        // feed the most-relevant few (not the most-recent), so the reply is
        // grounded in the memory that bears on it — neural on-device when the
        // inference server is up, BM25 otherwise, top-K + token bounded.
        let facts: Vec<(String, String)> =
            anthropic::grounded_facts_live(text, memory, &agent.namespace).await;
        // SHARED WORLD MODEL: the entities/relationships relevant to this request,
        // from the shared user.world.* tier (every agent reads the same world; the
        // world model never reads any agent's private namespace). Rides the uncached
        // tail so the tool-loop reply reasons over one coherent world picture.
        let world_context = anthropic::grounded_world_live(text, memory).await;
        // PERSONALIZATION: the bounded user-model summary (observed preferences/
        // patterns/topics/style), rides the same uncached tail so the reply
        // personalizes to the REAL observed user — never an invented one. Reads
        // only the shared user.model.* tier (no namespace), so it can never carry
        // another agent's private notes.
        let personalization = anthropic::grounded_personalization_live(memory).await;
        let history = fetch_history(memory).await;
        // The active agent's own persona (specialists) so the cloud reply is
        // voiced in its persona and caches per-agent; the orchestrator passes
        // None and voices the shared global persona. The shared grounding
        // preamble is always present (it carries the no-fabrication rules), so
        // even an agent whose file is missing degrades to a grounded reply.
        let agent_persona = anthropic::agent_persona_text(&agent.name, agent.is_orchestrator());
        match anthropic::complete_with_tools(
            model,
            cfg.cloud.max_tokens,
            text,
            &facts,
            &history,
            memory,
            &agent.tools,
            &agent.namespace,
            agent_persona.as_deref(),
            &world_context,
            &personalization,
            true, // a direct user turn — trusted, user-originated
        )
        .await
        {
            Ok(response) => {
                return Ok(RouteOutcome {
                    routed_to: "cloud",
                    response,
                    agent: agent.name.clone(),
                    namespace: agent.namespace.clone(),
                    spoken: None,
                })
            }
            Err(e) => {
                // Degrade to the local model rather than going silent.
                // error! (not warn!): recurring cloud failures are a
                // self-heal trigger; the watchdog's burst detector only
                // counts ERROR-level lines (audit fix).
                error!(error = %e, "cloud completion failed; degrading to local generate");
                telemetry::emit(
                    "cloud",
                    "route.cloud_failed",
                    json!({"intent": class.intent, "error": e.to_string()}),
                );
                // Cloud failed -> degrade to the local brain; pick the warm local
                // model by difficulty (None under single-resident => the base).
                let local_model = local_model_for_turn(cfg, class).await;
                let response = generate_in_persona(
                    text,
                    CLOUD_DEGRADE_NOTE,
                    memory,
                    infer,
                    agent,
                    local_model.as_deref(),
                )
                .await;
                return Ok(RouteOutcome {
                    routed_to: "local",
                    response,
                    agent: agent.name.clone(),
                    namespace: agent.namespace.clone(),
                    spoken: None,
                });
            }
        }
    }

    // Conversation-to-cloud (CONTRACT B): casual chat / greetings / opinions —
    // the CONVERSATION intent, the pure llm_voice conversation path — route to
    // a CLOUD PERSONA COMPLETION by default ([router].conversation_route =
    // "cloud_heavy"). The local 4B is near-deterministic on bare greetings (a
    // model-capacity ceiling), so chat goes to cloud Opus/Haiku for genuinely
    // varied, in-character personality. This is a PLAIN persona completion
    // (persona + recent history + known facts + the utterance) — NOT the tool
    // loop: a greeting must never trigger a tool call. Actions, system.query,
    // memory ops, and the heavy/low-confidence cloud routing above are all
    // untouched — only this one intent gains cloud-by-default. A cloud error
    // degrades gracefully to the local converse path below (never silent).
    if class.intent == "conversation" {
        // MODEL TIER: resolve the conversation tier (Override > Auto > Fallback)
        // and surface it for the HUD on EVERY answered conversation turn, whether
        // it lands on cloud or local. The reason distinguishes a manual override
        // from the auto pick from a degrade.
        let (brain, tier, reason) = conversation_brain(cfg, cloud_reachable, class);
        let mut tier_payload = json!({
            "tier": tier.as_str(),
            "reason": reason.as_str(),
            "manual": reason == crate::model_tier::Reason::Override,
            "intent": class.intent,
        });
        // When the turn lands on the LOCAL tier, surface the active warm sub-choice
        // (FAST/CAPABLE) for the HUD's resident-models indicator — only meaningful
        // under a multi-resident warm-set; single-resident omits it (the base
        // answers, no indicator). Does NOT change the tier/model already chosen.
        if matches!(brain, ConversationBrain::Local) {
            if let Some(sub) = local_sub_for_turn(cfg, class).await {
                tier_payload["local_sub"] = json!(sub);
            }
            // BATTERY/THERMAL THROTTLE (#38) indicator: surface the plan reason +
            // whether it actually throttled this local turn. When the plan is neutral
            // (adaptive off, or on AC + nominal thermal) it emits no throttle field so the HUD
            // shows no throttle — honest, never a phantom. Only emitted on local
            // turns (the throttle influences only the on-device sub-choice).
            let plan = power_throttle_plan(cfg).await;
            if plan.is_throttled() {
                tier_payload["throttle"] = json!({
                    "reason": plan.reason.as_str(),
                    "tier_pref": plan.tier_pref.as_str(),
                    "defer_heavy": plan.defer_heavy,
                });
            }
        }
        telemetry::emit("system", "model.tier", tier_payload);
        if let ConversationBrain::Cloud(model) = brain {
            // Same context the local converse path uses: namespaced facts +
            // recent history. Recall is scoped to the active agent so the cloud
            // reply respects constellation isolation like the local one.
            // PROACTIVE RAG: the facts are ranked by relevance to this turn and
            // trimmed to the most-relevant few (top-K + token bounded), so even a
            // casual reply is grounded in the memory that bears on it.
            let facts = anthropic::grounded_facts_live(text, memory, &agent.namespace).await;
            // SHARED WORLD MODEL context for this turn (entities/relationships
            // relevant to the request), from the shared user.world.* tier — every
            // agent reads the same world, never another agent's private notes.
            let world_context = anthropic::grounded_world_live(text, memory).await;
            // PERSONALIZATION: the bounded user-model summary (observed
            // preferences/patterns/topics/style) so the chat reply personalizes to
            // the real observed user. Shared tier only -> never another agent's
            // private notes. Rides the same uncached tail as the world context.
            let personalization = anthropic::grounded_personalization_live(memory).await;
            let history = fetch_history(memory).await;
            // Anti-repeat (CONTRACT B): the last few DARWIN replies, passed so
            // complete_persona can tell Opus not to reuse their wording. This is
            // the load-bearing variation mechanism — Opus 4.8 takes no
            // temperature, so changing the prompt per call is the only way a
            // repeated bare "Hi DARWIN" varies instead of collapsing to one line.
            let avoid = recent_replies(&history, AVOID_RECENT_REPLIES);
            // The live constellation roster, so the cloud brain can name/list/
            // describe DARWIN's agents when asked instead of denying the team
            // exists (the cloud persona carries no static roster). Grounded —
            // the persona prompt forbids inventing agents not in this list.
            // GUEST GATE: withhold the owner's configured agent roster from a guest
            // turn — consistent with the facts/world/personalization/history feeds
            // above (all empty for a guest) and with guest_denied_fast_path refusing
            // the roll-call / agent-query fast paths. A guest gets no owner config
            // (agents.toml can carry owner-chosen agent names/roles).
            let roster = if crate::threshold::is_guest_turn() {
                String::new()
            } else {
                agents.roster_brief()
            };
            // The first-contact brief is converse data — fold it into the
            // utterance so the persona still phrases it on the cloud chat path
            // (the proactive brief never rides a tool loop; this plain
            // completion has none, so it carries the brief safely).
            let chat_text = match brief {
                Some(brief) if !brief.is_empty() => {
                    format!("{text}\n\n[Context for your reply: {brief}]")
                }
                _ => text.to_string(),
            };
            telemetry::emit(
                "cloud",
                "route.cloud",
                json!({
                    "intent": class.intent,
                    "confidence": class.confidence,
                    "model": &model,
                    "conversation": true,
                }),
            );
            if let Err(e) = memory.record_event("cloud", "route.cloud", text).await {
                warn!(error = %e, "failed to record cloud conversation route event");
            }
            // The active agent's own persona (specialists) so the cloud chat
            // reply is voiced in its persona and caches per-agent; the
            // orchestrator passes None and voices the shared global persona.
            let agent_persona = anthropic::agent_persona_text(&agent.name, agent.is_orchestrator());
            match anthropic::complete_persona(
                &model,
                GENERATE_MAX_TOKENS,
                &chat_text,
                &facts,
                &history,
                &roster,
                &avoid,
                agent_persona.as_deref(),
                &world_context,
                &personalization,
            )
            .await
            {
                Ok(response) => {
                    return Ok(RouteOutcome {
                        routed_to: "cloud",
                        response,
                        agent: agent.name.clone(),
                        namespace: agent.namespace.clone(),
                        spoken: None,
                    })
                }
                Err(e) => {
                    // Graceful degrade to the LOCAL converse path below — never
                    // silent. error! (not warn!): recurring cloud failures feed
                    // the self-heal burst detector, like the tool-loop path.
                    error!(error = %e, "cloud conversation completion failed; degrading to local converse");
                    telemetry::emit(
                        "cloud",
                        "route.cloud_failed",
                        json!({"intent": class.intent, "error": e.to_string(), "conversation": true}),
                    );
                    // Fall through to the local converse path (the route.local
                    // telemetry below marks the brain that actually answered).
                }
            }
        } else {
            // OFFLINE AGENTIC TOOL-USE (task #3). The tier resolved to Local
            // (the "work offline" override, no cloud key, or a cloud-unreachable
            // fallback), so this conversation turn is answered ON-DEVICE. Before
            // the plain converse below, give the resident 4B BOUNDED agency over a
            // CURATED SAFE LOCAL-TOOL subset: prompt -> parse one tool call ->
            // EXECUTE via the SAME gated `execute_tool` (consequential confirmation
            // + voice-id + lockdown + per-action policy ALL apply offline) -> feed
            // the result back -> at most N rounds -> FALL BACK to a plain converse
            // when the 4B emits no tool call. ONLINE is untouched (this is the
            // `else` of the Cloud branch); a non-conversation intent never reaches
            // here. The 4B's tool-call adherence is a real ceiling — it is bounded
            // and degrades gracefully; the same safety gates that govern the cloud
            // loop govern this one.
            let facts_kv = agent_facts(memory, &agent.namespace).await;
            let facts: Vec<String> = facts_kv
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect();
            let history = fetch_history(memory).await;
            if let Some(outcome) = anthropic::complete_with_local_tools(
                cfg,
                infer,
                GENERATE_MAX_TOKENS,
                text,
                &history,
                &facts,
                memory,
                &agent.tools,
                &agent.namespace,
            )
            .await
            {
                if outcome.tools_used > 0 {
                    // A safe local tool actually engaged this turn. Surface the
                    // honest offline-agency telemetry for the HUD (ACTING OFFLINE),
                    // then voice the tool RESULTS in persona via the streamed
                    // converse path. The HUD copy is honest: the on-device 4B used
                    // local tools, it is less reliable at tool-calling than the
                    // cloud model, and the same safety gates applied (gated => a
                    // consequential tool parked/refused offline, exactly as online).
                    telemetry::emit(
                        "local",
                        "local_tools.engaged",
                        json!({
                            "tools_used": outcome.tools_used,
                            "tools": outcome.tool_names,
                            "gated": outcome.gated,
                            "intent": class.intent,
                        }),
                    );
                    // Fold the first-contact brief (converse data) into the tool
                    // results so the persona still phrases it on this offline path.
                    let mut data = outcome.data;
                    if let Some(brief) = brief {
                        if !brief.is_empty() {
                            data = if data.is_empty() {
                                brief.to_string()
                            } else {
                                format!("{data}\n\n{brief}")
                            };
                        }
                    }
                    let data_opt = (!data.is_empty()).then_some(data.as_str());
                    // Multi-resident LOCAL sub-choice (task #17): this is an
                    // on-device turn, so pick the warm local model by difficulty
                    // (None under the single-resident default => the base).
                    let local_model = local_model_for_turn(cfg, class).await;
                    match speech::converse_speak(
                        text,
                        GENERATE_MAX_TOKENS,
                        &history,
                        &facts,
                        data_opt,
                        &agent.voice,
                        Some(agent.persona_name()),
                        local_model.as_deref(),
                        infer,
                        started,
                        reply,
                    )
                    .await
                    {
                        Ok(spoken) => {
                            return Ok(RouteOutcome {
                                routed_to: "local",
                                response: spoken.response,
                                agent: agent.name.clone(),
                                namespace: agent.namespace.clone(),
                                spoken: Some(SpokenReply {
                                    route_ms: spoken
                                        .done_at
                                        .duration_since(route_entry)
                                        .as_millis()
                                        as u64,
                                    report: spoken.report,
                                }),
                            })
                        }
                        Err(e) => {
                            // converse_speak only errs when NOTHING played; degrade
                            // to generate+speak so the tool results are still voiced.
                            error!(error = %e, "offline tool-loop converse failed before audio; falling back to generate+speak");
                            // Same warm local model the converse path chose, so the
                            // fallback stays on the same on-device brain.
                            let response = generate_in_persona(
                                text,
                                &data,
                                memory,
                                infer,
                                agent,
                                local_model.as_deref(),
                            )
                            .await;
                            return Ok(RouteOutcome {
                                routed_to: "local",
                                response,
                                agent: agent.name.clone(),
                                namespace: agent.namespace.clone(),
                                spoken: None,
                            });
                        }
                    }
                }
                // No tool engaged (the 4B emitted no parseable call, or the bound
                // was reached with nothing run): fall through to the plain converse
                // path below — today's offline behavior, unchanged.
            }
        }
    }

    telemetry::emit(
        "local",
        "route.local",
        json!({"intent": class.intent, "confidence": class.confidence}),
    );

    // Tool-allowlist isolation: the local intent IS the tool name in the
    // allowlist (app.launch, web.open, system.query, memory.store, ...). The
    // selected agent must hold it; if not, hand the turn to the tool's real
    // owner (or the orchestrator, who holds everything) and re-announce so the
    // HUD core tracks the agent that actually acts. select_agent already
    // routes these intents to their owners, so a re-route here only fires for
    // a keyword pick that landed on the wrong specialist — isolation, enforced.
    let agent = enforce_tool(agents, agent, &class.intent);

    // Silicon Canvas voice control (SPEC §6): a precise control phrase ("show
    // me the 3V3 net", "trace this net", "run ERC", "open silicon canvas") maps
    // to a LAUNCH or a STRUCTURED op forwarded to the running app. Checked
    // before the generic local handlers, so an op phrase that REACHES HERE is
    // handled deterministically and the app never sees natural language — the
    // daemon forwards structured ops ONLY. The action's verified outcome is
    // converse data, phrased in persona on the streamed path below (llm_voice),
    // exactly like the app-launch path.
    //
    // WHAT "REACHES HERE" COSTS — re-derived from the call graph, because the
    // sentence this replaced ("an op phrase that would otherwise classify as
    // conversation/app.launch is handled deterministically") claimed more than
    // the code does. This whole seam sits BELOW two early returns, so an op
    // phrase does NOT reach it on every turn:
    //   * the cloud TOOL LOOP (`if actuating_cloud` above) returns on Ok, and it
    //     is entered whenever the turn is heavy or below the confidence
    //     threshold (`wants_cloud`);
    //   * the CONVERSATION branch (`if class.intent == "conversation"`) resolves
    //     its tier through `conversation_brain`, and on the SHIPPED default
    //     ([router].conversation_route = "cloud_heavy") a reachable cloud makes
    //     that tier Heavy or Fast — both cloud — so `complete_persona` answers
    //     and returns.
    // Neither cloud path substitutes for this seam: `anthropic`'s tool catalogue
    // carries open_app/quit_app but NO describe / generate-image /
    // identify-sound / Silicon-Canvas / Lumen-read / Vision / Nexus /
    // Mark-Forge op. The nearest thing the catalogue does carry is
    // `screen_recall`, and it is NOT a substitute: it RANKS a bounded, in-RAM,
    // TRANSIENT ring of PAST redacted OCR text (owner-gated on Screen-Recording
    // consent) and reads nothing NOW — no VLM description, no camera, no audio
    // clip, no app op. So on the shipped cloud-enabled config an utterance the
    // on-device intent classifier labels "conversation" — and its taxonomy
    // (inference/prompts/intent_classifier.txt) is ELEVEN intents, none of them
    // these apps — is answered conversationally and these gates are never
    // consulted, while the SAME utterance offline falls through and actuates.
    // Hoisting this seam above those returns would change WHEN camera capture
    // and screen reads can actuate, which is a posture decision — recorded here
    // (and in daemon/src/miss_offer.rs) rather than taken.
    // Vision voice control (mirrors Silicon Canvas): "what do you see", "who is
    // there", "watch the door|screen", "analyze this video" map to a LAUNCH or a
    // STRUCTURED op forwarded to the running Vision app. Checked alongside the
    // Silicon Canvas seam, before the generic local handlers, so a precise
    // vision phrase is handled deterministically and the app never sees natural
    // language — the daemon forwards structured ops ONLY (DEFENSIVE-ONLY: the
    // ops carry no identity query; the app detects presence/objects, not "who").
    // Nexus voice control (mirrors Silicon Canvas): "mute the mic", "route input
    // 1 to the monitor", "set input gain to -18", "load the vocal preset", "what
    // are the levels" map to a LAUNCH or a STRUCTURED op (gain.set / route.set /
    // monitor.set / preset.load / state.get) forwarded to the running Nexus app.
    // Same seam, same discipline: the app exposes ops only and never parses
    // natural language (SPEC §6) — the daemon classifies the phrase and forwards
    // the structured op line VERBATIM.
    // Mark-Forge voice control (mirrors the three seams above): "open the physics
    // sandbox", "drop a box", "reset the simulation", "set gravity to the moon",
    // "pause"|"step" map to a LAUNCH or a STRUCTURED op (body.spawn / world.reset /
    // set.gravity / world.step) forwarded to the running Mark-Forge engine. Same
    // discipline: the engine exposes ops only and never parses natural language
    // (SPEC §7) — the daemon classifies the phrase and forwards the structured op
    // line VERBATIM; the headless CPU/f64 engine acts, the DEVICE-GATED R3F render
    // is never opened here.
    // RC-11: mute the mic NOW, before any local handler actuates. A local
    // action (`open_url`, app launch) fires inside the handlers below — BEFORE
    // converse_speak's ensure_guard() would otherwise engage the SPEAKING guard
    // (instant_opener ships off, so that only happens when the first reply clip
    // plays). Without this, the ~1-2s of STT/handler/converse-setup latency ran
    // with is_speaking()=false and the capture gate wide open, so the user's own
    // just-spoken command was re-segmented, re-transcribed, and re-routed —
    // opening the URL a second and third time (the live triple-open). This is
    // the local path ONLY; the cloud path returned far above and stays mic-live
    // through its (silent, possibly long) round trip so the user can still
    // correct. The guard is shared via the SPEAKING refcount, so the later
    // ensure_guard() in converse/speak is a no-op and complete()/abandon()
    // releases the single guard after the echo tail — no double-count, no leak.
    // VLM DESCRIBE (task #2): "describe my screen" / "what am I looking at" /
    // "describe this image <path>" routes to the VISION agent and calls the
    // on-device describe_image op (DISTINCT from the OCR read.screen path in
    // vision_command). Checked FIRST so a describe verb is never shadowed by the
    // OCR screen-read or the bare vision launch, and re-pins the active agent to
    // Vision (the vision owner) so the HUD + persona track the agent that acts.
    // The image is read ON-DEVICE; pixels never leave the device. When the VLM is
    // off / not downloaded, handle_describe FALLS BACK honestly (OCR for a screen
    // request, an honest gate line for an image) — never a fabricated description.
    let describe = describe_command(text);
    // IMAGE GENERATION (task #18): "generate/make/draw/create an image of X"
    // routes to the VISION agent (the visual-capability owner, same as describe)
    // and calls the on-device generate_image op (MLX diffusion). DISTINCT from the
    // describe path above: describe REASONS about an existing image; generate
    // RENDERS a new one from a text prompt. The prompt + the pixels stay
    // ON-DEVICE (saved under state/images/); there is NO cloud image API. When the
    // [image] gate is off / no model is named / the model isn't downloaded,
    // handle_generate_image surfaces the gate HONESTLY — never a fabricated image,
    // never a cloud fallback. Checked AFTER describe so a describe verb is never
    // shadowed (generate_image_command already vetoes a describe phrase).
    let generate_image = generate_image_command(text);
    // IDENTIFY SOUND (task #15): "what was that sound" / "identify that noise" /
    // "what am I hearing" routes to the VISION agent and calls the on-device
    // classify.sound op over a clip the daemon ALREADY captured (DISTINCT from
    // STT — speech — which transcribes words). The clip is the daemon's last
    // captured segment under state/tmp (never user-named, no new mic open); when
    // there is none, the handler says so honestly rather than guessing. ONLY the
    // sound-class LABELS leave the op — the audio never leaves the device.
    let sound_clip = {
        let candidate = sound_clip_path(root);
        let latest = if candidate.exists() { Some(candidate.as_path()) } else { None };
        identify_sound_clip_or_request(text, latest)
    };
    // LUMEN (#45): "read me the screen / the buttons" (READ-ONLY narrate) + "click
    // the <ordinal|name>" (-> the UNCHANGED ui_actuate capstone). Computed here so
    // the ACT arm can re-pin the active agent to the ui_actuate OWNER below, and
    // dispatch below (before the Vision arm) so a control read/act is Lumen's.
    let lumen = lumen_command(text);
    // Re-pin the active agent to Vision (the vision owner) for the describe, the
    // image-generation, and the identify-sound intents so the HUD + persona track
    // the agent that acts. A Lumen ACTUATION re-pins to the ui_actuate-OWNING
    // specialist so execute_tool runs under ITS allowlist (the capstone gate is
    // unchanged — it is just applied as the owning agent, like any live tool call).
    let agent: &Agent = if describe.is_some() || generate_image.is_some() || sound_clip.is_some() {
        let vision = agents.get(VISION_APP).unwrap_or(agent);
        emit_agent_active(vision);
        vision
    } else if matches!(lumen, Some(LumenCommand::Act(_))) {
        let actuator = agents.owner_of("ui_actuate").unwrap_or(agent);
        emit_agent_active(actuator);
        actuator
    } else {
        agent
    };

    reply.mute_for_action();
    let mut out = if crate::threshold::is_guest_turn() {
        // THRESHOLD — GUEST MODE: a guest reaches NONE of the vision / image / sound
        // / design handlers below — each reads the owner's screen or last-captured
        // audio, or renders on their machine. (describe / generate-image / silicon-
        // canvas / vision / nexus / mark-forge are already refused upstream by the
        // fast-path gate; this ALSO covers the sound-identify path and is defense in
        // depth.) Fall to the guest-gated handle_local, which admits only
        // conversation + non-personal status and refuses the rest.
        handle_local(&class.intent, &class.args, text, memory, app_registry, agent).await
    } else if let Some(req) = describe {
        handle_describe(req, cfg, infer, app_registry, root).await
    } else if let Some(req) = generate_image {
        handle_generate_image(req, cfg, infer).await
    } else if let Some(req) = sound_clip {
        handle_identify_sound(req.clip, app_registry, root).await
    } else if let Some(cmd) = silicon_canvas_command(text) {
        handle_silicon_canvas(cmd, app_registry).await
    } else if let Some(cmd) = lumen {
        // Lumen (#45) BEFORE Vision: a control read/act is Lumen's; `agent` is
        // already the ui_actuate owner for the ACT arm (re-pinned above).
        handle_lumen(cmd, memory, app_registry, agent).await
    } else if let Some(cmd) = vision_command(text) {
        handle_vision(cmd, app_registry).await
    } else if let Some(cmd) = nexus_command(text) {
        handle_nexus(cmd, app_registry).await
    } else if let Some(cmd) = mark_forge_command(text) {
        handle_mark_forge(cmd, app_registry).await
    } else {
        handle_local(&class.intent, &class.args, text, memory, app_registry, agent).await
    };
    if !out.llm_voice {
        return Ok(RouteOutcome {
            routed_to: "local",
            response: out.data,
            agent: agent.name.clone(),
            namespace: agent.namespace.clone(),
            spoken: None,
        });
    }
    // First-contact brief: appended AFTER the llm_voice gate, so it can only
    // ever reach a persona-phrased path (converse, or the generate fallback
    // below) — raw data replies would speak it verbatim.
    if let Some(brief) = brief {
        if out.data.is_empty() {
            out.data = brief.to_string();
        } else {
            out.data = format!("{}\n\n{brief}", out.data);
        }
    }

    // Streamed path: one converse op fuses generation and TTS server-side;
    // the first sentence is audible while the rest is still decoding. The
    // active agent's OWN voice and persona are passed (per-agent voicing), and
    // recall is namespaced — the agent sees its own namespace plus shared
    // facts only (constellation isolation at the recall layer).
    let facts_kv = agent_facts(memory, &agent.namespace).await;
    let facts: Vec<String> = facts_kv
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect();
    let history = fetch_history(memory).await;
    let data_opt = (!out.data.is_empty()).then_some(out.data.as_str());
    // Multi-resident LOCAL sub-choice (task #17): the persona-voicing converse
    // runs on-device, so pick the warm local model by difficulty (None under the
    // single-resident default => the base, exactly today's wire).
    let local_model = local_model_for_turn(cfg, class).await;
    match speech::converse_speak(
        text,
        GENERATE_MAX_TOKENS,
        &history,
        &facts,
        data_opt,
        &agent.voice,
        Some(agent.persona_name()),
        local_model.as_deref(),
        infer,
        started,
        reply,
    )
    .await
    {
        Ok(spoken) => Ok(RouteOutcome {
            routed_to: "local",
            response: spoken.response,
            agent: agent.name.clone(),
            namespace: agent.namespace.clone(),
            spoken: Some(SpokenReply {
                route_ms: spoken.done_at.duration_since(route_entry).as_millis() as u64,
                report: spoken.report,
            }),
        }),
        Err(e) => {
            // converse_speak only errs when NOTHING played, so falling back
            // to the old generate -> speak path cannot double-speak and the
            // daemon is never mute. error!: a converse outage is a recurring
            // hard failure the self-heal burst detector must see.
            error!(error = %e, "converse failed before any audio; falling back to generate+speak");
            telemetry::emit(
                "system",
                "inference.unavailable",
                json!({"op": "converse", "error": e.to_string()}),
            );
            // Same warm local model the converse path chose, so the degraded
            // generate stays on the same on-device brain.
            let response = generate_in_persona(
                text,
                &out.data,
                memory,
                infer,
                agent,
                local_model.as_deref(),
            )
            .await;
            Ok(RouteOutcome {
                routed_to: "local",
                response,
                agent: agent.name.clone(),
                namespace: agent.namespace.clone(),
                spoken: None,
            })
        }
    }
}

/// Process-wide roll-call cancel flag. The roll-call checks it before each
/// agent and stops cleanly when set, so a barge-in hook or a shutdown path can
/// interrupt the sequence mid-team (the reel centerpiece must be
/// interruptible). Set+cleared around each roll-call by `roll_call`; exposed
/// via `interrupt_roll_call` for a future barge-in caller.
static ROLL_CALL_CANCEL: AtomicBool = AtomicBool::new(false);

/// Request the in-progress roll-call to stop after the current agent's clip.
/// Idempotent and safe to call when no roll-call is running.
pub fn interrupt_roll_call() {
    ROLL_CALL_CANCEL.store(true, Ordering::Relaxed);
}

/// Clear the roll-call cancel flag (RC-9). Called from speech::clear_barge_in
/// at the start of every new turn so both interrupt flags (BARGE_IN and
/// ROLL_CALL_CANCEL) share ONE lifecycle: a barge over a non-roll-call reply
/// could otherwise leave ROLL_CALL_CANCEL latched true (it is only cleared at
/// roll_call start), so the NEXT roll-call would abort before its first agent.
/// Idempotent.
pub fn clear_roll_call_interrupt() {
    ROLL_CALL_CANCEL.store(false, Ordering::Relaxed);
}

/// The constellation roll-call (item 3): every agent, in roster order, speaks
/// its ONE-LINE self-introduction in ITS OWN voice (one sequential speak op
/// per agent), and an agent.active is emitted for each so the HUD highlights
/// them in turn and the core color cycles. Interruptible: the cancel flag is
/// checked before each agent and the loop also yields, so a barge-in/shutdown
/// can stop it mid-team. Returns the joined intro text (for the transcript)
/// and the reply's timing report. Never errors — a per-agent synthesis failure
/// skips that one agent rather than aborting the whole reel.
async fn roll_call(
    agents: &AgentRegistry,
    infer: &mut InferenceClient,
    reply: &mut speech::ReplySession,
    started: Instant,
    root: &Path,
    cfg: &Config,
) -> (String, speech::SpeakReport) {
    // Fresh run: clear any stale cancel from a previous, interrupted roll-call.
    ROLL_CALL_CANCEL.store(false, Ordering::Relaxed);
    telemetry::emit(
        "local",
        "rollcall.started",
        json!({"agents": agents.all().len()}),
    );

    // ADDITIVE (Phase-2): streaming opt-in + pronunciation locator from [voice].
    // SpeakExtras::none() with the shipped defaults -> the speak wire is unchanged.
    let extras = crate::inference::SpeakExtras::from_config(cfg);

    let mut spoken_intros: Vec<String> = Vec::new();
    for agent in agents.all() {
        if ROLL_CALL_CANCEL.load(Ordering::Relaxed) {
            info!("roll-call interrupted; stopping after {} agents", spoken_intros.len());
            telemetry::emit(
                "local",
                "rollcall.interrupted",
                json!({"spoken": spoken_intros.len()}),
            );
            break;
        }
        // Highlight this agent first so the HUD core color leads its voice.
        emit_agent_active(agent);
        let intro = agent.intro(root);
        // Each agent speaks in ITS OWN voice: resolve the backend per agent so the
        // cloud voice tier (when on + key + non-offline + the agent mapped) uses
        // that agent's ElevenLabs voice id, else its on-device Kokoro voice. With
        // the tier OFF (default) this is exactly today's per-agent Kokoro voicing.
        let (backend, el_key) =
            speech::resolve_speak_backend(cfg, &agent.name, &agent.voice).await;
        // EXPRESSIVENESS (#33): a roll-call intro is a GREETING (=> Warm prosody on
        // the EL-v3 rich path; coarse/neutral elsewhere). A roll-call is never a
        // required confirmation. With adaptive_prosody OFF this is SpeakShape::neutral
        // -> the speak wire is byte-for-byte today's. Whisper folds in the same way as
        // the base speak path (process-global state, never silencing a confirm).
        let profile = crate::prosody::classify_prosody(crate::prosody::ReplyKind::Greeting, false);
        let mut intro_shape = crate::prosody::shape_speak_request(cfg, profile, &backend);
        let whisper_on = crate::prosody::whisper_state_is_on();
        intro_shape = crate::prosody::apply_whisper(intro_shape, whisper_on, false);
        crate::prosody::emit_telemetry(profile, &backend, &intro_shape, whisper_on);
        // English self-introduction — no Babel target language to thread.
        match infer.speak(&intro, &backend, el_key.as_deref(), None, &intro_shape, &extras).await {
            Ok(wav) => {
                if reply.push_clip(&wav).await {
                    spoken_intros.push(intro);
                } else {
                    warn!(agent = %agent.name, "roll-call intro produced no audio; skipping");
                }
            }
            Err(e) => {
                // One agent's synthesis failure must not abort the reel.
                warn!(agent = %agent.name, error = %e, "roll-call intro synthesis failed; skipping");
            }
        }
        // Cooperative yield: lets a cancel set from another task take effect
        // promptly between agents even on a busy runtime.
        tokio::task::yield_now().await;
    }

    telemetry::emit(
        "local",
        "rollcall.completed",
        json!({"spoken": spoken_intros.len()}),
    );
    let report = reply.finish_report(started).await;
    (spoken_intros.join(" "), report)
}

/// Answer an agent-ROSTER query from the live registry — never the classifier +
/// local model, which (lacking the roster in context) hallucinates agents that
/// do not exist. Cloud-reachable: phrase the REAL roster in persona (grounded —
/// persona.txt forbids inventing agents not in the roster, and the anti-repeat
/// hint keeps it fresh). Offline or on a cloud error: a deterministic spoken list
/// (still the real team, accurate, no model guessing). Returns the reply text and
/// the brain that produced it ("cloud"/"local") for the route telemetry.
async fn answer_agent_roster(
    text: &str,
    agents: &AgentRegistry,
    memory: &Memory,
    cfg: &Config,
    cloud_reachable: bool,
) -> (String, &'static str) {
    let roster = agents.roster_brief();
    // The roster reply is a simple, confident conversation-style answer: model it
    // as a light/high-confidence turn so the tier resolver (override + auto) keeps
    // today's cloud-when-reachable behavior, while still honoring an offline
    // override (which forces the deterministic local roster below). A model-control
    // override is respected here exactly as on the main conversation path.
    let roster_class = Classification {
        intent: "conversation".to_string(),
        complexity: "light".to_string(),
        confidence: 1.0,
        args: serde_json::Value::Null,
    };
    let (brain, tier, reason) = conversation_brain(cfg, cloud_reachable, &roster_class);
    let mut tier_payload = json!({
        "tier": tier.as_str(),
        "reason": reason.as_str(),
        "manual": reason == crate::model_tier::Reason::Override,
        "intent": "agent_query",
    });
    // Local-tier roster turn: surface the active warm sub-choice for the HUD
    // (only under multi-resident; single-resident omits it). Same honest readout
    // as the conversation path; no change to the tier/model chosen.
    if matches!(brain, ConversationBrain::Local) {
        if let Some(sub) = local_sub_for_turn(cfg, &roster_class).await {
            tier_payload["local_sub"] = json!(sub);
        }
        // #38 throttle indicator (absent when the plan is neutral).
        let plan = power_throttle_plan(cfg).await;
        if plan.is_throttled() {
            tier_payload["throttle"] = json!({
                "reason": plan.reason.as_str(),
                "tier_pref": plan.tier_pref.as_str(),
                "defer_heavy": plan.defer_heavy,
            });
        }
    }
    telemetry::emit("system", "model.tier", tier_payload);
    if let ConversationBrain::Cloud(model) = brain {
        let prime = agents.orchestrator();
        // PROACTIVE RAG: facts ranked by relevance to the roster query, scoped to
        // the orchestrator's namespace (same isolation-safe view), top-K + token
        // bounded — so any user fact that bears on the question is surfaced.
        let facts = anthropic::grounded_facts_live(text, memory, &prime.namespace).await;
        // SHARED WORLD MODEL context (relevant to the question) from the shared
        // user.world.* tier — consistent grounding even on the roster path.
        let world_context = anthropic::grounded_world_live(text, memory).await;
        let history = fetch_history(memory).await;
        let avoid = recent_replies(&history, AVOID_RECENT_REPLIES);
        telemetry::emit(
            "cloud",
            "route.cloud",
            json!({"intent": "agent_query", "model": &model, "conversation": true}),
        );
        // The roster query is answered by the orchestrator (darwin), which voices
        // the global persona — so no per-agent persona block (None), matching
        // the namespaced facts seeded from the orchestrator above.
        let agent_persona = anthropic::agent_persona_text(&prime.name, prime.is_orchestrator());
        match anthropic::complete_persona(
            &model,
            GENERATE_MAX_TOKENS,
            text,
            &facts,
            &history,
            &roster,
            &avoid,
            agent_persona.as_deref(),
            &world_context,
            // The roster query is ABOUT the team, not the user — no personalization
            // grounding is needed (and the deterministic roster below carries none),
            // so pass an empty summary: honest and focused.
            "",
        )
        .await
        {
            Ok(r) => return (r, "cloud"),
            Err(e) => {
                // Never go silent or let the local model freelance the roster —
                // fall through to the grounded deterministic list below.
                error!(error = %e, "cloud agent-roster reply failed; using the grounded deterministic roster");
                telemetry::emit(
                    "cloud",
                    "route.cloud_failed",
                    json!({"intent": "agent_query", "error": e.to_string()}),
                );
            }
        }
    }
    telemetry::emit("local", "route.local", json!({"intent": "agent_query"}));
    (agents.roster_spoken(), "local")
}

/// The UNCHANGED heavy/low-confidence cloud predicate: route to cloud iff the
/// classifier marked the turn heavy OR its confidence fell below the
/// configured threshold. Applies to EVERY intent (this is not the
/// conversation-specific routing) — extracted only so the contract's
/// "heavy -> cloud, action -> local" invariants are unit-testable.
fn wants_cloud(class: &Classification, cfg: &Config) -> bool {
    class.complexity == "heavy" || class.confidence < cfg.router.cloud_confidence_threshold
}

/// RC-6: whether this cloud-bound turn is an UNCERTAIN FALLBACK — a
/// conversation intent the classifier was not confident about (below the cloud
/// threshold). This is exactly the shape a garbled/echo transcript produces
/// (CLASSIFY_FALLBACK = conversation / 0.3 / heavy). Such a turn must take the
/// NO-TOOLS persona completion, never the actuating tool loop, so an unsure
/// transcript can speak but cannot open URLs or launch apps. A CONFIDENT
/// conversation turn, or any non-conversation intent (a real, if weakly
/// recognized, action), is NOT a fallback and keeps its existing routing. Pure,
/// so the boundary is unit-testable.
fn is_uncertain_fallback(class: &Classification, cfg: &Config) -> bool {
    class.intent == "conversation" && class.confidence < cfg.router.cloud_confidence_threshold
}

/// The UNCHANGED cloud model pick for the heavy/low-confidence path: the heavy
/// model (Opus) for deep reasoning, else the fast model (Haiku). Extracted so
/// "heavy -> opus" stays verified without a live call.
fn cloud_model(needs_deep_reasoning: bool, cfg: &Config) -> &str {
    cloud_model_under_budget(needs_deep_reasoning, cfg, crate::obol::current_budget_pressure(cfg))
}

/// THE DOLLAR CAP APPLIES TO ACTIONS TOO, not only to chat.
///
/// `obol::current_budget_pressure` had exactly two call sites: conversation_brain (the
/// CHAT path) and PRECOG (read-only). The ACTUATING cloud path — the heavy/action turns
/// that call tools and cost the most — picked its model straight from [cloud] with no
/// reference to the budget at all. So an operator who set `[obol].daily_usd_cap` had it
/// enforced on conversation and ignored on the very turns that spend hardest, while
/// PRECOG told them the cap was in force.
///
/// Pressure is REDUCE-ONLY, exactly as it is for chat: Ease steps a Heavy turn down to
/// the fast model, Floor pins to the fast model (the actuating path needs a cloud model
/// to drive tools at all — it cannot drop to on-device the way a conversation can, so
/// Floor buys the cheapest cloud brain rather than pretending to route local). Under
/// the shipped no-cap default this is Pressure::None and the choice is unchanged.
fn cloud_model_under_budget(
    needs_deep_reasoning: bool,
    cfg: &Config,
    pressure: crate::obol::Pressure,
) -> &str {
    let heavy = needs_deep_reasoning && matches!(pressure, crate::obol::Pressure::None);
    if heavy {
        &cfg.cloud.heavy_model
    } else {
        &cfg.cloud.fast_model
    }
}

/// Which brain answers a CONVERSATION turn, decided from [router].
/// conversation_route, the chosen model, and whether the cloud key is present.
/// Pure and unit-tested so the routing-decision table is verified without any
/// live cloud call or inference client.
#[derive(Debug, PartialEq)]
enum ConversationBrain {
    /// A plain cloud persona completion using this model (Opus for the Heavy
    /// tier, Haiku for the Fast tier). Owns the model string (resolved from the
    /// [cloud] config via the model-tier resolver).
    Cloud(String),
    /// The local 4B converse path — Local tier (route "local", no cloud key, an
    /// unknown route value, an offline override, or a cloud-unreachable fallback)
    /// all land here; a cloud error degrades to it too.
    Local,
}

/// Decide where a CONVERSATION turn is answered, now through the MODEL-TIER
/// resolver so the per-turn override + auto-difficulty heuristic apply. The
/// precedence is Override > Auto > Fallback ([`crate::model_tier::resolve_tier`]):
///   * an explicit voice override ("use the powerful model" / "go offline") wins;
///   * else AUTO maps [router].conversation_route (the durable default) refined by
///     THIS turn's difficulty (a trivial chat turn steps down to Fast, a heavy one
///     up to Heavy) — preserving today's behavior at the config default;
///   * a cloud tier with no cloud this turn (no key / offline / a Local override)
///     resolves Local (Reason::Fallback / Override) — NO cloud call is made.
///
/// `cloud_key_present` is whether a cloud call can be made at all this turn. The
/// resolved tier maps to a model string via [`crate::model_tier::tier_to_model`]
/// (Heavy -> heavy_model, Fast -> fast_model, Local -> the on-device path), so the
/// [cloud] config stays the single source of truth. Returns the brain AND the
/// `(Tier, Reason)` so the caller can emit `model.tier` telemetry for the HUD.
/// Pure + unit-tested — no live cloud call.
fn conversation_brain(
    cfg: &Config,
    cloud_key_present: bool,
    class: &Classification,
) -> (ConversationBrain, crate::model_tier::Tier, crate::model_tier::Reason) {
    // OBOL BUDGET-FLOOR: the current dollar-budget pressure (Pressure::None under
    // the shipped no-cap default, so byte-for-byte today's routing until the owner
    // sets `[obol].daily_usd_cap`). Read synchronously from the in-memory day total;
    // it is a REDUCE-ONLY precedence input (Override > Budget-floor > Auto > Fallback)
    // that can only step the tier DOWN toward the cheaper/on-device path.
    let budget = crate::obol::current_budget_pressure(cfg);
    let (tier, reason) = crate::model_tier::resolve_tier(
        cfg,
        crate::model_tier::current_override(),
        &class.complexity,
        class.confidence,
        cfg.router.cloud_confidence_threshold,
        cloud_key_present,
        budget,
    );
    let brain = match crate::model_tier::tier_to_model(tier, cfg) {
        crate::model_tier::ModelChoice::Cloud(model) => ConversationBrain::Cloud(model),
        crate::model_tier::ModelChoice::Local => ConversationBrain::Local,
    };
    (brain, tier, reason)
}

/// The Local-tier SUB-CHOICE for an on-device turn (task #17): which WARM local
/// model the local converse/generate op should answer with. Returns `Some(id)`
/// ONLY when the operator configured a MULTI-RESIDENT warm-set ([models].local_warm
/// + a budget that admits an extra) AND the AUTO-by-difficulty heuristic picks a
///   NON-base model for this turn; otherwise `None` -> the server answers on the base
///   single-resident model (today's behavior).
///
/// This is the conservative, honest wiring: it never names a model that is not in
/// the budget-bounded warm plan, it leaves the wire untouched (and so identical to
/// today) under the single-resident default, and an unknown id the server would
/// fall back to the base anyway. PURE given `cfg` + the classification — no cloud,
/// no load. It does NOT change WHICH tier is chosen; it only refines the already-
/// chosen Local tier, and makes no cloud call.
async fn local_model_for_turn(cfg: &Config, class: &Classification) -> Option<String> {
    let tel = crate::model_tier::local_warm_telemetry(cfg);
    // Single-resident (the default + low-RAM path): nothing to choose — the base
    // answers every local turn, exactly as today. Send no local_model.
    if !tel.multi_resident {
        return None;
    }
    // BATTERY/THERMAL THROTTLE (#38): the LIVE (TTL-cached) power reading is
    // consulted when [power].adaptive is on (the SHIPPED DEFAULT — so the default
    // config does read live power on a local turn, bounded by the 15s cache);
    // with the flag OFF the plan is NEUTRAL (Auto sub-tier, defer nothing),
    // byte-for-byte the prior AUTO-by-difficulty behavior. A throttled plan
    // biases the sub-choice toward the cheaper Fast warm model to save
    // battery/heat — but ONLY on an easy turn: throttled_sub_tier keeps AUTO on
    // a hard/low-confidence turn, so select_local_model keeps the capable base
    // and a throttle can NEVER degrade a genuinely hard offline turn.
    let plan = power_throttle_plan(cfg).await;
    let sub = crate::model_tier::throttled_sub_tier(
        &plan,
        &class.complexity,
        class.confidence,
        cfg.router.cloud_confidence_threshold,
    );
    let chosen = crate::model_tier::select_local_model(
        &tel.planned,
        sub,
        &class.complexity,
        class.confidence,
        cfg.router.cloud_confidence_threshold,
    );
    // Only thread a NON-base id (a base pick is the default wire => omit it).
    if chosen == tel.base {
        None
    } else {
        Some(chosen.to_string())
    }
}

/// The current battery/thermal throttle plan for this turn (#38). DEVICE-GATED:
/// when [power].adaptive is ON (the shipped default) this reads the LIVE
/// (TTL-cached) `pmset`/thermal state and feeds it to the pure throttle policy;
/// with the flag OFF it returns the NEUTRAL plan, so routing is byte-for-byte
/// today's. A failed read degrades to neutral (never a fabricated low battery).
async fn power_throttle_plan(cfg: &Config) -> crate::model_tier::ThrottlePlan {
    // [power].adaptive (ships ON): feed the LIVE (TTL-cached) battery + thermal
    // reading so a real on-battery / thermally-pressured state can actually
    // influence the on-device sub-choice (the throttle can now fire). OFF -> the
    // neutral reading, so routing is byte-for-byte today's. A failed read
    // degrades to neutral inside read_power_cached; NEVER a fabricated low
    // battery, and the throttle only ever steers the on-device sub-choice toward
    // the cheaper warm model — it NEVER loosens a gate or forces a cloud call.
    let reading = if cfg.power.adaptive {
        crate::power::read_power_cached().await
    } else {
        crate::power::PowerReading::neutral()
    };
    crate::power::current_plan(cfg, reading)
}

/// The ACTIVE local sub-choice label for THIS turn's `model.tier` telemetry — the
/// HUD's resident-models FAST/CAPABLE indicator (consumed by `applyLocalSub`).
/// Returns `Some("fast")` when the AUTO heuristic answered this turn on the faster
/// non-base warm model, `Some("capable")` when the capable base answered while a
/// multi-resident warm-set was in effect, and `None` under single-resident (the
/// default + low-RAM path: the base answers every local turn, so there is no
/// sub-choice to report and the HUD indicator stays empty — honest, not stale).
/// PURE; mirrors `local_model_for_turn`'s decision so the readout matches the model
/// that actually answered. Does NOT change which tier/model is chosen.
async fn local_sub_for_turn(cfg: &Config, class: &Classification) -> Option<&'static str> {
    // Single-resident => no sub-choice; the base answers (no indicator). Only
    // report a sub-choice when multi-resident actually selected among warm models,
    // so the label reflects the model that answered (Fast) or the base it kept
    // (Capable) — never a phantom choice under the single-resident default.
    match local_model_for_turn(cfg, class).await {
        Some(_) => Some(crate::model_tier::LocalSubTier::Fast.as_str()),
        None if crate::model_tier::local_warm_telemetry(cfg).multi_resident => {
            Some(crate::model_tier::LocalSubTier::Capable.as_str())
        }
        None => None,
    }
}

/// Route a non-default Capability-Selector [`Mode`](crate::selector::Mode) to its
/// pipeline, returning the finished [`RouteOutcome`] (or `None` to DECLINE and let
/// the normal pipeline below handle the turn — e.g. an empty world read).
///
/// Each mode's pipeline reuses the already-built, already-gated subsystem:
///   * `WorldQuery`  -> a DETERMINISTIC, READ-ONLY answer from the shared World
///     Model (no cloud, no tool loop). Declines (None) on an empty world so the
///     normal pipeline can still talk about the topic.
///   * `WorldUpdate` -> the cloud tool loop CONSTRAINED to the `world_update` tool,
///     which folds the stated fact into ONLY the shared `user.world.*` tier (never
///     a consequential external action, never a private namespace). Degrades
///     gracefully offline (no fabrication).
///   * `Mission`     -> FURY's bounded mission engine (`run_fury_mission`):
///     decompose -> dispatch each sub-task under its owning specialist's allowlist
///     + the consequential gate -> synthesize. Degrades to a friendly line offline.
///   * `Standing`    -> PROPOSE ONLY (`propose_standing_mission`): parks behind the
///     cross-turn confirmation gate + the armed-by-default master switch (a confirmed
///     action still needs a fresh per-action confirm). Creates nothing here (Rail 2).
///
/// `OneShot` never reaches this function (the caller falls straight through).
async fn route_capability(
    mode: crate::selector::Mode,
    text: &str,
    memory: &Memory,
    agents: &AgentRegistry,
    cloud_reachable: bool,
) -> Option<RouteOutcome> {
    use crate::selector::Mode;
    // The shared World Model is namespace-independent, and the selector's
    // capabilities are orchestrator-level dispatch, so they are voiced by the
    // orchestrator (DARWIN-Prime). WorldUpdate re-homes to the tool's owner below.
    let prime = agents.orchestrator();
    match mode {
        Mode::OneShot => None, // never routed here; the caller handles it.

        Mode::WorldQuery => {
            // DETERMINISTIC read-only answer from the shared world tier. If the
            // world holds nothing on the topic, DECLINE so the normal pipeline can
            // still answer conversationally instead of a dead "nothing recorded".
            let snapshot = anthropic::grounded_world_live(text, memory).await;
            if snapshot.trim().is_empty() {
                return None;
            }
            let response = anthropic::world_query_live(memory, text).await;
            emit_agent_active(prime);
            Some(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            })
        }

        Mode::WorldUpdate => {
            // Fold the stated fact into the SHARED world via the cloud tool loop,
            // constrained to ONLY the world_update tool — so extraction of the
            // structured (entity/attribute/value) or (from/relation/to) write is
            // done by the brain, but it can write nothing but user.world.* and can
            // fire no consequential action. world_update is in friday's allowlist
            // (a world-update-capable specialist); we re-home so isolation holds.
            let owner = agents
                .owner_of("world_update")
                .filter(|a| a.may_use("world_update"))
                .unwrap_or(prime);
            emit_agent_active(owner);
            if !cloud_reachable {
                // Honest offline degrade: record nothing we can't structure, never
                // fabricate a write. The normal pipeline isn't a better answer for
                // a world write, so we own the turn with a clear note.
                return Some(RouteOutcome {
                    routed_to: "local",
                    response: "I can note that into the world model once the cloud uplink is back, sir — I won't record a half-understood fact offline.".to_string(),
                    agent: owner.name.clone(),
                    namespace: owner.namespace.clone(),
                    spoken: None,
                });
            }
            let directive = format!(
                "Fold this stated fact into the shared world model using the world_update tool, \
                 then confirm what you recorded in one line: {text}"
            );
            let world_context = anthropic::grounded_world_live(text, memory).await;
            let only_world_update = vec!["world_update".to_string()];
            let agent_persona =
                anthropic::agent_persona_text(&owner.name, owner.is_orchestrator());
            match anthropic::complete_with_tools(
                cloud_model_for_world_update(),
                512,
                &directive,
                &[],
                &[],
                memory,
                &only_world_update,
                &owner.namespace,
                agent_persona.as_deref(),
                &world_context,
                // A focused world_update directive — no personalization grounding
                // needed; pass an empty summary.
                "",
                true, // the user's own stated fact — trusted (and only world_update is offered)
            )
            .await
            {
                Ok(response) => Some(RouteOutcome {
                    routed_to: "cloud",
                    response,
                    agent: owner.name.clone(),
                    namespace: owner.namespace.clone(),
                    spoken: None,
                }),
                Err(e) => {
                    warn!(error = %e, "world_update capability failed; degrading");
                    Some(RouteOutcome {
                        routed_to: "local",
                        response: "I couldn't fold that into the world model just now, sir.".to_string(),
                        agent: owner.name.clone(),
                        namespace: owner.namespace.clone(),
                        spoken: None,
                    })
                }
            }
        }

        Mode::Mission => {
            // FURY's bounded mission engine. run_fury_mission degrades to a
            // friendly offline line WITHOUT planning/dispatching when no key
            // resolves, so it is safe to call regardless of cloud state.
            // A mission the owner requested directly (Mission mode) — trusted.
            let response = anthropic::run_fury_mission(text, memory, true).await;
            let fury = agents.get("fury").unwrap_or(prime);
            emit_agent_active(fury);
            Some(RouteOutcome {
                routed_to: "local",
                response,
                agent: fury.name.clone(),
                namespace: fury.namespace.clone(),
                spoken: None,
            })
        }

        Mode::Standing => {
            // PROPOSE ONLY (Rail 2): park behind the confirmation gate + the
            // armed-by-default master switch (still per-action gated). Nothing is established here. The
            // proposing agent is the orchestrator; its allowlist is carried into
            // the pending so the spoken-yes replay re-checks it.
            emit_agent_active(prime);
            let (response, parked) =
                anthropic::propose_standing_mission(text, &prime.namespace, &prime.tools, memory).await;
            telemetry::emit(
                "local",
                "selector.standing_proposed",
                json!({"parked": parked}),
            );
            Some(RouteOutcome {
                routed_to: "local",
                response,
                agent: prime.name.clone(),
                namespace: prime.namespace.clone(),
                spoken: None,
            })
        }
    }
}

/// The model the constrained `world_update` capability loop uses — the fast model
/// is plenty for a single structured tool call; this keeps the world-write path
/// cheap. A bare const (not the full cfg plumbing) because this loop runs ONE tool
/// call to fold one fact, never deep reasoning.
fn cloud_model_for_world_update() -> &'static str {
    "claude-haiku-4-5"
}

/// Darwin-Prime delegation wrapper: pick the agent for this turn. Cloud
/// reachability gates the offline-survival route (hulk owns conversational
/// turns when the cloud is unreachable).
///
/// WHAT WENT WRONG: this used to take a `to_cloud` flag and compute
/// `effective_cloud = cloud_reachable || to_cloud`, justified by "if THIS turn is
/// already heading to the cloud, the cloud is by definition reachable for it".
/// The call site establishes no such thing. `to_cloud` is
/// `deny_cloud(deny_cloud(wants_cloud(class, cfg)))`, and `wants_cloud` reads
/// ONLY the classifier's complexity/confidence — while `cloud_reachable` is
/// `resolve_api_key().is_some()`. So on a KEYLESS install every heavy (or
/// below-threshold) turn had `to_cloud = true, cloud_reachable = false`, the OR
/// forced `effective_cloud = true`, and HULK — the agent that exists precisely
/// for "the cloud is unreachable" — was never selected. The turn was attributed
/// to darwin, recorded in the wrong memory namespace, spoken in the wrong
/// persona, and (for the heavy+confident case) route() first made a
/// guaranteed-to-fail `complete_with_tools` call that logged
/// `error!("cloud completion failed; degrading to local generate")` on EVERY such
/// turn — the exact ERROR level the self-heal burst detector counts.
///
/// The `to_cloud` argument is gone rather than ANDed in: `cloud_reachable && to_cloud`
/// would send an ordinary LIGHT conversation turn (to_cloud=false) to hulk even
/// with a working key, which is a different wrong answer. Reachability alone is
/// the right gate, and it is the only thing `AgentRegistry::select`'s offline
/// route ever meant to read.
///
/// SMARTER ROUTING: the deterministic intent map + keyword cues
/// (`AgentRegistry::select`) stay the fast, authoritative FIRST PASS. A SEMANTIC
/// fallback (`select_with_fallback`) engages ONLY when that pass would otherwise
/// fall to the orchestrator default for a non-trivial conversation turn — it
/// then picks the best-matching specialist by lexical (BM25) similarity of the
/// utterance to each agent's role text via [`agents::LexicalAgentScorer`]. The
/// scorer is PURE (no inference/network call — honest keyword-semantic, the same
/// fallback recall uses when the on-device embedder is down) and degrades to the
/// orchestrator on a weak/tied/absent signal, so an ambiguous fitness question
/// reaches hercules while a blank or low-confidence turn stays with darwin —
/// never a worse outcome than the deterministic pass alone. This changes only
/// DELEGATION: the caller still enforces the chosen agent's tool allowlist
/// (`enforce_tool`) and the confirmation gate, so isolation/safety are untouched.
fn select_agent<'a>(
    agents: &'a AgentRegistry,
    intent: &str,
    text: &str,
    cloud_reachable: bool,
) -> &'a Agent {
    agents.select_with_fallback(intent, text, cloud_reachable, &crate::agents::LexicalAgentScorer)
}

/// Handle a RUNBOOK voice command (runbook.rs): PLAN (PURE, read-only render of the
/// typed DAG + which steps PARK) or RUN (execute — re-issue each step FRESH through the
/// live tool gate, one at a time). Gated by [runbook].enabled (OFF by default): with it
/// off both verbs report the subsystem is off and do NOTHING.
///
/// RUN grants NO authority. It mirrors the macro-replay dispatch: each step routes once
/// through the SAME `anthropic::execute_tool` + gate a live tool call takes, so a
/// consequential step PARKS FRESH for a spoken confirm (the process-global single-slot
/// pending, exactly as a live consequential call installs). It never batches or
/// pre-approves; a parked step produces nothing, so its `${ref}` consumer BLOCKS rather
/// than run on a fabricated value (the executor in runbook.rs enforces this). The named
/// runbook is loaded from its CONFINED on-device store; an unsound runbook is refused
/// whole. Emits the secret-free `runbook.plan` / `runbook.run` frames — DIAGNOSTIC,
/// NOT "HUD frames" as this line used to call them: hud/src has no `runbook` handling
/// at all, so both fall through `applyEnvelope`'s exact-match default (runbook.rs says
/// the same at its module head). The user learns the outcome from the spoken reply and
/// from each consequential step's own fresh confirm.
async fn handle_runbook_command(
    cmd: crate::runbook::RunbookCommand,
    cfg: &Config,
    memory: &Memory,
    agent: &Agent,
    root: &Path,
) -> String {
    use crate::runbook::RunbookCommand;
    if !cfg.runbook.enabled {
        telemetry::emit("system", "runbook.blocked", json!({"reason": "disabled"}));
        return "Runbooks are off ([runbook].enabled = false), sir — I'm not planning or \
                running any."
            .to_string();
    }

    // Load + parse the named runbook from its confined `state/runbooks/*.runbook.toml`
    // store (load re-normalizes the name, so a path-y name is refused, never read).
    let rb = match crate::runbook::load(root, cmd.name()) {
        Ok(rb) => rb,
        Err(e) => return format!("{e}, sir."),
    };
    // Bound: one runbook may hold at most [runbook].max_steps steps — never an
    // unbounded DAG (mirrors the macro max_steps bound).
    if rb.steps.len() > cfg.runbook.max_steps {
        return format!(
            "Runbook \"{}\" has {} steps, over the {}-step bound, sir — I won't run an \
             unbounded DAG.",
            rb.name,
            rb.steps.len(),
            cfg.runbook.max_steps
        );
    }
    // The capability registry the checker resolves each step against; every TOOL's
    // privilege is pinned to the SAME confirm::is_consequential_tool source the gate
    // uses, so "will PARK" can never disagree with the gate.
    let reg = crate::runbook::Registry::builtin();

    match cmd {
        // PLAN: PURE, read-only. Render the whole DAG + which steps PARK; run nothing.
        RunbookCommand::Plan { .. } => {
            let plan = crate::runbook::plan(&rb, &reg);
            crate::runbook::emit_plan(&plan);
            let errs = plan
                .diagnostics
                .iter()
                .filter(|d| d.severity == crate::runbook::Severity::Error)
                .count();
            let mut out = format!(
                "Runbook \"{}\", sir: {} step{}, {} will park for a fresh spoken confirm.",
                plan.name,
                plan.steps.len(),
                if plan.steps.len() == 1 { "" } else { "s" },
                plan.park_count,
            );
            if plan.is_runnable() {
                out.push_str(" It type-checks and is ready to run.");
            } else {
                out.push_str(&format!(
                    " It has {errs} error{} and will NOT run until they're fixed.",
                    if errs == 1 { "" } else { "s" }
                ));
            }
            out
        }
        // RUN: execute — walk the DAG one step at a time through the live gate.
        RunbookCommand::Run { .. } => {
            // The LIVE router seam: route each ResolvedStep through the SAME gated entry
            // point (`anthropic::execute_tool`) a live tool call uses. It routes EXACTLY
            // ONE step per call (runbook::run never batches), so a consequential step
            // re-hits the confirmation gate + master switch + voice-id + lockdown FRESH.
            // Runs under the orchestrator (darwin): its `["*"]` allowlist admits every
            // tool, and execute_tool re-checks every safety gate regardless.
            struct LiveRunbookRouter<'a> {
                memory: &'a Memory,
                allowed: Vec<String>,
                namespace: String,
            }
            impl crate::runbook::RunbookRouter for LiveRunbookRouter<'_> {
                fn route_step<'a>(
                    &'a self,
                    step: &'a crate::runbook::ResolvedStep,
                ) -> std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::runbook::StepResult> + Send + 'a>,
                > {
                    Box::pin(async move {
                        let input = serde_json::Value::Object(step.input.clone());
                        // `user_originated = false`: a runbook is an automated re-issue
                        // whose later steps may consume an earlier step's output, so the
                        // egress continuation guard treats an outward GET conservatively
                        // — the runbook can never do MORE than a tool continuation could.
                        let (out, is_error) = anthropic::execute_tool(
                            &step.uses,
                            &input,
                            self.memory,
                            &self.allowed,
                            &self.namespace,
                            false,
                            // context_trusted=false: a runbook step PARKS fresh even
                            // under an Always policy — preserving the runbook's
                            // never-pre-approved invariant (the user confirms each
                            // consequential step, one at a time).
                            false,
                            &mut crate::anthropic::ToolEffect::DryRun,
                        )
                        .await;
                        // A consequential step is NEVER mapped to a produced value (it
                        // parked / previewed / was refused); a benign step yields its
                        // output. This is the load-bearing no-authority mapping.
                        crate::runbook::classify_step_outcome(step.consequential, out, is_error)
                    })
                }
            }

            let live = LiveRunbookRouter {
                memory,
                allowed: agent.tools.clone(),
                namespace: agent.namespace.clone(),
            };
            let report = crate::runbook::run(&rb, &reg, &live).await;
            crate::runbook::emit_run(&report);

            if report.refused_unsound {
                let errs = crate::runbook::check(&rb, &reg)
                    .iter()
                    .filter(|d| d.severity == crate::runbook::Severity::Error)
                    .count();
                return format!(
                    "Runbook \"{}\" didn't type-check, sir — I refused to run it ({errs} \
                     error{}). Say \"plan the runbook {}\" and I'll show you what's wrong.",
                    rb.name,
                    if errs == 1 { "" } else { "s" },
                    rb.name,
                );
            }
            let count = |o: crate::runbook::RunOutcome| {
                report.steps.iter().filter(|s| s.outcome == o).count()
            };
            let parked = count(crate::runbook::RunOutcome::Parked);
            let mut out = format!(
                "Ran runbook \"{}\" ({} step{}), sir: {} done, {} parked for confirm, {} \
                 refused, {} blocked.",
                report.name,
                report.steps.len(),
                if report.steps.len() == 1 { "" } else { "s" },
                count(crate::runbook::RunOutcome::Done),
                parked,
                count(crate::runbook::RunOutcome::Refused),
                count(crate::runbook::RunOutcome::Blocked),
            );
            if parked > 0 {
                // Honest about the single-slot pending: only the most recent parked step
                // is awaiting your "yes" — each consequential step re-gates on its own.
                out.push_str(
                    " The most recent parked step is awaiting your \"yes\"; each \
                     consequential step re-gates fresh — none was pre-approved.",
                );
            }
            out
        }
    }
}

/// Handle a non-replay MACRO control command (#27): start/stop recording, list, or
/// forget. Gated by [macros].enabled, which SHIPS ON (full-power default; replay
/// re-gates each step) — this line said "OFF by default" long after the flag
/// flipped, which is the same stale sentence macros.rs already had to correct in
/// its own header. With the switch explicitly off every verb
/// reports the subsystem is off and changes NOTHING. Recording captures only the
/// utterance + intent (redacted at persist time), so a secret is never stored, and
/// it never changes a gate. Emits HUD telemetry. (Replay is driven by the turn loop
/// so it can re-route each step through the full gate-honoring pipeline.)
async fn handle_macro_command(
    cmd: crate::macros::MacroCommand,
    cfg: &Config,
    memory: &Memory,
) -> String {
    use crate::macros::MacroCommand;
    if !cfg.macros.enabled {
        telemetry::emit("system", "macro.blocked", json!({"reason": "disabled"}));
        // Make sure no stray recording lingers if the flag was turned off mid-session.
        crate::macros::clear_recording();
        return "Macros are off ([macros].enabled = false), sir — I'm not recording or replaying anything."
            .to_string();
    }
    match cmd {
        MacroCommand::StartRecording { name } => {
            crate::macros::start_recording(&name);
            telemetry::emit("system", "macro.recording_started", json!({"name": name}));
            format!(
                "Recording macro \"{name}\", sir. Carry on with your commands — they'll still run normally; \
                 say 'stop recording' when you're done."
            )
        }
        MacroCommand::StopRecording => {
            let Some((name, steps)) = crate::macros::stop_recording() else {
                return "I wasn't recording a macro, sir.".to_string();
            };
            if steps.is_empty() {
                return format!("Stopped recording — \"{name}\" had no commands, so I saved nothing.");
            }
            match crate::macros::record(
                memory,
                cfg.macros.retention,
                cfg.macros.max_steps,
                &name,
                &steps,
            )
            .await
            {
                Ok(m) => {
                    telemetry::emit(
                        "system",
                        "macro.recorded",
                        json!({"name": m.name, "steps": m.steps.len()}),
                    );
                    format!(
                        "Saved macro \"{}\" with {} step{}. Say 'replay macro {}' to run it — each step \
                         re-runs fresh, and any consequential one still asks first.",
                        m.name,
                        m.steps.len(),
                        if m.steps.len() == 1 { "" } else { "s" },
                        m.name,
                    )
                }
                Err(e) => format!("I couldn't save that macro: {e}"),
            }
        }
        MacroCommand::List => {
            let macros = match crate::macros::list(memory).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = %e, "macro list failed");
                    return "I couldn't read your macros just now, sir.".to_string();
                }
            };
            if macros.is_empty() {
                return "You have no saved macros, sir.".to_string();
            }
            let mut out = String::from("Your macros:\n");
            for m in &macros {
                out.push_str(&format!("- \"{}\" ({} steps)\n", m.name, m.steps.len()));
            }
            out.push_str("Replay one with 'replay macro <name>'; each step re-runs through the gate fresh.");
            out
        }
        MacroCommand::Forget { name } => match crate::macros::forget(memory, &name).await {
            Ok(true) => {
                telemetry::emit("system", "macro.forgotten", json!({"name": name}));
                format!("Forgot macro \"{name}\", sir.")
            }
            Ok(false) => format!("I have no macro called \"{name}\" to forget."),
            Err(e) => format!("I couldn't forget that macro: {e}"),
        },
        // Replay is handled by the turn loop (it re-classifies + re-routes each
        // step). Reaching here would be a logic error; report honestly rather than
        // silently doing nothing.
        MacroCommand::Replay { .. } => {
            "Replay is handled live, sir — say it again and I'll run it.".to_string()
        }
    }
}

/// ONE-WORD UNDO (F2). Status answers from the journal and runs nothing. UndoLast
/// prepares the LAST executed action's inverse (never silently an older one) and
/// hands it to `anthropic::execute_tool` — the same entry point a live tool call
/// uses — under the SAME agent + allowlist snapshot that executed the forward
/// action. A gated inverse therefore parks for its own fresh spoken "confirm"; a
/// reversible-by-design inverse (standing_cancel) runs directly, exactly as if
/// spoken. Whether the park actually happened is read back from the pending slot
/// (never assumed), and a direct execution is only claimed as "Undone" when the
/// inverse is ungated or the master switch was on — a master-off dry-run preview
/// is relayed as the preview it is.
async fn handle_undo_command(cmd: crate::journal::UndoCommand, memory: &Memory) -> String {
    use crate::journal::{UndoCommand, UndoPrep};
    match cmd {
        UndoCommand::Status => crate::journal::status_line(),
        UndoCommand::UndoLast => match crate::journal::prepare_undo() {
            UndoPrep::Nothing => {
                "Nothing consequential has executed this session, so there's nothing to undo."
                    .to_string()
            }
            UndoPrep::AlreadyUndone => {
                "The last consequential action was already undone.".to_string()
            }
            UndoPrep::Irreversible { why } => {
                format!("I can't undo the last action — {why}.")
            }
            UndoPrep::Ready { seq, agent, tool, input, allowed, note, pending_id } => {
                telemetry::emit(
                    "system",
                    "undo.armed",
                    json!({"tool": tool, "agent": agent, "seq": seq}),
                );
                let gate_before = crate::integrations::consequential_allowed();
                // "undo that" is a DIRECT, user-present interactive command, so it is
                // user_originated=true AND context_trusted=true — the derived inverse
                // is treated exactly like a live utterance's tool call.
                // The undo path reads the PARKED slot back below to decide what to
                // say, so it does not need the reported effect — but the chokepoint
                // requires somewhere to report it.
                let mut effect_scratch = anthropic::ToolEffect::DryRun;
                let (outcome, is_error) = anthropic::execute_tool(
                    &tool, &input, memory, &allowed, &agent, true, true, &mut effect_scratch,
                )
                .await;
                // Read back whether the inverse is now the parked confirmation —
                // never assumed from the outcome text.
                let parked = crate::confirm::peek_pending(Instant::now())
                    .is_some_and(|p| p.id == pending_id);
                // "It ran" requires: no transport error, nothing parked, the
                // master gate on across the call for gated tools (both-sides
                // read — a racing flip degrades to relaying the outcome without
                // an undo claim), AND the outcome text confirming the inverse
                // took effect (standing_cancel reports a miss/failure as
                // friendly Ok prose — never claim "Undone." over a miss).
                let executed_directly = !is_error
                    && !parked
                    && (!crate::journal::master_gated(&tool)
                        || (gate_before && crate::integrations::consequential_allowed()))
                    && crate::journal::inverse_confirmed(&tool, &outcome);
                if executed_directly && !crate::journal::master_gated(&tool) {
                    // An ungated reversible inverse ran immediately (a gated one
                    // is journaled + marked by the replay chokepoint instead).
                    crate::journal::mark_undone(seq);
                }
                crate::journal::compose_undo_response(
                    &outcome,
                    is_error,
                    parked,
                    executed_directly,
                    &note,
                )
            }
        },
    }
}

/// COMPOSE-MUSIC VOICE INTENT (Phase-2 flagship "compose an 8-bit happy
/// birthday"). Returns the extracted song PROMPT when the utterance is an
/// explicit request to CREATE music, else None.
///
/// CONSERVATIVELY ANCHORED so it never trips on ordinary speech. A match needs
/// BOTH a music-CREATION verb AND a musical anchor:
///   * `compose` is inherently musical → it alone anchors (the flagship
///     "compose an 8-bit happy birthday" carries no "song" noun).
///   * the broader verbs `make` / `write` / `generate` / `produce` and the
///     phrasings `play me` / `make me` REQUIRE an explicit music OBJECT noun
///     (song / track / tune / beat / jingle / melody / riff) so "make me a
///     sandwich" and "write me an email" are NOT music.
///     "play some jazz" (no creation verb) and "what's the time" therefore return
///     None — only an explicit creation request routes to Jerome.
///
/// The returned String is the cleaned PROMPT: the verb/object/filler stripped
/// from the front and an "about/of" tail unwrapped, so "compose a song about
/// the rain" → "the rain" and "compose an 8-bit happy birthday" → "an 8-bit
/// happy birthday". An empty residue (e.g. a bare "compose a song") falls back
/// to a generic prompt so the op still has something to compose. Pure +
/// unit-tested.
pub fn classify_music_intent(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let lower = lower.trim();

    const OBJECTS: &[&str] = &["song", "track", "tune", "beat", "jingle", "melody", "riff"];
    // WHAT WENT WRONG: the verb side was already word-anchored but the OBJECT side
    // was a bare `contains`, so "tracking", "track record", "heartbeat", "beaten",
    // "fortune" and "tuner" all counted as a music object. With `[voice]
    // .cloud_music` shipping true, "write down my tracking number" therefore never
    // reached memory.store: route() spawned a JEROME composition on the prompt
    // "down my tracking number" and replied "Composing your track now, sir" — the
    // user's note simply lost, with no error. Same for "make a note about the
    // heartbeat monitor" and "write down the tuner settings". The same
    // misclassification also made `guest_denied_fast_path` refuse those turns as
    // "music generation". Whole-word matching, the same primitive the destructive-
    // verb classifiers use.
    //
    // Whole words are still not enough for a fixed compound in which the music
    // noun is the MODIFIER — "my track record" contains the standalone word
    // "track". Those are a short, closed list, so blank them before the scan
    // rather than widening the rule.
    const OBJECT_COMPOUNDS: &[&str] = &["track record", "track records", "beat cop"];
    let mut scan = lower.to_string();
    for c in OBJECT_COMPOUNDS {
        if scan.contains(c) {
            scan = scan.replace(c, " ");
        }
    }
    // MEASURED RECALL MISS: "generate some background music" reached nothing —
    // the object list held every word for a PIECE of music (song/track/tune/…)
    // and not the word "music" itself.
    //
    // THE BARE WORD "music" IS NOT ADDED, and that is the whole design of this
    // fix. "music" is an ordinary modifier ("the music festival", "my music
    // teacher", "some music recommendations"), and the broad verbs here are
    // make/write/generate/produce — so a bare "music" object would put "write
    // down some music recommendations" and "make a note about the music
    // festival" straight back into the composer, which is the note-losing class
    // the OBJECT_COMPOUNDS comment above is about. Only phrases in which "music"
    // is the HEAD NOUN of a thing to be composed are admitted, as whole phrases.
    const OBJECT_PHRASES: &[&str] = &[
        "background music",
        "instrumental music",
        "ambient music",
        "lo-fi music",
        "lofi music",
        "music track",
        "piece of music",
        "bit of music",
    ];
    // ...and a MEMO request is never a composition request. "make a note about
    // the background music in that film" carries a broad verb AND an object
    // phrase; without this veto it would be composed instead of written down —
    // the exact note-losing failure the OBJECT_COMPOUNDS comment above records.
    // The veto applies ONLY to the new phrase path, so the proven OBJECTS path
    // behaves exactly as it did.
    const MEMO_PHRASES: &[&str] =
        &["make a note", "take a note", "write down", "jot down", "note about", "remind me"];
    let is_memo = MEMO_PHRASES.iter().any(|p| lower.contains(p));
    let has_object = crate::utterance::mentions_any_word(&scan, OBJECTS)
        || (!is_memo && OBJECT_PHRASES.iter().any(|p| scan.contains(p)));

    // The creation verb must appear as a leading/standalone word, not buried in a
    // longer token. `compose` anchors on its own (inherently musical); the broader
    // verbs need a music object noun present so non-music "make/write/generate"
    // requests are excluded.
    let has_word = |w: &str| {
        lower == w
            || lower.starts_with(&format!("{w} "))
            || lower.contains(&format!(" {w} "))
    };

    let compose_verb = has_word("compose");
    let broad_verb = ["make", "write", "generate", "produce"]
        .iter()
        .any(|v| has_word(v));
    // "play me a tune" / "play me a beat" is a creation-ish ask ONLY with an object;
    // a bare "play some jazz" (no object noun, no creation verb) must NOT match.
    let play_me = lower.contains("play me");

    // ...AND A CURATION REQUEST IS NOT A COMPOSITION REQUEST.
    //
    // THIS BRANCH EGRESSES. With `[voice].cloud_music` shipping true, reaching it
    // POSTs the owner's sentence to ElevenLabs and spends generation credit; the
    // request cannot be un-sent. MEASURED at HEAD: "generate a playlist of ambient
    // music for the drive" composed a piece of music. The owner asked DARWIN to
    // ASSEMBLE EXISTING TRACKS — which it cannot do, and which the router already
    // refuses on purpose ("play some lo-fi music" is a recorded NO-GO, not a
    // phrase gap: the only music capability is COMPOSE). So the utterance was
    // answered by doing a different thing AND paying a third party to do it.
    //
    // A playlist / mixtape / queue names a SET of pre-existing works. Composing
    // one continuous track and calling it that is the wrong answer whatever the
    // verb is, so this veto is checked against the WHOLE classifier — including
    // the `compose` verb, which otherwise anchors on its own: "compose a playlist
    // for the drive" is the same ask worded differently.
    const CURATION_PHRASES: &[&str] =
        &["playlist", "play list", "mixtape", "mix tape", "queue up", "list of"];
    if CURATION_PHRASES.iter().any(|p| lower.contains(p)) {
        return None;
    }
    let is_music = compose_verb || ((broad_verb || play_me) && has_object);
    if !is_music {
        return None;
    }

    Some(extract_music_prompt(lower))
}

/// Strip the creation verb / object / leading filler from a matched music
/// utterance and unwrap an "about/of" tail, yielding the song PROMPT. A bare
/// request with nothing left to describe falls back to a generic prompt so the
/// op always has a non-empty thing to compose. Pure helper for
/// [`classify_music_intent`].
fn extract_music_prompt(lower: &str) -> String {
    let mut s = lower.to_string();

    // Drop a leading polite/address preamble so "darwin, compose ..." reduces to
    // the request before we strip the verb.
    for prefix in ["darwin", "hey darwin", "ok darwin", "please"] {
        let p = format!("{prefix},");
        if let Some(rest) = s.strip_prefix(&p) {
            s = rest.trim().to_string();
        }
        if let Some(rest) = s.strip_prefix(&format!("{prefix} ")) {
            s = rest.trim().to_string();
        }
    }

    // Strip the leading creation verb (+ a "me" indirect object).
    for verb in ["compose", "make", "write", "generate", "produce", "play"] {
        for lead in [format!("{verb} me "), format!("{verb} ")] {
            if let Some(rest) = s.strip_prefix(&lead) {
                s = rest.trim().to_string();
                break;
            }
        }
    }

    // Drop a leading article.
    for art in ["a ", "an ", "the ", "some "] {
        if let Some(rest) = s.strip_prefix(art) {
            s = rest.trim().to_string();
            break;
        }
    }

    // Strip a leading music object noun (+ trailing article), so
    // "song about the rain" -> "about the rain".
    const OBJECTS: &[&str] = &["song", "track", "tune", "beat", "jingle", "melody", "riff"];
    for obj in OBJECTS {
        // Exact match: the residual IS just the object noun ("compose a song").
        if s == *obj {
            s = String::new();
            break;
        }
        // Otherwise strip only a BOUNDARY-anchored lead ("song ..."), never a bare
        // prefix: a bare `obj` prefix strips the object noun even when it is merely
        // the start of a longer word ("beatles" -> "les"), corrupting the prompt.
        if let Some(rest) = s.strip_prefix(&format!("{obj} ")) {
            s = rest.trim().to_string();
            break;
        }
    }

    // Unwrap an "about/of" tail: "... about the rain" -> "the rain".
    for joiner in ["about ", "of ", "for ", "that goes "] {
        if let Some(idx) = s.find(joiner) {
            s = s[idx + joiner.len()..].trim().to_string();
            break;
        }
    }

    let s = s.trim().trim_end_matches(['.', '!', '?']).trim();
    if s.is_empty() {
        // A bare "compose a song" — nothing described; give the op a usable prompt
        // rather than an empty one.
        "a short, pleasant instrumental piece".to_string()
    } else {
        s.to_string()
    }
}

/// Announce the handling agent so the HUD highlights it in the roster, shifts
/// the core color to its hue, and shows the active-agent chip. Emitted at
/// final selection (after any allowlist re-route) so the HUD always tracks the
/// agent that actually acts.
fn emit_agent_active(agent: &Agent) {
    telemetry::emit(
        "local",
        "agent.active",
        json!({"name": agent.name, "role": agent.role, "hue": agent.hue}),
    );
}

/// Resolve the agent that parked a confirmation from its memory namespace
/// ("agent.<name>") back to a live registry entry, for the HUD highlight and the
/// reply's voice/bookkeeping. Falls back to the orchestrator if the namespace no
/// longer maps to a roster agent (defensive — the parked action still replays).
fn agent_for_namespace<'a>(agents: &'a AgentRegistry, namespace: &str) -> &'a Agent {
    let name = namespace.strip_prefix("agent.").unwrap_or(namespace);
    agents.get(name).unwrap_or_else(|| agents.orchestrator())
}

/// A short, spoken-friendly phrase for a consequential tool, used only in the
/// "Cancelled. I won't <phrase>." acknowledgement. Generic fallback keeps it
/// honest for any tool not individually named.
fn action_phrase(tool: &str) -> &'static str {
    match tool {
        "gmail_send" => "send that email",
        "slack_post_message" => "post that Slack message",
        "x_post" => "post that tweet",
        "linkedin_post" => "publish that LinkedIn post",
        "github_open_pr" => "open that pull request",
        "github_comment_issue" => "post that comment",
        "gcal_create_event" => "create that event",
        "gdrive_upload_text" => "upload that file",
        "dume_control" => "make that change",
        "gads_pause_campaign" | "meta_pause_campaign" => "pause that campaign",
        "gads_enable_campaign" | "meta_resume_campaign" => "resume that campaign",
        "gads_set_budget" | "meta_set_budget" => "change that budget",
        _ => "go ahead with that",
    }
}

/// Enforce the active agent's tool allowlist for a local intent. The intent is
/// the tool name; if the agent may use it, it stays. Otherwise the turn is
/// handed to the tool's real owner (or the orchestrator when only it holds the
/// tool) and the new agent is announced — isolation: no agent ever acts
/// through another agent's exclusive tool. Returns the agent that will act.
fn enforce_tool<'a>(agents: &'a AgentRegistry, agent: &'a Agent, intent: &str) -> &'a Agent {
    if agent.may_use(intent) {
        return agent;
    }
    let owner = agents.owner_of(intent).unwrap_or_else(|| agents.orchestrator());
    info!(
        from = %agent.name,
        to = %owner.name,
        tool = intent,
        "tool outside agent allowlist; re-routing to the owning agent"
    );
    telemetry::emit(
        "local",
        "agent.reroute",
        json!({"from": agent.name, "to": owner.name, "tool": intent}),
    );
    emit_agent_active(owner);
    owner
}

/// Facts visible to one agent: its own namespace plus shared facts, meta.*
/// filtered. Failures degrade to an empty list (a busy DB must never kill a
/// reply) — same policy fetch_history uses for history.
async fn agent_facts(memory: &Memory, namespace: &str) -> Vec<(String, String)> {
    // THRESHOLD — GUEST MODE recall WITHHOLDING (WIRING POINT 2): a GUEST turn feeds
    // NO owner memory to the LOCAL prompt at all. The whole store is the owner's
    // personal data (the "shared" tier still holds the owner's user.* rows), so a
    // bystander reads none of it. Return an empty feed (fail-closed). Owner path:
    // byte-for-byte today's.
    if crate::threshold::is_guest_turn() {
        return Vec::new();
    }
    memory
        .agent_scoped_facts(namespace, FACTS_LIMIT)
        .await
        .unwrap_or_else(|e| {
            warn!(error = %e, namespace, "failed to load namespaced facts for prompt");
            Vec::new()
        })
}

/// Phrase a reply in persona via the local LLM, fed with recent exchanges,
/// the active agent's namespaced facts, and the handler's verified data. If
/// the inference server is down, speak the raw data itself — degraded but
/// honest, never canned personality and never silence. The generate op has no
/// per-agent persona override (only converse does), so this fallback speaks in
/// the base persona; recall is still namespaced so an agent never sees another
/// agent's private facts even on the degrade path.
///
/// `local_model` is the multi-resident LOCAL sub-choice (task #17): when the
/// converse path that fell back here had selected a warm local model, the same
/// model answers the degraded generate (so the fallback stays on the same brain).
/// `None` (the single-resident default + the cloud-degrade path) -> the base.
async fn generate_in_persona(
    text: &str,
    data: &str,
    memory: &Memory,
    infer: &mut InferenceClient,
    agent: &Agent,
    local_model: Option<&str>,
) -> String {
    let facts_kv = agent_facts(memory, &agent.namespace).await;
    let facts: Vec<String> = facts_kv
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect();
    let history = fetch_history(memory).await;
    let data_opt = (!data.is_empty()).then_some(data);
    match infer
        .generate(text, GENERATE_MAX_TOKENS, &history, &facts, data_opt, local_model)
        .await
    {
        Ok(reply) => reply,
        Err(e) => {
            // error!: total local-LLM loss — exactly what self-heal watches.
            error!(error = %e, "local generate unavailable; falling back to raw data");
            telemetry::emit(
                "system",
                "inference.unavailable",
                json!({"op": "generate", "error": e.to_string()}),
            );
            if data.is_empty() {
                // Nothing factual to fall back on (conversation intent):
                // state the system condition rather than staying mute.
                "The local language model is not responding.".to_string()
            } else {
                data.to_string()
            }
        }
    }
}

async fn fetch_history(memory: &Memory) -> Vec<(String, String)> {
    // THRESHOLD — GUEST MODE: a GUEST turn's prompt carries NO conversation history.
    // The recent exchanges are the OWNER's private dialogue (from before the mic was
    // handed over); feeding them would let a bystander's turn be answered with — and
    // echo — the owner's prior conversation. Return an empty history (fail-closed).
    // Owner path: byte-for-byte today's.
    if crate::threshold::is_guest_turn() {
        return Vec::new();
    }
    memory
        .recent_exchanges(HISTORY_EXCHANGES)
        .await
        .unwrap_or_else(|e| {
            warn!(error = %e, "failed to load history for prompt");
            Vec::new()
        })
}

/// The most recent up-to-`n` DARWIN replies from oldest-first history, for the
/// cloud conversation anti-repeat `avoid` list. Pulls the DARWIN side of each
/// exchange, drops blanks, and keeps the last `n` (the freshest) — exactly the
/// wording a repeated greeting would otherwise echo. Pure, so the selection is
/// unit-testable. Empty history yields an empty list (the prompt is then left
/// untouched, which is correct for a first turn).
fn recent_replies(history: &[(String, String)], n: usize) -> Vec<String> {
    history
        .iter()
        .filter_map(|(_, darwin)| {
            let r = darwin.trim();
            (!r.is_empty()).then(|| r.to_string())
        })
        .rev()
        .take(n)
        .collect()
}

/// Local handlers gather data; they no longer write final prose. Live:
/// app.launch/app.control (open/quit via the fuzzy matcher, plus the
/// web-reroute belt-and-suspenders), web.open/web.search (open_url /
/// Google search via the classifier args), file.op (Spotlight search, plus
/// open-on-single-strong-match), system.query (real sysinfo stats),
/// conversation (context only), memory.store/recall. `args` is the
/// classifier's pass-through args object (Null on old servers). `agent` is the
/// active agent: memory.store writes under its namespace
/// ("<namespace>.note.<content-hash>", one key per distinct note — never a
/// clobbering fixed key) and memory.recall reads its namespaced view (own
/// namespace + shared facts), so each agent's notes stay isolated
/// (constellation namespacing, item 4).
/// THRESHOLD guest-mode fast-path admissibility. Returns `Some(category)` when a
/// GUEST turn's utterance would trigger a route() fast-path handler that reads the
/// owner's personal data or takes a consequential / owner-control action — each of
/// which BYPASSES the tool-loop + recall gates. Returns `None` for a guest-safe
/// turn (plain conversation / translation / non-personal status), which matches
/// none of these anchored classifiers and flows through to the already guest-gated
/// conversational path. ONLY consulted when a guest scope is installed.
///
/// PURE: every check is a side-effect-free classifier. NOTE it uses the pure
/// `policy::classify_policy_command` — NOT `handle_user_policy_text`, which APPLIES
/// the policy write — so testing admissibility never mutates state. Fail-closed:
/// any owner-data / consequential specialized path is refused; only genuinely
/// non-personal turns fall through. New fast paths added to `route()` must be
/// mirrored here.
fn guest_denied_fast_path(text: &str, cfg: &Config) -> Option<&'static str> {
    let now = chrono::Local::now();
    // -- Owner CONTROLS / CONSEQUENTIAL actions --------------------------------
    if crate::policy::classify_policy_command(text).is_some() {
        return Some("policy controls");
    }
    if crate::model_tier::classify_model_swap(text).is_some() {
        return Some("model controls");
    }
    if crate::prosody::parse_whisper_command(text).is_some() {
        return Some("voice-mode controls");
    }
    if crate::vault::classify_vault_command(text).is_some() {
        return Some("vault controls");
    }
    if crate::macros::classify_macro_command(text).is_some() {
        return Some("saved macros");
    }
    // WHAT WENT WRONG: the RUNBOOK arm was the ONE route() fast path missing from
    // this mirror, and it is the only one that EXECUTES TOOLS. `route()` reaches
    // `handle_runbook_command` -> `runbook::run` -> `LiveRunbookRouter::route_step`
    // -> `anthropic::execute_tool` under the ORCHESTRATOR's `["*"]` allowlist, so
    // an unrecognized speaker under an auto-installed guest scope could say "run
    // the runbook morning" and drive the owner's automation DAG — benign steps
    // outright, and "plan the runbook <name>" leaks the owner's runbook structure
    // to a bystander. `macros::classify_macro_command` does NOT cover it (it only
    // strips "run the macro "/"run macro "), and the analogous macro REPLAY path
    // was hand-guarded one level up in main.rs while this one was not. Refused
    // here regardless of `[runbook].enabled` — the gate is deny-by-default and
    // must not depend on a config the guest can't see.
    if crate::runbook::classify_runbook_command(text).is_some() {
        return Some("runbooks");
    }
    if crate::journal::classify_undo_command(text).is_some() {
        return Some("undo history");
    }
    if classify_music_intent(text).is_some() {
        return Some("music generation");
    }
    if generate_image_command(text).is_some() {
        return Some("image generation");
    }
    if silicon_canvas_command(text).is_some() || mark_forge_command(text).is_some() {
        return Some("design tools");
    }
    if nexus_command(text).is_some() {
        return Some("audio tools");
    }
    if vision_command(text).is_some() {
        return Some("vision tools");
    }
    if cfg.artifact.enabled && crate::artifact::classify_peek_intent(text) {
        return Some("artifacts");
    }
    if crate::chart::classify_chart_intent(text).is_some() {
        return Some("charts");
    }
    // -- Owner PERSONAL-DATA readers -------------------------------------------
    if crate::aperture::classify_aperture_intent(text, &now).is_some() {
        return Some("activity timeline");
    }
    if crate::screen_context::classify_screen_context_intent(text).is_some() {
        return Some("screen context");
    }
    if crate::pasteboard::classify_pasteboard_intent(text).is_some() {
        return Some("clipboard");
    }
    if crate::notebook::classify_notebook_intent(text).is_some() {
        return Some("notebooks");
    }
    if crate::report::classify_report_intent(text).is_some() {
        return Some("reports");
    }
    if crate::simulate::extract_hypothetical(text).is_some() {
        return Some("personal simulations");
    }
    if crate::lifelog::classify_lifelog_intent(text).is_some() {
        return Some("lifelog");
    }
    if crate::rewind::classify_rewind_intent(text, now.fixed_offset()).is_some() {
        return Some("session rewind");
    }
    if crate::explain::classify_explain_intent(text).is_some() {
        return Some("decision traces");
    }
    if crate::user_model::classify_mirror_intent(text).is_some() {
        return Some("personal profile");
    }
    if describe_command(text).is_some() {
        return Some("vision describe");
    }
    // The agent ROSTER / roll-call — route() fast paths that expose the owner's
    // configured agent constellation. Not owner-personal data, but the guest
    // allowlist is deny-by-default for EVERY route() fast path, so refuse these too
    // (a guest gets conversation / translation / non-personal status, nothing about
    // the owner's private setup).
    if crate::agents::is_roll_call(text) || crate::agents::is_agent_query(text) {
        return Some("agent roster");
    }
    None
}

async fn handle_local(
    intent: &str,
    args: &serde_json::Value,
    text: &str,
    memory: &Memory,
    app_registry: &Arc<AppRegistry>,
    agent: &Agent,
) -> HandlerOutput {
    // THRESHOLD — GUEST MODE fast-path gate (finding 3). handle_local is the
    // structured-intent FAST PATH — it BYPASSES the tool-loop + recall gates and can
    // READ owner memory (memory.recall), WRITE owner memory (memory.store),
    // launch/control apps (app.launch / app.control), open URLs (web.open), search
    // the web (web.search), touch files (file.op), or (re)build the owner's doc
    // index / knowledge graph. For a GUEST, DENY BY DEFAULT: allow ONLY genuinely
    // non-personal intents — plain conversation (falls through to the guest-gated
    // LLM path) and non-personal machine status — and refuse EVERYTHING else
    // (including any future intent) with an honest message, performing NO read and
    // NO write. On the owner path (no scope installed) this is a no-op and handling
    // is byte-for-byte today's.
    if crate::threshold::is_guest_turn() && !matches!(intent, "conversation" | "system.query") {
        telemetry::emit(
            "local",
            "threshold.local_refused",
            json!({"intent": intent, "agent": agent.name}),
        );
        return HandlerOutput {
            data: format!(
                "I can't do that in guest mode — '{intent}' would read or change the owner's \
                 data or act on their machine, and a guest is limited to conversation, \
                 translation, and non-personal status. The owner can do it."
            ),
            // Spoken verbatim, NOT sent to the LLM — no owner context is assembled.
            llm_voice: false,
        };
    }
    if let Err(e) = memory.record_event("local", intent, text).await {
        warn!(error = %e, "failed to record local intent event");
    }
    telemetry::emit(
        "local",
        "intent.handled",
        json!({"intent": intent, "text": text, "agent": agent.name}),
    );

    let data = match intent {
        "app.launch" | "app.control" => handle_app_intent(intent, text, args, app_registry).await,
        "web.open" => handle_web_open(text, args).await,
        "web.search" => handle_web_search(text, args).await,
        "file.op" => handle_file_intent(text).await,
        "docsearch.index" => handle_docsearch_index().await,
        "docsearch.forget" => handle_docsearch_forget().await,
        "docsearch.build_graph" | "knowledge.build" => {
            handle_build_knowledge_graph(memory).await
        }
        "system.query" => actions::system_status_data().await,
        "memory.store" => {
            // Namespaced, CONTENT-KEYED note (e.g. "agent.pepper.note.3fa9…"):
            // one key per distinct note text, so storing a second note never
            // silently CLOBBERS the first (the old fixed "<ns>.note" key kept
            // only the latest note), while re-storing identical text stays a
            // no-growth upsert. Recall is prefix-scoped (agent_scoped_facts
            // LIKE '<ns>.%'), so suffixed keys surface unchanged.
            // upsert_user_fact keeps the meta.* guard in front of every
            // model/agent-driven write.
            let suffix = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                text.hash(&mut h);
                h.finish()
            };
            let key = format!("{}.note.{suffix:016x}", agent.namespace);
            match memory.upsert_user_fact(&key, text).await {
                Ok(()) => format!("Stored fact: {text}"),
                Err(e) => {
                    warn!(error = %e, "failed to store fact");
                    format!("Failed to store the fact (database error: {e})")
                }
            }
        }
        "memory.recall" => match memory.agent_scoped_facts(&agent.namespace, 50).await {
            Ok(facts) => {
                if facts.is_empty() {
                    "No facts stored yet".to_string()
                } else {
                    let lines: Vec<String> =
                        facts.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                    format!("Stored facts:\n{}", lines.join("\n"))
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to recall facts");
                format!("Failed to read stored facts (database error: {e})")
            }
        },
        "conversation" => String::new(),
        other => {
            info!(intent = other, text, "unknown intent; no local handler");
            format!("No local handler exists for intent '{other}'")
        }
    };
    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// What an app.launch/app.control utterance actually asks for. Decided
/// before any process is spawned, so the inverse-of-command bug ("quit
/// Safari" launching Safari) and the dead web bug ("open apple.com" opening
/// nothing) cannot recur regardless of classifier output.
#[derive(Debug, PartialEq)]
enum AppRequest {
    Launch,
    Quit,
    Web,
}

/// Pure decision: quit-class verbs first (a quit must NEVER feed the
/// launcher — audit fix), then the app.launch->web reroute (belt and
/// suspenders against the classifier missing web.open), else launch. The
/// web probe is the extracted remainder, or the whole utterance when no
/// trigger verb was found (the launcher would fall back to the whole
/// utterance too).
fn classify_app_request(intent: &str, text: &str, extracted: &str) -> AppRequest {
    if wants_quit(text) {
        return AppRequest::Quit;
    }
    let probe = if extracted.is_empty() { text } else { extracted };
    if intent == "app.launch" && suggests_web(probe) {
        return AppRequest::Web;
    }
    AppRequest::Launch
}

/// Quit-class verbs anywhere in the utterance.
fn wants_quit(text: &str) -> bool {
    split_words(text)
        .iter()
        .any(|w| matches!(w.as_str(), "quit" | "close" | "exit" | "stop" | "kill"))
}

/// Web markers in an app-launch remainder: website/web/site as words, or a
/// .com/.org/http fragment inside any token ("apple.com", "https://x.org").
fn suggests_web(remainder: &str) -> bool {
    split_words(remainder).iter().any(|w| {
        matches!(w.as_str(), "website" | "web" | "site")
            || w.contains(".com")
            || w.contains(".org")
            || w.contains("http")
    })
}

/// app.launch/app.control: extract the app name from the utterance (words
/// after a trigger verb minus stopwords), decide what kind of request this
/// is, and dispatch. A registered MICRO-APP is resolved FIRST (before the
/// macOS launcher): "open global scan" starts the global-scan micro-app,
/// "close global scan" stops it. Otherwise launch/quit hand the name to the
/// fuzzy macOS-app matcher with the whole utterance as fallback; web requests
/// reroute to the web.open handler. Outcomes become converse data.
async fn handle_app_intent(
    intent: &str,
    text: &str,
    args: &serde_json::Value,
    app_registry: &Arc<AppRegistry>,
) -> String {
    let extracted = extract_app_name(text);
    let request = classify_app_request(intent, text, &extracted);

    // Micro-app resolution comes BEFORE the macOS open/quit path. The probe is
    // the extracted remainder; only Launch/Quit can target a micro-app (a Web
    // request is never an app name). A miss falls straight through to the
    // existing macOS launcher — micro-apps never shadow a real application.
    if matches!(request, AppRequest::Launch | AppRequest::Quit) && !extracted.is_empty() {
        if let Some(app) = app_registry.resolve_name(&extracted).await {
            return match request {
                AppRequest::Launch => match apps::start(app_registry, &app).await {
                    Ok(()) => {
                        info!(app = %app, "micro-app launch requested");
                        telemetry::emit(
                            "system",
                            "action.executed",
                            json!({"tool": "start_app", "outcome": format!("Starting the {app} panel.")}),
                        );
                        format!("Bringing up the {app} panel now, sir.")
                    }
                    Err(e) => {
                        warn!(app = %app, error = %e, "micro-app launch failed");
                        format!("The {app} panel could not be started: {e}")
                    }
                },
                AppRequest::Quit => match apps::stop(app_registry, &app).await {
                    Ok(()) => {
                        info!(app = %app, "micro-app stop requested");
                        telemetry::emit(
                            "system",
                            "action.executed",
                            json!({"tool": "stop_app", "outcome": format!("Stopping the {app} panel.")}),
                        );
                        format!("Closing the {app} panel, sir.")
                    }
                    Err(e) => {
                        warn!(app = %app, error = %e, "micro-app stop failed");
                        format!("The {app} panel could not be stopped: {e}")
                    }
                },
                AppRequest::Web => unreachable!("guarded by the matches! above"),
            };
        }
    }

    match request {
        AppRequest::Web => handle_web_open(text, args).await,
        AppRequest::Quit => match actions::quit_app_with_fallback(&extracted, text).await {
            Ok(outcome) => {
                info!(outcome, "app quit completed");
                telemetry::emit(
                    "system",
                    "action.executed",
                    json!({"tool": "quit_app", "outcome": first_chars(&outcome, 120)}),
                );
                outcome
            }
            Err(e) => {
                warn!(error = %e, "app quit failed");
                format!("The app could not be quit: {e}")
            }
        },
        AppRequest::Launch => match actions::open_app_with_fallback(&extracted, text).await {
            Ok(outcome) => {
                info!(outcome, "app action completed");
                telemetry::emit(
                    "system",
                    "action.executed",
                    json!({"tool": "open_app", "outcome": first_chars(&outcome, 120)}),
                );
                outcome
            }
            Err(e) => {
                warn!(error = %e, "app action failed");
                format!("The app could not be opened: {e}")
            }
        },
    }
}

/// Execute a Silicon Canvas voice command: LAUNCH the app, or forward a
/// STRUCTURED op line to the already-running app. Returns the verified outcome
/// as converse data (llm_voice) so the active agent's persona phrases the
/// confirmation, mirroring the app-launch path. The daemon forwards only the
/// op string built by [`silicon_canvas_command`]; it never interprets the op
/// body and the app never parses natural language (SPEC §6).
///
/// An op aimed at a NOT-running Silicon Canvas reports that plainly (apps::
/// send_op errors) rather than silently launching it — launching mid-trace
/// would lose the user's selection, so "trace this net" before "open silicon
/// canvas" should tell the user to open it first.
async fn handle_silicon_canvas(
    cmd: SiliconCanvasCommand,
    app_registry: &Arc<AppRegistry>,
) -> HandlerOutput {
    let data = match cmd {
        SiliconCanvasCommand::Launch => {
            match apps::start(app_registry, SILICON_CANVAS_APP).await {
                Ok(()) => {
                    info!(app = SILICON_CANVAS_APP, "silicon canvas launch requested");
                    telemetry::emit(
                        "system",
                        "action.executed",
                        json!({"tool": "start_app", "outcome": "Starting the Silicon Canvas panel."}),
                    );
                    "Bringing up Silicon Canvas now, sir.".to_string()
                }
                Err(e) => {
                    warn!(app = SILICON_CANVAS_APP, error = %e, "silicon canvas launch failed");
                    format!("Silicon Canvas could not be started: {e}")
                }
            }
        }
        SiliconCanvasCommand::Op(op_line) => {
            match apps::send_op(app_registry, SILICON_CANVAS_APP, &op_line).await {
                Ok(()) => {
                    info!(app = SILICON_CANVAS_APP, op = %op_line, "forwarded silicon canvas op");
                    telemetry::emit(
                        "system",
                        "app.op_forwarded",
                        json!({"name": SILICON_CANVAS_APP, "op": op_line}),
                    );
                    // DELIVERY-HONEST, not completion-honest. `apps::send_op` is
                    // fire-and-forget: it returns Ok the moment the line lands on
                    // the app's UNBOUNDED in-process queue ("send_op can only
                    // report 'queued'"). No app-side result is awaited or
                    // correlated, so an op the app cannot parse — or one queued in
                    // the window before the child dies — is silently dropped. Saying
                    // "Done" there would claim an action DARWIN never verified.
                    "Forwarded that to Silicon Canvas, sir — sent to the app; it doesn't report back, so I can't confirm it ran.".to_string()
                }
                Err(e) => {
                    warn!(app = SILICON_CANVAS_APP, op = %op_line, error = %e, "silicon canvas op forward failed");
                    format!("I couldn't reach Silicon Canvas: {e}. Open it first, sir.")
                }
            }
        }
    };
    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// Execute a Vision voice command: LAUNCH the Vision micro-app, or forward a
/// STRUCTURED op line to the already-running app. Mirrors [`handle_silicon_canvas`]
/// exactly — verified outcome as converse data (llm_voice) so the active agent's
/// persona phrases the confirmation; the daemon forwards only the op string built
/// by [`vision_command`] and never interprets the op body; the app never parses
/// natural language.
///
/// DEFENSIVE-ONLY framing in the spoken confirmations: capture is of the user's
/// OWN devices and is GATED BY macOS TCC (a runtime consent prompt the daemon
/// cannot grant) — so a watch op that the app cannot honor without consent still
/// returns cleanly here; the on-device consent is the app's to request.
///
/// An op aimed at a NOT-running Vision reports that plainly (apps::send_op
/// errors) rather than silently launching it.
async fn handle_vision(cmd: VisionCommand, app_registry: &Arc<AppRegistry>) -> HandlerOutput {
    let data = match cmd {
        VisionCommand::Launch => match apps::start(app_registry, VISION_APP).await {
            Ok(()) => {
                info!(app = VISION_APP, "vision launch requested");
                telemetry::emit(
                    "system",
                    "action.executed",
                    json!({"tool": "start_app", "outcome": "Starting the Vision panel."}),
                );
                "Bringing up Vision now, sir. I'll need your camera or screen consent on-device.".to_string()
            }
            Err(e) => {
                warn!(app = VISION_APP, error = %e, "vision launch failed");
                format!("Vision could not be started: {e}")
            }
        },
        VisionCommand::Op(op_line) => match apps::send_op(app_registry, VISION_APP, &op_line).await {
            Ok(()) => {
                info!(app = VISION_APP, op = %op_line, "forwarded vision op");
                telemetry::emit(
                    "system",
                    "app.op_forwarded",
                    json!({"name": VISION_APP, "op": op_line}),
                );
                // The read.screen op's recognized text arrives ASYNCHRONOUSLY on
                // the vision.screen telemetry event (relayed to the HUD), NEVER in
                // this synchronous reply — so the SENSITIVE on-screen text never
                // rides the persisted response. The spoken acknowledgment is
                // deliberately content-free (no recognized text) and honest about
                // the on-device TCC gate. PRIVACY: the recognized text is kept
                // transient by `is_screen_read` gating in main.rs.
                if op_line.contains("read.screen") {
                    "Reading your screen now, sir — the readout will appear on the Vision panel. I'll need your Screen Recording consent on-device.".to_string()
                } else if op_line.contains("read.handwriting") {
                    // #28: content-free acknowledgment — the recognized handwriting
                    // arrives async on the vision.screen telemetry, never in this
                    // persisted reply. Honest about the TCC device gate + that
                    // recognition quality is device-dependent.
                    "Reading the handwriting now, sir — the transcription will appear on the Vision panel. I'll need your camera consent on-device, and how well it reads depends on the writing.".to_string()
                } else if op_line.contains("scan.document") {
                    // #29: content-free acknowledgment — the scanned page text
                    // arrives async on the vision.screen telemetry. Honest about the
                    // TCC camera gate + that no page means an honest empty (never a
                    // fabricated document).
                    "Scanning the document now, sir — the text will appear on the Vision panel. I'll need your camera consent on-device; if I don't find a page I'll say so rather than guess.".to_string()
                } else {
                    // DELIVERY-HONEST (see handle_silicon_canvas): send_op only
                    // proves the op was QUEUED — no app-side result is awaited —
                    // so "Done" would claim an action DARWIN never verified.
                    "Forwarded that to Vision, sir — sent to the app; it doesn't report back, so I can't confirm it ran.".to_string()
                }
            }
            Err(e) => {
                warn!(app = VISION_APP, op = %op_line, error = %e, "vision op forward failed");
                format!("I couldn't reach Vision: {e}. Open it first, sir.")
            }
        },
    };
    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// Execute a LUMEN (#45) voice command — the screen-narration + hands-free
/// voice-navigation dispatch. Two arms, both READ-ONLY except the actuation, which
/// runs entirely through the UNCHANGED capstone:
///
///   * READ — forward the READ-ONLY Vision `read.screen` locate (the SAME op the
///     OCR read uses; DEVICE-gated by Screen-Recording TCC), then speak a
///     content-free acknowledgment. The recognized control labels arrive
///     ASYNCHRONOUSLY on the `vision.screen` telemetry event (relayed to the HUD,
///     never in this synchronous reply — kept transient by `is_screen_read`); at
///     integration that relay also parses them into Lumen's remembered readout
///     (`lumen::remember_readout`) + narrates them via `lumen::narrate_controls`,
///     so a follow-up "click the third" selects over exactly what was read.
///
///   * ACT — select the ONE named target over the REMEMBERED controls
///     (`lumen::resolve_voice_action`), or REFUSE honestly (a miss / ambiguity /
///     out-of-range / no-location / nothing-read-yet never becomes a wrong click).
///     A resolved target is handed to `anthropic::execute_tool("ui_actuate", …)` —
///     the SAME entry a live tool call uses — under the ui_actuate-OWNING agent's
///     allowlist. The capstone still PARKS it per action for a spoken yes (master
///     switch + voice-id + `!lockdown` + the pure planner); Lumen NEVER actuates,
///     gates, or batches. ONE resolved phrase = ONE request = (after the capstone's
///     own gate) at most ONE actuation. The park prompt / refusal is spoken
///     VERBATIM (llm_voice=false) so the exact "say confirm" wording survives.
///
/// The `actor` is the ui_actuate-owning specialist (re-pinned by the caller); the
/// ACT arm runs execute_tool under ITS allowlist + namespace, exactly like a
/// mission sub-task runs as its owning specialist.
async fn handle_lumen(
    cmd: LumenCommand,
    memory: &Memory,
    app_registry: &Arc<AppRegistry>,
    actor: &Agent,
) -> HandlerOutput {
    match cmd {
        LumenCommand::Read => {
            // READ-ONLY: forward the existing Vision `read.screen` locate (device-
            // gated OCR). The readout is relayed async (HUD) + remembered at
            // integration; here we only forward + acknowledge (content-free).
            let op = op_read_screen(None);
            let data = match apps::send_op(app_registry, VISION_APP, &op).await {
                Ok(()) => {
                    telemetry::emit(
                        "system",
                        "lumen.read",
                        json!({"narrate": crate::lumen::is_narrating()}),
                    );
                    "Reading your screen now, sir — I'll read out the on-screen controls so you can \
                     tell me which to click. I'll need your Screen Recording consent on-device."
                        .to_string()
                }
                Err(e) => {
                    warn!(app = VISION_APP, error = %e, "lumen read.screen forward failed");
                    format!("I couldn't reach Vision to read the screen: {e}. Open it first, sir.")
                }
            };
            HandlerOutput { data, llm_voice: true }
        }
        LumenCommand::Act(phrase) => {
            // Select the ONE target over the REMEMBERED controls (or refuse). No
            // OCR/AX runs here — the list was captured by a prior read.
            let controls = crate::lumen::snapshot_controls();
            let resolved = crate::lumen::resolve_voice_action(&phrase, &controls);
            // SECRET-FREE telemetry (control count + selected + refusal class only).
            telemetry::emit(
                "system",
                "lumen.action",
                crate::lumen::resolved_action_frame(controls.len(), &resolved),
            );
            let data = match resolved {
                Ok(req) => {
                    // Hand the request to the UNCHANGED capstone via the SAME entry a
                    // live tool call uses — it plans + gates + PARKS per action; Lumen
                    // adds nothing to the gate. `confirm` is omitted (never self-set).
                    let input = ui_actuate_input(&req);
                    let (outcome, _is_error) = anthropic::execute_tool(
                        "ui_actuate",
                        &input,
                        memory,
                        &actor.tools,
                        &actor.namespace,
                        true,
                        // context_trusted=true: a live, attended voice actuation
                        // (ui_actuate is NEVER_AUTO_APPROVE regardless, so it parks).
                        true,
                        &mut crate::anthropic::ToolEffect::DryRun,
                    )
                    .await;
                    outcome
                }
                // A miss / ambiguity / out-of-range / no-location / nothing-read-yet
                // is an HONEST spoken refusal — sentence-cased for the verbatim path.
                Err(e) => capitalize_first(&e.reason()),
            };
            // Spoken VERBATIM: the park prompt's exact "say confirm" wording (and the
            // precise refusal) must not be re-paraphrased by the persona converse.
            HandlerOutput { data, llm_voice: false }
        }
    }
}

/// Capitalize the first alphabetic character of a spoken line (the SelectError
/// reasons are authored mid-sentence, but the Lumen ACT arm speaks them VERBATIM,
/// so they lead a sentence here). Pure; leaves everything else byte-identical.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Execute a Nexus voice command: LAUNCH the Nexus micro-app, or forward a
/// STRUCTURED op line to the already-running app. Mirrors [`handle_silicon_canvas`]
/// and [`handle_vision`] exactly — verified outcome as converse data (llm_voice)
/// so the active agent's persona phrases the confirmation; the daemon forwards
/// only the op string built by [`nexus_command`] and never interprets the op
/// body; the app never parses natural language (SPEC §6).
///
/// An op aimed at a NOT-running Nexus reports that plainly (apps::send_op errors)
/// rather than silently launching it — launching mid-session would reset the
/// matrix, so "mute the mic" before "open nexus" should tell the user to open it
/// first. The realtime CoreAudio path itself is DEVICE-GATED and never opened
/// headlessly; forwarding an op is a control-plane message the daemon can always
/// send to a running control plane regardless of whether a device is bound.
async fn handle_nexus(cmd: NexusCommand, app_registry: &Arc<AppRegistry>) -> HandlerOutput {
    let data = match cmd {
        NexusCommand::Launch => match apps::start(app_registry, NEXUS_APP).await {
            Ok(()) => {
                info!(app = NEXUS_APP, "nexus launch requested");
                telemetry::emit(
                    "system",
                    "action.executed",
                    json!({"tool": "start_app", "outcome": "Starting the Nexus panel."}),
                );
                "Bringing up Nexus now, sir.".to_string()
            }
            Err(e) => {
                warn!(app = NEXUS_APP, error = %e, "nexus launch failed");
                format!("Nexus could not be started: {e}")
            }
        },
        NexusCommand::Op(op_line) => match apps::send_op(app_registry, NEXUS_APP, &op_line).await {
            Ok(()) => {
                info!(app = NEXUS_APP, op = %op_line, "forwarded nexus op");
                telemetry::emit(
                    "system",
                    "app.op_forwarded",
                    json!({"name": NEXUS_APP, "op": op_line}),
                );
                // DELIVERY-HONEST (see handle_silicon_canvas): send_op only proves
                // the op was QUEUED — no app-side result is awaited — so "Done"
                // would claim an action DARWIN never verified.
                "Forwarded that to Nexus, sir — sent to the app; it doesn't report back, so I can't confirm it ran.".to_string()
            }
            Err(e) => {
                warn!(app = NEXUS_APP, op = %op_line, error = %e, "nexus op forward failed");
                format!("I couldn't reach Nexus: {e}. Open it first, sir.")
            }
        },
    };
    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// Execute a Mark-Forge voice command: LAUNCH the Mark-Forge micro-app, or
/// forward a STRUCTURED op line to the already-running app. Mirrors
/// [`handle_silicon_canvas`] / [`handle_vision`] / [`handle_nexus`] exactly —
/// verified outcome as converse data (llm_voice) so the active agent's persona
/// phrases the confirmation; the daemon forwards only the op string built by
/// [`mark_forge_command`] and never interprets the op body; the app never parses
/// natural language (SPEC §7).
///
/// An op aimed at a NOT-running Mark-Forge reports that plainly (apps::send_op
/// errors) rather than silently launching it — launching mid-session would wipe
/// the bodies the user spawned, so "drop a box" before "open the physics sandbox"
/// should tell the user to open it first. The engine is CPU/f64 and headless; the
/// R3F render is DEVICE-GATED and never opened here — forwarding an op is a
/// control-plane message the daemon can always send to a running engine.
async fn handle_mark_forge(
    cmd: MarkForgeCommand,
    app_registry: &Arc<AppRegistry>,
) -> HandlerOutput {
    let data = match cmd {
        MarkForgeCommand::Launch => match apps::start(app_registry, MARK_FORGE_APP).await {
            Ok(()) => {
                info!(app = MARK_FORGE_APP, "mark-forge launch requested");
                telemetry::emit(
                    "system",
                    "action.executed",
                    json!({"tool": "start_app", "outcome": "Starting the Mark-Forge panel."}),
                );
                "Bringing up the physics sandbox now, sir.".to_string()
            }
            Err(e) => {
                warn!(app = MARK_FORGE_APP, error = %e, "mark-forge launch failed");
                format!("Mark-Forge could not be started: {e}")
            }
        },
        MarkForgeCommand::Op(op_line) => {
            match apps::send_op(app_registry, MARK_FORGE_APP, &op_line).await {
                Ok(()) => {
                    info!(app = MARK_FORGE_APP, op = %op_line, "forwarded mark-forge op");
                    telemetry::emit(
                        "system",
                        "app.op_forwarded",
                        json!({"name": MARK_FORGE_APP, "op": op_line}),
                    );
                    // DELIVERY-HONEST (see handle_silicon_canvas): send_op only
                    // proves the op was QUEUED — no app-side result is awaited —
                    // so "Done" would claim an action DARWIN never verified.
                    "Forwarded that to the physics sandbox, sir — sent to the app; it doesn't report back, so I can't confirm it ran.".to_string()
                }
                Err(e) => {
                    warn!(app = MARK_FORGE_APP, op = %op_line, error = %e, "mark-forge op forward failed");
                    format!("I couldn't reach the physics sandbox: {e}. Open it first, sir.")
                }
            }
        }
    };
    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// A non-empty trimmed string field of the classifier args object (Null and
/// {} both yield None — old servers and argless intents look identical).
fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// web.open: open args.url when the classifier supplied one; otherwise fall
/// back to a web search over the utterance's content words — the user
/// clearly wanted the web, guessing a domain would be worse.
async fn handle_web_open(text: &str, args: &serde_json::Value) -> String {
    let browser = arg_str(args, "browser");
    let result = match arg_str(args, "url") {
        Some(url) => actions::open_url(url, browser).await,
        None => actions::search_url(&extract_web_query(text), browser).await,
    };
    finish_web_action("open_url", result)
}

/// web.search: args.query, or the utterance's content words when absent.
async fn handle_web_search(text: &str, args: &serde_json::Value) -> String {
    let browser = arg_str(args, "browser");
    let query = match arg_str(args, "query") {
        Some(q) => q.to_string(),
        None => extract_web_query(text),
    };
    finish_web_action("search_url", actions::search_url(&query, browser).await)
}

fn finish_web_action(tool: &str, result: Result<String>) -> String {
    match result {
        Ok(outcome) => {
            info!(outcome, tool, "web action completed");
            telemetry::emit(
                "system",
                "action.executed",
                json!({"tool": tool, "outcome": first_chars(&outcome, 120)}),
            );
            outcome
        }
        Err(e) => {
            warn!(error = %e, tool, "web action failed");
            format!("The web request failed: {e}")
        }
    }
}

/// file.op: Spotlight search on the utterance's content words; the result
/// list is the converse data. If exactly one strong match comes back and the
/// utterance says to open it, open it too.
/// Resolve the daemon's project root the same way the rest of the daemon does
/// (`DARWIN_ROOT` env, else the cwd) — used to locate config/darwin.toml and
/// state/docsearch.db for the on-device file-RAG index trigger.
fn project_root() -> std::path::PathBuf {
    std::env::var("DARWIN_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
}

/// The "index my documents" / "reindex" intent: rebuild the on-device file-RAG
/// index over the EXPLICITLY-ALLOWLISTED `[docsearch].roots`. CONFIG-GATED
/// ([`crate::docsearch::index_documents`] enforces `[docsearch].enabled` AND a
/// non-empty `roots` before touching the disk), so an OFF subsystem or an empty
/// allowlist indexes NOTHING — it never silently scans the disk. The index runs
/// 100% on-device: file contents + embeddings never leave the device, and when the
/// on-device embedder is down the chunks are stored vector-less so search falls
/// back to BM25. Returns an honest status line (or the off/not-configured message).
/// The honest spoken reply for a completed reindex. PURE over the three counts,
/// so the diagnosis it hands the user is unit-testable.
///
/// WHAT WENT WRONG: this was two inline arms — "on-device embeddings" when
/// `embedded_chunks == chunks && chunks > 0`, else "the on-device embedder was
/// unavailable". `DocIndex::reindex` returns WHOLE-STORE counts, so a root with
/// nothing indexable in it yields files=0, chunks=0, embedded_chunks=0: `0 == 0`
/// holds but `chunks > 0` does not, so the EMPTY case fell into the embedder-
/// failure arm. The user was told, specifically and falsely, that the on-device
/// embedder was down — about an embedder that was never even asked — and went off
/// to debug the inference server when the real problem was an allowlist matching
/// no indexable file. In a subsystem whose whole contract is honest status
/// reporting, that is a wrong answer the user acts on.
fn reindex_reply(files: u64, chunks: u64, embedded_chunks: u64) -> String {
    if chunks == 0 {
        return format!(
            "I found nothing to index in your allowlisted folders, sir — {files} file(s), \
             0 chunks. Nothing left the machine. Check `[docsearch].roots`: the folders \
             listed there hold no indexable text (the embedder was never asked, so this \
             is not an embedder problem)."
        );
    }
    let method = if embedded_chunks == chunks {
        "on-device embeddings"
    } else {
        "lexical BM25 (the on-device embedder was unavailable, so search will be keyword-based)"
    };
    format!(
        "Indexed {files} file(s) into {chunks} chunk(s) from your allowlisted folders — \
         all on-device, nothing left the machine. Search will use {method}."
    )
}

async fn handle_docsearch_index() -> String {
    use crate::docsearch::index_documents;
    let root = project_root();
    let (cfg, _issues) = Config::load(&root.join("config").join("darwin.toml"));
    // Honest, actionable copy when the feature isn't set up — never a silent scan.
    if !crate::docsearch::indexing_permitted(cfg.docsearch.enabled, &cfg.docsearch.roots) {
        if !cfg.docsearch.enabled {
            return "On-device file search is off. Enable [docsearch] and add a folder to \
                    index in the config — it ships disabled and indexes only the folders \
                    you allowlist, never your whole disk."
                .to_string();
        }
        return "On-device file search is on, but no folder is allowlisted to index yet. \
                Add a folder under [docsearch].roots — nothing else is ever read."
            .to_string();
    }
    let index = match crate::crypto::open_doc_index(&root.join("state").join("docsearch.db")) {
        Ok(idx) => idx,
        Err(e) => {
            warn!(error = %e, "docsearch: could not open the file index");
            return format!("I couldn't open the file index to reindex: {e}");
        }
    };
    let embedder = anthropic::inference_embedder();
    match index_documents(&cfg.docsearch, &index, &*embedder).await {
        Ok(Some(status)) => {
            telemetry::emit(
                "local",
                "docsearch.indexed",
                json!({
                    "files": status.files,
                    "chunks": status.chunks,
                    "embedded_chunks": status.embedded_chunks,
                }),
            );
            reindex_reply(status.files, status.chunks, status.embedded_chunks)
        }
        Ok(None) => "On-device file search isn't configured to index anything yet.".to_string(),
        Err(e) => {
            warn!(error = %e, "docsearch: reindex failed");
            format!("The file index could not be rebuilt: {e}")
        }
    }
}

/// The "forget my file index" / "clear my indexed files" intent: CLEAR the
/// on-device file-RAG index ([`crate::docsearch::DocIndex::forget`]) so no file
/// chunk or embedding remains — the FORGETTABLE half of the contract. It only
/// ever touches the local `state/docsearch.db` the index/search paths use;
/// nothing else is read, nothing leaves the device. Opening the store creates an
/// empty one, so "forget" with nothing indexed is honestly a no-op ("there was
/// nothing to forget") rather than an error. No config gate is needed: clearing
/// the user's own local index is always safe and never widens any surface.
async fn handle_docsearch_forget() -> String {
    let root = project_root();
    let index = match crate::crypto::open_doc_index(&root.join("state").join("docsearch.db")) {
        Ok(idx) => idx,
        Err(e) => {
            warn!(error = %e, "docsearch: could not open the file index to forget");
            return format!("I couldn't open the file index to clear it: {e}");
        }
    };
    match index.forget().await {
        Ok(0) => "Your on-device file index was already empty, sir — there was nothing to forget."
            .to_string(),
        Ok(removed) => {
            // Mirror the index path's telemetry so the HUD index-status panel
            // reflects the now-empty store (0 files / 0 chunks). Local 127.0.0.1
            // broadcast only — nothing leaves the device.
            telemetry::emit(
                "local",
                "docsearch.indexed",
                json!({"files": 0, "chunks": 0, "embedded_chunks": 0}),
            );
            format!(
                "Done — I've forgotten your indexed files ({removed} chunk(s) cleared). \
                 Nothing of them remains on the device; reindex whenever you'd like to search again."
            )
        }
        Err(e) => {
            warn!(error = %e, "docsearch: forget failed");
            format!("The file index could not be cleared: {e}")
        }
    }
}

/// Serialize a bounded [`crate::world_model::WorldState`] into the HUD-facing
/// `graph` payload of the `knowledge_graph.built` event. Each entity carries its
/// stable type token + id + display name and its `source` PROVENANCE attribute
/// (the only attribute the deterministic build writes; absent for an entity that
/// somehow has none, so the HUD shows the honest "no citation"); each relationship
/// carries the from/relation/to ids + the `source file:offset` detail on the
/// co-occurrence edge. Counts/ids/names/source strings ONLY — no chunk text. The
/// view is already bounded by the world model's read/structure caps; this caps the
/// emitted lists again defensively so one event can never balloon the broadcast.
fn world_snapshot_json(state: &crate::world_model::WorldState) -> serde_json::Value {
    const MAX_EMIT_ENTITIES: usize = 256;
    const MAX_EMIT_RELATIONS: usize = 512;
    let entities: Vec<serde_json::Value> = state
        .entities
        .iter()
        .take(MAX_EMIT_ENTITIES)
        .map(|e| {
            let source = e
                .attributes
                .iter()
                .find(|(a, _)| a == "source")
                .map(|(_, v)| v.clone());
            json!({
                "type": e.entity_type.as_str(),
                "id": e.id,
                "name": e.name,
                "source": source,
            })
        })
        .collect();
    let relationships: Vec<serde_json::Value> = state
        .relationships
        .iter()
        .take(MAX_EMIT_RELATIONS)
        .map(|r| {
            json!({
                "from": r.from,
                "relation": r.relation,
                "to": r.to,
                "source": r.value,
            })
        })
        .collect();
    json!({ "entities": entities, "relationships": relationships })
}

/// The "build/map a knowledge graph from my documents" intent: mine the user's
/// ALREADY-INDEXED docsearch chunks for grounded entities/relationships and upsert
/// them into the SHARED world model. DOUBLE-GATED ([`knowledge_graph::build_permitted`]:
/// `[docsearch].enabled` AND `[docsearch].build_graph`, both ship false) — an OFF
/// subsystem mines NOTHING. It reads only chunks the confined, allowlisted indexer
/// already produced (it never re-walks the disk) and writes only the shared
/// `user.world.*` tier (never an agent's private namespace, never a fabricated
/// node). The shipped extractor is the CONSERVATIVE deterministic heuristic — the
/// copy says so. Returns an honest status line (or the off/not-configured message).
async fn handle_build_knowledge_graph(memory: &Memory) -> String {
    use crate::knowledge_graph::{self, DeterministicExtractor, Extractor};
    let root = project_root();
    let (cfg, _issues) = Config::load(&root.join("config").join("darwin.toml"));
    if !knowledge_graph::build_permitted(cfg.docsearch.enabled, cfg.docsearch.build_graph) {
        if !cfg.docsearch.enabled {
            return "Building a knowledge graph needs on-device file search, which is off. \
                    Enable [docsearch] (and set [docsearch].build_graph = true), then add a \
                    folder to index — it ships disabled and reads only the folders you allowlist."
                .to_string();
        }
        return "On-device file search is on, but the knowledge-graph build is off. \
                Set [docsearch].build_graph = true to let me map your indexed documents \
                into the shared world model — it stays off until you turn it on."
            .to_string();
    }
    let index = match crate::crypto::open_doc_index(&root.join("state").join("docsearch.db")) {
        Ok(idx) => idx,
        Err(e) => {
            warn!(error = %e, "knowledge_graph: could not open the file index");
            return format!("I couldn't open the file index to build the graph: {e}");
        }
    };
    let chunks = match index.chunks_for_graph().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "knowledge_graph: could not read indexed chunks");
            return format!("I couldn't read the indexed chunks to build the graph: {e}");
        }
    };
    if chunks.is_empty() {
        return "There are no indexed documents to map yet. Index your allowlisted \
                folders first, then I can build the knowledge graph from them."
            .to_string();
    }
    // Pick the extractor: the conservative deterministic heuristic (default) OR
    // the OPT-IN LLM-grounded extractor when [docsearch].graph_extractor = "llm".
    // The LLM path connects to the on-device inference server; if it is
    // unreachable at build start we FALL BACK to the deterministic extractor
    // honestly (never a half-wired LLM build). Either way `map_documents`
    // re-checks the gate (defense-in-depth) and the grounding contract holds.
    let det = DeterministicExtractor;
    let llm = if cfg.docsearch.graph_extractor.trim() == "llm" {
        let sock = root.join("state").join("ipc").join("inference.sock");
        match knowledge_graph::LlmExtractor::connect(&sock).await {
            Some(e) => Some(e),
            None => {
                warn!("knowledge_graph: LLM extractor requested but inference server unreachable; using the deterministic extractor");
                None
            }
        }
    } else {
        None
    };
    let extractor: &dyn Extractor = match &llm {
        Some(e) => e,
        None => &det,
    };
    match knowledge_graph::map_documents(
        cfg.docsearch.enabled,
        cfg.docsearch.build_graph,
        memory,
        extractor,
        &chunks,
    )
    .await
    {
        Ok(Some(stats)) => {
            // Read back the bounded SHARED world snapshot so the HUD can render the
            // grouped entities + their provenance + relationships. This is the same
            // structured view `world_query` returns; it is `user.world.*` only (no
            // agent.* private note can appear), reads ONLY counts/ids/names/source
            // strings the build just grounded, and rides the local 127.0.0.1
            // broadcast. A read failure is non-fatal — the build already landed, so
            // emit the stats with an empty graph rather than dropping the event.
            let graph = match crate::world_model::snapshot(memory).await {
                Ok(state) => world_snapshot_json(&state),
                Err(e) => {
                    warn!(error = %e, "knowledge_graph: snapshot read for HUD failed");
                    json!({ "entities": [], "relationships": [] })
                }
            };
            telemetry::emit(
                "local",
                "knowledge_graph.built",
                json!({
                    "chunks_scanned": stats.chunks_scanned,
                    "entities_written": stats.entities_written,
                    "relationships_written": stats.relationships_written,
                    "skipped_at_cap": stats.skipped_at_cap,
                    "graph": graph,
                    "extractor": extractor.method(),
                }),
            );
            let cap_note = if stats.skipped_at_cap > 0 {
                format!(
                    " ({} were skipped because the world model is at its bound — I never grow it past its cap)",
                    stats.skipped_at_cap
                )
            } else {
                String::new()
            };
            let method_note = if extractor.method() == "llm-grounded" {
                "with the on-device LLM extractor, then STRICTLY filtered to only names \
                 that appear verbatim in your documents (anything the model invented was \
                 dropped, and relationships are recorded as honest co-occurrence)"
            } else {
                "with a conservative heuristic (it errs toward missing rather than inventing)"
            };
            format!(
                "Mapped your documents into the shared world model: {} entit(ies) and {} \
                 relationship(s) from {} indexed chunk(s){}. These were extracted from YOUR \
                 documents {} and each is tagged with its source file — nothing ungrounded \
                 was written.",
                stats.entities_written, stats.relationships_written, stats.chunks_scanned, cap_note, method_note
            )
        }
        // Unreachable in practice (the gate was checked above), but the gated entry
        // point can return None when off — keep the off message honest if it does.
        Ok(None) => "The knowledge-graph build is off, so I mapped nothing.".to_string(),
        Err(e) => {
            warn!(error = %e, "knowledge_graph: build failed");
            format!("The knowledge graph could not be built: {e}")
        }
    }
}

async fn handle_file_intent(text: &str) -> String {
    let query = extract_content_words(text);
    if query.is_empty() {
        return "The request did not include anything to search for; ask what file they mean."
            .to_string();
    }
    match actions::search_files_raw(&query, 5).await {
        Ok(hits) => {
            telemetry::emit(
                "system",
                "action.executed",
                json!({"tool": "search_files", "outcome": format!("{} hits for '{query}'", hits.len())}),
            );
            let mut data = actions::format_file_hits(&query, &hits);
            if hits.len() == 1 && utterance_wants_open(text) {
                match actions::open_path(&hits[0].path_str()).await {
                    Ok(opened) => data = format!("{data}\n{opened}"),
                    Err(e) => {
                        warn!(error = %e, "open_path after search failed");
                        data = format!("{data}\nIt could not be opened: {e}");
                    }
                }
            }
            data
        }
        Err(e) => {
            warn!(error = %e, "file search failed");
            format!("The file search failed: {e}")
        }
    }
}

/// Words the matchers should never see — command verbs, fillers, articles.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "any", "app", "application", "can", "close", "could", "do", "exit",
    "find", "for", "go", "hey", "in", "is", "it", "darwin", "kill", "launch", "look",
    "looking", "me", "my", "now", "of", "on", "open", "please", "quit", "search", "show",
    "some", "start", "that", "the", "then", "this", "to", "up", "where", "with", "would",
    "you",
];

/// Extra noise words for web requests: the command vocabulary around what
/// the user actually wants opened or searched.
const WEB_STOPWORDS: &[&str] = &[
    "browser", "google", "internet", "online", "page", "site", "web", "website",
];

/// Extra noise words for file searches (the command vocabulary around the
/// actual content words).
const FILE_STOPWORDS: &[&str] = &[
    "called", "computer", "document", "documents", "file", "files", "folder", "folders",
    "named", "recent",
];

fn split_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '.' && c != '-' && c != '_')
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Simple heuristic: the words after the first trigger verb (open/launch/
/// start plus the quit-class verbs, kept in sync with wants_quit so a quit
/// utterance extracts its app name instead of feeding the launcher), minus
/// stopwords. Empty when no trigger verb is present — the caller then feeds
/// the whole utterance to the fuzzy matcher instead.
fn extract_app_name(text: &str) -> String {
    let words = split_words(text);
    let Some(pos) = words.iter().position(|w| {
        matches!(
            w.as_str(),
            "open" | "launch" | "start" | "quit" | "close" | "exit" | "stop" | "kill"
        )
    }) else {
        return String::new();
    };
    words[pos + 1..]
        .iter()
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Content words of a file request: everything minus the command vocabulary.
fn extract_content_words(text: &str) -> String {
    split_words(text)
        .into_iter()
        .filter(|w| !STOPWORDS.contains(&w.as_str()) && !FILE_STOPWORDS.contains(&w.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Content words of a web request: everything minus the command vocabulary
/// and the web noise words ("search the web for rust tutorials" -> "rust
/// tutorials").
fn extract_web_query(text: &str) -> String {
    split_words(text)
        .into_iter()
        .filter(|w| !STOPWORDS.contains(&w.as_str()) && !WEB_STOPWORDS.contains(&w.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn utterance_wants_open(text: &str) -> bool {
    text.to_lowercase().contains("open")
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ===========================================================================
// Silicon Canvas voice control (SPEC §6 — the daemon forwards STRUCTURED ops
// ONLY; the app never parses natural language).
//
// Voice reaches a micro-app today only via app.launch/app.control -> the fuzzy
// app matcher -> apps::start/stop (launch & quit only). That seam does NOT
// generalize to "select.net"/"trace.start"/"erc.run" — those are ops sent to an
// ALREADY-RUNNING app, for which the host had no forwarding path. The SMALLEST
// honest addition is: (1) apps::send_op forwards one structured op line to a
// running app (apps.rs), and (2) the deterministic NL->op classifier below maps
// the spoken control phrases to those op lines. The op JSON is built here to
// match Silicon Canvas's `apps/silicon-canvas/src/ops.rs` wire form VERBATIM
// (the `#[serde(tag="op")]` dotted names); the daemon never imports that
// standalone crate, so a round-trip test in ops.rs locks the two sides
// together. The classifier is checked BEFORE the normal classifier route (like
// roll-call) so a precise control phrase never lands on the cloud/LLM.
// ===========================================================================

/// The Silicon Canvas micro-app's registered name (its manifest `[app].name`
/// and the key into the app registry / its socket).
pub const SILICON_CANVAS_APP: &str = "silicon-canvas";

/// What a Silicon-Canvas voice command resolves to. Either LAUNCH the app
/// (handled by the existing apps::start path) or forward a STRUCTURED op line
/// to the already-running app (apps::send_op). The daemon never sends anything
/// but these two; the op body is opaque to it.
#[derive(Debug, Clone, PartialEq)]
pub enum SiliconCanvasCommand {
    /// "open silicon canvas" — start the micro-app.
    Launch,
    /// A structured op line to forward verbatim to the running app. The String
    /// is the COMPLETE JSON op object (one line), e.g.
    /// `{"op":"select.net","name":"3V3"}`.
    Op(String),
}

/// Whether the utterance names Silicon Canvas itself ("silicon canvas",
/// "silicon-canvas", "the canvas"). Used to gate the launch phrase and to
/// disambiguate a bare "open" so an unrelated "open safari" is never captured.
fn mentions_silicon_canvas(lower: &str) -> bool {
    lower.contains("silicon canvas")
        || lower.contains("silicon-canvas")
        || lower.contains("siliconcanvas")
        || lower.contains("the schematic")
        || lower.contains("the board view")
}

// ---------------------------------------------------------------------------
// THE GATES. Every op below used to fire on a bare substring, which is how a
// corpus of 1,897 ORDINARY utterances (health, weather, cooking, travel, work,
// chat — not one of them about a circuit board) put 132 turns into this app:
// 80 ran an electrical rule check because "erc" is spelled inside "percent",
// "mercy", "merchant" and "commerce"; 31 entered trace mode because "trace" is
// ordinary English; 20 selected a net named MY / S / IT / MOSQUITO; 1 re-framed
// the viewport because "actually" contains "all". A captured turn never reaches
// conversation (router.rs:1953 is an else-if chain), so every one of those was
// an answer the user did not get.
//
// Three rules run through everything here.
//
// WHOLE-WORD, via the shared `crate::utterance::mentions_word` primitive — a
// substring is not a word.
//
// OBJECT POSITION: whole-word is only half a fix, because "the safety net PLAN"
// and "the whole board OF DIRECTORS" pass it. What makes an utterance a command
// is where the words sit: a speaker names the object of a command LAST
// ("highlight the batt net", "fit the board") while ordinary English keeps
// going. So a keyword has to END its phrase — where "end" means end, or trail
// off into politeness ("... please"), a "for me", or a locus that points at the
// board ("... on my screen", "... in the sandbox"). That last allowance is not
// decoration: "darwin show me the 3v3 net on my screen" is how people actually
// talk to this assistant, and an earlier draft of this fix rejected it.
//
// A CLOSED CONTINUATION SET for trace, which object position alone cannot
// handle because a trace command legitimately continues ("trace the gnd net",
// "advance the trace one step"). What follows the trace word must be PCB
// material — a net/connection/route/pad, or mode/step/segment/forward — and
// then settle. Adjacency of a lifecycle verb is NOT enough on its own, which is
// the specific defect that sank the previous attempt: "please stop the trace on
// my credit report", "resume the trace of the phone call", "i want to begin a
// trace on the missing package", "enter the trace number on the website" and
// "stop tracing the outline and color it in" all have the verb sitting right on
// the word, and not one of them is about a circuit board.
// ---------------------------------------------------------------------------

/// The tokens of an utterance, split exactly the way
/// `crate::utterance::mentions_word` splits: every non-alphanumeric character is
/// a boundary. The gates below need POSITIONS, which no presence test can give.
fn speech_words(lower: &str) -> Vec<&str> {
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect()
}

/// The determiners a speaker slips between a verb and its object ("step THE
/// trace", "fit THE board").
const SPEECH_DETERMINERS: &[&str] = &[
    "the", "a", "an", "this", "that", "these", "those", "my", "your", "our", "their", "its",
];

/// Words that can trail a noun phrase without continuing it, so that "show me
/// the sda net PLEASE" is still a selection.
const SPEECH_TRAILERS: &[&str] = &[
    "please", "now", "sir", "darwin", "again", "thanks", "thank", "ok", "okay", "alright", "first",
];

/// Nouns that say WHERE the board is. A command may point at it after naming
/// its object ("fit the board ON MY SCREEN", "highlight the clk net IN THE
/// SANDBOX") and still be a command. Deliberately a closed list of surfaces:
/// "on my CREDIT REPORT" and "on the SHIPMENT" must not qualify, and they are
/// the exact shapes that made the previous attempt capture ordinary speech.
const SPEECH_LOCUS_NOUNS: &[&str] = &[
    "screen", "display", "monitor", "view", "viewport", "canvas", "board", "schematic", "sandbox",
    "pcb", "layout", "window", "editor",
];

/// Tokens skipped when looking BACK from a noun for its verb. Determiners plus
/// the prepositions a speaker drops in ("next step IN the trace"). Skipping
/// prepositions here is safe ONLY because the continuation test below already
/// rejected "begin TO trace THE PAYMENTS TOMORROW" on its tail; without that
/// test this list would re-open the hole.
const SPEECH_LOOKBACK_SKIP: &[&str] = &[
    "the", "a", "an", "this", "that", "these", "those", "my", "your", "our", "their", "its", "in",
    "on", "of", "to", "at", "along", "through", "with", "up", "over",
];

/// Is everything from `i` onward mere tail — a politeness trailer, a "for me",
/// or a locus phrase that points at the board? This one test separates "select
/// the safety net" from "select the safety net PLAN", "show the whole board"
/// from "show me the whole board GAME COLLECTION", and "stop the trace" from
/// "stop the trace ON MY CREDIT REPORT".
fn settles_from(words: &[&str], mut i: usize) -> bool {
    while i < words.len() {
        let w = words[i];
        if SPEECH_TRAILERS.contains(&w) {
            i += 1;
            continue;
        }
        if w == "for" && matches!(words.get(i + 1).copied(), Some("me" | "us")) {
            i += 2;
            continue;
        }
        if matches!(w, "on" | "in" | "onto" | "upon" | "at" | "over" | "to") {
            let mut j = i + 1;
            while j < words.len() && SPEECH_DETERMINERS.contains(&words[j]) {
                j += 1;
            }
            if j < words.len() && SPEECH_LOCUS_NOUNS.contains(&words[j]) {
                i = j + 1;
                continue;
            }
            return false;
        }
        return false;
    }
    true
}

/// Does the noun phrase END at `idx` (modulo the tail material above)?
fn phrase_settles_at(words: &[&str], idx: usize) -> bool {
    settles_from(words, idx + 1)
}

/// A locus phrase that names the BOARD specifically, not just any surface. Used
/// only by "fit EVERYTHING on the board", where the object is too generic to
/// carry the utterance on its own — "can we fit everything in the car" must
/// stay out.
fn board_locus_from(words: &[&str], i: usize) -> bool {
    if !matches!(words.get(i).copied(), Some("on" | "in" | "onto" | "upon")) {
        return false;
    }
    let mut j = i + 1;
    while j < words.len() && SPEECH_DETERMINERS.contains(&words[j]) {
        j += 1;
    }
    match words.get(j) {
        Some(&("board" | "boards" | "pcb" | "schematic" | "canvas")) => settles_from(words, j + 1),
        _ => false,
    }
}

/// Nouns that can only mean a printed circuit board. Deliberately NOT "board"
/// or "circuit": a board is a plank, a committee, an exam and a game, and a
/// circuit is training, a court and a race track. Deliberately not "copper" or
/// "footprint" either, which an earlier draft included — "the copper pipes are
/// leaking, we should trace them" and "my carbon footprint" are ordinary
/// English, and a board noun BYPASSES the position tests below, so a wrong
/// entry here is expensive. Deliberately not "net": see `selected_net` for how
/// "net" earns its board reading.
fn mentions_board_noun(lower: &str) -> bool {
    mentions_silicon_canvas(lower)
        || crate::utterance::mentions_any_word(
            lower,
            &[
                "pcb", "pcbs", "netlist", "netlists", "silkscreen", "kicad", "gerber", "gerbers",
            ],
        )
}

/// The nouns a PCB trace can be traced ALONG. This is the closed continuation
/// set: after the trace word, either the utterance settles immediately, or the
/// next thing named is one of these.
fn is_trace_object_noun(w: &str) -> bool {
    matches!(
        w,
        "net" | "nets"
            | "trace"
            | "traces"
            | "connection"
            | "connections"
            | "route"
            | "routes"
            | "track"
            | "tracks"
            | "wire"
            | "wires"
            | "signal"
            | "signals"
            | "pad"
            | "pads"
            | "pin"
            | "pins"
            | "path"
            | "paths"
    )
}

/// What kind of thing follows the trace word.
#[derive(PartialEq, Clone, Copy, Debug)]
enum TraceHead {
    /// Nothing, or nothing recognizable ("stop the trace", "... trace of gluten").
    Empty,
    /// A PCB object ("trace THE GND NET", "trace THIS CONNECTION").
    Object,
    /// "trace MODE".
    Mode,
    /// A stepping word ("next trace STEP", "advance the trace ONE STEP").
    Step,
}

/// Parse what follows the trace word at `at`. Returns the index where the tail
/// begins (everything from there must `settles_from`) and what was consumed.
fn trace_head(words: &[&str], at: usize) -> (usize, TraceHead) {
    let mut i = at + 1;
    // "trace ALONG this net" / "trace FROM this pad" — a locus preposition may lead
    // the object. Skipping one keeps the object visible; without this the head
    // reads as the preposition and the whole command is refused.
    if i < words.len() && matches!(words[i], "along" | "from" | "on" | "down" | "through" | "out") {
        i += 1;
    }
    if i >= words.len() {
        return (i, TraceHead::Empty);
    }
    if words[i] == "mode" {
        return (i + 1, TraceHead::Mode);
    }
    if matches!(words[i], "step" | "steps" | "segment" | "segments" | "forward") {
        return (i + 1, TraceHead::Step);
    }
    if words[i] == "next" {
        let mut j = i + 1;
        if j < words.len() && matches!(words[j], "step" | "steps" | "segment" | "segments") {
            j += 1;
        }
        return (j, TraceHead::Step);
    }
    if words[i] == "one" && matches!(words.get(i + 1).copied(), Some("step" | "steps")) {
        return (i + 2, TraceHead::Step);
    }
    // "[determiner] [name] <pcb noun>" — "the net", "this connection",
    // "the gnd net". At most ONE name token, so "the recipe with me" and "the
    // outline and color it in" do not reach a noun.
    let mut j = i;
    if SPEECH_DETERMINERS.contains(&words[j]) {
        j += 1;
    }
    if j < words.len() && is_trace_object_noun(words[j]) {
        return (j + 1, TraceHead::Object);
    }
    if j + 1 < words.len() && is_trace_object_noun(words[j + 1]) {
        return (j + 2, TraceHead::Object);
    }
    (i, TraceHead::Empty)
}

/// The nearest content token before `at`, skipping determiners and the
/// prepositions in `SPEECH_LOOKBACK_SKIP`; None when `at` opens the utterance.
fn trace_verb_before<'a>(words: &[&'a str], at: usize) -> Option<&'a str> {
    let mut i = at;
    while i > 0 {
        i -= 1;
        if !SPEECH_LOOKBACK_SKIP.contains(&words[i]) {
            return Some(words[i]);
        }
    }
    None
}

/// Which trace op the utterance is asking for, or None when "trace" was just
/// ordinary English.
///
/// WHAT WENT WRONG: this replaces three branches that each keyed on
/// `lower.contains("trace")`. The word is a noun ("is there any trace of gluten
/// in this bread") and a verb ("can the bank trace the wire transfer") in
/// everyday speech, it hides inside "retrace" and even "exTRACEllular", and the
/// lifecycle verbs beside it were `contains` too, so "spend" matched "end" and
/// "steps" matched "step". 31 ordinary utterances entered trace mode.
///
/// The rule is CONTINUATION, not adjacency. An adjacency-only draft of this fix
/// was measured and rejected: it still captured "please stop the trace on my
/// credit report", "restart the trace on the shipment", "let's cancel the trace
/// request with the bank" and seven more, because in every one of them a
/// lifecycle verb does sit on the word. What those sentences do NOT do is
/// continue with PCB material. So each occurrence must first pass its tail
/// (`trace_head` + `settles_from`), and only then is the verb read off it.
/// Precedence is the old branch order — step, then stop, then the broad start.
fn trace_command(lower: &str) -> Option<String> {
    let words = speech_words(lower);
    let spoken: Vec<usize> = words
        .iter()
        .enumerate()
        .filter(|(_, w)| matches!(**w, "trace" | "traces" | "tracing"))
        .map(|(i, _)| i)
        .collect();
    if spoken.is_empty() {
        return None;
    }
    // A one-word utterance IS the command: nobody says a bare "trace." in
    // conversation, and that is how the mode gets re-armed mid-session.
    if words.len() == 1 {
        return Some(op_trace_start());
    }
    // The bare imperative "trace it" / "trace this" / "trace that", but ONLY
    // when the trace word opens the utterance and nothing but tail follows.
    // "can they trace it back to me" and "can you trace this charge on my card"
    // fail both halves.
    if spoken[0] == 0
        && matches!(words.get(1).copied(), Some("it" | "this" | "that"))
        && settles_from(&words, 2)
    {
        return Some(op_trace_start());
    }
    // A named board licenses a trace phrase whose tail we cannot parse ("on the
    // schematic, start the trace at the connector"). It does NOT supply an op
    // on its own — the verb or the object still has to be there.
    let board = mentions_board_noun(lower);
    let (mut step, mut stop, mut start) = (false, false, false);
    for at in spoken {
        let (tail, head) = trace_head(&words, at);
        if !(settles_from(&words, tail) || board) {
            continue;
        }
        let before = trace_verb_before(&words, at);
        if head == TraceHead::Step
            || matches!(before, Some("next" | "step" | "advance" | "forward"))
        {
            step = true;
        }
        if matches!(
            before,
            Some("stop" | "end" | "exit" | "cancel" | "quit" | "finish")
        ) {
            stop = true;
        }
        if matches!(
            before,
            Some(
                "start" | "begin" | "resume" | "enter" | "restart" | "reenter" | "keep"
                    | "continue"
            )
        ) || head == TraceHead::Mode
            || head == TraceHead::Object
        {
            start = true;
        }
    }
    if step {
        return Some(op_trace_step());
    }
    if stop {
        return Some(op_trace_stop());
    }
    if start {
        return Some(op_trace_start());
    }
    None
}

/// Is this "erc" one of the three ordinary-English ERCs rather than an
/// electrical rule check? "erc 20" / "erc-721" / "erc 1155" is the crypto token
/// standard (a number directly after the acronym gives it away); an ERC GRANT is
/// the European Research Council; an ERC REFUND/CREDIT is the US employee
/// retention credit. None of the three is worth a swallowed turn.
fn says_erc_in_a_non_pcb_sense(lower: &str) -> bool {
    let words = speech_words(lower);
    let token_standard = words.iter().enumerate().any(|(i, w)| {
        *w == "erc"
            && words
                .get(i + 1)
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    });
    token_standard
        || crate::utterance::mentions_any_word(
            lower,
            &[
                "grant", "grants", "refund", "refunds", "credit", "credits", "token", "tokens",
                "crypto", "nft", "nfts", "wallet", "council", "irs", "payroll",
            ],
        )
}

/// Does the extracted name look like a net LABEL? Either a letters-and-digits
/// mix (3V3, 12V, GPIO2 — the MIX matters: the bare "2019" in "the 2019 net
/// income was higher" is not a net) or one of the standard power/signal names.
/// IN and OUT are real labels on an analog sheet, which is why they are here
/// rather than treated as English stopwords; object position is what keeps "how
/// much do you take home IN NET PAY" out instead.
fn looks_like_net_label(name: &str) -> bool {
    (name.chars().any(|c| c.is_ascii_digit()) && name.chars().any(|c| c.is_ascii_alphabetic()))
        || matches!(
            name,
            "GND" | "AGND" | "DGND" | "GROUND" | "VCC" | "VDD" | "VSS" | "VBUS" | "VIN" | "VOUT"
                | "VREF" | "VBAT" | "SDA" | "SCL" | "CLK" | "MCLK" | "SCK" | "MISO" | "MOSI"
                | "RESET" | "NRST" | "SWDIO" | "SWCLK" | "CS" | "EN" | "INT" | "TX" | "RX"
                | "USB" | "PWM" | "CHASSIS" | "IN" | "OUT"
        )
}

/// The modifier half of the English "<X> net" compounds. This is a closed class
/// — the language has about two dozen — and no PCB net is named MOSQUITO, so an
/// entry here can never cost a real command. It exists because the select-verb
/// path below would otherwise read "show me the mosquito net" as a selection of
/// a net called MOSQUITO.
fn is_ordinary_net_compound(name: &str) -> bool {
    matches!(
        name,
        "MOSQUITO" | "SAFETY" | "TENNIS" | "VOLLEYBALL" | "BASKETBALL" | "BADMINTON" | "SOCCER"
            | "GOAL" | "HAIR" | "FISHING" | "FISH" | "BUTTERFLY" | "CARGO" | "DRAG" | "GILL"
            | "BUG" | "INSECT" | "BIRD" | "LANDING" | "DIP" | "TRAWL" | "CASTING" | "SCREEN"
            | "SHRIMP"
    )
}

/// Verbs that mean "act on this object on screen". "go" is accepted only in the
/// navigation phrase "GO TO the batt net" — bare "go" would let "go grab the
/// net" select a net called GRAB.
fn mentions_select_verb(lower: &str) -> bool {
    if crate::utterance::mentions_any_word(
        lower,
        &[
            "show", "shows", "highlight", "highlights", "select", "selects", "isolate", "probe",
            "jump", "zoom", "find",
        ],
    ) {
        return true;
    }
    let words = speech_words(lower);
    words.windows(2).any(|w| w[0] == "go" && w[1] == "to")
}

/// The net a selection utterance names, or None when "net" was ordinary
/// English.
///
/// WHAT WENT WRONG: the branch below ran `extract_net_name` on EVERY utterance
/// with no verb, no app gate and no test of the name it produced, then forwarded
/// that name to the app verbatim — "my net calories were way under yesterday"
/// selected the net MY, "what's the net weight of a can of chickpeas" selected S
/// (the tail of the contraction), "nothing but net" selected BUT. Twenty of them
/// in the corpus.
///
/// A selection now needs the board named, or a name that looks like a net label
/// (or carries a select verb) AND a phrase that settles on it. The bare-
/// reference form is deliberately kept: with the board open, "the 3v3 net" with
/// no verb at all is the likeliest thing a user says.
fn selected_net(lower: &str) -> Option<String> {
    let name = extract_net_name(lower)?;
    // The board itself is named ("... net on the schematic"): any name, and the
    // net need not end the phrase.
    if mentions_board_noun(lower) {
        return Some(name);
    }
    let words = speech_words(lower);
    let pos = words.iter().position(|w| *w == "net")?;
    if !phrase_settles_at(&words, pos) {
        return None;
    }
    if looks_like_net_label(&name)
        || (mentions_select_verb(lower) && !is_ordinary_net_compound(&name))
    {
        Some(name)
    } else {
        None
    }
}

/// Does the reference look like a KiCad reference designator (a letter prefix
/// then digits: U3, R12, C5)? A bare "5" does not, which is what made
/// "component 5 is backordered" a selection.
fn looks_like_designator(reference: &str) -> bool {
    reference
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && reference.chars().any(|c| c.is_ascii_digit())
}

/// The component a selection utterance names. Same defect as the net branch —
/// the extractor ran on every utterance with no verb and no app gate — and the
/// same shape of fix. A bare number is only a component when a select verb is
/// present AND the phrase settles on it, because a select verb alone still let
/// "show me component 4 of the essay" and "find component 7 of the rubric"
/// through.
/// "select r14" / "highlight u3" — a bare KiCad reference designator with no
/// "component" noun anywhere.
///
/// MEASURED RECALL MISS: "select r14" reached nothing. [`extract_component_ref`]
/// keys on the literal word "component", and nobody working a schematic says it —
/// they say the refdes. So the single most common Silicon Canvas utterance was
/// unroutable.
///
/// CLOSED-VOCABULARY, like the Nexus/Mark-Forge bare idioms: the WHOLE utterance
/// must be a select verb, at most two frame words, and ONE token shaped like a
/// designator (1-3 letters then 1-4 digits). One content word from outside that
/// list and it is a sentence, not a command — which is what keeps "select all the
/// rows in column b2" and "highlight the r14 paragraph" out. Pure.
fn bare_designator_selection(lower: &str) -> Option<String> {
    const VERBS: &[&str] = &["select", "highlight", "isolate", "probe"];
    const FRAME: &[&str] = &["the", "darwin", "please", "now", "ok", "okay", "hey", "component"];
    let words = speech_words(lower);
    if !(2..=4).contains(&words.len()) || !VERBS.contains(&words[0]) {
        return None;
    }
    let mut designator: Option<&str> = None;
    for w in &words[1..] {
        if FRAME.contains(w) {
            continue;
        }
        if designator.is_some() {
            return None; // two content words: a sentence, not a refdes command
        }
        designator = Some(w);
    }
    let d = designator?;
    let letters = d.chars().take_while(|c| c.is_ascii_alphabetic()).count();
    let digits: Vec<char> = d.chars().skip(letters).collect();
    ((1..=3).contains(&letters)
        && (1..=4).contains(&digits.len())
        && digits.iter().all(char::is_ascii_digit)
        && REFDES_PREFIXES.contains(&&d[..letters]))
    .then(|| d.to_uppercase())
}

/// The reference-designator PREFIXES this idiom will accept.
///
/// The doc above already calls this rule "CLOSED-VOCABULARY" — it was not. The
/// shape test was "one to three letters then one to four digits", which is also
/// the shape of every SPREADSHEET CELL and every CHESS SQUARE. MEASURED at HEAD,
/// all reaching `select.component`: "select a1", "highlight b2", "select e4",
/// "highlight g7", "select h8", "highlight aa1", "select b12", "highlight a5" —
/// eight of the nine cell/square utterances probed. `apps::send_op` forwards the
/// op fire-and-forget, so the owner working a spreadsheet gets a PCB editor sent
/// a selection for a part that does not exist, with no result to correlate.
///
/// The prefix is the only thing that separates them, and unlike the cell space it
/// really is closed: these are the standard schematic class letters. Naming them
/// makes the closed-vocabulary claim TRUE.
///
/// TEN LETTERS STAY AMBIGUOUS AND STAY ADMITTED, not one. Every SINGLE-letter
/// class below is also a spreadsheet column, so `select <letter><digits>` stays
/// ambiguous for C, D, F, J, K, L, Q, R, U and Y — MEASURED still reaching
/// `select.component` at this revision: "select c3", "select d4", "highlight f5",
/// "select j2", "highlight k4", "select l7", "highlight q9", "select u3",
/// "highlight y2". Three of them (C, D, F) are chess files too, so 24 of the 64
/// squares are admitted as well.
///
/// Saying "the one left open is select c3" would be a HAND-PICKED SUBSET — the
/// same shape as the "6 of 6 vacuous" figure that became 5 of 79 on full
/// enumeration. What the prefix list actually closes is the letters that are NOT
/// designator classes: A, B, E, G, H, M, N, O, P, S, T, V, W, X, Z and every
/// multi-letter column (AA1, AB2, …). That is a real narrowing and it is what the
/// test measures; it is not a closed rule over spreadsheet cells, and the
/// "CLOSED-VOCABULARY" claim above is about the DESIGNATOR vocabulary only.
///
/// The overlap is admitted rather than closed because the op is a non-destructive
/// SELECTION: it costs a mis-sent op rather than anything the owner cannot undo.
/// Dropping single-letter prefixes wholesale would take R14, U3, C5 and Q2 — the
/// actual shipped probes — with it.
const REFDES_PREFIXES: &[&str] = &[
    "c",  // capacitor
    "d",  // diode
    "f",  // fuse
    "fb", // ferrite bead
    "j",  // connector
    "jp", // jumper
    "k",  // relay
    "l",  // inductor
    "ls", // loudspeaker
    "mh", // mounting hole
    "q",  // transistor
    "r",  // resistor
    "rn", // resistor network
    "rv", // varistor
    "sw", // switch
    "tp", // test point
    "u",  // integrated circuit
    "y",  // crystal / oscillator
];

fn selected_component(lower: &str) -> Option<String> {
    let Some(reference) = extract_component_ref(lower) else {
        return bare_designator_selection(lower);
    };
    if looks_like_designator(&reference) || mentions_board_noun(lower) {
        return Some(reference);
    }
    if !mentions_select_verb(lower) {
        return None;
    }
    let words = speech_words(lower);
    let pos = words.iter().position(|w| *w == "component")?;
    if phrase_settles_at(&words, pos + 1) {
        Some(reference)
    } else {
        None
    }
}

/// The object of a fit phrase may be followed by the redundant "view" /
/// "viewport" / "layout" ("fit the board VIEW") before the tail.
fn fit_object_settles(words: &[&str], mut k: usize) -> bool {
    if matches!(words.get(k).copied(), Some("view" | "viewport" | "layout")) {
        k += 1;
    }
    settles_from(words, k)
}

/// Is this a "re-frame the viewport" phrase?
///
/// WHAT WENT WRONG: `contains("fit")` matches inside nonprofit / benefit /
/// outfit, `contains("board")` inside keyboard / boarding / cardboard, and the
/// "whole board" arm carried no verb requirement at all, so "the whole board of
/// directors approved the budget" re-framed the view. Whole-word is only half
/// the fix — "the fit of these boards is off" and "how many bodies can that
/// minivan actually fit with luggage" pass whole-word too. `fit` has to be the
/// VERB (its object follows it) and the object has to settle the phrase.
fn view_fit_requested(lower: &str) -> bool {
    let words = speech_words(lower);
    let fit_arm = words.iter().enumerate().any(|(i, w)| {
        if *w != "fit" {
            return false;
        }
        let mut j = i + 1;
        while j < words.len() && SPEECH_DETERMINERS.contains(&words[j]) {
            j += 1;
        }
        match words.get(j).copied() {
            Some("board" | "boards" | "all") => fit_object_settles(&words, j + 1),
            Some("whole" | "entire") => {
                matches!(words.get(j + 1).copied(), Some("board" | "boards"))
                    && fit_object_settles(&words, j + 2)
            }
            // "fit everything ON THE BOARD" — too generic without the board.
            Some("everything") => board_locus_from(&words, j + 1),
            _ => false,
        }
    });
    let whole_arm = words.iter().enumerate().any(|(i, w)| {
        matches!(*w, "board" | "boards")
            && i > 0
            && matches!(words[i - 1], "whole" | "entire")
            && fit_object_settles(&words, i + 1)
            && crate::utterance::mentions_any_word(
                lower,
                &["show", "fit", "zoom", "view", "display", "frame", "center"],
            )
    });
    fit_arm || whole_arm
}

/// Map a spoken utterance to a Silicon Canvas command, or None when it is not a
/// Silicon-Canvas control phrase (the turn then falls through to normal
/// routing). Deterministic and pure so the mapping is unit-tested without a
/// socket, a running app, or the classifier. Order matters: the most specific
/// ops (trace step/stop, ERC, view fit, component/net selection) are matched
/// before the broad "open/show silicon canvas" launch so "trace this net" does
/// not get mistaken for a launch.
///
/// Recognized phrases (all case-insensitive, whole lowercased utterance):
///   - "open/show/launch/bring up silicon canvas"            -> Launch
///   - "show me the <X> net" / "highlight the <X> net" /
///     "select the <X> net"                                  -> select.net {X}
///   - "show/select component <REF>"                         -> select.component
///   - "trace this net" / "start trace/tracing"              -> trace.start
///   - "next/step (the) trace" / "advance the trace"         -> trace.step
///   - "stop/end/exit trace/tracing"                         -> trace.stop
///   - "run erc" / "run the electrical rule check(s)" /
///     "check the electrical rules"                          -> erc.run
///   - "fit the board" / "show the whole board" / "fit all"  -> view.set fit all
///
/// EVERY one of those needs the board in the sentence, not merely the keyword.
/// The op branches are gated by the helpers above (`trace_command`,
/// `selected_net`, `selected_component`, `view_fit_requested`): what follows a
/// trace word has to be PCB material, a net name has to look like a net label
/// (or the board has to be named) and settle the phrase, a component reference
/// has to look like a designator or carry a select verb and settle, and "fit" /
/// "whole board" have to be a view verb with the board as its object. A bare
/// keyword is never enough, because 132 of 1,897 ordinary utterances contained
/// one.
pub fn silicon_canvas_command(text: &str) -> Option<SiliconCanvasCommand> {
    let lower = text.to_lowercase();

    // --- trace mode (specific verbs before the broad launch) ---------------
    // "trace this net", "start tracing", "next trace step", "exit trace mode".
    // The step-before-stop-before-start precedence lives inside
    // `trace_command` so the three cannot drift apart; see it for what a bare
    // `contains("trace")` used to do to ordinary speech.
    if let Some(op) = trace_command(&lower) {
        return Some(SiliconCanvasCommand::Op(op));
    }

    // --- ERC ---------------------------------------------------------------
    // WHAT WENT WRONG: `contains("erc")` is three letters that live inside
    // percent, percentage, mercy, merchant, commerce and e-commerce — 80 of the
    // 132 corpus captures came through this single line ("what percent of my
    // paycheck goes to taxes", "have mercy, this curry is spicy", "the chamber
    // of commerce sent me a membership bill"). Whole-word alone closes all 80:
    // the token "erc" appears ZERO times in the 1,897-utterance corpus.
    //
    // The acronym still needs a reason to be a rule check, and it gets the same
    // choice the rest of this file offers — a run/check verb, an unambiguous
    // ERC noun ("any erc errors"), or object position ("show me the erc"). The
    // spelled-out "electrical rule" arm keeps its verb requirement and may NOT
    // trade it for a board co-word: an earlier draft allowed that, and "what
    // are the electrical rules for a subpanel on this board" — an electrician's
    // question the shipped code leaves alone — started running an ERC. A fix
    // that closes captures must not open one.
    let erc_verb = crate::utterance::mentions_any_word(
        &lower,
        &[
            "run", "runs", "running", "rerun", "reruns", "check", "checks", "checking", "recheck",
            "rechecks",
        ],
    );
    let erc_words = speech_words(&lower);
    let erc_is_the_object = erc_words
        .iter()
        .position(|w| *w == "erc")
        .is_some_and(|pos| phrase_settles_at(&erc_words, pos));
    if (mentions_word(&lower, "erc")
        && !says_erc_in_a_non_pcb_sense(&lower)
        && (erc_verb
            || crate::utterance::mentions_any_word(
                &lower,
                &[
                    "error", "errors", "violation", "violations", "warning", "warnings", "report",
                    "reports", "results",
                ],
            )
            || erc_is_the_object))
        || (lower.contains("electrical rule") && erc_verb)
    {
        return Some(SiliconCanvasCommand::Op(op_erc_run()));
    }

    // --- net selection -----------------------------------------------------
    // "show me the 3V3 net", "highlight the GND net", "select the VBUS net",
    // and the bare mid-session "the 3v3 net". `selected_net` holds the gate:
    // the extractor on its own handed the app nets called MY, S and BUT.
    if let Some(net) = selected_net(&lower) {
        return Some(SiliconCanvasCommand::Op(op_select_net(&net)));
    }

    // --- component selection ----------------------------------------------
    // "select component u3", "show component r12", the bare "component r12".
    // `selected_component` holds the gate; ungated, "component 5 is
    // backordered" selected component 5, and gated only on a select verb,
    // "show me component 4 of the essay" still did.
    if let Some(reference) = selected_component(&lower) {
        return Some(SiliconCanvasCommand::Op(op_select_component(&reference)));
    }

    // --- view fit ----------------------------------------------------------
    // "fit the board", "fit all", "show the whole board". `view_fit_requested`
    // holds the gate: `contains` here matched "nonprofit", "boarding", and the
    // "all" inside "actually".
    if view_fit_requested(&lower) {
        return Some(SiliconCanvasCommand::Op(op_view_fit_all()));
    }

    // --- launch ------------------------------------------------------------
    // Only when the utterance actually names Silicon Canvas AND carries an
    // open-class verb — "open silicon canvas", "show me silicon canvas",
    // "bring up the schematic". This is last so an op phrase that also says
    // "show" (e.g. "show me the 3V3 net") was already handled above.
    if mentions_silicon_canvas(&lower)
        && (lower.contains("open")
            || lower.contains("launch")
            || lower.contains("start")
            || lower.contains("bring up")
            || lower.contains("show"))
    {
        return Some(SiliconCanvasCommand::Launch);
    }

    None
}

/// Extract the net name from a "<verb> the <NAME> net" phrase. Returns the token
/// immediately before the word "net" (the net's name as spoken), uppercased to
/// match KiCad net-label convention (3V3, GND, VBUS); None when there is no
/// "net" keyword or no name precedes it. The net name is forwarded verbatim in
/// the op — Silicon Canvas resolves it against the open document.
fn extract_net_name(lower: &str) -> Option<String> {
    // Require the standalone word "net" so "network"/"netflix" never match.
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '.' && c != '-' && c != '+')
        .filter(|w| !w.is_empty())
        .collect();
    let net_pos = words.iter().position(|w| *w == "net")?;
    if net_pos == 0 {
        return None;
    }
    // The token just before "net", skipping a trailing article ("the net" has
    // no name). Walk back over "the"/"a" if they sit right before "net".
    let mut idx = net_pos - 1;
    while matches!(words[idx], "the" | "a" | "an") {
        if idx == 0 {
            return None;
        }
        idx -= 1;
    }
    let name = words[idx];
    // A pure stopword/verb is not a net name. The PRONOUNS and copulas matter
    // as much as the verbs did: with only the verbs listed, "my net calories"
    // handed the app the net MY and "what's the net weight" handed it S — the
    // tail of the contraction, because the split above breaks "what's" into
    // "what" and "s". The remaining select verbs are here for the same reason
    // the first three were: "can you find a net" walks back over the article
    // and offers FIND as the name. A bogus name is not a harmless miss; it is
    // forwarded to the app verbatim as a real selection.
    //
    // "in" and "out" are deliberately NOT here — they are ordinary net labels
    // on an analog sheet ("show me the in net"), and object position is what
    // keeps "how much do you take home in net pay" out instead.
    if matches!(
        name,
        "show" | "me" | "highlight" | "select" | "the" | "this" | "that" | "trace" | "find"
            | "get" | "buy" | "go" | "jump" | "zoom" | "isolate" | "probe" | "my" | "your"
            | "our" | "their" | "its" | "it" | "i" | "you" | "s" | "is" | "are" | "was" | "of"
            | "and" | "or" | "to" | "for" | "on" | "at"
    ) {
        return None;
    }
    Some(name.to_uppercase())
}

/// Extract a component reference designator from "show/select component <REF>".
/// The reference is the token after the word "component", uppercased (KiCad
/// refs are like U3, R12, C5). None when there is no "component" keyword or
/// nothing follows it.
fn extract_component_ref(lower: &str) -> Option<String> {
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let pos = words.iter().position(|w| *w == "component")?;
    let reference = words.get(pos + 1)?;
    // A bare reference looks like a letter-prefix + digits; require at least one
    // digit so "select component now" does not pick up "now".
    if reference.chars().any(|c| c.is_ascii_digit()) {
        Some(reference.to_uppercase())
    } else {
        None
    }
}

// The op-string builders. Each produces the EXACT wire JSON Silicon Canvas's
// ops.rs deserializes (verified by a round-trip test there). serde_json builds
// them so a net/component name with a quote can never break the JSON framing.

fn op_select_net(name: &str) -> String {
    json!({"op": "select.net", "name": name}).to_string()
}
fn op_select_component(reference: &str) -> String {
    json!({"op": "select.component", "name": reference}).to_string()
}
fn op_trace_start() -> String {
    json!({"op": "trace.start"}).to_string()
}
fn op_trace_step() -> String {
    json!({"op": "trace.step"}).to_string()
}
fn op_trace_stop() -> String {
    json!({"op": "trace.stop"}).to_string()
}
fn op_erc_run() -> String {
    json!({"op": "erc.run"}).to_string()
}
fn op_view_fit_all() -> String {
    json!({"op": "view.set", "mode": "fit", "target": "all"}).to_string()
}

// ===========================================================================
// Vision voice control (mirrors the Silicon Canvas seam above: the daemon
// forwards STRUCTURED ops ONLY; the Vision app never parses natural language).
//
// The Vision micro-app (apps/vision) is a binary micro-app on the same runtime.
// Its HOST -> APP op wire form is FROZEN in apps/vision/Sources/vision/Op.swift:
// every op is `{"type":"op","op":"<name>", ...}` (note the `"type":"op"`
// envelope — UNLIKE Silicon Canvas's bare `{"op":...}`; the Swift Op.decode
// dispatches ops only when type == "op"). The op-string builders below produce
// that EXACT wire shape so the daemon forwards a line the app already accepts in
// its own IPCTests (e.g. {"type":"op","op":"watch.start","source":"camera"}).
//
// DEFENSIVE-ONLY: the recognized phrases map to PRESENCE/OBJECT detection and
// capture lifecycle on the user's OWN devices — never an identity query. There
// is no "who is <NAME>" op; "who is there" asks the app for a generic presence
// status snapshot, not a face match. Capture itself is gated by macOS TCC
// (runtime consent), which the daemon cannot grant.
// ===========================================================================

/// The Vision micro-app's registered name (its manifest `[app].name` and the
/// key into the app registry / its socket).
pub const VISION_APP: &str = "vision";

/// What a Vision voice command resolves to: LAUNCH the app, or forward a
/// STRUCTURED op line to the already-running app. The op body is opaque to the
/// daemon (built to match Op.swift verbatim).
#[derive(Debug, Clone, PartialEq)]
pub enum VisionCommand {
    /// "open vision" — start the micro-app.
    Launch,
    /// A complete JSON op line (one line) to forward verbatim, e.g.
    /// `{"type":"op","op":"watch.start","source":"camera"}`.
    Op(String),
}

/// Whether the utterance names the Vision app / capability itself ("vision",
/// "the camera", "the camera feed"). Used to gate the bare launch verb so an
/// unrelated "open safari" is never captured.
fn mentions_vision(lower: &str) -> bool {
    contains_word(lower, "vision")
        || lower.contains("the camera")
        || lower.contains("camera feed")
        || lower.contains("the screen feed")
}

// ===========================================================================
// VISION INTENT GATES — the locks the app NOUN always had and the app's VERBS
// never did.
//
// WHAT WENT WRONG: `mentions_vision` above is careful ("television"/"revision"
// can never launch Vision, because `contains_word` demands a whole token), but
// every branch below it matched its VERB with a bare `contains()`. We replayed
// 1,897 ordinary sentences (health, weather, cooking, travel, work, chat) that
// name no app at all through `vision_command`: 46 of them became Vision ops, and
// 39 of those TURNED THE CAMERA ON — "is there a tornado watch in effect", "I
// want to watch the sunset from the porch", "we should watch for black ice on
// the way up", "my watch says I've been standing in this kitchen for three
// hours". Opening a camera because somebody asked about the weather is a privacy
// incident, not a misroute.
//
// Three failures stacked:
//   1. SUBSTRINGS. "end" hides in sp-END-ing, "read" in alREADy, "scan" in
//      SCANdal, "form" in perFORMance, "on" in wr-ON-g, "locate" in reLOCATEd.
//   2. NO OBJECT. "watch" is one of the commonest verbs in English and also the
//      noun for the thing on your wrist. The verb alone says nothing; WHAT the
//      user is watching is the whole signal.
//   3. A CAMERA DEFAULT. Three separate branches ended in "…otherwise, the
//      camera" — so an unrecognized object did not fall through, it opened a
//      lens.
//
// The helpers below are the shared locks: whole-word tokens (via
// `crate::utterance`), the verb in COMMAND position, and the verb's DIRECT
// OBJECT. Their callers then NAME a source instead of defaulting to one.
// ===========================================================================

/// Tokens that may legally sit BEFORE a spoken command's verb: an address, a
/// politeness, a modal request frame ("can you …", "i need you to …"), an aspect
/// verb ("keep watching", "start watching"), and the fragments an apostrophe
/// leaves behind once the utterance is split on non-alphanumerics ("i'd" -> "i"
/// + "d").
///
/// WHAT WENT WRONG: a bare `contains("read")` cannot tell a COMMAND from a
/// NARRATION. "already read the whiteboard notes" and "she read this to the
/// kids" are both past-tense reports about a human reading something, and both
/// used to open the camera / capture the screen. A command puts its verb first,
/// behind nothing but this frame; a narration puts a subject or an adverb there.
/// This is the same discipline `lumen_is_act` already applies to "tap" (which
/// counts only as the LEADING imperative, so "is the tap water safe" is inert).
const VISION_COMMAND_FRAME: &[&str] = &[
    "darwin", "hey", "ok", "okay", "please", "and", "then", "now", "just", "go",
    "ahead", "also", "alright", "yes", "yeah", "sure", "could", "can", "will",
    "would", "should", "you", "i", "we", "let", "lets", "us", "me", "my", "to",
    "want", "wants", "need", "needs", "like", "try", "help", "gonna", "going",
    "keep", "keeps", "keeping", "start", "starts", "continue",
    // The PHRASAL continuation verbs. "resume"/"begin" were added and these were
    // missed, so "go back to watching the driveway" and "carry on watching the
    // front door" — the two most ordinary ways to say it — were refused.
    "get", "gets", "carry", "back", "on",
    // The rest of the ASPECT verbs ("resume watching the front door", "begin
    // watching the driveway") — "keep"/"start"/"continue" were here and their
    // synonyms were not — and the DISCOURSE MARKERS and manner adverbs a spoken
    // command actually opens with. WHAT WENT WRONG: "quickly scan this receipt",
    // "first, read this", "actually, read this to me", "maybe scan the receipt"
    // and "excuse me, read this" were all refused, because the frame admitted
    // "please" but nothing else a person says before getting to the verb. These
    // only ever PERMIT a verb to be read as a command; the verb's own object still
    // has to name a Vision target, so none of them can trigger anything alone.
    "resume", "resumes", "begin", "begins", "quickly", "quick", "first",
    "actually", "maybe", "right", "excuse", "simply", "kindly",
    "m", "s", "t", "d", "ll", "re", "ve",
    // …and the APOSTROPHE-FREE spellings of the same contractions. WHAT WENT
    // WRONG: dictation writes "im"/"id"/"ill"/"ive" as ONE token, so while
    // "i'm going to need you to watch the front door" passed (as the fragments
    // "i" + "m"), the identical sentence without the apostrophe was refused. These
    // four are the exact twins of "m"/"d"/"ll"/"ve" above and carry no new
    // meaning. The "re" twins ("were"/"youre"/"theyre") are deliberately NOT
    // here: "were" is also the past tense of "be", which already sits in
    // VISION_OBJECT_CLOSERS as a phrase boundary, and no command needs it.
    "im", "id", "ill", "ive",
];

/// The frame tokens that make what follows a REQUEST rather than a report.
const VISION_REQUEST_MODALS: &[&str] = &[
    "could", "can", "will", "would", "should", "please", "want", "wants", "need",
    "needs", "like", "let", "lets", "try", "help", "gonna", "going", "d", "ll",
    "darwin", "hey", "ok", "okay", "keep", "keeps", "keeping", "start", "starts",
    "continue",
];

/// Bare subject pronouns. English spells the PAST tense of "read" and "set"
/// exactly like the imperative, so "we read the whiteboard notes" and "i set the
/// sensitivity myself last week" are indistinguishable from a command by the
/// verb alone — but a subject sitting directly in front of the verb, with no
/// modal ahead of it, is the tell. "can YOU read my screen" keeps working
/// because the modal came first; "WE read the whiteboard" does not.
const VISION_BARE_SUBJECTS: &[&str] = &["i", "we", "you"];

/// Whether the FIRST content token of `lower` is one of `verbs` — i.e. the verb
/// is in COMMAND position, preceded by nothing but [`VISION_COMMAND_FRAME`], and
/// not sitting behind a bare subject (see [`VISION_BARE_SUBJECTS`]). Whole-word
/// by construction (the split is the same alnum-boundary rule
/// `crate::utterance::mentions_word` uses), so "proofread this" is not a "read"
/// and "already read …" is not a command. Single pass, no allocation — an
/// oversize junk utterance must stay cheap.
fn vision_verb_in_command_position(lower: &str, verbs: &[&str]) -> bool {
    let mut modal_seen = false;
    let mut prev_was_subject = false;
    for w in lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        if verbs.contains(&w) {
            return !prev_was_subject || modal_seen;
        }
        if !VISION_COMMAND_FRAME.contains(&w) {
            return false;
        }
        if VISION_REQUEST_MODALS.contains(&w) {
            modal_seen = true;
        }
        prev_was_subject = VISION_BARE_SUBJECTS.contains(&w);
    }
    false
}

/// Tokens that END the direct object of a Vision verb: prepositions, conjunctions,
/// subordinators, pronouns and auxiliaries all start a NEW phrase, so whatever
/// follows them is no longer what the user asked us to watch/scan. This is what
/// separates "watch the door" from "watch the sunset FROM the porch" and "watch
/// the front desk ON tuesdays".
///
/// "that" is deliberately NOT here: it is a DETERMINER far more often than a
/// subordinator in this position ("watch that door", "scan that receipt"), and
/// listing it in both places made the object scan stop before it ever saw a head.
const VISION_OBJECT_CLOSERS: &[&str] = &[
    "for", "while", "when", "until", "till", "in", "on", "at", "from", "to",
    "with", "without", "about", "over", "under", "into", "onto", "after",
    "before", "during", "through", "around", "near", "by", "and", "or", "but",
    "so", "if", "because", "than", "i", "we", "he", "she", "they", "it", "you",
    "there", "is", "was", "were", "are", "am", "be", "been", "says", "said",
    "will", "can", "could", "would", "should", "do", "does", "did", "has",
    "have", "had", "out", "up", "down", "off", "all", "again", "tonight", "today",
    "tomorrow", "yesterday", "instead", "too", "together",
];

/// Determiners/possessives introduce the object; they are never its head.
const VISION_OBJECT_DETERMINERS: &[&str] = &[
    "the", "a", "an", "my", "our", "your", "this", "that", "these", "those",
    "his", "her", "their", "its", "any", "some", "every", "each", "both",
];

/// Generic MEDIUM nouns and post-object ADVERBS that trail a real target without
/// changing it: "watch the camera feed" is still the camera and "watch the door
/// closely" is still the door. Skipped so the head stays the subject. Note how
/// narrowly this differs from ordinary speech — "feed" is here, "fee" is not,
/// which is exactly why "watch the entrance fee, it went up" must not fire.
const VISION_OBJECT_TAILS: &[&str] = &[
    "feed", "feeds", "stream", "streams", "view", "now", "right", "here",
    "please", "closely", "carefully", "quickly", "quick", "real", "continuously",
    "constantly", "intently", "live", "later", "awhile",
    // A DEGREE/MEASURE word trailing the head names the same thing: "set the
    // sensitivity LEVEL to high" is the sensitivity and "set the sensitivity
    // HIGHER" is the sensitivity. Without these the head read as "level" /
    // "higher" and the config write — the one command in this whole seam the user
    // has to repeat until it works — silently stopped happening.
    "level", "levels", "higher", "lower",
    // …and the VALUE itself when it trails the head: "set motion sensitivity
    // HIGH" and "set sensitivity HIGH please" read their head as "high" and
    // stopped writing. This cannot open the value up to anything new — the head
    // must still be "sensitivity", so "set a HIGH sensitivity ALARM at the shop"
    // (head "alarm") stays refused.
    "high", "low", "medium", "max",
];

/// The verbs that can SET the motion sensitivity — a MUTATION of the app's
/// config, so the branch below reads this verb's own object rather than trusting
/// that the word "sensitivity" appeared somewhere in the sentence.
const VISION_SENSITIVITY_VERBS: &[&str] = &[
    "set", "sets", "turn", "turns", "adjust", "adjusts", "change", "changes",
    "raise", "lower", "increase", "decrease", "put", "make",
];

/// The HEAD of the noun phrase that is the DIRECT OBJECT of the first `verbs`
/// token in `lower`, together with the token immediately in front of it (the
/// compound-noun modifier, `None` when only a determiner precedes it). `None`
/// when the verb has no object at all.
///
/// WHAT WENT WRONG: the watch/scan branches fired on the VERB and ignored what
/// followed it, so "watch out for the strike", "watch a movie with my sister",
/// "watch the earnings call" and "scan the performance report" were all
/// commands. Reading the head is what tells "watch the door" (a target) from
/// "watch the front DESK" and "watch the front ROW" (not targets) — a fixed-size
/// keyword WINDOW cannot, because it sees "front" in all three. The scan stops
/// at the first closer and after six tokens (a spoken direct object is short),
/// so this stays a single cheap pass over an oversize utterance.
///
/// "of" does not merely CLOSE the object, it REJECTS it: a partitive/possessive
/// head belongs to whatever follows, so "watch the entrance OF the movie", "the
/// hall OF fame game" and "the front OF the house" name no Vision target even
/// though "entrance"/"front" would pass on their own.
fn vision_object_head<'a>(lower: &'a str, verbs: &[&str]) -> Option<(&'a str, Option<&'a str>)> {
    let mut it = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty());
    loop {
        match it.next() {
            Some(w) if verbs.contains(&w) => break,
            Some(_) => continue,
            None => return None,
        }
    }
    let mut head: Option<&str> = None;
    let mut modifier: Option<&str> = None;
    let mut seen = 0usize;
    // A leading quantifier is part of the determiner, not a closer. "all" sits in
    // VISION_OBJECT_CLOSERS (it closes an object elsewhere), so "watch all the
    // doors" broke the scan before any head was seen. Skipping ONE leading "all"
    // also has to permit the partitive that follows it — "watch all OF the doors"
    // — which the `of` guard below would otherwise refuse.
    let mut it = it.peekable();
    let mut allow_one_of = false;
    if it.peek() == Some(&"all") {
        it.next();
        allow_one_of = true;
    }
    for w in it {
        if w == "of" {
            if allow_one_of {
                allow_one_of = false;
                continue;
            }
            return None;
        }
        // A POSSESSIVE OWNER IS NOT A MODIFIER. "baby's room" tokenizes to
        // baby / s / room, and the bare "s" took the modifier slot, so every
        // "watch the baby's room" / "watch my son's room" was refused — the
        // apostrophe-fragment class that was fixed on the frame side (im/id/ive)
        // and missed here. The owner constrains nothing about the target, so the
        // slot is cleared rather than filled.
        if w == "s" {
            // Clear the HEAD as well as the modifier. Clearing only the modifier
            // leaves the owner sitting in `head`, and the next word shifts it INTO
            // the modifier slot — so "watch the baby's room" passed (because
            // "baby" happens to be a target word) while "watch my son's room" and
            // "watch the neighbor's driveway" were still refused on "son" and
            // "neighbor". The owner is not part of the target description at all.
            head = None;
            modifier = None;
            continue;
        }
        if VISION_OBJECT_CLOSERS.contains(&w) {
            break;
        }
        seen += 1;
        if seen > 6 {
            break;
        }
        if VISION_OBJECT_DETERMINERS.contains(&w) || VISION_OBJECT_TAILS.contains(&w) {
            continue;
        }
        modifier = head;
        head = Some(w);
    }
    head.map(|h| (h, modifier))
}

/// Which SOURCE a "watch …" utterance NAMES — `"screen"`, `"camera"`, or None
/// when it names no Vision target, in which case it is not a Vision command at
/// all and the turn falls through to normal routing.
///
/// WHAT WENT WRONG: this branch used to be `contains("watch")` and then
/// `else { "camera" }` — anything not about a screen opened the lens. Across
/// 1,897 ordinary sentences that default started 39 camera watches. The verb
/// alone is worthless: "watch" is a wristwatch ("my watch band broke"), a
/// weather alert ("a winter storm watch for the county"), an entertainment verb
/// ("watch a movie with my sister"), an attention verb ("watch for black ice")
/// and a cooking verb ("watch the milk so it doesn't boil over"). So the source
/// must now be NAMED, as the HEAD of the verb's own direct object, with the verb
/// itself in command position — there is no default left to fall into.
///
/// The camera list stays SHORT on purpose: a word earns its place only if
/// "watch the <target>", said to DARWIN, can only mean a lens. "hall", "gate",
/// "front", "office" and "yard" were tried and REMOVED — "watch the hall of fame
/// game", "watch the gate for our flight number", "watch the front of the house"
/// and "watch the office for me" are ordinary English, and a list that holds
/// them opens a camera on all four.
///
/// The MODIFIER check is the compound-noun lock: a target noun with a foreign
/// noun glued in front of it is a different object. "watch the front door" and
/// "watch the nursery camera" are camera watches; "watch the OVEN door", "watch
/// the CAR door" and "watch the BABY monitor" are not, and only the word in
/// front of the head can tell them apart.
///
/// A screen word ANYWHERE still wins for a camera-headed object — that is the
/// old predicate, kept verbatim, so this branch can only ever move a turn AWAY
/// from the camera, never toward it.
fn watch_start_source(lower: &str) -> Option<&'static str> {
    const WATCH_VERBS: &[&str] = &["watch", "watching"];
    // The user's OWN display.
    const SCREEN_TARGETS: &[&str] =
        &["screen", "screens", "display", "displays", "monitor", "monitors"];
    // The camera itself, and the places a camera watch covers.
    const CAMERA_TARGETS: &[&str] = &[
        "camera", "cameras", "webcam", "webcams", "door", "doors", "doorway",
        "doorways", "doorstep", "entrance", "entrances", "entryway", "hallway",
        "room", "rooms", "driveway", "driveways", "porch", "backyard", "garage",
        "crib", "nursery", "baby",
        // "cam" names a lens exactly as "camera" does. It used to sit in
        // VISION_OBJECT_TAILS, where it was SKIPPED — so "watch the doorbell cam"
        // read its head as "doorbell" and was refused while "watch the doorbell
        // camera" worked.
        "cam", "cams",
    ];
    // Words that may sit between the determiner and the head WITHOUT making it a
    // different thing — locations, directions, and the camera's own kinds; never
    // foreign objects.
    const TARGET_MODIFIERS: &[&str] = &[
        "front", "back", "rear", "side", "main", "second", "other", "spare",
        "upstairs", "downstairs", "outside", "outdoor", "indoor", "basement",
        "patio", "garden", "kitchen", "living", "dining", "guest", "security",
        "entry", "sliding", "laundry", "utility", "hall", "corner", "new", "old",
        "doorbell", "video", "wifi", "ip",
        // ROOM KINDS and CAMERA BRANDS. Without these "watch the kitchen door"
        // worked and "watch the bedroom door" did not — the same command, refused
        // on which room the user happens to have.
        "bedroom", "bathroom", "office", "closet", "apartment", "lobby", "shed",
        "balcony", "deck", "gate", "kids", "kid", "ring", "nest",
        // Plain ADJECTIVES of size / material / position. WHAT WENT WRONG: only
        // the ONE token in front of the head is read, so in "watch the sliding
        // GLASS door" and "watch my EXTERNAL monitor" an adjective sat in the
        // modifier slot and locked out its own noun — both went to None while the
        // old code watched both. An adjective does not make a door a different
        // OBJECT the way "oven", "car" and "baby" do, and stopping those compounds
        // is the only thing this lock is for. The head still has to be a target,
        // so these can never introduce one on their own.
        "glass", "wooden", "metal", "double", "big", "small", "little", "whole",
        "entire", "external", "primary", "left", "right",
        // Door kinds and display ordinals from the same probe: "watch the STORM
        // door", "watch the FRENCH doors", "watch the THIRD monitor", "watch my
        // LAPTOP screen".
        "storm", "french", "third", "fourth", "laptop", "desktop",
    ];
    if !vision_verb_in_command_position(lower, WATCH_VERBS) {
        return None;
    }
    // The OLD source predicate, unchanged, used only to keep a camera-headed
    // object on the SCREEN when the utterance also names one ("watch the door on
    // my screen"). Deliberately the substring form: it must not be narrower than
    // what it replaces, or this could open a camera the old code did not.
    let names_a_screen =
        lower.contains("screen") || lower.contains("display") || lower.contains("monitor");
    // "watch what's on the screen": the object is a free relative clause whose
    // head sits inside the embedded "on <screen>" phrase, so the head walk below
    // cannot see it. Only the SCREEN source is reachable this way — a free
    // relative never names a camera — so this arm cannot activate a lens.
    if lower.contains("watch what") && asks_what_is_on_screen(lower) {
        return Some("screen");
    }
    let (head, modifier) = vision_object_head(lower, WATCH_VERBS)?;
    // The modifier has to belong to the HEAD'S OWN family. Sharing one list across
    // both families is what let "watch the BABY monitor" through: "baby" is a
    // legitimate camera target, "monitor" a legitimate screen target, and the
    // compound is neither.
    let modifier_fits = |family: &[&str]| {
        modifier.is_none_or(|m| TARGET_MODIFIERS.contains(&m) || family.contains(&m))
    };
    if SCREEN_TARGETS.contains(&head) {
        return modifier_fits(SCREEN_TARGETS).then_some("screen");
    }
    if CAMERA_TARGETS.contains(&head) && modifier_fits(CAMERA_TARGETS) {
        return Some(if names_a_screen { "screen" } else { "camera" });
    }
    None
}

/// Whether `lower` asks us to STOP an existing watch: a stop verb whose OBJECT
/// is the watch ("stop watching", "end the watch", "stop the camera watch").
///
/// WHAT WENT WRONG: the old test was `contains("watch")` AND any of
/// `contains("stop"|"end"|"quit"|"cancel")` — five substring tests, no whole-word
/// rule, no order, no adjacency. "I need to watch my spending this month" matched
/// because "end" hides inside sp-END-ing; "my watch stopped syncing with my
/// phone" and "my grandfather's old pocket watch stopped working" matched because
/// the thing on your wrist is also a "watch" and "stopped" contains "stop".
/// Nobody in any of those sentences asked DARWIN to stop anything.
///
/// The shape that separates them is ORDER: a real stop names the watch AFTER the
/// stop verb ("stop watching the door"), while a wristwatch sentence puts the
/// watch FIRST ("watch stopped"). Only determiners and Vision's own nouns may sit
/// between the two, and at most three of them.
/// "turn the camera off" / "turn off the camera" / "shut the webcam off".
///
/// MEASURED RECALL MISS: "turn the camera off" reached nothing.
/// [`watch_stop_targets_the_watch`] only models the stop/end/quit/cancel/kill/halt
/// verbs taking "watch"/"watching" as their object, and nobody phrases it that way
/// about a lens — they say turn it off.
///
/// THIS ONLY EVER STOPS. It is the one direction of the camera control that is not
/// a posture decision: turning capture OFF removes a capability, it never grants
/// one, and the corresponding "turn ON the camera" stays refused (arming a lens is
/// a consent decision, made deliberately elsewhere and NOT here). Both anchors are
/// required — a camera noun AND an off particle bound to a turn/shut/kill verb —
/// so "the camera off the coast" is not a command. Pure.
fn camera_off_command(lower: &str) -> bool {
    const NOUNS: &[&str] = &["camera", "cameras", "webcam", "webcams"];
    if !crate::utterance::mentions_any_word(lower, NOUNS) {
        return false;
    }
    const FORMS: &[&str] = &[
        "turn off the camera", "turn off camera", "turn the camera off",
        "turn off the cameras", "turn the cameras off",
        "turn off the webcam", "turn the webcam off",
        "shut off the camera", "shut the camera off", "shut the camera down",
        "kill the camera",
        // A BARE "camera off" is NOT here: "the camera off the coast picked up
        // the storm" is the sentence it would take. Every form kept carries the
        // verb that makes it an order.
    ];
    FORMS.iter().any(|f| crate::agents::contains_phrase(lower, f))
}

fn watch_stop_targets_the_watch(lower: &str) -> bool {
    // "kill"/"halt" are the two other things people say to a running watch. They
    // are worth listing because of what the OLD code did with them: "kill the
    // watch" and "halt the watch" carry no stop SUBSTRING, so they fell past the
    // stop branch into the start branch and TURNED THE CAMERA ON — the user asked
    // to end a watch and got a lens. They still have to take the watch as their
    // object, exactly like the other four.
    const STOP_VERBS: &[&str] = &["stop", "end", "quit", "cancel", "kill", "halt"];
    const FILLERS: &[&str] = &[
        "the", "this", "that", "my", "our", "your", "a", "an", "it", "all", "any",
        "vision", "camera", "cameras", "screen", "door", "room", "video", "live",
    ];
    // What "stop WATCHING <x>" may legally take as its object: nothing at all, a
    // trailing adverb, or one of Vision's own targets behind determiners and
    // location modifiers. Anything else is somebody describing their own viewing
    // habits — "i need to stop watching so much tv", "stop watching the news",
    // "you should stop watching that show". None of them asked DARWIN for
    // anything, and all three used to stop a running watch.
    const OBJECTS: &[&str] = &[
        "watch", "watching", "camera", "cameras", "webcam", "webcams", "screen",
        "screens", "display", "displays", "monitor", "monitors", "door", "doors",
        "doorway", "doorways", "doorstep", "entrance", "entrances", "entryway",
        "hallway", "room", "rooms", "driveway", "driveways", "porch", "backyard",
        "garage", "crib", "nursery", "baby", "vision", "everything", "feed", "feeds",
        // "back YARD" as two tokens. The START branch deliberately does NOT take
        // "yard" as a camera target — "watch the yard sale" must not open a lens —
        // but STOPPING a watch is not a device activation, and refusing to stop
        // the watch the user just started is the worse failure of the two.
        "yard", "yards",
    ];
    // Tokens that may sit between "watching" and its target without being the
    // target: determiners, pro-forms, and the same location modifiers the START
    // branch accepts ("stop watching the FRONT door").
    const NEUTRAL: &[&str] = &[
        "the", "this", "that", "my", "our", "your", "a", "an", "his", "her", "its",
        "their", "it", "them", "all", "any", "front", "back", "rear", "side",
        "main", "second", "other", "spare", "upstairs", "downstairs", "outside",
        "outdoor", "indoor", "basement", "patio", "garden", "kitchen", "living",
        "dining", "guest", "security", "entry", "sliding", "hall", "doorbell",
        "video", "live", "new", "old",
    ];
    // Adverbs and connectives that END the request ("stop watching now").
    const CLOSERS: &[&str] =
        &["now", "please", "again", "ok", "okay", "darwin", "for", "and", "then", "already"];
    // The stop verb has to be the utterance's own leading imperative. Without
    // that, "START the STOP WATCH" armed on its middle word and stopped a
    // running watch.
    if !vision_verb_in_command_position(lower, STOP_VERBS) {
        return false;
    }
    let mut it = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .peekable();
    let mut armed = false;
    let mut gap = 0usize;
    while let Some(w) = it.next() {
        if STOP_VERBS.contains(&w) {
            armed = true;
            gap = 0;
            continue;
        }
        // "stop the watch" / "end the watch" — the watch as a NOUN, which then has
        // to END the request: "end the WATCH PARTY at nine" is somebody's evening.
        if armed && w == "watch" {
            if it.peek().is_none_or(|n| CLOSERS.contains(n)) {
                return true;
            }
            armed = false;
            continue;
        }
        // "stop watching …" — the watch as a VERB, so read its object.
        if armed && w == "watching" {
            let mut seen = 0usize;
            loop {
                match it.next() {
                    // Nothing (more) followed: a bare "stop watching".
                    None => return true,
                    Some(n) if OBJECTS.contains(&n) || CLOSERS.contains(&n) => return true,
                    Some(n) if NEUTRAL.contains(&n) => {
                        seen += 1;
                        if seen > 4 {
                            break;
                        }
                    }
                    // A foreign object ("… so much tv", "… the news"): not our watch.
                    Some(_) => break,
                }
            }
            armed = false;
            continue;
        }
        if armed && gap < 3 && FILLERS.contains(&w) {
            gap += 1;
            continue;
        }
        armed = false;
    }
    false
}

/// Whether `lower` is a "what is ON (my|the) SCREEN" question — the OCR read's
/// question form.
///
/// WHAT WENT WRONG: the old arm was `contains("what") && contains("on") &&
/// (contains("screen") || contains("display"))`, three substrings that ordinary
/// English satisfies by accident: "on" hides in wr-ON-g, d-ON't, g-ON-e, and
/// "what's wrong with my screen", "i don't know what to do about my cracked
/// screen" and "what's going on with my display at work" all captured the user's
/// screen. Worse, `is_screen_read` then flagged them TRANSIENT, so DARWIN
/// neither answered them nor learned from them.
///
/// The fix is structural, not just whole-word: "on" must be the preposition that
/// governs the screen noun, with at most one determiner between them. That also
/// settles the one genuine idiom — "what was ON DISPLAY at the museum" is an
/// exhibition, "what's on THE display" is a screen — so a bare "on display"
/// needs its determiner while the fixed collocation "on screen" does not. The
/// glued "onscreen"/"fullscreen" are listed because the old `contains("screen")`
/// matched them and real speech uses them.
fn asks_what_is_on_screen(lower: &str) -> bool {
    const DETS: &[&str] = &["the", "my", "our", "your", "this", "that", "a"];
    // ONE adjective may sit between the determiner and the screen noun. WHAT WENT
    // WRONG: "what's on my SECOND screen" walked det -> "second" -> fell out of
    // the on-run before it ever reached "screen". This is the same
    // adjective-between-determiner-and-noun shape the watch branch had.
    const SCREEN_ADJS: &[&str] = &[
        "second", "third", "other", "main", "external", "laptop", "big", "left",
        "right", "primary", "whole", "entire",
    ];
    // WHAT WENT WRONG, in the first cut of THIS function: the guard was
    // `mentions_word(lower, "what")`, and "whats" is a SINGLE alphanumeric token
    // — dictation drops the apostrophe, so the canonical screen read arrives as
    // "whats on my screen" and the whole arm went dark, where the old
    // `contains("what")` had matched. Eight real phrasings died silently
    // ("whats on my screen", "tell me whats on the screen", "whats displayed on
    // the screen", …) and nothing downstream caught them: `lumen_is_read` tests
    // "what's on"/"what is on" only, and `describe_command` needs a describe verb.
    // The whole-word rule is right; the word list was one spelling short.
    if !crate::utterance::mentions_any_word(lower, &["what", "whats"]) {
        return false;
    }
    let mut it = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|x| !x.is_empty())
        .peekable();
    let mut in_on_run = false;
    let mut saw_det = false;
    let mut saw_adj = false;
    while let Some(w) = it.next() {
        if w == "onscreen" || w == "fullscreen" {
            return true;
        }
        if w == "on" {
            in_on_run = true;
            saw_det = false;
            saw_adj = false;
            continue;
        }
        if !in_on_run {
            continue;
        }
        if !saw_det && DETS.contains(&w) {
            saw_det = true;
            continue;
        }
        if !saw_adj && SCREEN_ADJS.contains(&w) {
            saw_adj = true;
            continue;
        }
        if w == "screen" || w == "screens" || (saw_det && (w == "display" || w == "displays")) {
            // The screen word has to END its phrase. A noun glued after it names
            // something else entirely — "what is on the display CASE at the
            // bakery", "what's on my screen PROTECTOR", "what's on the screen
            // DOOR" — and all three used to capture the screen.
            match it.peek() {
                None => return true,
                Some(n)
                    if VISION_OBJECT_CLOSERS.contains(n)
                        || VISION_OBJECT_TAILS.contains(n)
                        || *n == "that" =>
                {
                    return true
                }
                _ => {}
            }
        }
        in_on_run = false;
        saw_det = false;
        saw_adj = false;
    }
    false
}

/// Whether `lower` is the bare hands-free "read this / read that" — the most
/// common "read what's in front of me".
///
/// WHAT WENT WRONG: the old arms were `contains("read this")` and
/// `contains("read that")`, which is a two-word substring and therefore fires
/// inside "proofREAD THIS for me before I turn it in", and fires on every
/// past-tense sentence that happens to contain the pair: "i read this book last
/// night and loved it", "did you read that email i forwarded you", "she read
/// this to the kids", "can you read that back to me". Each one captured the
/// user's screen through TCC and was then flagged TRANSIENT, so DARWIN answered
/// none of them and remembered none of them.
///
/// Two locks, both structural: the read verb must be in COMMAND position (which
/// kills "proofread"/"already read"/"she read"/"i read"), and the this/that must
/// be the END of the request — nothing after it, or a directional tail ("read
/// this for me", "read that out", "read this to me"). An OBJECT after it ("read
/// this BOOK", "read that EMAIL", "read this MORNING") means the user is talking
/// about something else entirely.
///
/// WHAT WENT WRONG, AGAIN: the paragraph above listed "can you read that back to
/// me" as one of the sentences the two locks had closed — and it had not. "can"
/// is a modal, so `vision_verb_in_command_position` forgives the bare subject
/// "you" and the verb still reads as a command; then "back" was in OK_TAIL, so
/// the tail lock passed too. The user dictates a message, asks DARWIN to repeat
/// it, and instead the daemon fires a whole-screen ScreenCaptureKit OCR
/// (`read.screen`), answers "Reading your screen now, sir…", and — because the
/// dispatch is an else-if chain — never answers what was actually asked. "read
/// that back (to me)" is the REPEAT-WHAT-I-SAID idiom in English, never a request
/// to OCR the display, so "back" is not a directional tail and is no longer
/// accepted as one.
fn reads_this_or_that(lower: &str) -> bool {
    if !vision_verb_in_command_position(lower, &["read"]) {
        return false;
    }
    const OK_TAIL: &[&str] = &["for", "out", "aloud", "please", "now", "again"];
    let mut it = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty());
    for w in it.by_ref() {
        if w == "read" {
            break;
        }
    }
    match it.next() {
        Some("this") | Some("that") => {}
        _ => return false,
    }
    match it.next() {
        None => true,
        Some(w) if OK_TAIL.contains(&w) => true,
        // "read this to me"/"to us" is the request; "read this to the kids" is not.
        Some("to") => matches!(it.next(), Some("me") | Some("us")),
        _ => false,
    }
}

/// The on-screen CONTROL kinds a "where is / find / locate" request can name.
/// These are the only things Vision's structured OCR blocks can be matched
/// against; anything else the user is looking for is not on their screen.
const VISION_CONTROL_NOUNS: &[&str] = &[
    "button", "buttons", "icon", "icons", "field", "fields", "control", "controls",
    "link", "links", "tab", "tabs", "menu", "menus", "checkbox", "checkboxes",
    "toggle", "toggles", "dropdown", "dropdowns", "bar", "bars", "box", "boxes",
    "slider", "sliders", "panel", "panels", "pane", "scrollbar", "scrollbars",
    "arrow", "window", "windows", "dialog", "dialogs", "textbox", "switch",
    "option", "options", "setting", "settings", "gear", "gears",
];

/// Whether `phrase` — the body of a "where is / where's / find / locate …"
/// request, with the locate verb and its leading article already stripped — is
/// asking about something ON THE USER'S SCREEN.
///
/// WHAT WENT WRONG, twice, in opposite directions. Requiring NO control at all
/// made every where-is question in English a screen OCR: "where's my rolling
/// pin", "where's the coldest place in the world right now" and "where is the
/// Voyager probe now" all captured the screen, and `is_screen_read` then flagged
/// them TRANSIENT, so DARWIN answered none of them and remembered none of them.
/// Then requiring the LAST token of the whole phrase to be a control broke the
/// canonical locate, because a trailing prepositional phrase moves the head:
/// "where's the submit button ON MY SCREEN" ended at "screen", not "button", and
/// emitted no Vision op at all.
///
/// So two tests, and both have to pass:
///
///   1. THE HEAD, taken before any real prepositional phrase, is a control kind.
///      What makes a preposition "real" is the noun after it: "sign IN button"
///      and "log IN field" are control NAMES and must not be split, while
///      "button ON my screen" starts a new phrase.
///
///   2. THE PLACE, when the user named one WITH a determiner, is a screen. "on
///      THE tv", "on THE router", "on MY headphones" and "on MY shirt" are
///      physical objects in the room — we cannot locate a control on any of them,
///      and the query we used to emit for them was garbage. A place named WITHOUT
///      a determiner is an application ("in Photoshop", "in Chrome"), which is a
///      UI context rather than an object, so it is left alone.
fn locate_targets_a_control(phrase: &str) -> bool {
    const PREPS: &[&str] = &[
        "on", "in", "at", "of", "under", "over", "above", "below", "near",
        "inside", "within", "from", "by", "beside", "behind",
        // "for" was missing, so "where's the CHECKBOX for terms" read its head as
        // "terms" and located nothing.
        "for",
    ];
    const DETS: &[&str] = &[
        "the", "my", "our", "your", "this", "that", "these", "those", "a", "an",
        "his", "her", "its", "their",
    ];
    // Adverbs a spoken question trails off with; never the thing asked for.
    const TAILS: &[&str] =
        &["now", "please", "again", "right", "exactly", "currently", "here", "today"];
    // The only PLACE we can look.
    const SCREENS: &[&str] = &[
        "screen", "screens", "display", "displays", "monitor", "monitors",
        "desktop", "page", "pages", "window", "windows", "browser", "tab", "tabs",
        "dialog", "toolbar", "sidebar", "menu", "panel", "view", "form", "site",
        "website", "app", "ui",
    ];
    let mut it = phrase
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .peekable();
    let mut head: Option<&str> = None;
    let mut place: Option<&str> = None;
    let mut in_place = false;
    let mut place_has_det = false;
    while let Some(w) = it.next() {
        if !in_place && PREPS.contains(&w) {
            if it.peek().is_some_and(|n| VISION_CONTROL_NOUNS.contains(n)) {
                continue;
            }
            in_place = true;
            continue;
        }
        if in_place {
            if PREPS.contains(&w) || TAILS.contains(&w) {
                continue;
            }
            if DETS.contains(&w) {
                place_has_det = true;
                continue;
            }
            place = Some(w);
            continue;
        }
        if TAILS.contains(&w) {
            continue;
        }
        head = Some(w);
    }
    if !head.is_some_and(|h| VISION_CONTROL_NOUNS.contains(&h)) {
        return false;
    }
    if place_has_det {
        return place.is_some_and(|p| SCREENS.contains(&p));
    }
    true
}


/// Map a spoken utterance to a Vision command, or None when it is not a Vision
/// control phrase (the turn falls through to normal routing). Deterministic and
/// pure so the mapping is unit-tested without a socket, a running app, or the
/// classifier. Order matters: specific ops (watch start/stop, analyze file,
/// sensitivity, status) are matched before the broad launch.
///
/// Recognized phrases (case-insensitive, whole lowercased utterance):
///   - "watch the door|room|camera"                 -> watch.start {camera}
///   - "watch the screen|display"                   -> watch.start {screen}
///   - "stop watching" / "stop the watch"           -> watch.stop
///   - "analyze this video" / "analyze <file>.mp4"  -> analyze.file {path}
///   - "what's on my screen" / "read my screen" / "read this"
///     -> read.screen (OCR; TRANSIENT)
///   - "where's the <X> button" / "find the <X> button" / "locate the <X>"
///     -> read.screen {query:<X>} (LOCATE, read-only)
///   - "what do you see" / "who is there" / "anyone there"
///     -> status (presence snapshot)
///   - "set sensitivity to <0..1 | a percent>"      -> set.sensitivity {value}
///   - "open/launch/start vision"                   -> Launch
pub fn vision_command(text: &str) -> Option<VisionCommand> {
    let lower = text.to_lowercase();

    // --- watch lifecycle (specific before the broad launch) ----------------
    // STOP first so "stop watching the door" is unambiguous.
    if watch_stop_targets_the_watch(&lower)
        || camera_off_command(&lower)
        || (mentions_vision(&lower)
            && crate::utterance::mentions_any_word(&lower, &["watch", "watching"])
            && vision_verb_in_command_position(&lower, &["stop", "end", "quit", "cancel"]))
    {
        return Some(VisionCommand::Op(op_watch_stop()));
    }
    // START. THE SOURCE IS THE GATE: "watch the screen|display|monitor" -> screen,
    // "watch the door|room|camera|doorway|driveway|…" -> camera, and anything else
    // is not a Vision command at all. The verb alone no longer starts anything and
    // there is NO camera default left to fall into — see `watch_start_source` for
    // the 39 ordinary sentences that used to open a lens here.
    if let Some(source) = watch_start_source(&lower) {
        return Some(VisionCommand::Op(op_watch_start(source)));
    }

    // --- analyze a video file ----------------------------------------------
    // "analyze this video", "analyze <name>.mp4", "analyze the video clip".
    // The verb must be WHOLE-WORD and IN COMMAND POSITION. WHAT WENT WRONG:
    // `contains("analyze")` also fires inside "analyzed"/"analyzes", so "the
    // doctor analyzed the video of my swallowing" and "our team analyzes the
    // clip every monday" were both orders to hand a file to the Vision app.
    if vision_verb_in_command_position(&lower, &["analyze", "analyse"])
        && (lower.contains("video") || lower.contains("clip") || extract_video_path(&lower).is_some())
    {
        // A named file (…/foo.mp4) is forwarded verbatim. A bare "analyze this
        // video" (no filename) forwards an EMPTY path: Vision's Op.swift requires
        // a non-empty path, so it decodes to .unknown and the Pipeline reports a
        // clean vision.error — i.e. the app cleanly says it has no file to run,
        // it never crashes and never guesses. (The persona then asks which file.)
        let path = extract_video_path(&lower).unwrap_or_default();
        return Some(VisionCommand::Op(op_analyze_file(&path)));
    }

    // --- sensitivity -------------------------------------------------------
    // A MUTATION of the app's config, so the set VERB has to be in command
    // position AND the sensitivity has to be the thing it is setting.
    //
    // WHAT WENT WRONG: the old gate accepted ANY utterance containing "sensitiv…"
    // plus the substring "to" — and `extract_sensitivity` reads a bare
    // "high"/"low" as a value, so "she is sensitive to high pollen counts" and "i
    // am sensitive to loud noises" rewrote the motion threshold. Requiring the
    // verb alone is not enough either: "set a high sensitivity ALARM at the shop"
    // puts the verb in command position and still means nothing to Vision, which
    // is why the object HEAD (not the mere presence of the word) is the test.
    //
    // AND WHAT WENT WRONG IN THE FIX ITSELF: an earlier cut of this gate DELETED
    // the old `set|to|at` conjunct and put VISION_SENSITIVITY_VERBS in its place.
    // That verb list carries raise/lower/increase/decrease — and
    // `extract_sensitivity` reads "lower" as "low" and returns 0.25 — so the VERB
    // SUPPLIED ITS OWN VALUE and a bare "lower the sensitivity", "lower the mic
    // sensitivity", "lower the sensitivity on my hearing aid" became config
    // WRITES the old code never made. A gate on the ONE state-mutating op in this
    // seam must not be able to widen. So the value-bearing "set"/"to"/"at" is
    // required again, now as a WHOLE WORD — strictly narrower than the substring
    // test it replaces, which is what keeps this branch a subset of the old one.
    if crate::utterance::mentions_any_word(&lower, &["sensitivity", "sensitive"])
        && crate::utterance::mentions_any_word(&lower, &["set", "to", "at"])
        && vision_verb_in_command_position(&lower, VISION_SENSITIVITY_VERBS)
        && vision_object_head(&lower, VISION_SENSITIVITY_VERBS)
            .is_some_and(|(head, _)| head == "sensitivity" || head == "sensitive")
    {
        if let Some(value) = extract_sensitivity(&lower) {
            return Some(VisionCommand::Op(op_set_sensitivity(value)));
        }
    }

    // --- HANDWRITING read (#28) / DOCUMENT scan (#29) ----------------------
    // "read this handwriting" / "read the whiteboard" / "scan this document".
    // Both are READ-ON-REQUEST OCR variants of the user's OWN camera/screen
    // (TCC-gated), DISTINCT from the plain on-screen OCR below — so they are
    // matched FIRST (a "read this handwriting" must reach the handwriting
    // recognizer, a "scan this document" the camera scanner, not the generic
    // screen OCR). The recognized text is SENSITIVE + TRANSIENT (`is_screen_read`
    // covers these too). READ-ONLY: transcribes glyphs, never an identity.
    if let Some(op) = handwriting_document_op(&lower) {
        return Some(VisionCommand::Op(op));
    }

    // --- screen READ (OCR) — "what's on my screen" / "read my screen" / -----
    // "read this" / "where's the <X> button". DISTINCT from "watch the screen"
    // (a continuous detection watch) and from the presence STATUS below: this is
    // a one-shot OCR read of the user's OWN screen via ScreenCaptureKit, gated by
    // macOS TCC. The recognized text is SENSITIVE (it can contain on-screen
    // passwords/messages) and TRANSIENT — see `is_screen_read` + main.rs, which
    // keep it out of lifelong memory / optimizer traces. READ-ONLY: a where-is
    // query LOCATES a control, it never clicks. Checked before the presence
    // status so "what's on my screen" is an OCR read, not a presence snapshot.
    if let Some(op) = screen_read_op(&lower) {
        return Some(VisionCommand::Op(op));
    }

    // --- presence status ("what do you see" / "who is there") --------------
    // DEFENSIVE-ONLY: "who is there" is a PRESENCE query, not identity — it maps
    // to the same generic status snapshot as "what do you see". There is no
    // face-match / name-lookup op anywhere in the contract.
    if lower.contains("what do you see")
        || lower.contains("what can you see")
        || lower.contains("who is there")
        || lower.contains("who's there")
        || lower.contains("anyone there")
        || lower.contains("anybody there")
        || lower.contains("someone there")
        || lower.contains("somebody there")
        || lower.contains("what are you seeing")
        // MEASURED RECALL MISS: "what is the vision app doing" reached nothing.
        // The status snapshot IS the answer to that question, and the utterance
        // NAMES the app — the strongest anchor in this whole classifier.
        //
        // EVERY form here says "vision APP", and the word app is load-bearing.
        // MEASURED HIJACK (adversary pass): a bare "vision doing" / "status of
        // vision" captured "how is my vision doing since the surgery", "is my
        // vision doing any better" and "what's the status of vision therapy" —
        // three sentences about EYESIGHT, all CLEAN at HEAD, each answered with
        // a camera-app status snapshot. No probe ever needed the bare forms.
        || lower.contains("vision app doing")
        || lower.contains("vision app status")
        || lower.contains("status of the vision app")
        || lower.contains("status of vision app")
    {
        return Some(VisionCommand::Op(op_status()));
    }

    // --- launch ------------------------------------------------------------
    // Only when the utterance names Vision AND carries an open-class verb.
    if mentions_vision(&lower)
        && (lower.contains("open")
            || lower.contains("launch")
            || lower.contains("start")
            || lower.contains("bring up")
            || lower.contains("fire up"))
    {
        return Some(VisionCommand::Launch);
    }

    None
}

/// Whether `lower` contains `word` as a STANDALONE token (alnum boundaries), so
/// "vision" matches in "open vision" but not inside "television"/"revision".
fn contains_word(lower: &str, word: &str) -> bool {
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == word)
}

/// Extract a video file path/name from an "analyze <…>.<ext>" phrase. Returns
/// the token that carries a known video extension (mp4/mov/m4v/avi), forwarded
/// verbatim so the app resolves it against its own videos/input dir. None when
/// no such token is present (a bare "analyze this video"). The token is taken
/// from the ORIGINAL-case text via a case-insensitive extension match so a
/// path's case survives (file systems are case-sensitive).
fn extract_video_path(lower: &str) -> Option<String> {
    lower
        .split(|c: char| c.is_whitespace())
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '/' && c != '_' && c != '-'))
        .find(|w| {
            let lw = w.to_lowercase();
            lw.ends_with(".mp4")
                || lw.ends_with(".mov")
                || lw.ends_with(".m4v")
                || lw.ends_with(".avi")
        })
        .map(|w| w.to_string())
}

/// Extract a sensitivity value in 0..=1 from a "set sensitivity to <X>" phrase.
/// Accepts a bare 0..1 float ("0.7"), a percent ("70 percent"/"70%"), or the
/// words low/medium/high. None when no value is present. Clamped to 0..=1.
///
/// WHAT WENT WRONG: the word arms ran FIRST and matched with `contains`, so the
/// VERB supplied the value and the user's explicit number was never read. "lower
/// the sensitivity to 0.1" wrote 0.25 — 2.5x what was asked for, on the ONE
/// state-mutating op in the whole Vision seam — and "lower the sensitivity to 30
/// percent" wrote the same 0.25. The gate above already knows about this reading
/// ("`extract_sensitivity` reads 'lower' as 'low'"), but the earlier fix only
/// tightened the GATE; the extractor was left alone, so every utterance the
/// tightened gate now admits with the verb "lower" still got the wrong value. The
/// acknowledgment says nothing about the value, so the user could not tell.
///
/// Three passes, in this order:
///   1. a number INTRODUCED BY A VALUE CONNECTOR ("to 0.1", "at 70%") — what the
///      user explicitly asked for always beats a word the verb happens to carry;
///   2. the word forms, matched WHOLE-WORD so "below"/"allow"/"slow" cannot
///      supply a threshold (the comparatives are listed by hand because "set the
///      sensitivity higher/lower" is a real supported phrasing — see
///      VISION_OBJECT_TAILS — that bare "high"/"low" would drop);
///   3. any remaining number, for the connector-free "set sensitivity 0.3".
///
/// Pass 1 is anchored on the connector rather than simply run first so that a
/// stray count elsewhere in the sentence ("set the sensitivity to high on camera
/// 2") cannot be mistaken for the threshold.
fn extract_sensitivity(lower: &str) -> Option<f64> {
    // Percent if the utterance carries a '%' or the word "percent"; a value > 1
    // is also read as a percent (nobody means a sensitivity of 70.0).
    let is_percent = lower.contains('%') || lower.contains("percent");
    let as_value = |n: f64| {
        let v = if is_percent || n > 1.0 { n / 100.0 } else { n };
        v.clamp(0.0, 1.0)
    };
    // The tokens, with '%' as a boundary so "70%" yields "70".
    let toks: Vec<&str> = lower
        .split(|c: char| c.is_whitespace() || c == '%')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();

    // (1) A number the user introduced with a connector.
    const VALUE_CONNECTORS: &[&str] = &["to", "at", "of"];
    for w in toks.windows(2) {
        if VALUE_CONNECTORS.contains(&w[0]) {
            if let Ok(n) = w[1].parse::<f64>() {
                return Some(as_value(n));
            }
        }
    }

    // (2) The word forms, WHOLE-WORD.
    if crate::utterance::mentions_any_word(lower, &["low", "lower", "lowest", "min", "minimum"]) {
        return Some(0.25);
    }
    if crate::utterance::mentions_any_word(lower, &["medium", "normal", "mid", "moderate"]) {
        return Some(0.5);
    }
    if crate::utterance::mentions_any_word(lower, &["high", "higher", "highest", "max", "maximum"])
    {
        return Some(0.85);
    }

    // (3) Any remaining number ("set sensitivity 0.3").
    for t in toks {
        if let Ok(n) = t.parse::<f64>() {
            return Some(as_value(n));
        }
    }
    None
}

// The op-string builders — EXACT Vision Op.swift wire form: every op carries the
// `"type":"op"` envelope (unlike Silicon Canvas's bare ops). serde_json builds
// them so a path/source with a quote can never break the JSON framing.

fn op_watch_start(source: &str) -> String {
    json!({"type": "op", "op": "watch.start", "source": source}).to_string()
}
fn op_watch_stop() -> String {
    json!({"type": "op", "op": "watch.stop"}).to_string()
}
fn op_analyze_file(path: &str) -> String {
    json!({"type": "op", "op": "analyze.file", "path": path}).to_string()
}
fn op_set_sensitivity(value: f64) -> String {
    json!({"type": "op", "op": "set.sensitivity", "value": value}).to_string()
}
fn op_status() -> String {
    json!({"type": "op", "op": "status"}).to_string()
}

/// The Vision Sound-Analysis op (Op.swift wireName "classify.sound", task #15).
/// Classifies ONE supplied audio CLIP at `path` through Apple Sound Analysis (the
/// built-in ~300-class `SNClassifierIdentifier.version1`, on-device/ANE-eligible)
/// and emits a `vision.sound` readout with the top sound classes {label,
/// confidence}. `path` is REQUIRED (the host names the confined clip the daemon
/// wrote from its captured buffer); a classify.sound WITHOUT a non-empty path
/// decodes to `.unknown` Swift-side and the app refuses to classify — it NEVER
/// opens the mic. Mirrors `describe.capture`'s path-required pattern. serde_json
/// builds the line so a path with a quote can never break the JSON framing. ONLY
/// the sound-class LABELS leave the op; the AUDIO never leaves the device.
fn op_classify_sound(path: &str) -> String {
    json!({"type": "op", "op": "classify.sound", "path": path}).to_string()
}

/// The Vision HANDWRITING/WHITEBOARD read op (#28, Op.swift wireName
/// "read.handwriting"). Captures ONE frame from a TCC-gated source and runs the
/// handwriting recognizer (VNRecognizeTextRequest, .accurate + language
/// correction — the config best for handwriting/whiteboard text), emitting a
/// `vision.screen` readout (tagged read_kind=handwriting) with the recognized
/// LINES + boxes. The default source is `.camera` (handwriting/whiteboard is most
/// naturally read off the camera). A `screen` request stamps the screen source.
/// READ-ON-REQUEST + READ-ONLY: it transcribes glyphs, never an identity, never a
/// click. The recognized text is SENSITIVE + TRANSIENT (see `is_screen_read` +
/// main.rs). serde_json builds the line so a source token can never break framing.
fn op_read_handwriting(source: Option<&str>) -> String {
    match source {
        Some(s) if s == "screen" || s == "camera" => {
            json!({"type": "op", "op": "read.handwriting", "source": s}).to_string()
        }
        // Default (no source) -> the app's .camera default; keep the line minimal.
        _ => json!({"type": "op", "op": "read.handwriting"}).to_string(),
    }
}

/// The Vision camera DOCUMENT-SCANNER op (#29, Op.swift wireName "scan.document").
/// Captures ONE frame from a TCC-gated source (default `.camera`) and runs the
/// document scanner (VNDetectDocumentSegmentationRequest -> CIPerspectiveCorrection
/// -> VNRecognizeTextRequest), emitting a `vision.screen` readout (tagged
/// read_kind=document) with the text off the CORRECTED page plus the HONEST
/// document-detected bool. When NO document is found, the readout is honestly
/// empty (never a fabricated page). READ-ON-REQUEST + READ-ONLY: transcribes
/// glyphs, never an identity. The recognized text is SENSITIVE + TRANSIENT.
fn op_scan_document(source: Option<&str>) -> String {
    match source {
        Some(s) if s == "screen" || s == "camera" => {
            json!({"type": "op", "op": "scan.document", "source": s}).to_string()
        }
        _ => json!({"type": "op", "op": "scan.document"}).to_string(),
    }
}

/// Map a lowercased utterance to a `read.handwriting` (#28) or `scan.document`
/// (#29) op line, or None when it is neither. PURE + unit-tested (no socket, no
/// app). Both are READ-ON-REQUEST OCR variants of the user's OWN camera/screen,
/// DISTINCT from the plain on-screen OCR (`screen_read_op`) — checked BEFORE it so
/// "read this handwriting" is the handwriting recognizer, not the generic screen
/// OCR. Recognized intents:
///   - "read this handwriting" / "read the whiteboard" / "what does this say"
///     (with a handwriting/whiteboard/note cue)            -> read.handwriting
///   - "scan this document" / "scan the page" / "scan this receipt"  -> scan.document
///     The recognized text is SENSITIVE + TRANSIENT (`is_screen_read` covers these).
fn handwriting_document_op(lower: &str) -> Option<String> {
    // SCAN a document/page/receipt with the camera (#29). Checked first so "scan
    // this document" never falls into a handwriting/OCR read.
    //
    // WHAT WENT WRONG: this was `contains("scan")` AND a substring document noun,
    // and this branch runs BEFORE the screen-read seam, so it was the FIRST camera
    // default in the function. "scan" hides inside SCANdal, "form" inside
    // perFORMance and "paper" inside PAPERwork, so "there was a scandal about the
    // paperwork" and "i need to scan the performance report" both opened the
    // camera. Now the verb must be whole-word AND in command position (a past-tense
    // "he … scanned the invoice" is a report, not an order), and the document noun
    // must be the HEAD of the verb's own object — which is what tells "scan the
    // page" from "scan the qr code on the invoice".
    const SCAN_VERBS: &[&str] = &["scan", "scanning"];
    const DOCUMENT_NOUNS: &[&str] = &[
        "document", "documents", "page", "pages", "receipt", "receipts",
        "paper", "papers", "invoice", "invoices", "form", "forms",
    ];
    let names_scan = vision_verb_in_command_position(lower, SCAN_VERBS);
    // A PARTITIVE tail says WHICH page, not a different thing: "scan the second
    // page OF the contract" is still a page scan, and it stopped working.
    // `vision_object_head` REJECTS a partitive object outright, and on the
    // camera-WATCH branch it must — there the noun after "of" is the real
    // referent ("watch the entrance OF the movie"), and letting a camera target
    // through on the noun BEFORE it is the exact mistake that gate exists to
    // stop. A scan is the other way round, so this arm — and only this arm —
    // reads the object up to the partitive. It cannot widen past the old code:
    // that fired on the bare substrings "scan" + a document noun anywhere.
    let object_span = lower.split(" of ").next().unwrap_or(lower);
    let mentions_document = vision_object_head(object_span, SCAN_VERBS)
        .is_some_and(|(head, _)| DOCUMENT_NOUNS.contains(&head));
    if names_scan && mentions_document {
        // A document is scanned with the camera by default; honor an explicit
        // "on screen" / "on my display" request.
        let source = if lower.contains("screen") || lower.contains("display") {
            Some("screen")
        } else {
            None // -> the app's .camera default
        };
        return Some(op_scan_document(source));
    }

    // READ HANDWRITING / a whiteboard / a handwritten note (#28). A read/transcribe
    // verb (or "what does this say") plus a handwriting/whiteboard cue.
    let mentions_handwriting = lower.contains("handwriting")
        || lower.contains("handwritten")
        || lower.contains("whiteboard")
        || lower.contains("white board")
        || lower.contains("hand writing");
    // The read verb must be WHOLE-WORD and IN COMMAND POSITION.
    //
    // WHAT WENT WRONG: `contains("read")` fires inside alREADy, so "i already
    // erased the whiteboard" opened the camera, and a plain whole-word "read" is
    // not enough either — "we read the whiteboard notes yesterday" is somebody
    // REPORTING that they read it, in the past tense English spells exactly like
    // the imperative. This is the second of the two camera defaults that run
    // ahead of the screen-read seam.
    let names_read = vision_verb_in_command_position(
            lower,
            &["read", "reread", "transcribe", "transcribing"],
        )
        // "what does this say" / "what does this handwriting say" / "what does it
        // say" — a "what does … say" question over the handwriting cue is a read.
        || (lower.contains("what does") && mentions_word(lower, "say"))
        || lower.contains("what's written")
        || lower.contains("whats written");
    if mentions_handwriting && names_read {
        // Handwriting/whiteboard is read off the camera by default; honor an
        // explicit "on screen" request (e.g. a whiteboard shared on screen).
        let source = if lower.contains("screen") || lower.contains("display") {
            Some("screen")
        } else {
            None // -> the app's .camera default
        };
        return Some(op_read_handwriting(source));
    }

    None
}

/// The Vision OCR screen-read op (Op.swift wireName "read.screen"). Captures ONE
/// frame from the user's OWN .screen source (ScreenCaptureKit, TCC-gated), runs
/// the .text OCR detector, structures the blocks, and emits a `vision.screen`
/// event carrying the recognized text + control candidates. The default source
/// is `.screen` (the on-wire `{"type":"op","op":"read.screen"}` form), so we do
/// not stamp a `source` field — keeping the line byte-identical to the FROZEN
/// default the Swift `testFrozenOpWireNamesUnchanged` pins. An optional `query`
/// rides along ONLY for a "where is <X>" locate request (READ-ONLY: locate, not
/// click). serde_json builds the line so a query with a quote can never break
/// the JSON framing.
fn op_read_screen(query: Option<&str>) -> String {
    match query {
        Some(q) if !q.trim().is_empty() => {
            json!({"type": "op", "op": "read.screen", "query": q.trim()}).to_string()
        }
        _ => json!({"type": "op", "op": "read.screen"}).to_string(),
    }
}

/// Is the screen this utterance names SOMEBODY ELSE'S DEVICE?
///
/// ONE IMPLEMENTATION, TWO CALLERS — [`lumen_is_read`] and [`screen_read_op`].
/// They are two gates onto the SAME consequence (a capture of the owner's screen,
/// read back aloud) and they overlap by design, so a veto in one alone moves the
/// hijack instead of closing it. That is not hypothetical here: with only Lumen
/// narrowed, "read the display on the thermostat" and "read the screen on the
/// treadmill" still captured the screen — through Vision — and the owner would
/// have seen no change at all.
///
/// MEASURED at HEAD, all capturing: "what buttons are on the display of the
/// microwave", "what controls are on the display of the oven", "read the display
/// on the thermostat", "what buttons are on the dashboard display of the car",
/// "what fields are on the display of the printer", "read the screen on the
/// treadmill", "what buttons are on the screen of the coffee machine", "list the
/// controls on the elevator display" — eight of the twelve sentences probed in
/// this shape, against four real phrasings that must keep working.
///
/// `is_screen_read` calls a screen read the thing that "can surface on-screen
/// passwords/messages", and the readout is SPOKEN — so an owner asking about a
/// kitchen appliance, out loud, with company in the room, had their screen read
/// back to them. A disclosure cannot be undone through the channel that caused it.
///
/// "display" and "screen" are ordinary words for ANY device's readout, so the noun
/// cannot be the discriminator. THE POSSESSOR IS: real phrasings leave the screen
/// unowned ("read the screen") or own it to THIS machine ("my screen", "on this
/// screen"), while an appliance phrasing always names its owner in an of/on
/// possessor. So when a possessor is present it must name this machine or one of
/// its own surfaces.
///
/// ALLOWLIST, NOT AN APPLIANCE DENYLIST. Appliances are an open class, and an
/// open-class denylist is precisely the guard that reads as though it works.
/// "What a screen may belong to" is short and closed. Being incomplete on this
/// side can only leave a hijack open — it can never open a new one — and the
/// failure mode is a wasted sentence rather than a capture.
fn reads_another_devices_display(lower: &str) -> bool {
    const POSSESSIVE: &[&str] = &[
        " of the ", " of a ", " of my ", " of this ", " on the ", " on a ", " on my ",
        " on this ",
    ];
    // Matched on the FIRST token of the possessor, because that is where its owner
    // sits: "the display OF THE MICROWAVE" and "the ELEVATOR display" both name
    // their device there, while "the settings page" and "the login screen" name a
    // qualifier. Matching the possessor's HEAD noun instead would admit "the
    // elevator display" — its head IS "display" — and lose the fix, so the
    // qualifiers are enumerated here on the ALLOW side.
    const OWN_SURFACE: &[&str] = &[
        // This machine.
        "mac", "macbook", "computer", "laptop", "machine", "desktop", "monitor", "system",
        // Its own surfaces.
        "screen", "display", "window", "page", "dialog", "app", "toolbar", "sidebar",
        "form", "browser", "tab", "panel", "ui", "interface",
        // LUMEN'S OWN CONTROL VOCABULARY — the list that makes an utterance a
        // Lumen read in the first place. `lumen_mentions_control_noun` is
        // button / link / tab / checkbox / field / menu / control / icon, and
        // FIVE of those eight were missing here, so the veto was eating the
        // capability it exists to protect. MEASURED at this revision, all four
        // reaching a Lumen read at 7731042 and NOTHING after: "read the label on
        // the button", "read the labels on the checkboxes", "read the labels on
        // the icons", "read me the labels on the tabs", plus "read the options on
        // the dropdown menu". A screen read of the owner's own screen is the
        // capability; refusing it is not a safe default, it is the feature off.
        //
        // These cannot admit an appliance, which is why they are safe to restore:
        // "the button", "the checkbox", "the icon" name PARTS OF A SCREEN, never
        // something that HAS a screen. Re-proved on the appliance side rather
        // than asserted — see `a_restored_control_noun_does_not_carry_an_
        // appliance_past_the_veto`, whose sentences put a restored word in the
        // FIRST possessor and the device in the SECOND.
        "button", "checkbox", "check box", "control", "field", "icon", "link",
        "dropdown", "label",
        // Qualifiers that still name one of THIS machine's surfaces.
        "settings", "login", "log", "sign", "lock", "home", "start", "menu", "options",
        "preferences", "search", "checkout", "payment", "current", "active", "front",
        // WHICH of this machine's screens. "what's on my second screen" is a
        // harvested capability-index phrase, and its test went red on the first
        // draft of this list — a positional qualifier names a monitor, not another
        // device.
        "second", "first", "third", "other", "main", "primary", "secondary",
        "external", "extra", "left", "right", "top", "bottom", "big", "small",
    ];
    POSSESSIVE.iter().any(|p| {
        lower.match_indices(p).any(|(i, _)| {
            let tail = &lower[i + p.len()..];
            !OWN_SURFACE.iter().any(|s| {
                tail.strip_prefix(s).is_some_and(|r| {
                    // ...IN THE PLURAL TOO. Every control noun above is one the
                    // owner says in the plural far more often than the singular
                    // ("read the labels on the ICONS"), and a singular-only match
                    // refused all of them: "icons" leaves "s" and "checkboxes"
                    // leaves "es", both alphanumeric, so the word-boundary test
                    // rejected its own entry. Bounded to those two suffixes, and
                    // it cannot re-open a capture: no appliance name is an
                    // own-surface word plus s/es (there is no "screens" or
                    // "buttones" that is a device), so the only thing this admits
                    // is the plural of something already allowed.
                    let r = r.strip_prefix("es").or_else(|| r.strip_prefix('s')).unwrap_or(r);
                    !r.starts_with(|c: char| c.is_alphanumeric())
                })
            })
        })
    })
}

/// Map a lowercased utterance to a `read.screen` op line, or None when it is not
/// a screen-read request. PURE so the mapping is unit-tested without a socket or
/// a running app. Recognized intents:
///   - "what's on my screen" / "what is on screen" / "read my screen" /
///     "read the screen" / "read this" / "read what's on screen"  -> read.screen
///   - "where's the <X> button" / "where is the submit button" / "find the
///     <X> button" / "locate the <X>"                              -> read.screen{query:<X>}
///     A where-is query carries the control phrase so the app's structuring can
///     LOCATE (not click) the best-matching block.
fn screen_read_op(lower: &str) -> Option<String> {
    // NOT SOMEBODY ELSE'S DEVICE. Shared with Lumen's read arm so the two gates
    // onto this capture cannot drift — see [`reads_another_devices_display`].
    if reads_another_devices_display(lower) {
        return None;
    }
    // Where-is a control: "where is/where's the <X> button", "find the <X>
    // button", "locate the <X>". The query is the control phrase; the app
    // locates it READ-ONLY (returns its box/center, never a click).
    if let Some(query) = extract_where_is_query(lower) {
        return Some(op_read_screen(Some(&query)));
    }
    // Plain screen read. "read this" alone is a screen read (the most common
    // hands-free "read what's in front of me"); "read my/the screen", "what's
    // on (my) screen", "read what's on screen" all map here too. Guarded so a
    // continuous "watch the screen" (handled above) never reaches this.
    const SCREEN_WORDS: &[&str] =
        &["screen", "screens", "display", "displays", "onscreen", "fullscreen"];
    let mentions_screen = crate::utterance::mentions_any_word(lower, SCREEN_WORDS);
    let read_screen = (mentions_screen
        && vision_verb_in_command_position(lower, &["read", "reread"]))
        || asks_what_is_on_screen(lower)
        || reads_this_or_that(lower);
    if read_screen {
        return Some(op_read_screen(None));
    }
    None
}

/// Extract the control phrase from a "where is the <X> button / find the <X> /
/// locate the <X>" locate request, lowercased. Returns the trimmed phrase (e.g.
/// "submit", "sign in") or None when the utterance is not a where-is request.
/// PURE + unit-tested. READ-ONLY semantics: this only NAMES the control to
/// locate; nothing here (or downstream) clicks it.
fn extract_where_is_query(lower: &str) -> Option<String> {
    // WHAT WENT WRONG: `contains("locate")` fires inside reLOCATEd / alLOCATEd /
    // disLOCATEd and `contains("find")` inside FINDings, so "they relocated the
    // office to the fourth floor" and "the findings mention every button" were
    // both screen captures.
    let is_locate = lower.contains("where is")
        || lower.contains("where's")
        || mentions_word(lower, "locate")
        || (mentions_word(lower, "find")
            && crate::utterance::mentions_any_word(lower, &["button", "buttons"]));
    if !is_locate {
        return None;
    }
    // Pull the phrase between a leading article and a trailing "button"/control
    // noun. Strip the locate verb + article, then drop a trailing control noun
    // so "where's the submit button" -> "submit", "find the sign in button" ->
    // "sign in", "locate the settings icon" -> "settings".
    let mut s = lower;
    for lead in [
        "where is the ", "where's the ", "where is ", "where's ", "locate the ",
        "locate ", "find the ", "find ",
    ] {
        if let Some(idx) = s.find(lead) {
            s = &s[idx + lead.len()..];
            break;
        }
    }
    // A locate is a SCREEN question only when an on-screen CONTROL is the HEAD of
    // what is being asked for, in a PLACE we can actually look — see
    // `locate_targets_a_control` for the two tests and the two ways an earlier
    // cut of this gate got them wrong.
    let mut phrase = s.trim();
    if !locate_targets_a_control(phrase) {
        return None;
    }
    for tail in [" button", " control", " icon", " field", " link", " tab", " menu", "?"] {
        if let Some(stripped) = phrase.strip_suffix(tail) {
            phrase = stripped.trim();
        }
    }
    // Also drop a lone trailing "button"/"on the screen" remnant.
    let phrase = phrase
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .trim();
    if phrase.is_empty() || phrase.len() > 64 {
        return None;
    }
    Some(phrase.to_string())
}

/// Whether an utterance is a Vision SCREEN-READ request (an OCR read of the
/// user's own screen). PUBLIC so the pipeline (main.rs) can keep the result
/// TRANSIENT: a screen read can surface on-screen passwords/messages, so its
/// utterance + acknowledgment must NOT seed lifelong memory (fact extraction)
/// or optimizer traces. Pure over `screen_read_op`, so this and the routing
/// agree by construction — anything that maps to a `read.screen` op is transient.
pub fn is_screen_read(text: &str) -> bool {
    let lower = text.to_lowercase();
    // The plain on-screen OCR read, PLUS the handwriting (#28) / document-scan
    // (#29) reads — all three surface SENSITIVE recognized text (a handwritten
    // note / a scanned page can carry private content just like an on-screen
    // password/message), so all three must be kept TRANSIENT (off lifelong memory
    // / optimizer traces). Agree-by-construction with the routing: anything that
    // maps to one of these ops is flagged transient here. The LUMEN read arm
    // (#45, "read me the buttons / what's on screen") is ALSO a screen read — it
    // surfaces the on-screen CONTROL labels — so it is unioned in for the same
    // transience (the ACT arm is NOT a read and is intentionally excluded).
    screen_read_op(&lower).is_some()
        || handwriting_document_op(&lower).is_some()
        || matches!(lumen_command(text), Some(LumenCommand::Read))
}

// ===========================================================================
// LUMEN (#45) — SCREEN-NARRATION + hands-free VOICE-NAVIGATION dispatch. Maps
//   (a) "read me the screen / the buttons / what's on screen" -> the READ-ONLY
//       Vision `read.screen` locate + Lumen's control narration (through the
//       speech path); the async readout is remembered (lumen::remember_readout at
//       integration) so a follow-up can select over it, AND
//   (b) "click / press / tap the <ordinal|name>" -> lumen::resolve_voice_action
//       over the remembered controls -> the UNCHANGED, per-action-gated
//       `ui_actuate` CAPSTONE (via anthropic::execute_tool, the SAME entry a live
//       tool call uses). Lumen only SELECTS the one target + builds the request;
//       the capstone still owns EVERY gate (the pure single-action planner, the
//       consequential spoken confirm PER ACTION, the master switch, voice-id, and
//       `!lockdown`). Lumen weakens, bypasses, and re-implements NONE of it.
//
// CONSERVATIVE by construction: the ACT arm anchors on unambiguous UI-actuation
// verbs (a bare "click"/"tap", or "press"/"push" WITH a control noun / ordinal —
// so "press play" / "push harder" never trip it); the READ arm requires a read
// verb WITH a screen/controls anchor and defers the where-is/locate/watch/scan/
// handwriting phrasings to the more-specific Vision ops.
// ===========================================================================

/// What a Lumen voice command resolves to. The READ arm forwards the READ-ONLY
/// screen locate + narrates; the ACT arm names the ONE target to actuate (the raw
/// phrase, which [`crate::lumen::resolve_voice_action`] parses over the remembered
/// controls).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LumenCommand {
    /// "read me the screen / the buttons / what's on screen" — READ-ONLY narrate.
    Read,
    /// "click / press the <ordinal|name>" — carries the lowercased phrase the
    /// selector parses over the remembered controls.
    Act(String),
}

/// Map a spoken utterance to a [`LumenCommand`], or None when it is neither a
/// Lumen read nor a Lumen actuation phrase (the turn falls through to the rest of
/// routing). PURE + deterministic so the mapping is unit-tested without a socket,
/// a running app, the OCR/AX locate, or the capstone. The ACT arm is checked
/// FIRST so "click the third button" (which mentions "button") is an actuation,
/// never a control read.
pub fn lumen_command(text: &str) -> Option<LumenCommand> {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return None;
    }
    if lumen_is_act(&lower) {
        return Some(LumenCommand::Act(lower));
    }
    if lumen_is_read(&lower) {
        return Some(LumenCommand::Read);
    }
    None
}

/// Whether `lower` is a Lumen ACTUATION phrase. CONSERVATIVE: a bare strong verb
/// ("click"/"tap"/"double-click") counts on its own (these almost never appear in
/// ordinary speech), but the broader "press"/"push" count ONLY alongside a control
/// noun or an ordinal — so "press play" / "push harder" / "press on" never trip
/// it. A degenerate "click" with no target still routes here and is REFUSED
/// honestly by the selector (never a guess), which is the correct place to say so.
fn lumen_is_act(lower: &str) -> bool {
    // Only "click"/"double-click" are rare enough in ordinary speech to count BARE.
    // "tap"/"press"/"push" are common English ("tap water", "on tap", "press on",
    // "push harder"), so they count ONLY alongside a control noun or an ordinal.
    // "click"/"double-click" are rare enough in ordinary speech to count anywhere.
    if contains_word(lower, "click")
        || lower.contains("double click")
        || lower.contains("double-click")
    {
        return true;
    }
    // "tap" is common English ("tap water", "on tap", "tap out"), so it counts as a
    // command ONLY when it is the LEADING imperative ("tap Submit", "tap the third")
    // — never mid-sentence, so "is the tap water safe?" never triggers.
    let trimmed = lower.trim_start();
    if trimmed == "tap" || trimmed.starts_with("tap ") {
        return true;
    }
    // "press"/"push" (and a non-leading "tap") require a concrete UI target.
    let has_targeted_verb = contains_word(lower, "tap")
        || contains_word(lower, "press")
        || contains_word(lower, "push");
    if !has_targeted_verb {
        return false;
    }
    lumen_mentions_control_noun(lower) || lumen_mentions_ordinal(lower)
}

/// Whether `lower` is a Lumen READ (control-narration) phrase. Requires a read/
/// narrate/list verb (or a "what's on / what are" question) WITH a screen or
/// controls anchor. DEFERS the where-is/locate, watch, scan, handwriting, and
/// describe phrasings to the more-specific Vision ops (checked here so, even
/// though Lumen dispatch runs before Vision, those never get swallowed).
fn lumen_is_read(lower: &str) -> bool {
    let deferred = lower.contains("where is")
        || lower.contains("where's")
        || lower.contains("locate")
        || (lower.contains("find") && lower.contains("button"))
        || lower.contains("watch")
        || lower.contains("scan")
        || lower.contains("handwriting")
        || lower.contains("handwritten")
        || lower.contains("whiteboard")
        || lower.contains("white board")
        || lower.contains("describe");
    if deferred {
        return false;
    }
    // ...AND NOT WHEN THE THING BEING READ BELONGS TO SOME OTHER DEVICE. Shared
    // with Vision's `screen_read_op` — see [`reads_another_devices_display`].
    if reads_another_devices_display(lower) {
        return false;
    }
    let mentions_screen = lower.contains("screen") || lower.contains("display");
    let mentions_controls = lumen_mentions_control_noun(lower);
    let reads = lower.contains("read")
        || lower.contains("narrate")
        || lower.contains("list")
        || lower.contains("what's on")
        || lower.contains("what is on")
        || lower.contains("what are");
    // MEASURED RECALL MISS: "what buttons are on this screen" reached nothing.
    // "what are" was in `reads` but this word order interposes the noun ("what
    // BUTTONS are"), so the most natural way to ask Lumen what it can see fell
    // through to Vision's OCR.
    //
    // THE SCREEN IS REQUIRED, not merely a control noun. `reads` above is ORed
    // with `mentions_controls`, and every one of these cues CONTAINS a control
    // noun — so folding them into `reads` would have made "what buttons should i
    // sew on this coat" a Lumen read (button + question). Anchoring the new cues
    // on the screen instead is what keeps the coat out.
    let control_inventory = mentions_screen
        && (lower.contains("what buttons")
            || lower.contains("what controls")
            || lower.contains("what fields")
            || lower.contains("what links")
            || lower.contains("what menus"));
    control_inventory || (reads && (mentions_screen || mentions_controls))
}

/// Whether `lower` names an on-screen CONTROL kind (button/link/tab/…). Used to
/// narrow the conservative act/read triggers. Substring-based (matches plurals).
fn lumen_mentions_control_noun(lower: &str) -> bool {
    ["button", "link", "tab", "checkbox", "check box", "field", "menu", "control", "icon"]
        .iter()
        .any(|n| lower.contains(n))
}

/// Whether any whitespace/punctuation-delimited token in `lower` is an ordinal —
/// a number word ("first".."tenth"), a digit+suffix ("1st".."10th"), or a short
/// bare number (an id/code-length digit run is deliberately NOT one). PURE.
fn lumen_mentions_ordinal(lower: &str) -> bool {
    const WORDS: &[&str] = &[
        "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth",
        "tenth", "1st", "2nd", "3rd", "4th", "5th", "6th", "7th", "8th", "9th", "10th",
    ];
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .any(|t| {
            WORDS.contains(&t)
                || (t.len() <= 3 && t.chars().all(|c| c.is_ascii_digit()))
        })
}

/// Build the `ui_actuate` tool INPUT (its `UiActuateArgs` JSON) from a resolved
/// [`crate::ui_automation::ActuationRequest`] — the SAME shape a live tool call
/// carries, so [`anthropic::execute_tool`] plans + gates it through the UNCHANGED
/// capstone. `confirm` is deliberately OMITTED (defaults false): only the
/// confirmation gate's `force_confirm` ever sets it, never Lumen — so the request
/// can only ever PARK for a spoken yes, never self-authorize. PURE.
fn ui_actuate_input(req: &crate::ui_automation::ActuationRequest) -> serde_json::Value {
    use crate::ui_automation::Action;
    match &req.action {
        Action::Click { x, y } => {
            json!({"action": "click", "target": req.target_desc, "x": x, "y": y})
        }
        Action::Type { text } => json!({"action": "type", "target": req.target_desc, "text": text}),
        Action::Key { combo } => json!({"action": "key", "target": req.target_desc, "combo": combo}),
    }
}

// ===========================================================================
// VLM DESCRIBE — on-device VISION-LANGUAGE understanding (task #2, build 2/3).
//
// DISTINCT from the OCR `read.screen` intent above (OCR = reading the TEXT
// GLYPHS off the screen; VLM = REASONING about the visual scene). "Describe my
// screen" / "what am I looking at" / "describe this image <path>" routes to the
// VISION agent, captures a screen frame (reuses the Vision app's screen capture)
// OR takes a PATH-CONFINED user image path, and calls the inference
// `describe_image` op (an on-device mlx-vlm model). The image's pixels go ONLY
// to the on-device VLM — NEVER to the cloud, never off the device.
//
// DEVICE-GATED + ON by default but INERT WITHOUT A MODEL ([vision].enabled ships
// true, [vision].model ships empty): the VLM
// needs mlx-vlm + a multi-GB checkpoint + enough RAM, so when it is off / the
// model isn't named / isn't downloaded, the op honestly reports "unavailable"
// and the daemon FALLS BACK honestly (to the OCR read.screen path for a screen
// request, or an honest "the vision-language model isn't downloaded" line) —
// it NEVER fabricates a description. The actual description QUALITY is
// device/runtime-gated and is never claimed measured.
//
// PATH CONFINEMENT: a user image path is canonicalized and asserted to live
// under the allowed root (the project root) BEFORE it is ever handed to the op
// (symlink-escape / `..` / absolute-elsewhere are REJECTED) — mirrors the
// docsearch `confine` primitive exactly.
// ===========================================================================

/// What a "describe" request resolves to: describe the user's SCREEN (capture a
/// frame), or describe a specific user IMAGE at a path. The path here is the RAW
/// candidate the user named; it is PATH-CONFINED by the handler BEFORE any op
/// call (the parser never touches the disk, so it stays pure + unit-testable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescribeRequest {
    /// "describe my screen" / "what am I looking at" — capture + describe a
    /// screen frame (reuses the Vision app's screen capture). `question` carries a
    /// SPECIFIC visual question for the VLM to answer ("what's the error on my
    /// screen?", "ask my screen which button rebuilds") — VQA; `None` = a generic
    /// description (the op applies its default describe prompt).
    Screen { question: Option<String> },
    /// "describe this image <path>" / "what's in <path>" — describe a specific
    /// image file (RAW candidate path; confined before the op). `question` = a
    /// specific question about the file ("in cat.png, is the cat asleep?");
    /// `None` = a generic description.
    Image { path: String, question: Option<String> },
}

/// Map a spoken utterance to a [`DescribeRequest`], or None when it is not a
/// VLM-describe request (the turn falls through to normal routing — including the
/// OCR `read.screen` path, which is DISTINCT). PURE + deterministic so the
/// mapping is unit-tested without a socket, a running app, the VLM, or the
/// classifier.
///
/// Recognized (case-insensitive, whole lowercased utterance):
///   - "describe this image <path>" / "what's in this picture <path>" /
///     "describe the photo <path>"                 -> Image(path)
///   - "describe my screen" / "what am I looking at" / "describe what's on my
///     screen" / "what do you make of my screen"   -> Screen
///
/// DISTINCT from OCR: "read my screen" / "what's on my screen" (text glyphs) is
/// handled by [`screen_read_op`] and is NOT a describe request. The describe
/// verbs ("describe", "what am I looking at", "what do you make of") never
/// overlap the OCR read verbs ("read", "what's on") — checked here so an OCR
/// phrase never lands on the VLM and vice versa.
pub fn describe_command(text: &str) -> Option<DescribeRequest> {
    let lower = text.to_lowercase();

    // An image-FILE describe ("describe this image ~/pics/cat.png", "what's in
    // photo.jpg"): a describe/what-is verb PLUS a token carrying an image
    // extension. Checked before the screen describe so a named file wins.
    // MEASURED RECALL MISS: "look at ~/Desktop/diagram.png and tell me what it
    // shows" reached nothing. Widening this list is SAFE IN A WAY THE SCREEN
    // BRANCH BELOW IS NOT: `names_describe` gates only the IMAGE branch, and that
    // branch additionally requires `extract_image_path` to find a real
    // image-extension token in the utterance. A sentence that names a .png and
    // asks to look at it is a describe request; a sentence that merely says "look
    // at that" still reaches nothing here.
    let names_describe = lower.contains("describe")
        || lower.contains("look at")
        || lower.contains("tell me what it shows")
        || lower.contains("tell me what this shows")
        || lower.contains("what's in")
        || lower.contains("whats in")
        || lower.contains("what is in")
        || lower.contains("what am i looking at")
        || lower.contains("what do you make of")
        || lower.contains("what's this")
        || lower.contains("what is this");
    if names_describe {
        if let Some(path) = extract_image_path(text) {
            let question = vqa_question(text, Some(&path));
            return Some(DescribeRequest::Image { path, question });
        }
    }

    // An EXPLICIT screen-VQA trigger ("ask my screen <question>", "ask about my
    // screen <question>"). A dedicated, unambiguous form so a SPECIFIC visual
    // question ("ask my screen which button rebuilds") reaches the VLM even
    // without a "describe" verb. Begins with "ask <the screen>", so it cannot
    // collide with an OCR read ("read"/"what's on") or a Lumen control read, and
    // "ask <a person> ..." never matches (the object must be the screen/display).
    if let Some(q) = explicit_screen_vqa(&lower, text) {
        return Some(DescribeRequest::Screen { question: q });
    }

    // A SCREEN describe ("describe my screen", "what am I looking at",
    // "describe what's on my screen", "what do you make of my screen"). MUST be
    // a describe verb (NOT an OCR "read"/"what's on" verb): the VLM describes the
    // scene, the OCR path reads the text. "what am I looking at" with no image
    // file is a screen describe (the most natural hands-free phrasing). When the
    // utterance asks something SPECIFIC beyond the generic describe scaffolding
    // (e.g. "describe my screen — is there an error?"), that question is threaded
    // to the VLM (VQA); a bare "describe my screen" stays a generic caption.
    let mentions_screen =
        lower.contains("screen") || lower.contains("display") || lower.contains("looking at");
    let describe_screen = (lower.contains("describe") && mentions_screen)
        || lower.contains("what am i looking at")
        || (lower.contains("what do you make of") && mentions_screen)
        // MEASURED RECALL MISS: "tell me what you see on my screen" reached the
        // OCR path instead (Vision's `screen_read_op`), which reads the screen's
        // TEXT back. "what you see" asks for the SCENE, which is what the VLM
        // describe produces — so it is claimed here, ahead of the OCR read.
        // Both anchors required: the see-phrase AND the screen noun, so a bare
        // "what do you see" stays Vision's presence status.
        || (lower.contains("what you see") && mentions_screen)
        || (lower.contains("what you can see") && mentions_screen);
    if describe_screen {
        let question = vqa_question(text, None);
        return Some(DescribeRequest::Screen { question });
    }

    None
}

/// The explicit screen-VQA trigger: an utterance that begins with "ask" whose
/// OBJECT is the screen/display ("ask my screen …", "ask about the display …").
/// Returns `Some(question)` when it matches — the `question` is the user's words
/// with the trigger prefix stripped (or `None` when nothing substantive follows,
/// which routes to a generic screen describe). Returns `None` (does not match)
/// otherwise. PURE. The prefix set is exhaustive on purpose: "ask <a person>
/// about the screen" never matches (it does not START with one of these), so a
/// message-a-contact intent is never poached.
fn explicit_screen_vqa(lower: &str, original: &str) -> Option<Option<String>> {
    const PREFIXES: &[&str] = &[
        "ask about my screen",
        "ask about the screen",
        "ask about my display",
        "ask about the display",
        "ask my screen",
        "ask the screen",
        "ask my display",
        "ask the display",
    ];
    let prefix = PREFIXES.iter().find(|p| lower.starts_with(**p))?;
    // Strip the matched prefix from the ORIGINAL-case text, then trim leading
    // filler/punctuation ("about", ":", ",", "-"). What remains is the question.
    let rest = original[prefix.len()..]
        .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | ',' | '-' | '?' | '.'))
        .trim();
    if rest.is_empty() {
        // "ask my screen" with no question — a generic look at the screen.
        Some(None)
    } else {
        Some(Some(rest.to_string()))
    }
}

/// Extract the SPECIFIC visual question a describe utterance carries, or `None`
/// for a generic description. `path`, when present, is the recognized image-file
/// token — it is removed first so a file path never leaks into the VLM prompt.
///
/// Rule: after removing the path, tokenize the remainder; if EVERY token is
/// generic describe/scaffolding vocabulary ("describe", "my", "screen", "what",
/// "looking", …) the user only asked for a plain description -> `None` (the op
/// then uses its default prompt). If ANY token is substantive ("error", "button",
/// "dog", "asleep", …) the user asked something specific -> `Some(utterance)`,
/// passed verbatim so the VLM answers THAT. PURE + unit-tested without a model.
fn vqa_question(text: &str, path: Option<&str>) -> Option<String> {
    // Remove the path token (first occurrence, case-insensitive) from a working
    // copy; the returned question is built from the ORIGINAL text minus the path.
    // Remove the image-path token WITHOUT byte-offset math on the original: an
    // offset from `text.to_lowercase()` desyncs on any char whose lowercase form
    // has a different byte length (e.g. `İ`), which would panic replace_range on a
    // char boundary. Instead drop the whitespace token whose punctuation-trimmed
    // form equals the path (extract_image_path built the path exactly that way),
    // which is boundary-safe by construction. Also removes any punctuation
    // attached to the path token — fine for a VLM prompt.
    let stripped: String = match path {
        Some(p) if !p.is_empty() => {
            let pl = p.to_lowercase();
            text.split_whitespace()
                .filter(|tok| {
                    let trimmed = tok.trim_matches(|c: char| {
                        !c.is_alphanumeric()
                            && c != '.'
                            && c != '/'
                            && c != '_'
                            && c != '-'
                            && c != '~'
                    });
                    trimmed.to_lowercase() != pl
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => text.to_string(),
    };
    let rest = stripped.trim();
    if rest.is_empty() {
        return None;
    }
    // Generic describe / scaffolding vocabulary. A remnant made ONLY of these is a
    // plain "just describe it" request (generic caption). Anything else is a
    // specific question the VLM should answer.
    const SCAFFOLD: &[&str] = &[
        "please", "can", "could", "would", "will", "you", "tell", "give", "show",
        "let", "me", "us", "for", "a", "an", "the", "to", "and", "so", "just",
        "describe", "description", "what", "whats", "s", "is", "are", "in", "of",
        "on", "at", "am", "i", "looking", "look", "do", "does", "make", "makes",
        "see", "seeing", "this", "that", "these", "those", "it", "here", "there",
        "my", "your", "screen", "display", "monitor", "image", "picture", "photo",
        "pic", "photograph", "snapshot", "now", "right", "currently",
    ];
    let has_substantive = rest
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .any(|t| !SCAFFOLD.contains(&t.as_str()));
    if has_substantive {
        // Collapse whitespace runs (a stripped path leaves a gap) so the VLM
        // prompt is clean — and, by construction, carries no file path.
        Some(rest.split_whitespace().collect::<Vec<_>>().join(" "))
    } else {
        None
    }
}

/// Extract an image file path/name from a describe phrase. Returns the token
/// that carries a known image extension (png/jpg/jpeg/gif/webp/heic/bmp/tiff),
/// taken from the ORIGINAL-case text (file systems are case-sensitive) via a
/// case-insensitive extension match. None when no such token is present (a bare
/// "describe this image"). Pure — never touches the disk; the confinement +
/// existence check happen in the handler.
fn extract_image_path(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace())
        .map(|w| {
            w.trim_matches(|c: char| {
                !c.is_alphanumeric() && c != '.' && c != '/' && c != '_' && c != '-' && c != '~'
            })
        })
        .find(|w| {
            let lw = w.to_lowercase();
            lw.ends_with(".png")
                || lw.ends_with(".jpg")
                || lw.ends_with(".jpeg")
                || lw.ends_with(".gif")
                || lw.ends_with(".webp")
                || lw.ends_with(".heic")
                || lw.ends_with(".bmp")
                || lw.ends_with(".tiff")
                || lw.ends_with(".tif")
        })
        .map(|w| w.to_string())
}

/// Whether an utterance is a VLM-DESCRIBE request (visual understanding via the
/// on-device VLM). PUBLIC so the pipeline (main.rs) can keep its result
/// TRANSIENT exactly like an OCR screen read: a describe of the screen / a
/// private photo can surface sensitive visual content, so its utterance +
/// acknowledgment must NOT seed lifelong memory or optimizer traces. Pure over
/// [`describe_command`], so this and the routing agree by construction.
pub fn is_describe_request(text: &str) -> bool {
    describe_command(text).is_some()
}

/// Honest copy when the on-device VLM is unavailable for a SCREEN describe — the
/// daemon falls back to the OCR `read.screen` path (it can still read the text on
/// screen). Kept as a function so the handler and tests share the exact wording.
fn describe_screen_fallback_copy(reason: &str) -> String {
    format!(
        "I can't describe the scene right now, sir — {reason}. I'll read the text on your \
         screen instead; the visual-description model runs on-device and isn't set up yet."
    )
}

/// Honest copy when the on-device VLM is unavailable for an IMAGE describe (there
/// is no OCR fallback for an arbitrary file, so we state the gate plainly).
fn describe_image_fallback_copy(reason: &str) -> String {
    format!(
        "I can't describe that image, sir — {reason}. The vision-language model runs \
         entirely on-device and isn't downloaded yet, so I won't guess at what's in it."
    )
}

/// Execute a VLM-describe request. Routes to the VISION agent (the caller
/// re-pins it). The image is read ON-DEVICE by the inference `describe_image` op;
/// pixels NEVER leave the device. Returns persona-voiced converse data
/// (`llm_voice`), exactly like the OCR / app-op handlers.
///
/// GATES + FALLBACK (honesty-first):
///   * [vision].enabled OFF or [vision].model EMPTY: the VLM is not set up — the
///     daemon does NOT call the op. A SCREEN request falls back to the OCR
///     read.screen path (it can still read the text); an IMAGE request reports
///     the gate honestly. NEVER a fabricated description.
///   * A user IMAGE path is PATH-CONFINED (canonicalize + under the allowed
///     root) BEFORE the op call; an escape (symlink/`..`/absolute-elsewhere) or
///     a nonexistent path is REJECTED with an honest message (never sent).
///   * The op itself returns [`DescribeOutcome::Unavailable`] when mlx-vlm /
///     the checkpoint isn't present (the server's "vlm_unavailable") — the daemon
///     falls back honestly on that too.
///
/// Emits a `vision.describe` telemetry event carrying ONLY the source kind +
/// availability + latency bucket — NEVER any pixels or the description text.
async fn handle_describe(
    req: DescribeRequest,
    cfg: &Config,
    infer: &mut InferenceClient,
    app_registry: &Arc<AppRegistry>,
    allowed_root: &Path,
) -> HandlerOutput {
    // GATE: the VLM is OFF or no model is named -> do not call the op; fall back
    // honestly. The two reasons are distinct so the spoken copy is honest about
    // which gate is closed.
    let gate_reason: Option<&str> = if !cfg.vision.enabled {
        Some("the on-device vision-language model is turned off")
    } else if cfg.vision.model.trim().is_empty() {
        Some("no on-device vision-language model is configured")
    } else {
        None
    };

    // Whether the user asked a SPECIFIC question (VQA) vs a generic describe. Only
    // the boolean is emitted below — never the question text (it can name what is
    // on the most-sensitive surface, the screen).
    let is_vqa = matches!(
        &req,
        DescribeRequest::Screen { question: Some(_) }
            | DescribeRequest::Image { question: Some(_), .. }
    );

    let (source, available, data) = match req {
        DescribeRequest::Screen { question } => {
            if let Some(reason) = gate_reason {
                // Honest fall back to OCR: forward the read.screen op so the user
                // still gets the on-screen TEXT (best-effort; an op error is itself
                // reported honestly by handle_vision's send_op path).
                let ocr = handle_vision(
                    VisionCommand::Op(op_read_screen(None)),
                    app_registry,
                )
                .await;
                let copy = format!(
                    "{}\n\n{}",
                    describe_screen_fallback_copy(reason),
                    ocr.data
                );
                ("screen", false, copy)
            } else {
                // The VLM is configured + on. Capture a screen frame by forwarding
                // the Vision app's capture op (reusing its ScreenCaptureKit path),
                // then describe it (answering the user's specific `question` when
                // one was asked — VQA — else a generic caption). The captured frame
                // is the Vision app's to produce on-device; the daemon never holds
                // the pixels. The frame path is the app's confined capture output.
                match capture_screen_frame(app_registry, allowed_root).await {
                    Ok(frame) => describe_confined_path(
                        &frame,
                        question.as_deref(),
                        infer,
                        allowed_root,
                        "screen",
                    )
                    .await
                    .unwrap_or_else(|reason| {
                        ("screen", false, describe_screen_fallback_copy(&reason))
                    }),
                    Err(reason) => {
                        // Couldn't get a frame — fall back to the OCR read path.
                        let ocr = handle_vision(
                            VisionCommand::Op(op_read_screen(None)),
                            app_registry,
                        )
                        .await;
                        let copy = format!(
                            "{}\n\n{}",
                            describe_screen_fallback_copy(&reason),
                            ocr.data
                        );
                        ("screen", false, copy)
                    }
                }
            }
        }
        DescribeRequest::Image { path: raw_path, question } => {
            if let Some(reason) = gate_reason {
                ("image", false, describe_image_fallback_copy(reason))
            } else {
                match describe_confined_path(
                    Path::new(&raw_path),
                    question.as_deref(),
                    infer,
                    allowed_root,
                    "image",
                )
                .await
                {
                    Ok(out) => out,
                    Err(reason) => ("image", false, describe_image_fallback_copy(&reason)),
                }
            }
        }
    };

    // TELEMETRY: source kind + availability + nothing visual. No pixels, no
    // description text, no path — the event proves the wiring ran without leaking
    // what was seen (the visual content is the most sensitive thing in this op).
    telemetry::emit(
        "local",
        "vision.describe",
        json!({"source": source, "available": available, "vlm": cfg.vision.enabled, "vqa": is_vqa}),
    );

    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// PATH-CONFINE `candidate` under `allowed_root`, then call the on-device
/// `describe_image` op. On success returns `Ok((source, true, description))`;
/// on a confinement reject / a missing path / the op's UNAVAILABLE arm / a
/// transport error it returns `Err(honest_reason)` so the caller renders the
/// right fall-back copy. NEVER returns a fabricated description.
async fn describe_confined_path(
    candidate: &Path,
    question: Option<&str>,
    infer: &mut InferenceClient,
    allowed_root: &Path,
    source: &'static str,
) -> std::result::Result<(&'static str, bool, String), String> {
    // PATH CONFINEMENT (the security primitive, mirrors docsearch::confine):
    // canonicalize the candidate + assert it resolves under the canonicalized
    // allowed root. A symlink-escape / `..` / absolute-elsewhere / nonexistent
    // path is REJECTED here — the path is NEVER handed to the op.
    let canon_root = match std::fs::canonicalize(allowed_root) {
        Ok(r) => r,
        Err(_) => return Err("I couldn't resolve a safe location to read the image from".to_string()),
    };
    let confined = crate::docsearch::confine(candidate, std::slice::from_ref(&canon_root));
    let Some(real) = confined else {
        return Err(
            "that image isn't in a folder I'm allowed to read from, so I won't open it"
                .to_string(),
        );
    };

    // Clamp the decode budget defensively at the daemon boundary too (the client
    // also clamps); None lets the client apply the shared default + cap.
    match infer.describe_image(&real, question, None).await {
        Ok(DescribeOutcome::Available { text, model }) => {
            info!(source = source, model = %model, "vlm describe ok");
            // The DESCRIPTION is the spoken data; the model id is non-secret. The
            // text is the model's VISUAL understanding — distinct from OCR glyphs.
            Ok((source, true, text))
        }
        Ok(DescribeOutcome::Unavailable { error }) => {
            // The op reported the device-gated unavailable path (or a caller-bug
            // ValueError). Honest fall back — never a fabricated description.
            warn!(source = source, reason = %error, "vlm describe unavailable; falling back");
            Err(error)
        }
        Err(e) => {
            // Transport failure (inference server down). Honest fall back.
            warn!(source = source, error = %e, "vlm describe transport error; falling back");
            telemetry::emit(
                "system",
                "inference.unavailable",
                json!({"op": "describe_image", "error": e.to_string()}),
            );
            Err("the inference server isn't reachable".to_string())
        }
    }
}

// ===========================================================================
// On-device TEXT->IMAGE GENERATION (task #18) — DISTINCT from the VLM describe
// path above (describe = reasoning ABOUT an image; generate = rendering a NEW
// image from a text prompt). "generate / make / draw / create an image of X"
// routes to the VISION agent (the visual-capability owner, same as describe) and
// calls the inference `generate_image` op (an on-device MLX diffusion model). The
// PROMPT and the generated PIXELS go ONLY to the on-device model and the image is
// saved on-device under state/images/ — NEVER to the cloud, never off the device
// (there is NO cloud image API anywhere on this path).
//
// DEVICE-GATED + ON by default but INERT WITHOUT A MODEL ([image].enabled ships
// true, [image].model ships empty): the diffusion
// model needs an MLX package + a multi-GB checkpoint + enough RAM, so when it is
// off / the model isn't named / isn't downloaded, the op honestly reports
// "image_model_unavailable" and the daemon surfaces an honest "the on-device
// image model isn't set up" line — it NEVER fabricates an image and NEVER falls
// back to a cloud image API. The actual image QUALITY/speed are device/runtime-
// gated and are never claimed measured.
// ===========================================================================

/// A parsed "generate an image of X" request: the extracted image PROMPT (the
/// subject after the generate verb). PURE + deterministic so the mapping is
/// unit-tested without a socket, the diffusion model, or the classifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerateImageRequest {
    pub prompt: String,
}

/// Map a spoken utterance to a [`GenerateImageRequest`], or None when it is not
/// an image-generation request (the turn falls through to normal routing —
/// including the VLM DESCRIBE path, which is DISTINCT: describe reasons ABOUT an
/// existing image; generate renders a NEW one). PURE + deterministic.
///
/// Recognized (case-insensitive): a GENERATE verb ("generate" / "make" / "draw"
/// / "create" / "paint" / "render") applied to "an image / a picture / a photo /
/// a drawing / a painting / art of <X>", and the shorthand "image of <X>". The
/// SUBJECT after "of"/"showing"/"depicting" (or after the image-noun) becomes the
/// prompt. A describe verb ("describe", "what's in") is NOT a generate verb, so
/// the two intents never collide.
pub fn generate_image_command(text: &str) -> Option<GenerateImageRequest> {
    let lower = text.to_lowercase();

    // DISTINCT from the VLM describe path: a describe/what-is verb is never an
    // image-GENERATION request (describe reasons about an EXISTING image).
    if describe_command(text).is_some() {
        return None;
    }

    // A GENERATE verb must COMMAND an IMAGE noun — see `image_noun_is_commanded`
    // for the three things that used to be missing (whole words, base forms, and
    // an object relation between the two).
    if !image_noun_is_commanded(&lower) {
        return None;
    }

    // Extract the SUBJECT (the prompt) from the ORIGINAL-case text so the user's
    // phrasing survives. Prefer the explicit "of/showing/depicting <X>" tail; the
    // first such connector AFTER an image noun is where the subject begins.
    if let Some(prompt) = extract_image_prompt(text) {
        let prompt = prompt.trim();
        // A CLAUSE IS NOT A DEPICTABLE SUBJECT. "paint a picture of WHAT next
        // year looks like", "paint me a picture of HOW the meeting went", "draw
        // me a picture of WHY that matters" are the English idiom for EXPLAIN,
        // and they satisfy every lexical half of this gate. A real image request
        // names a thing ("a lighthouse", "a red bicycle", "a fox in the snow");
        // a wh-word after the connector means the tail is a proposition, and the
        // diffusion model would be handed a sentence instead of a subject.
        const CLAUSE_OPENERS: &[&str] = &[
            "what", "how", "why", "where", "when", "who", "whether",
        ];
        let opener = prompt
            .split(|c: char| !c.is_alphanumeric())
            .find(|w| !w.is_empty())
            .unwrap_or_default()
            .to_lowercase();
        if CLAUSE_OPENERS.contains(&opener.as_str()) {
            return None;
        }
        if !prompt.is_empty() {
            return Some(GenerateImageRequest { prompt: prompt.to_string() });
        }
    }
    None
}

/// Whether a GENERATE verb actually COMMANDS an image noun in `lower` — i.e. the
/// noun is that verb's OBJECT, not merely another word in the same sentence.
///
/// WHAT WENT WRONG: the gate was two independent `contains` scans — "does any
/// generate verb appear as a SUBSTRING anywhere" AND "does any image noun appear
/// as a SUBSTRING anywhere". Neither half was a whole word, neither was a base
/// form, and nothing tied them together, so ordinary speech generated pictures:
///   "the photosynthesis chapter makes more sense with the diagram of the leaf"
///        ("photo" inside photosynthesis, "make" inside makes)
///   "art therapy makes a difference with kids who have trouble talking"
///        ("art " as a substring, "make" inside makes)
///   "my daughter created a drawing of a dinosaur with a crayon"   (narration)
///   "that painting of the harbor is my favourite thing in the house"
///   "remake that photo album of the wedding with the newer prints"
///   "the big picture of this quarter is that we make less with more effort"
///   "make an effort with the picture of professionalism you project"
///   "I cannot imagine the pressure of taking a photo with a broken lens"
/// Three rules close all of those and cost none of the shipped phrasings:
///   1. WHOLE WORDS on both halves — kills photosynthesis/art-therapy/remake.
///   2. BASE FORMS only for the verb. A request to DARWIN is imperative
///      ("draw me a picture"), so the inflected forms are narration about
///      somebody else drawing: makes / created / painted / drew / draws /
///      rendered / painting-as-a-noun. Dropping them costs no imperative.
///   3. The noun must be the verb's OBJECT: it follows the verb across at most
///      two determiner-ish words. That is what separates "make a picture of X"
///      from "make an effort with the picture of X" and from "the big picture …
///      we make less".
///
/// "imagine" is NOT a generate verb. It was, and it is the one verb on the list
/// whose ordinary imperative sense ("imagine the artwork of a whole generation
/// with no galleries left") is the DOMINANT one; no shipped phrasing uses it to
/// mean render, and it cannot be told apart by position because "imagine the
/// artwork" is a textbook verb-object pair.
fn image_noun_is_commanded(lower: &str) -> bool {
    const GEN_VERBS: &[&str] =
        &["generate", "make", "draw", "create", "paint", "render", "sketch"];
    const IMAGE_NOUNS: &[&str] = &[
        "image", "images", "picture", "pictures", "photo", "photos", "drawing", "drawings",
        "painting", "paintings", "illustration", "illustrations", "artwork", "art",
    ];
    // The words a speaker slips between a generate verb and its object — "draw
    // ME A picture", "generate AN image", "make MY picture". Deliberately only
    // determiners and object pronouns: one content word in the gap ("make an
    // EFFORT with the picture") and the noun is not what is being generated.
    const GAP: &[&str] = &["me", "us", "a", "an", "the", "my", "your", "one", "some", "another"];
    let words = speech_words(lower);
    for (i, w) in words.iter().enumerate() {
        if !GEN_VERBS.contains(w) {
            continue;
        }
        for w2 in words.iter().skip(i + 1).take(3) {
            if IMAGE_NOUNS.contains(w2) {
                return true;
            }
            if !GAP.contains(w2) {
                break;
            }
        }
    }
    false
}

/// Extract the image PROMPT (subject) from a generate phrase, in ORIGINAL case.
/// Takes the tail after the first subject connector ("of"/"showing"/"depicting"/
/// "that shows"/"with") — e.g. "draw a picture of a red bicycle" -> "a red
/// bicycle". None when there is no connector (a bare "generate an image" with no
/// subject), which the caller treats as "no prompt" rather than guessing. Pure —
/// never touches the disk or the network.
fn extract_image_prompt(text: &str) -> Option<String> {
    // The subject connectors, longest first so "that shows" wins over a bare
    // "shows" overlap. All ASCII, so a case-insensitive byte compare is exact.
    const CONNECTORS: &[&str] = &[" that shows ", " depicting ", " showing ", " of ", " with "];
    // Locate the EARLIEST connector by scanning `text`'s CHAR boundaries directly
    // and comparing each candidate window case-insensitively (ASCII). This yields
    // a `start` that is always a valid char boundary IN `text`. The earlier
    // `lower.find()` approach returned a byte offset into `text.to_lowercase()`,
    // which can differ from `text` whenever a char's lowercase form has a
    // different byte length (e.g. Turkish 'İ' U+0130 -> "i̇") — slicing `text`
    // with that mismatched offset could land mid-codepoint (or past the end) and
    // PANIC the whole daemon on an STT transcript carrying such a character.
    let bytes = text.as_bytes();
    // WHAT WENT WRONG: this scanned the WHOLE utterance and kept the earliest
    // connector, while the caller's comment promised "the first such connector
    // AFTER an image noun is where the subject begins". For "instead of a photo,
    // make a drawing of the house" the earliest connector is the "of" in "instead
    // of", so the prompt handed to the on-device diffusion model was "a photo,
    // make a drawing of the house" — the user got an image of something they did
    // not ask for. Both halves of the gate pass for that sentence ("make" +
    // "photo") and `describe_command` returns None, so it really did route.
    //
    // Only connectors that START AT OR AFTER the first image noun are considered;
    // with no image noun before any connector this falls back to the old
    // whole-text scan, so nothing that used to extract stops extracting.
    const IMAGE_NOUNS: &[&str] = &[
        "image", "picture", "photo", "drawing", "painting", "illustration", "artwork", "art ",
    ];
    let first_noun = {
        let mut best: Option<usize> = None;
        for (i, _) in text.char_indices() {
            for n in IMAGE_NOUNS {
                let nb = n.as_bytes(); // image nouns are ASCII
                if i + nb.len() <= bytes.len() && bytes[i..i + nb.len()].eq_ignore_ascii_case(nb) {
                    best = Some(best.map_or(i, |b: usize| b.min(i)));
                }
            }
        }
        best
    };
    let scan = |floor: usize| -> Option<usize> {
        let mut best_start: Option<usize> = None;
        for (i, _) in text.char_indices() {
            if i < floor {
                continue;
            }
            for c in CONNECTORS {
                let cb = c.as_bytes(); // connectors are ASCII
                if i + cb.len() <= bytes.len() && bytes[i..i + cb.len()].eq_ignore_ascii_case(cb) {
                    // ASCII connector -> `i + cb.len()` is a valid char boundary.
                    let tail = i + cb.len();
                    // Prefer the EARLIEST qualifying connector so "a picture of X
                    // with Y" keeps the full "X with Y" subject rather than
                    // starting at " with ".
                    if best_start.is_none_or(|b| tail < b) {
                        best_start = Some(tail);
                    }
                }
            }
        }
        best_start
    };
    let start = match first_noun {
        Some(n) => scan(n).or_else(|| scan(0))?,
        None => scan(0)?,
    };
    Some(text[start..].to_string())
}

/// Whether an utterance is an image-GENERATION request. PUBLIC so the pipeline
/// (main.rs) can keep its result TRANSIENT exactly like a VLM describe — a
/// generated image (and its prompt) can be personal, so its utterance +
/// acknowledgment must NOT seed lifelong memory or optimizer traces. Pure over
/// [`generate_image_command`], so this and the routing agree by construction.
pub fn is_generate_image_request(text: &str) -> bool {
    generate_image_command(text).is_some()
}

/// Honest copy when the on-device image model is unavailable (off / no model
/// named / not downloaded / a runtime failure). There is NO cloud fallback — the
/// daemon states the gate plainly and never fabricates an image. Kept as a
/// function so the handler and tests share the exact wording.
fn generate_image_unavailable_copy(reason: &str) -> String {
    format!(
        "I can't generate that image, sir — {reason}. The image model runs entirely \
         on-device and isn't set up yet, so I won't invent a picture or send your \
         prompt to the cloud."
    )
}

/// Execute an image-GENERATION request. Routes to the VISION agent (the caller
/// re-pins it). The prompt is handed ONLY to the on-device `generate_image` op
/// (MLX diffusion) and the image is saved ON-DEVICE under state/images/; the
/// prompt + pixels NEVER leave the device — there is NO cloud image API. Returns
/// persona-voiced converse data (`llm_voice`), exactly like the describe handler.
///
/// GATES + FALLBACK (honesty-first):
///   * [image].enabled OFF or [image].model EMPTY: the model is not set up — the
///     daemon does NOT call the op and surfaces the gate honestly. NEVER a
///     fabricated image, NEVER a cloud call.
///   * The op itself returns [`GenerateOutcome::Unavailable`] when the diffusion
///     package / checkpoint isn't present (the server's "image_model_unavailable")
///     — the daemon surfaces that honestly too (NO cloud fallback).
///
/// Emits an `image.generated` telemetry event carrying ONLY availability + the
/// saved ON-DEVICE path + the NON-secret model/size/steps metadata — NEVER the
/// prompt and NEVER any pixels, and NEVER over the network (the telemetry sink is
/// the local HUD).
async fn handle_generate_image(
    req: GenerateImageRequest,
    cfg: &Config,
    infer: &mut InferenceClient,
) -> HandlerOutput {
    // GATE: the image model is OFF or no model is named -> do not call the op;
    // surface the gate honestly. The two reasons are distinct so the spoken copy
    // is honest about which gate is closed.
    let gate_reason: Option<&str> = if !cfg.image.enabled {
        Some("on-device image generation is turned off")
    } else if cfg.image.model.trim().is_empty() {
        Some("no on-device image-generation model is configured")
    } else {
        None
    };

    let (available, saved_path, model, size, steps, data) = if let Some(reason) = gate_reason {
        // OFF / unconfigured: never reach the op. Honest gate line, no cloud call.
        (false, None, None, None, None, generate_image_unavailable_copy(reason))
    } else {
        // Configured + on: call the on-device op. None for size/steps/seed lets
        // the server apply its defaults (the client also clamps any explicit ask).
        match infer.generate_image(&req.prompt, None, None, None).await {
            // The NON-secret `seed` is intentionally ignored: the daemon never
            // surfaces it spoken and never forwards it anywhere off-device.
            Ok(GenerateOutcome::Available { path, model, size, steps, seed: _ }) => {
                info!(model = %model, size, steps, "image generated on-device");
                // The SAVED ON-DEVICE PATH is what the user gets — the image stays
                // on the machine. The spoken data names the local path (never the
                // pixels, never the prompt back to the cloud).
                let copy = format!(
                    "Done, sir — I generated that image on-device and saved it to {}. \
                     The prompt and the picture stayed on this machine; nothing went to the cloud.",
                    path.display()
                );
                (
                    true,
                    Some(path.display().to_string()),
                    Some(model),
                    Some(size),
                    Some(steps),
                    copy,
                )
            }
            Ok(GenerateOutcome::Unavailable { error }) => {
                // The op reported the device-gated unavailable path (or a caller-bug
                // ValueError). Honest surface — never a fabricated image, never a
                // cloud fallback.
                warn!(reason = %error, "image generation unavailable; reporting honestly (no cloud)");
                (false, None, None, None, None, generate_image_unavailable_copy(&error))
            }
            Err(e) => {
                // Transport failure (inference server down). Honest surface — still
                // NO cloud fallback.
                warn!(error = %e, "image generation transport error; reporting honestly (no cloud)");
                telemetry::emit(
                    "system",
                    "inference.unavailable",
                    json!({"op": "generate_image", "error": e.to_string()}),
                );
                (
                    false,
                    None,
                    None,
                    None,
                    None,
                    generate_image_unavailable_copy("the inference server isn't reachable"),
                )
            }
        }
    };

    // TELEMETRY: availability + the saved ON-DEVICE path + NON-secret model/size/
    // steps metadata. NEVER the prompt, NEVER any pixels, and NEVER over the
    // network — the event proves the wiring ran (and where the image landed on the
    // device) without leaking what was asked for or generated. The HUD reads this
    // to render the local-image readout / the unavailable state.
    telemetry::emit(
        "local",
        "image.generated",
        json!({
            "available": available,
            "path": saved_path,
            "model": model,
            "size": size,
            "steps": steps,
            "image": cfg.image.enabled,
        }),
    );

    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// How long to wait for the Vision app to write the describe frame. Screen capture is
/// fast; this only has to cover process scheduling and the ScreenCaptureKit round trip.
/// Well under the daemon's 30 s request budget, which the describe itself then spends.
const CAPTURE_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);
/// Poll granularity while waiting for that frame.
const CAPTURE_FRAME_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Capture ONE screen frame for the VLM by forwarding the Vision app's screen
/// capture op (reusing its ScreenCaptureKit path — pixels stay in the app's
/// process / on-device). Returns the confined frame path the app wrote, or an
/// honest reason on failure (Vision not running, capture not consented, no
/// frame produced). DEVICE/TCC-GATED: the daemon forwards the op; the on-device
/// consent + the actual capture are the app's to perform.
///
/// HONESTY: the daemon does not itself open the screen — it asks the running
/// Vision micro-app (which owns the capture + the TCC consent) to produce a
/// frame under the project root, then path-confines that frame before the op.
async fn capture_screen_frame(
    app_registry: &Arc<AppRegistry>,
    allowed_root: &Path,
) -> std::result::Result<std::path::PathBuf, String> {
    // The frame the Vision app writes for a VLM describe, under the project
    // state dir (an allowlisted root). The op asks the app to capture + save one
    // frame here; the app owns the on-device capture + TCC consent.
    // INSIDE THE APP'S WRITE GRANT. apps/vision/manifest.toml grants
    // fs_write = ["state/tmp/vision"] and generate_sbpl emits exactly one
    // (allow file-write* (subpath ...)) per entry — so the old state/vision/ path was
    // seatbelt-denied and the app could never have written this frame, on any run.
    let frame = allowed_root
        .join("state")
        .join("tmp")
        .join("vision")
        .join("describe-frame.png");
    let op = json!({
        "type": "op",
        "op": "describe.capture",
        "path": frame.display().to_string(),
    })
    .to_string();
    // WAIT FOR THE ARTIFACT, and do not accept a frame the app did not just write.
    //
    // History, because both mistakes are instructive. Originally this was a
    // fire-and-forget `send_op` followed on the very next line by `frame.exists()`,
    // with no await between them — so the first screen question always failed with an
    // untrue "Screen Recording consent is needed" and later ones described a STALE
    // frame. I then switched it to `apps::request_op`, which waits for a
    // `{"type":"result","id":...}` line — and the Vision app has no such message type
    // at all (RelayType is items|status|log|modules), so every screen question stalled
    // for the whole APP_REQUEST_TIMEOUT and then failed. That was strictly worse: a
    // fast wrong answer became a slow one.
    //
    // The app cannot reply, but it does not need to: THE FRAME IS THE RESULT. So the
    // old frame is removed first (a stale file can never be mistaken for a fresh one)
    // and we poll for the new one under a deadline. A frame that never appears is the
    // honest "no frame" case — which is now genuinely what the message says.
    let _ = tokio::fs::remove_file(&frame).await;
    if let Some(dir) = frame.parent() {
        // The daemon is not sandboxed; the app is, and cannot create its own scratch
        // directory outside the granted subpath's existing tree.
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    apps::send_op(app_registry, VISION_APP, &op)
        .await
        .map_err(|e| format!("I couldn't reach Vision to capture your screen ({e})"))?;

    let deadline = std::time::Instant::now() + CAPTURE_FRAME_TIMEOUT;
    loop {
        if frame.exists() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(
                "the screen frame wasn't captured (Screen Recording consent is needed \
                 on-device)"
                    .to_string(),
            );
        }
        tokio::time::sleep(CAPTURE_FRAME_POLL).await;
    }
    Ok(frame)
}

// ===========================================================================
// AUDIO SCENE UNDERSTANDING — on-device Sound Analysis (task #15, build 2/3).
//
// DISTINCT from STT (speech-to-text). STT answers "what did someone SAY" (words);
// this answers "what was that SOUND" (a doorbell, an alarm, glass breaking, a
// dog, music) via Apple Sound Analysis — the built-in ~300-class
// SNClassifierIdentifier.version1, on-device/ANE-eligible. The two never overlap:
// the STT path transcribes the user's utterance into the router; this path takes
// an ALREADY-CAPTURED audio CLIP (the daemon's VAD/cpal buffer, written to a WAV
// the SAME way an utterance is) and hands it to the Vision app's `classify.sound`
// op, which returns the top sound CLASSES.
//
// PRIVACY / HONESTY:
//   * ONLY the sound-class LABELS (+ confidence) ever leave the op — the AUDIO
//     never leaves the device (the op reads the local clip; the daemon never
//     ships the clip anywhere; the telemetry carries labels only, never samples).
//   * The classifier knows a FIXED ~300 classes — NOT "any sound". An unknown /
//     too-short / undecodable clip yields the op's honest `no_sound_classes`
//     vision.error, never a fabricated label.
//   * The one-shot "what was that sound" intent runs on a clip the daemon ALREADY
//     has — it opens NO new microphone. CONTINUOUS ambient monitoring is the
//     SEPARATE [audio].sound_monitor path, which SHIPS ON (opt-OUT, not opt-in —
//     this line used to say "OFF", which was simply wrong) and is TCC/mic-gated:
//     macOS consent, not the flag, is what keeps it inert on a fresh install.
//     See `ambient_monitor_should_start`.
// ===========================================================================

/// An "identify this sound" turn: the SOUND-identify intent fired, carrying the
/// clip to classify (the daemon's last captured segment, supplied by the caller)
/// — or `None` when there is no clip, so the handler reports that honestly rather
/// than the turn silently falling through to a generic answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifySoundRequest {
    /// The already-captured clip to classify, or None when the daemon has none.
    /// NEVER user-named, never a fresh capture — no microphone is opened to fill it.
    pub clip: Option<PathBuf>,
}

/// Map a spoken utterance to an [`IdentifySoundRequest`], or None when it is not a
/// sound-identify request (the turn falls through to normal routing). PURE +
/// deterministic so the mapping is unit-tested without a socket, a running app,
/// the classifier, or a microphone.
///
/// The clip is the daemon's most-recent captured audio segment — supplied by the
/// caller (`latest_clip`), NOT named by the user — so this never opens the mic: it
/// classifies sound the daemon ALREADY heard. When the intent fires but there is
/// no clip, the request still routes (with `clip: None`) so the handler answers
/// honestly ("no recent clip") instead of guessing.
///
/// Recognized (case-insensitive, whole lowercased utterance) — a SOUND-identify
/// verb, never a SPEECH/transcription verb (STT stays distinct):
///   - "what was that sound" / "what was that noise" / "what's that sound" /
///     "identify that sound" / "what am i hearing" / "what do you hear" /
///     "what sound was that" / "name that sound"
fn identify_sound_clip_or_request(
    text: &str,
    latest_clip: Option<&Path>,
) -> Option<IdentifySoundRequest> {
    if !is_identify_sound_request(text) {
        return None;
    }
    // The clip is the daemon's last captured segment — never user-named, never a
    // fresh capture. None => no clip to classify (the handler reports it honestly).
    Some(IdentifySoundRequest {
        clip: latest_clip.map(|p| p.to_path_buf()),
    })
}

/// Whether the utterance is an "identify this sound" request — a SOUND-scene
/// query, DISTINCT from STT (speech). PUBLIC so the pipeline (main.rs) can keep
/// this turn's handling consistent with the other transient perception reads.
/// Pure over the same recognition `identify_sound_clip` uses, so the predicate
/// and the routing agree by construction.
///
/// Guarded so a SPEECH-transcription phrasing ("what did I/he/she/they say",
/// "transcribe", "what did you hear me say") NEVER lands here — that is the STT
/// path's job. The trigger is a SOUND/NOISE/HEAR verb with no "say"/"said"/
/// "transcribe"/"word" speech cue.
pub fn is_identify_sound_request(text: &str) -> bool {
    let lower = text.to_lowercase();

    // STT VETO: a speech-transcription phrasing is the STT path, never this one.
    // "what did <someone> say", "transcribe", "what were the words" must fall
    // through so the sound-scene classifier never shadows speech understanding.
    const SPEECH_CUES: &[&str] = &[
        " say", " said", "transcribe", "transcription", " words", " spoken", "what did i ", "what did you hear me",
    ];
    if SPEECH_CUES.iter().any(|c| lower.contains(c)) {
        return false;
    }

    // SOUND-identify phrasings. A "sound"/"noise" object with an identify/what-was
    // verb, or a bare "what am i hearing" / "what do you hear" (hearing a SOUND,
    // not parsing speech — the speech veto above already removed "hear me say").
    let mentions_sound = lower.contains("sound") || lower.contains("noise");
    let identify_verb = lower.contains("what was that")
        || lower.contains("what's that")
        || lower.contains("what is that")
        || lower.contains("what was")
        || lower.contains("identify")
        || lower.contains("name that")
        || lower.contains("what kind of");
    if mentions_sound && identify_verb {
        return true;
    }
    // Bare hearing queries (no "sound" word needed): "what am I hearing", "what
    // do you hear", "what are you hearing". Speech ("hear me say") was vetoed.
    lower.contains("what am i hearing")
        || lower.contains("what do you hear")
        || lower.contains("what are you hearing")
        || lower.contains("what's that i hear")
}

/// The confined clip path the daemon writes (or already wrote) for a one-shot
/// sound classification, under the project state dir (an allowlisted root). This
/// mirrors the utterance-WAV location the VAD/cpal capture loop uses — the clip
/// the daemon ALREADY captured — so no new microphone is opened to answer "what
/// was that sound". The handler path-confines this before the op exactly like a
/// describe frame.
fn sound_clip_path(root: &Path) -> PathBuf {
    root.join("state").join("tmp").join("sound-clip.wav")
}

/// Execute an "identify this sound" request: PATH-CONFINE the already-captured
/// clip, forward the Vision app's on-device `classify.sound` op, and surface the
/// top sound classes. Routes to the VISION agent (the caller re-pins it). ONLY
/// the sound-class LABELS leave the op; the AUDIO never leaves the device — the
/// daemon hands the op a LOCAL clip path and never ships the audio anywhere.
///
/// HONESTY-FIRST:
///   * No clip to classify (`clip` is None) -> say so plainly; never fabricate a
///     label and never open the mic to make one.
///   * The clip path is PATH-CONFINED under the allowed root BEFORE the op
///     (symlink-escape / `..` / absolute-elsewhere / nonexistent are REJECTED) —
///     mirrors the describe-frame confinement.
///   * The recognized classes arrive ASYNCHRONOUSLY on the `vision.sound`
///     telemetry event (relayed to the HUD by the app relay), NEVER in this
///     synchronous reply — so the acknowledgment is content-free about the labels.
///   * On an empty/too-short/undecodable clip the op emits the honest
///     `no_sound_classes` vision.error — the daemon never invents a class.
async fn handle_identify_sound(
    clip: Option<PathBuf>,
    app_registry: &Arc<AppRegistry>,
    allowed_root: &Path,
) -> HandlerOutput {
    let data = match clip {
        None => {
            // Nothing captured to classify — honest, no mic opened, no guess.
            "I don't have a recent sound clip to identify, sir. The sound classifier \
             runs on-device over audio I've already captured — it never opens the mic on its own."
                .to_string()
        }
        Some(candidate) => {
            // PATH CONFINEMENT (the security primitive, mirrors describe + docsearch::
            // confine): canonicalize + assert the clip resolves under the allowed
            // root. An escape / nonexistent clip is REJECTED — never sent to the op.
            match std::fs::canonicalize(allowed_root)
                .ok()
                .and_then(|canon_root| {
                    crate::docsearch::confine(&candidate, std::slice::from_ref(&canon_root))
                }) {
                None => {
                    "That sound clip isn't in a folder I'm allowed to read from, sir, so I won't classify it."
                        .to_string()
                }
                Some(real) => {
                    let op = op_classify_sound(&real.display().to_string());
                    match apps::send_op(app_registry, VISION_APP, &op).await {
                        Ok(()) => {
                            info!(app = VISION_APP, op = %op, "forwarded classify.sound op");
                            // TELEMETRY: the wiring ran. LABELS-ONLY by construction —
                            // the actual classes ride the async vision.sound relay
                            // (the app emits {label,confidence} only; the audio never
                            // leaves the device). This event carries NO audio, NO clip
                            // samples, NO path — just that the on-device classify ran.
                            telemetry::emit(
                                "local",
                                "audio.sound",
                                json!({
                                    "op": "classify.sound",
                                    "classifier": "SNClassifierIdentifier.version1",
                                    "labels_only": true,
                                    "audio_left_device": false,
                                }),
                            );
                            "Listening back on that now, sir — the sound classes will appear on the Vision panel. \
                             It's on-device Apple Sound Analysis, so only the labels surface; the audio never leaves the Mac."
                                .to_string()
                        }
                        Err(e) => {
                            warn!(app = VISION_APP, op = %op, error = %e, "classify.sound forward failed");
                            format!("I couldn't reach Vision to classify that sound: {e}. Open it first, sir.")
                        }
                    }
                }
            }
        }
    };
    HandlerOutput {
        data,
        llm_voice: true,
    }
}

/// PURE gate for the ambient sound monitor (task #15). The monitor
/// PERIODICALLY classifies ambient audio + emits sound-class events ONLY when
/// `[audio].sound_monitor` is on. Factored out so the "inert without consent + never
/// auto-starts the mic" invariant is unit-testable without a clock, a mic, or a spawn.
///
/// Returns `true` (the monitor may start) ONLY when `[audio].sound_monitor` is
/// true (the SHIPPED default is true, but INERT WITHOUT mic/TCC consent). With it
/// false this returns false: the monitor NEVER starts, the mic is never opened for
/// ambient classification, and the audio path is byte-for-byte today's. macOS mic/TCC
/// consent is a SEPARATE on-device gate the daemon cannot grant — even when this
/// returns true, the actual ambient capture is device-gated and is NOT exercised
/// here (the one-shot intent + this gate are what the tests cover).
///
/// PRIVACY — READ THIS CAREFULLY, BECAUSE IT USED TO SAY THE OPPOSITE. This
/// paragraph claimed the switch was "opt-in" with "no default-on / auto-arm
/// anywhere". That was FALSE, and it inverted the load-bearing privacy fact about
/// the daemon's CONTINUOUS ambient-microphone path in the first place an auditor
/// looks: `[audio].sound_monitor` ships **true** (config.rs `impl Default for
/// AudioConfig`, `config/darwin.toml`, and this file's own
/// `sound_monitor_ships_on_and_keys_are_known` test all say so), so on a default
/// config this function returns TRUE and main.rs takes the OPTED-IN branch. The
/// switch is therefore opt-OUT, and the only thing keeping the monitor inert on a
/// fresh install is macOS mic/TCC consent — NOT the flag. What IS true: the flag
/// lives in the user-owned config, so no tool/agent/model route can flip it, and
/// an operator who sets `sound_monitor = false` gets a monitor that never starts.
pub fn ambient_monitor_should_start(sound_monitor_enabled: bool) -> bool {
    sound_monitor_enabled
}

// ===========================================================================
// Nexus voice control (SPEC §6 — the daemon forwards STRUCTURED ops ONLY; the
// Nexus app never parses natural language).
//
// Nexus (apps/nexus) is a PYTHON control plane hosting a native Rust DSP core.
// Its HOST -> APP op wire form is the BARE `{"op":"<name>", ...}` object (NOT
// the `{"type":"op",...}` envelope Vision uses) — its OpDispatcher in
// apps/nexus/main.py reads `msg["op"]` and dispatches on the dotted name. The
// op-string builders below produce that EXACT wire shape, matching the SPEC §5
// op table and the dispatch handlers verbatim:
//   gain.set   {"op":"gain.set","channel":N,"mute":bool,"stage":"input"}  (mute)
//   gain.set   {"op":"gain.set","channel":N,"gain_db":F,"stage":"input"|"output"}
//   route.set  {"op":"route.set","in":N,"out":M,"gain_db":F}
//   monitor.set{"op":"monitor.set","in":N,"out":M,"on":bool}
//   preset.load{"op":"preset.load","name":"<name>"}
//   state.get  {"op":"state.get"}
// serde_json builds each line so a preset name with a quote can never break the
// JSON framing. The classifier is checked alongside the Silicon Canvas / Vision
// seams, before the generic local handlers, so a precise audio-control phrase is
// handled deterministically and never lands on the cloud/LLM.
//
// The realtime CoreAudio path is DEVICE-GATED and is NEVER touched here: these
// ops are control-plane messages to the Python host; whether a device is bound
// is the app's concern. The daemon only classifies the utterance and forwards
// the structured op — it opens no audio device and plays no audio.
// ===========================================================================

/// The Nexus micro-app's registered name (its manifest `[app].name` and the key
/// into the app registry / its socket).
pub const NEXUS_APP: &str = "nexus";

/// The Nexus monitor bus output index. "route input 1 to the monitor" / "mute
/// the mic" need a default output and a default input to be actionable without
/// the user naming channel numbers. Output 0 is the monitor bus and input 0 is
/// the SM7dB mic by the SPEC §3 gain-staging convention (the mic is the primary
/// input; the monitor is the direct-monitor output). These are the targets a
/// bare "the mic" / "the monitor" resolves to; an explicit "input N" / "output
/// M" in the utterance overrides them.
const NEXUS_MONITOR_OUT: u32 = 0;
const NEXUS_MIC_INPUT: u32 = 0;

/// What a Nexus voice command resolves to: LAUNCH the app, or forward a
/// STRUCTURED op line to the already-running app. The op body is opaque to the
/// daemon (built to match apps/nexus/main.py's OpDispatcher wire form).
#[derive(Debug, Clone, PartialEq)]
pub enum NexusCommand {
    /// "open nexus" — start the micro-app.
    Launch,
    /// A complete JSON op line (one line) to forward verbatim, e.g.
    /// `{"op":"gain.set","channel":0,"mute":true,"stage":"input"}`.
    Op(String),
}

/// Whether the utterance names the Nexus app / capability itself ("nexus", "the
/// matrix", "the routing matrix", "the mixer"). Used to gate the bare launch
/// verb so an unrelated "open safari" is never captured.
fn mentions_nexus(lower: &str) -> bool {
    contains_word(lower, "nexus")
        || lower.contains("routing matrix")
        || lower.contains("the audio matrix")
        || lower.contains("the mixer")
        || lower.contains("the routing grid")
}

/// Map a spoken utterance to a Nexus command, or None when it is not a Nexus
/// control phrase (the turn then falls through to normal routing). Deterministic
/// and pure so the mapping is unit-tested without a socket, a running app, or
/// the classifier. Order matters: the specific ops (mute, route, gain, monitor,
/// preset, levels) are matched before the broad "open nexus" launch so a control
/// phrase that also says "open" is never mistaken for a launch.
///
/// Recognized phrases (all case-insensitive, whole lowercased utterance):
///   - "mute/unmute the mic" / "mute input N"          -> gain.set {mute}
///   - "set input/output gain to <dB>" /
///     "set the gain on input N to <dB>"               -> gain.set {gain_db}
///   - "route input N to the monitor/output M" /
///     "unroute input N from output M"                 -> route.set {gain_db|-inf}
///   - "monitor input N" / "stop monitoring"           -> monitor.set {on}
///   - "load the <name> preset" / "load preset <name>" -> preset.load {name}
///   - "what are the levels" / "show me the meters" /
///     "what's the matrix / routing state"             -> state.get
///   - "open/launch/start/bring up nexus"              -> Launch
pub fn nexus_command(text: &str) -> Option<NexusCommand> {
    let lower = text.to_lowercase();

    // --- mute / unmute (specific verb; before gain/route/launch) -----------
    // "mute the mic", "unmute input 2", "mute the microphone".
    //
    // WHAT WENT WRONG: `contains("mute")` fires inside "commute". Measured over
    // 1,897 everyday utterances, "my commute was a nightmare this morning"
    // MUTED THE OWNER'S MICROPHONE. Whole-word now, plus the utterance must
    // either name the hardware ("mute the mic", "mute input 2") or be nothing
    // but the bare mute idiom ("mute me", "unmute everything").
    if crate::utterance::mentions_any_word(&lower, &["mute", "unmute"])
        && (nexus_hardware_context(&lower) || nexus_bare_mute_phrase(&lower))
    {
        let unmute = mentions_word(&lower, "unmute") || lower.contains("un-mute");
        // WHAT WENT WRONG: the stage was hard-coded "input" and the channel was
        // always resolved against "input", while NEXUS_BARE_MUTE_VOCAB
        // deliberately admits the OUTPUT-side nouns ("speaker(s)",
        // "headphone(s)", "monitor", "output(s)"). So "mute the speakers" muted
        // the SM7dB MICROPHONE and left the speakers playing — an unrequested mic
        // mute reached through a legitimate command, which is the exact failure
        // this file spends paragraphs preventing elsewhere — and a later "unmute
        // the speakers" un-muted the mic. `set_output_mute` was unreachable from
        // voice at all, even though the app dispatches on the stage
        // (apps/nexus/main.py `_gain_set`). Same stage resolution as the gain
        // branch now.
        let (stage, channel) = if mentions_output(&lower) {
            ("output", extract_channel(&lower, "output").unwrap_or(NEXUS_MONITOR_OUT))
        } else {
            ("input", extract_channel(&lower, "input").unwrap_or(NEXUS_MIC_INPUT))
        };
        return Some(NexusCommand::Op(op_gain_mute(channel, !unmute, stage)));
    }

    // --- gain set ----------------------------------------------------------
    // "set input gain to -18", "set the gain on output 1 to -3 dB", "turn the
    // mic gain down to -24". Requires an explicit dB value to be a gain.set.
    //
    // WHAT WENT WRONG: `contains("gain")` fires inside "again", "against" and
    // "bargain", and the dB value is NOT a second safety net — extract_db takes
    // the first number after the LAST "to"/"at" anywhere in the sentence, so
    // "let's start again at 6" wrote +6 dB onto the mic and "how much weight
    // gain at 6 months is normal" wrote +6 dB. Whole words now, and the gain
    // word must be BOUND to a channel noun ("input gain", "the gain on input
    // 1", "output 1 gain"), or TAKE one as its direct object with an explicit
    // dB target ("trim input 1 to -6 db"), or the utterance must be nothing but
    // a bare gain instruction ("set the gain to -6").
    if crate::utterance::mentions_any_word(&lower, NEXUS_GAIN_WORDS)
        && (nexus_head_names_a_channel(&lower, NEXUS_GAIN_WORDS, NEXUS_GAIN_NOUNS)
            || nexus_gain_verb_targets_a_channel(&lower)
            || nexus_bare_gain_phrase(&lower))
    {
        if let Some(gain_db) = extract_db(&lower) {
            // Stage: "output" if the utterance names an output, else input
            // (the SM7dB chain trims the input by default — SPEC §3).
            let (stage, channel) = if mentions_output(&lower) {
                ("output", extract_channel(&lower, "output").unwrap_or(NEXUS_MONITOR_OUT))
            } else {
                ("input", extract_channel(&lower, "input").unwrap_or(NEXUS_MIC_INPUT))
            };
            return Some(NexusCommand::Op(op_gain_set(channel, gain_db, stage)));
        }
    }

    // --- gain set on a bare NUMBERED CHANNEL -------------------------------
    // "set channel 1 to -6 db". MEASURED RECALL MISS: reached nothing, because
    // the gain branch above keys on one of NEXUS_GAIN_WORDS and this phrasing
    // names the CHANNEL instead of the parameter — which is how the utterance
    // comes out when the person is looking at a mixer strip.
    //
    // THREE anchors, all required: a NUMBERED "channel", a set verb, and an
    // explicit DECIBEL unit. The route branch above measured that 0 of the 1,897
    // everyday utterances contain even a bare "input"/"output"; a sentence that
    // additionally states a dB figure after a set verb is about audio.
    if let Some(channel) = extract_channel(&lower, "channel") {
        if crate::utterance::mentions_any_word(&lower, &["set", "trim", "turn", "put", "bring"])
            && (lower.contains("db") || lower.contains("decibel"))
        {
            if let Some(gain_db) = extract_db(&lower) {
                let stage = if mentions_output(&lower) { "output" } else { "input" };
                return Some(NexusCommand::Op(op_gain_set(channel, gain_db, stage)));
            }
        }
    }

    // --- route / unroute ---------------------------------------------------
    // "route input 1 to the monitor", "route input 2 to output 3", "unroute
    // input 1 from the monitor". A "route … to the monitor" without an explicit
    // output targets the monitor bus.
    //
    // WHAT WENT WRONG, and this is the worst branch in the classifier because
    // it is the one that can DESTROY a crosspoint. It matched on five bare
    // `contains()` calls, one of them "send" — one of the most ordinary verbs
    // in English — paired with a bare "input"/"monitor"/"output". That emitted
    // a route.set for "send me the output of the report", "please send the
    // output to the printer", and for "my router keeps dropping and the input
    // lags" ("route" INSIDE "router"). Worse, `clear` was `contains("from") &&
    // !contains(" to ")`, so an ordinary "from" with no "to" wrote -inf and
    // CLEARED the crosspoint: "send me the output from the meeting" silently
    // tore down a route.
    //
    // The gate is deliberately ASYMMETRIC, and that asymmetry is the fix for
    // the previous attempt, which gated this branch on "the sentence mentions
    // some audio hardware somewhere" and thereby made the DESTRUCTIVE branch
    // WIDER than the code it replaced: with only a noun required, "send the
    // microphone back to amazon", "send me the mic drop clip" and "send a clear
    // photo of the microphone to the seller" all became crosspoint writes, the
    // last one a -inf CLEAR, on utterances the original code ignored. So:
    //   * a NUMBERED channel ("input 1", "output 3") admits any of the verbs,
    //     including the ordinary-English "send" and "disconnect" — nobody says
    //     "input 1" by accident, and 0 of the 1,897 everyday utterances contain
    //     even a bare "input" or "output";
    //   * otherwise the utterance must be a routing SENTENCE, not a sentence
    //     that happens to contain routing words. An unmistakable ROUTING verb
    //     (route/patch and their un-/re- forms) must TAKE the signal chain as
    //     its object, and a prepositional phrase must name where the signal
    //     goes. Each of those three is load-bearing and each was measured:
    //     without the verb restriction "send the microphone back to amazon" was
    //     a write; without the destination "the mic patch cable is broken" and
    //     "there's a patch for the preamp driver" were writes; without the
    //     object binding "route the kids from the mic stand to the door" was a
    //     write. A patch CABLE is a parcel, and a mic STAND is furniture.
    // The `from`/`to` heuristic is gone entirely; clearing now requires an
    // explicit unroute/unpatch/clear/disconnect verb, whole-word so "clear"
    // never fires inside "nuclear".
    if crate::utterance::mentions_any_word(
        &lower,
        &[
            "route", "routes", "reroute", "unroute", "patch", "patches", "repatch", "unpatch",
            "send", "disconnect",
        ],
    ) && (mentions_nexus_numbered_channel(&lower)
        || (nexus_routing_verb_takes_the_signal_chain(&lower)
            && nexus_names_a_routing_destination(&lower)))
    {
        let clear = crate::utterance::mentions_any_word(
            &lower,
            &["unroute", "unpatch", "clear", "disconnect"],
        ) || lower.contains("un-route")
            || lower.contains("un-patch");
        let input = extract_channel(&lower, "input").unwrap_or(NEXUS_MIC_INPUT);
        // The destination output: an explicit "output M", else the monitor bus
        // when "monitor" is named, else the monitor bus as the sensible default.
        let output = extract_channel(&lower, "output").unwrap_or(NEXUS_MONITOR_OUT);
        // 0 dB unity on connect; -inf clears the crosspoint (SPEC §5 route.set).
        let gain_db = if clear { f64::NEG_INFINITY } else { 0.0 };
        return Some(NexusCommand::Op(op_route_set(input, output, gain_db)));
    }

    // --- monitor on/off ----------------------------------------------------
    // "monitor input 1", "stop monitoring", "turn off the monitor". This is the
    // direct-monitor route toggle (SPEC §5 monitor.set), distinct from a generic
    // crosspoint route above (which already matched if "route"/"send" was said).
    //
    // WHAT WENT WRONG: this fired on a bare `contains("monitor")` with no gate
    // at all, and "monitor" is a verb ordinary people use about their bodies
    // and their money. Over 1,897 everyday utterances it REWROTE a monitor
    // crosspoint 25 times — "I need to monitor my blood pressure twice a day",
    // "the rangers monitor the snowpack all winter", "the bank offers free
    // credit monitoring", "is a curved monitor worth the extra money". A
    // sentence about blood pressure mutated the audio matrix.
    //
    // The utterance must now name the hardware ("monitor input 1", "monitor the
    // mic") or be nothing but the bare toggle. Note what is NOT accepted as
    // hardware: a bare "channel", "input", "output" or "audio". Those are what
    // an earlier draft of this fix used, and "monitor the slack channel for
    // updates", "can you monitor the youtube channel" and "monitor the output
    // of the build script" walked straight through it.
    //
    // "unmonitor" is whole-word alongside "monitor"/"monitoring": as a raw
    // substring it half-matched inside "unmonitored" and turned "the server
    // input is unmonitored overnight" into monitor.set{on:false}.
    //
    // WHAT WENT WRONG IN THE OFF-WORDS, and this one is the dangerous
    // direction. Making the off-words whole-word stopped them matching the
    // PAST and PROGRESSIVE forms that `contains("stop")`/`contains("disable")`
    // used to catch, and the failure is not a missed command — it is a FLIP
    // into the device-activating direction. Measured: "I stopped monitoring the
    // mic last week", "I've stopped monitoring my mic levels", "we stopped
    // monitoring input 1 months ago" and "they disabled the mic monitoring
    // already" each went on:false -> on:TRUE. A sentence that literally says
    // the user STOPPED monitoring would have OPENED a live mic-to-monitor-bus
    // crosspoint where the code being replaced closed one. Every inflection is
    // therefore listed. "kill" is listed for the same reason: it is already in
    // the bare-toggle vocabulary, so "kill the monitor" reaches here — and
    // without it in this list it turned the monitor ON. These words can only
    // ever be read inside this already-gated branch, so naming more of them
    // cannot widen the classifier; it can only stop it opening a mic.
    //
    // A whole-word "off" is added because "turn the monitor off" is the same
    // command as "turn off the monitor" and the substring test only caught one
    // of the two word orders.
    if crate::utterance::mentions_any_word(&lower, &["monitor", "monitoring", "unmonitor"])
        && (nexus_hardware_context(&lower) || nexus_bare_monitor_toggle(&lower))
    {
        let off = crate::utterance::mentions_any_word(
            &lower,
            &[
                "stop", "stopped", "stopping", "disable", "disabled", "disabling", "kill",
                "killed", "unmonitor", "unmonitored", "off",
                // Admitting a verb to the toggle's TRIGGER list without adding it
                // here is how "quit monitoring" and "pause monitoring" ended up
                // turning the monitor ON: the branch fired, saw no off-word it
                // recognized, and defaulted to on. Every off-verb the trigger
                // accepts must be readable here or the toggle opens a live mic on
                // a request to close one.
                "quit", "quitting", "pause", "paused", "pausing", "end", "ended",
                "ending", "shut", "cut", "switch off",
            ],
        ) || lower.contains("turn off")
            || lower.contains("no longer");
        let input = extract_channel(&lower, "input").unwrap_or(NEXUS_MIC_INPUT);
        let output = extract_channel(&lower, "output").unwrap_or(NEXUS_MONITOR_OUT);
        return Some(NexusCommand::Op(op_monitor_set(input, output, !off)));
    }

    // --- preset load -------------------------------------------------------
    // "load the vocal preset", "load preset podcast", "recall the streaming
    // preset". Only LOAD (preset.save is a panel/manual action, not voiced).
    //
    // LEFT ALONE DELIBERATELY. "preset" occurs in 0 of the 1,897 everyday
    // utterances, the branch needs BOTH a load verb and an extractable name,
    // and whole-wording "load" here would break "download the vocal preset" /
    // "reload the preset" for no measured gain.
    if (lower.contains("load") || lower.contains("recall") || lower.contains("apply"))
        && lower.contains("preset")
    {
        if let Some(name) = extract_preset_name(&lower) {
            return Some(NexusCommand::Op(op_preset_load(&name)));
        }
    }

    // --- state / levels query ----------------------------------------------
    // "what are the levels", "show me the meters", "what's the routing state",
    // "read out the matrix". A read-only snapshot request (SPEC §5 state.get).
    // "matrix" is a routing snapshot ONLY in a Nexus/routing context — a bare
    // "matrix" (e.g. "the matrix movie") is conversational and must fall
    // through, so it is gated on a routing/read co-word or a Nexus mention.
    //
    // WHAT WENT WRONG: the "matrix" arm got that gate. "level" and "meter"
    // never did, and they are two of the most ordinary nouns in English. A bare
    // `contains("level") || contains("meter")` answered 93 of the 1,897
    // everyday utterances with an audio-matrix snapshot instead of a real
    // answer: "my stress levels have been through the roof", "sea level is
    // rising a little every year", "I got a parking meter ticket", "the gas
    // meter guy is coming Thursday", "how many meters is a lap in that pool".
    // It did not even require the word to BE a word — it fired inside
    // "thermometer" ("do I own a meat thermometer") and inside "levelled". The
    // matrix arm carried the same disease one level down: `contains("rout")` is
    // a FRAGMENT and fired inside "routine", so "my whole routine is a matrix
    // of pills and timers" read the routing matrix.
    //
    // A wrong read is an annoyance rather than a privacy incident, so this arm
    // keeps a wider escape than the mutating branches above — but it is an
    // escape of the same KIND. Three ways in, in narrowing order: the utterance
    // names the hardware; or the level word is BOUND to a channel noun ("my
    // input levels", "the mic level"); or the whole utterance is nothing but a
    // bare read of "the levels"/"the meters".
    //
    // "whats" is listed beside "what" and that is not cosmetic. Making this a
    // whole-word test broke the apostrophe-free form STT emits constantly:
    // "whats routed to output 1" stopped reading the matrix, and because the
    // route branch above still saw "output 1", the question about the routing
    // fell into the branch that WRITES it. A contraction must not be the
    // difference between reading a crosspoint and rewiring one.
    let matrix_state_query = mentions_word(&lower, "matrix")
        && !mentions_nexus_launch_verb(&lower)
        && (mentions_nexus(&lower)
            || crate::utterance::mentions_any_word(
                &lower,
                &["route", "routes", "routed", "routing", "crosspoint", "crosspoints"],
            )
            || lower.contains("read out")
            || lower.contains("read me")
            || mentions_word(&lower, "state"));
    let levels_query = crate::utterance::mentions_any_word(&lower, NEXUS_LEVEL_WORDS)
        && (nexus_hardware_context(&lower)
            || nexus_head_names_a_channel(&lower, NEXUS_LEVEL_WORDS, NEXUS_LEVEL_NOUNS)
            || nexus_bare_levels_read(&lower));
    if levels_query
        || matrix_state_query
        || lower.contains("routing state")
        || lower.contains("route state")
        // MEASURED RECALL MISS: "what's the mixer state" reached nothing. The
        // read-only snapshot was reachable through "matrix"/"levels"/"routed" but
        // not through the name of the thing itself. Each phrase binds the state
        // noun to the MIXER, so a bare "state" is never enough.
        || lower.contains("mixer state")
        || lower.contains("state of the mixer")
        || lower.contains("nexus state")
        || lower.contains("state of nexus")
        || (crate::utterance::mentions_any_word(&lower, &["what", "whats"])
            && mentions_word(&lower, "routed"))
    {
        return Some(NexusCommand::Op(op_state_get()));
    }

    // --- launch ------------------------------------------------------------
    // Only when the utterance actually names Nexus AND carries an open-class
    // verb — "open nexus", "bring up the routing matrix". Last so a control
    // phrase that also says "open" was already handled above.
    if mentions_nexus(&lower) && mentions_nexus_launch_verb(&lower) {
        return Some(NexusCommand::Launch);
    }

    None
}

/// Whether the utterance carries an open-class verb (used both to gate the Nexus
/// launch and to keep "open the matrix" from being read as a state query).
fn mentions_nexus_launch_verb(lower: &str) -> bool {
    lower.contains("open")
        || lower.contains("launch")
        || lower.contains("start")
        || lower.contains("bring up")
        || lower.contains("fire up")
        || lower.contains("show")
}

/// The PHYSICAL signal chain: words that are only ever audio gear. Shared by
/// `mentions_nexus_signal_chain` (does the sentence name this app's hardware at
/// all) and `nexus_routing_verb_takes_the_signal_chain` (is that hardware the
/// thing being routed), so the two can never drift apart.
const NEXUS_SIGNAL_CHAIN_NOUNS: &[&str] = &[
    "mic", "mics", "microphone", "microphones", "fader", "faders", "preamp", "preamps",
    "crosspoint", "crosspoints", "submix", "submixes",
];

/// Whether the utterance names THIS APP'S HARDWARE by a word that is only ever
/// audio gear — the physical signal chain ("the mic", "the faders", "the
/// preamp", "a crosspoint") or one of the Nexus aliases that is already a
/// multi-word phrase ("the routing matrix", "the mixer").
///
/// The bare word "nexus" is NOT here, and that omission is load-bearing.
/// `mentions_nexus` accepts it, which is right for the LAUNCH ("open nexus") —
/// but "nexus" is an ordinary English noun and a very common venue/brand name.
/// It appears in 12 of the 1,897 everyday utterances measured against this
/// classifier ("book us a table at Nexus Bistro", "do I have tax nexus in
/// another state", "the region is a nexus of trade disputes", "who's playing at
/// the Nexus arena"). Letting it satisfy the gate below would turn "send the
/// Nexus card statement to my accountant" into a crosspoint WRITE that the
/// original code correctly ignored. A mutation has to be aimed at a mic or at a
/// numbered channel, not at a hotel.
fn mentions_nexus_signal_chain(lower: &str) -> bool {
    lower.contains("routing matrix")
        || lower.contains("the audio matrix")
        || lower.contains("the mixer")
        || lower.contains("the routing grid")
        || crate::utterance::mentions_any_word(lower, NEXUS_SIGNAL_CHAIN_NOUNS)
}

/// Whether the utterance names an EXPLICITLY NUMBERED channel — "input 1",
/// "output 3". Deliberately the numbered form only: reusing `extract_channel`
/// means the keyword must be followed by an integer, so "input 2" counts and
/// "the input queue" / "the output of the build script" do not. Bare
/// "input"/"output"/"channel"/"audio" are what made the first version of this
/// gate useless — in ordinary speech "channel" is Slack or YouTube, "input" and
/// "output" are data.
fn mentions_nexus_numbered_channel(lower: &str) -> bool {
    extract_channel(lower, "input").is_some() || extract_channel(lower, "output").is_some()
}

/// The context an op needs before it may act: the utterance must name this
/// app's hardware. The per-branch idioms below are the only other way past it,
/// and they get narrower the more destructive the op is.
fn nexus_hardware_context(lower: &str) -> bool {
    mentions_nexus_signal_chain(lower) || mentions_nexus_numbered_channel(lower)
}

/// The verbs that can only mean CROSSPOINT ROUTING. "send" and "disconnect" are
/// NOT here: they are ordinary English and are admitted by the numbered-channel
/// path only.
const NEXUS_ROUTING_VERBS: &[&str] = &[
    "route", "routes", "reroute", "unroute", "patch", "patches", "repatch", "unpatch",
];

/// Whether a routing verb actually takes the signal chain as its OBJECT —
/// "route THE MIC to the monitor", "unpatch THE STUDIO MIC from the monitor" —
/// rather than merely sharing a sentence with an audio noun.
///
/// WHAT WENT WRONG WITHOUT THIS: requiring a routing verb, an audio noun and a
/// destination preposition is still CO-OCCURRENCE, just with three terms
/// instead of two. "route the kids from the mic stand to the door" has all
/// three and WROTE A CROSSPOINT the original code ignored. The mic there is a
/// modifier inside the SOURCE phrase ("the mic stand"), not the signal being
/// routed.
///
/// The object is everything between the verb and the FIRST preposition after
/// it, which is where English puts the thing being moved — "route [the mic] to
/// the monitor", "route [the kids] from the mic stand to the door". Taking the
/// span up to the FIRST preposition rather than the last, or the whole
/// sentence, is what separates those two, and unlike a bare direct-object test
/// it still admits an adjective between the determiner and the noun ("route the
/// studio mic to the monitor"), which people say constantly.
///
/// Only the PHYSICAL signal chain counts as the object, never the app aliases:
/// "can you patch the mixer software to the new version tonight" is a software
/// chore, and letting "the mixer" be a routable object would have made it a
/// crosspoint write. Routing the mixer itself is addressed by naming channels.
fn nexus_routing_verb_takes_the_signal_chain(lower: &str) -> bool {
    const PREPS: &[&str] = &[
        "to", "into", "onto", "from", "of", "for", "at", "on", "with", "by", "in",
    ];
    let toks: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    for (i, t) in toks.iter().enumerate() {
        if !NEXUS_ROUTING_VERBS.contains(t) {
            continue;
        }
        for w in toks.iter().skip(i + 1) {
            if PREPS.contains(w) {
                break;
            }
            if NEXUS_SIGNAL_CHAIN_NOUNS.contains(w) {
                return true;
            }
        }
    }
    false
}

/// Whether a routing PREPOSITIONAL PHRASE binds the verb to a place — "route
/// the mic TO the monitor", "patch the mic INTO output 2" — or, for the
/// CLEARING direction only, to a source: "unpatch the mic FROM the monitor".
///
/// WHAT WENT WRONG WITHOUT THIS: the signal-chain-noun path below was pure
/// CO-OCCURRENCE — a routing verb anywhere plus an audio noun anywhere, with
/// nothing binding them — and "patch" and "route" are ordinary English NOUNS.
/// Ten sentences that name real studio gear became crosspoint WRITES the
/// original code ignored: "the mic patch cable is broken", "I need a longer
/// patch cable for the mic", "did the mic firmware patch land", "there's a
/// patch for the preamp driver", "can you patch the mixer software tonight",
/// "where did I put the mic patch bay diagram", "the fader cap fell off, order
/// a patch kit", "is there a patch note for the crosspoint bug", "my running
/// route goes past the microphone store", "the mixer is on the delivery route
/// today". A patch CABLE is a parcel too; the verb alone does not establish
/// command shape. A crosspoint has a destination, so the utterance must name
/// one.
///
/// "from" is admitted ONLY beside an explicit clearing verb, and that asymmetry
/// is deliberate. "unroute the mic from the monitor" and "unpatch the mic from
/// the monitor" are real commands with no "to" in them, so a destination-only
/// rule would silently kill the whole noun-form UNROUTE family — but a bare
/// "from" is the most ordinary preposition in the language ("there's a patch
/// from the vendor for the preamp") and must not open a write on its own.
fn nexus_names_a_routing_destination(lower: &str) -> bool {
    lower.contains(" to ")
        || lower.contains(" into ")
        || lower.contains(" onto ")
        || (lower.contains(" from ")
            && crate::utterance::mentions_any_word(
                lower,
                &["unroute", "unpatch", "clear", "disconnect"],
            ))
}

/// Nouns a GAIN or a TRIM can belong to. Only ever consulted through
/// `nexus_head_names_a_channel` / `nexus_gain_verb_targets_a_channel`, i.e.
/// bound by adjacency — the word has to be the head's neighbour, not merely
/// present somewhere in the sentence. Wider than `NEXUS_LEVEL_NOUNS` below
/// because the shipped gain idioms name the monitor side by its speaker ("set
/// the speaker gain to -12 db", "set the headphone gain to -6"), and because a
/// gain.set also needs an explicit dB value before it can fire.
const NEXUS_GAIN_NOUNS: &[&str] = &[
    "input", "inputs", "output", "outputs", "mic", "mics", "microphone", "microphones", "fader",
    "faders", "preamp", "preamps", "monitor", "monitors", "headphone", "headphones", "speaker",
    "speakers", "mixer", "crosspoint", "crosspoints",
];

/// Nouns a LEVEL or a METER can belong to. Deliberately just the I/O words, and
/// deliberately much shorter than the gain list: every other audio noun is
/// already `mentions_nexus_signal_chain`, so all this list adds is the bare,
/// UNNUMBERED "my input levels" / "the output meters" phrasing that the read
/// idiom needs. Widening it costs real accuracy for nothing — "speaker" would
/// make "the speaker levels at the conference were painful" a mixer read, and
/// "channel" would take "the channel levels on that stream".
const NEXUS_LEVEL_NOUNS: &[&str] = &["input", "inputs", "output", "outputs"];

/// Whether one of `heads` ("gain"/"trim", or "level"/"meter") is BOUND to one
/// of `nouns` — either the noun sits to its LEFT ("input gain", "output 1
/// gain", "my input levels") or a PREPOSITION links them to its RIGHT ("the
/// gain on input 1", "the level of the mic"). Articles, possessives, a channel
/// index and a spoken dB target may sit in between; nothing else may.
///
/// Adjacency rather than co-occurrence is the whole point, and the preposition
/// on the right side is not decoration. "I need to trim the mic budget by 5
/// percent to 2 people" contains "trim" AND "mic" and sails through any
/// list-based gate; it also sails through a bare right-hand neighbour test,
/// because "the mic" is literally "trim"'s next content word. It is the DIRECT
/// OBJECT of the verb, not the owner of a trim — and "trim X" and "the trim on
/// X" are different sentences. Requiring on/of/for is what separates them. On
/// the other side, "let me check the gain we booked, bump it to 5" and "the
/// gain on the portfolio last year came to 12" have no channel noun adjacent to
/// "gain" at all; both of those wrote a gain value onto the mic before this
/// existed.
fn nexus_head_names_a_channel(lower: &str, heads: &[&str], nouns: &[&str]) -> bool {
    let toks: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let is_filler = |t: &str| {
        matches!(t, "the" | "a" | "an" | "my" | "our" | "its" | "this" | "that")
            || t.chars().all(|c| c.is_ascii_digit())
    };
    for (i, t) in toks.iter().enumerate() {
        if !heads.contains(t) {
            continue;
        }
        let mut j = i;
        while j > 0 {
            j -= 1;
            if is_filler(toks[j]) {
                continue;
            }
            if nouns.contains(&toks[j]) {
                return true;
            }
            break;
        }
        let mut k = i + 1;
        let mut linked = false;
        while k < toks.len() {
            if matches!(toks[k], "on" | "of" | "for") {
                linked = true;
                k += 1;
                continue;
            }
            // WHAT WENT WRONG: the walk stopped dead at "to", so "set the gain
            // TO -6 DB on input 1" — the single most common way anyone says it,
            // and the phrasing the SPEC's own gain-staging examples use — never
            // reached "input" and the whole command was thrown away. A spoken dB
            // TARGET is not a content word and it is not the thing the gain
            // belongs to; step over it and keep looking for the channel the
            // preposition binds. Note "to"/"at" deliberately do NOT set `linked`
            // — only on/of/for do — so "the gain to the input of the fund" is
            // still not a channel reference.
            if matches!(
                toks[k],
                "to" | "at" | "db" | "dbs" | "dbfs" | "decibel" | "decibels" | "minus" | "negative"
            ) {
                k += 1;
                continue;
            }
            if is_filler(toks[k]) {
                k += 1;
                continue;
            }
            if linked && nouns.contains(&toks[k]) {
                return true;
            }
            break;
        }
    }
    false
}

/// Whether the utterance carries an EXPLICIT dB target: a decibel unit word, or
/// a value that is NEGATIVE (spoken "minus 6" is normalized to -6 by
/// `extract_db`). Trims and monitor gains are cut, not boosted, so the negative
/// sign is the everyday form; a bare positive number is not evidence of
/// anything.
///
/// This is the ONLY thing separating "trim the mic to -6" (a command) from "I
/// need to trim the mic budget by 5 percent to 2 people" (a sentence about
/// staffing that contains both trigger words and puts the mic in the verb's
/// direct-object slot). The second one yields +2 with no unit and is refused.
fn nexus_explicit_db_target(lower: &str) -> bool {
    crate::utterance::mentions_any_word(lower, &["db", "dbs", "dbfs", "decibel", "decibels"])
        || matches!(extract_db(lower), Some(v) if v < 0.0)
}

/// Whether a gain/trim word takes a CHANNEL as its DIRECT OBJECT and the
/// utterance carries an explicit dB target — "trim input 1 to -6 db", "trim
/// output 1 to -3 db", "trim the mic to -6", "trim the input to -6 db".
///
/// WHAT WENT WRONG WITHOUT THIS: `nexus_head_names_a_channel` recognizes only
/// the NOUN form of a trim ("the input trim", "the trim on input 1"), and the
/// must-match corpus it was tuned against happened to contain only that form.
/// The VERB form — which is what "trim" mostly is, and the SPEC's own word for
/// input gain staging (§ "Gain staging policy": the interface preamp is
/// *trimmed* to -18 dBFS nominal) — was killed outright, including utterances
/// that name a numbered channel, which is this classifier's strongest safety
/// signal. Whole-word "trim" occurs in 0 of the 1,897 everyday utterances and
/// "gain" in 1 (with no dB target), so restoring the verb form behind an
/// explicit dB target costs nothing measurable.
fn nexus_gain_verb_targets_a_channel(lower: &str) -> bool {
    if !nexus_explicit_db_target(lower) {
        return false;
    }
    let toks: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    for (i, t) in toks.iter().enumerate() {
        if !NEXUS_GAIN_WORDS.contains(t) {
            continue;
        }
        // The direct object is the FIRST content word to the right. Articles and
        // possessives may intervene; a preposition may not — "the gain ON input
        // 1" is the noun form and is `nexus_head_names_a_channel`'s job, not
        // this one.
        for w in toks.iter().skip(i + 1) {
            if matches!(*w, "the" | "a" | "an" | "my" | "our" | "its" | "this" | "that") {
                continue;
            }
            return NEXUS_GAIN_NOUNS.contains(w);
        }
    }
    false
}

/// The head words of the two branches that use the adjacency test.
const NEXUS_GAIN_WORDS: &[&str] = &["gain", "gains", "trim", "trims"];
const NEXUS_LEVEL_WORDS: &[&str] = &["level", "levels", "meter", "meters", "metering"];

/// The subset of those the BARE read (the one path that names no channel at
/// all) will anchor on: the PLURAL/mass forms only. "the levels" and "the
/// meters" are how anyone talks about a mixer, which has many of both; the
/// SINGULAR is where ordinary speech lives — "check the meter" is a parking
/// meter, "what's the level" is a tank. Both singulars still work the moment
/// the utterance names a channel ("what's the level on input 1", "the mic
/// level"), which is the only context in which they mean this app.
const NEXUS_BARE_READ_HEADS: &[&str] = &["levels", "meters", "metering"];

/// Audio QUALIFIERS that may sit between the article and the level word — "the
/// PEAK levels", "the AUDIO levels", "the CURRENT meters", "the MASTER levels".
///
/// WHAT WENT WRONG WITHOUT THIS: the bare read demanded the head be literally
/// the next token after "the", so a single adjective — the most ordinary thing
/// in the world to say — destroyed the command. "show me the peak levels",
/// "what are the audio levels" and "show me the current levels" all died while
/// "show me the levels" worked. These words widen NOTHING on their own: they
/// are only reachable inside the CLOSED VOCABULARY below, which still refuses
/// any utterance carrying a content word from outside the list, so "the peak
/// levels of tourism in july" is rejected on "tourism" exactly as before.
const NEXUS_READ_QUALIFIERS: &[&str] = &[
    "peak", "peaks", "rms", "audio", "current", "master", "mix", "input", "inputs", "output",
    "outputs", "mic", "mics",
];

/// Whether EVERY token of the utterance is drawn from `vocab` (numbers too,
/// when `allow_numbers`), and there is at least one token.
///
/// This is the shape of a bare control phrase: "stop monitoring", "what are the
/// levels", "set the gain to -6" are complete utterances whose whole content IS
/// the command. One content word from outside the list — "reservoir", "credit",
/// "portfolio", "blood" — and it is somebody talking about their life, not
/// driving a mixer. A closed vocabulary is used instead of a keyword list
/// precisely because it cannot be satisfied by ADDING words; the previous
/// attempt gated on "does the sentence contain an audio-ish noun somewhere",
/// which every one of those sentences also satisfied.
fn nexus_closed_vocabulary(lower: &str, vocab: &[&str], allow_numbers: bool) -> bool {
    let mut any = false;
    for w in lower.split(|c: char| !c.is_alphanumeric()) {
        if w.is_empty() {
            continue;
        }
        any = true;
        if allow_numbers && w.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if !vocab.contains(&w) {
            return false;
        }
    }
    any
}

/// The bare mute idiom — "mute", "mute me", "unmute everything". "mute" is the
/// one trigger here that is unambiguous as a WHOLE word (the adjective sense,
/// "he went mute", brings its own content words and so fails this vocabulary),
/// so it may act without naming a channel. It still must be a whole word:
/// `contains("mute")` fires inside "commute", and "my commute was a nightmare
/// this morning" MUTED THE OWNER'S MICROPHONE in the measured corpus.
const NEXUS_BARE_MUTE_VOCAB: &[&str] = &[
    "mute", "unmute", "un", "the", "my", "a", "me", "myself", "yourself", "everything", "all",
    "audio", "sound", "input", "inputs", "output", "outputs", "channel", "channels", "mic",
    "mics", "microphone", "microphones", "monitor", "headphone", "headphones", "speaker",
    "speakers", "please", "darwin", "hey", "ok", "okay", "thanks", "thank", "you", "just", "can",
    "could", "would", "and", "now", "it", "again", "right", "for",
];

/// The bare gain idiom — "set the gain to -6", "turn the gain down to -12".
/// Also requires a set verb, because without one "my gain at 10" (every token
/// is in the list) wrote +10 dB onto the mic.
///
/// "right"/"for"/"me"/"us" are in the list for a measured reason: a closed
/// vocabulary is only as good as its coverage of the words people wrap a
/// command in, and "set the gain to -6 RIGHT NOW" died while "set the gain to
/// -6" worked. A trailing "right now" or "for me" is not a change of subject.
/// Adding them cannot widen the gate — every other token must still come from
/// this list AND the utterance must still carry a set verb and a dB value.
const NEXUS_BARE_GAIN_VOCAB: &[&str] = &[
    "gain", "trim", "set", "put", "turn", "make", "bring", "it", "the", "a", "my", "to", "at",
    "up", "down", "by", "db", "dbs", "decibel", "decibels", "minus", "negative", "plus",
    "please", "darwin", "hey", "ok", "okay", "thanks", "thank", "you", "just", "can", "could",
    "would", "and", "now", "right", "for", "me", "us",
    // THE VERBS ENGINEERS ACTUALLY USE. A closed vocabulary is only as good as
    // its coverage, and this list originally held set/turn/put/make/bring —
    // so "drop the gain to -12 db" and "lower the gain to -12 db" died while
    // "lower the INPUT gain to -12" survived, purely because a channel noun
    // happened to sit left of "gain". Same command, same speaker, served or not
    // by an accident of word order.
    "drop", "lower", "raise", "cut", "push", "pull", "adjust", "dial", "crank", "knock",
    "take", "back", "off", "ease", "nudge", "bump", "roll", "leave",
];

/// The bare levels read — "what are the levels", "show me the meters on my
/// screen", "what do the meters say", "what are the meters showing", "look at
/// the levels". Every word people wrap the read in has to be here or the
/// command dies on a preposition, which is how "look AT the levels" was lost.
const NEXUS_BARE_READ_VOCAB: &[&str] = &[
    "level", "levels", "meter", "meters", "metering", "what", "whats", "s", "is", "are", "how",
    "show", "showing", "read", "out", "check", "give", "tell", "look", "looking", "doing", "say",
    "saying", "like", "do", "does", "sitting", "currently", "right", "now", "on", "at", "screen", "the",
    "me", "us", "my", "you", "it", "please", "darwin", "hey", "ok", "okay", "thanks", "thank",
    "just", "can", "could", "would", "and", "peak", "peaks", "rms", "audio", "current", "master",
    "mix", "input", "inputs", "output", "outputs", "mic", "mics",
    // "are the levels clipping" is the most natural question anyone asks a meter,
    // and it died on "clipping". "again" was already in the mute and monitor
    // vocabularies and missing only here — the signature of a list fitted to a
    // corpus rather than to speech.
    "again", "more", "one", "time", "to", "we", "clipping", "peaking", "moving", "any",
    "over", "hot", "still", "there", "reading", "readings",
];

/// The bare monitor toggle — "stop monitoring", "turn the monitor off please",
/// "disable the monitor", "turn the monitor back on". Deliberately WITHOUT "me"
/// and without "my": "stop monitoring me" is a privacy complaint about the
/// assistant, and "I should monitor my credit" is a sentence about money —
/// neither is a request to drop a monitor crosspoint.
const NEXUS_MONITOR_TOGGLE_VOCAB: &[&str] = &[
    "monitor", "monitoring", "unmonitor", "stop", "start", "begin", "resume", "turn", "kill",
    "enable", "disable", "on", "off", "the", "a", "please", "darwin", "hey", "ok", "okay",
    "thanks", "thank", "you", "just", "can", "could", "would", "and", "then", "already", "right",
    "now", "back", "for", "again",
    // "switch off the monitor" / "quit monitoring" answered ON before this — the
    // toggle saw no off-verb it knew and defaulted the wrong way.
    "shut", "switch", "quit", "pause", "paused", "end", "ended",
];

/// Whether the utterance is nothing but a bare gain instruction.
fn nexus_bare_gain_phrase(lower: &str) -> bool {
    nexus_closed_vocabulary(lower, NEXUS_BARE_GAIN_VOCAB, true)
        && crate::utterance::mentions_any_word(
            lower,
            &[
                "set", "turn", "put", "make", "bring", "drop", "lower", "raise", "cut", "push",
                "pull", "adjust", "dial", "crank", "knock", "take", "ease", "nudge", "bump",
                "roll", "back",
            ],
        )
}

/// Whether the utterance is nothing but a bare mute instruction.
///
/// Numbers are allowed because a channel number is part of the command —
/// "mute channel 3", "unmute channel 3". The vocabulary already lists "channel"
/// but rejected the digit beside it, so those two died while "mute input 2"
/// lived. Admitting digits cannot widen this into ordinary speech: EVERY other
/// token still has to come from the mute vocabulary, and the branch is separately
/// gated on a whole-word "mute"/"unmute".
fn nexus_bare_mute_phrase(lower: &str) -> bool {
    nexus_closed_vocabulary(lower, NEXUS_BARE_MUTE_VOCAB, true)
}

/// The BARE monitor toggle: the whole utterance is the toggle, with nothing but
/// address and politeness around it, and it carries an actual on/off word.
///
/// A closed vocabulary rather than a list of fixed phrases, because the phrase
/// list has to be complete to be correct and it never is — "disable the
/// monitor" and "turn the monitor off please" are the same command as "turn off
/// the monitor" and a phrase list silently drops them. What this separates is
/// "stop monitoring" (a Nexus command) from "the bank told me to stop
/// monitoring my credit so obsessively" (a sentence about someone's money): a
/// sentence that has a subject or an object keeps them, fails the vocabulary,
/// and falls through to a real answer.
fn nexus_bare_monitor_toggle(lower: &str) -> bool {
    nexus_closed_vocabulary(lower, NEXUS_MONITOR_TOGGLE_VOCAB, false)
        && crate::utterance::mentions_any_word(
            lower,
            &[
                "stop", "start", "begin", "resume", "turn", "kill", "enable", "disable", "on",
                "off", "unmonitor", "quit", "pause", "end", "shut", "switch", "cut",
            ],
        )
}

/// The bare levels/meters read — "what are the levels", "show me the meters".
/// Every token must come from the read vocabulary AND the noun must carry the
/// definite article (an audio qualifier may sit between the two).
///
/// The article is doing real work, not stylistic work. Ordinary speech
/// qualifies the noun with a possessive or a scope, and the closed vocabulary
/// alone still admitted "can you check my levels" and "what are my levels" —
/// which are about bloodwork. Requiring "the levels" / "the meters" drops
/// exactly those and costs nothing: every idiom this app is actually driven by
/// carries the article, and the one real phrasing that does not ("what are my
/// input levels") names a channel and is admitted by the adjacency test
/// instead.
fn nexus_bare_levels_read(lower: &str) -> bool {
    if !nexus_closed_vocabulary(lower, NEXUS_BARE_READ_VOCAB, false) {
        return false;
    }
    let toks: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    toks.iter().enumerate().any(|(i, w)| {
        if !NEXUS_BARE_READ_HEADS.contains(w) || i == 0 {
            return false;
        }
        // Walk left over any audio qualifiers ("the PEAK levels") to the
        // article. Everything skipped here is itself confined to the closed
        // vocabulary above, so this cannot admit a sentence the vocabulary
        // already refused.
        let mut j = i;
        while j > 0 {
            j -= 1;
            if NEXUS_READ_QUALIFIERS.contains(&toks[j]) {
                continue;
            }
            return toks[j] == "the";
        }
        false
    })
}

/// Whether the utterance names an OUTPUT channel (so a gain.set / a mute targets
/// the output stage rather than the default input).
///
/// WHAT WENT WRONG: this doc used to claim that "output", "out", "speaker(s)",
/// "headphone(s)" and "monitor" "all name the output side" — but the body checked
/// only five words, and "monitor" was not one of them. That is not academic:
/// `NEXUS_GAIN_NOUNS` deliberately lists "monitor"/"monitors" (its own comment
/// says the gain idioms name the monitor side), so "set the monitor gain to -12
/// db" passed `nexus_head_names_a_channel`, fired the gain branch, fell to the
/// INPUT arm, and attenuated the SM7dB microphone 12 dB. The user turns down what
/// they hear and instead their voice goes quiet to everyone else — decided purely
/// by whether they said "monitor" or "speaker".
///
/// "monitor"/"monitors" are now checked. "out" deliberately is NOT: it is far too
/// common a word to bind a stage on ("check it out", "the mic cut out"), and no
/// shipped idiom needs it — that half of the old claim is simply dropped rather
/// than implemented.
fn mentions_output(lower: &str) -> bool {
    contains_word(lower, "output")
        || contains_word(lower, "outputs")
        || contains_word(lower, "speaker")
        || contains_word(lower, "speakers")
        || contains_word(lower, "headphone")
        || contains_word(lower, "headphones")
        || contains_word(lower, "monitor")
        || contains_word(lower, "monitors")
}

/// Extract the integer channel index following a `kind` keyword ("input" /
/// "output"), e.g. "input 1" -> 1, "output 3" -> 3. Returns None when the
/// keyword is absent or no number follows it — the caller then falls back to the
/// sensible default (the mic input / the monitor output). The number is taken as
/// spoken: Nexus indexes channels from 0, and the SM7dB mic is input 0, so a
/// user saying "input 1" means index 1 by the same convention the panel shows.
fn extract_channel(lower: &str, kind: &str) -> Option<u32> {
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    // Walk every occurrence of the keyword; the first one followed by a number
    // wins ("on input 2" and "input 2" both resolve).
    for (i, w) in words.iter().enumerate() {
        if *w == kind {
            if let Some(next) = words.get(i + 1) {
                if let Ok(n) = next.parse::<u32>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Extract a decibel value from a "set … gain to <X>" phrase. Accepts a signed
/// integer or float, with or without a "db"/"dB" suffix, and handles a spoken
/// "minus"/"negative" prefix ("minus 18", "negative 6 db") since speech-to-text
/// often spells the sign. Returns None when no numeric value is present (so a
/// gainless "set the gain" is not a gain.set and falls through). Not clamped —
/// the engine's set_*_trim is the authority on the valid range (SPEC §1:
/// -inf..+12 dB), and forwarding the spoken value verbatim keeps the daemon out
/// of the DSP policy.
///
/// The dB value is the number after the "to"/"at" target preposition when one is
/// present ("set the gain on input 1 to -12" -> -12, never the channel "1"); a
/// channel index spoken AFTER the preposition is impossible since the target is
/// the value itself. With no preposition, the first numeric token is taken (a
/// bare "gain -6" form).
fn extract_db(lower: &str) -> Option<f64> {
    // Normalize a spoken sign word into a leading '-' so "minus 18" parses, and
    // drop the dB suffix words so they don't fuse onto the number.
    let normalized = lower
        .replace("minus ", "-")
        .replace("negative ", "-")
        .replace("db", " ")
        .replace("decibels", " ")
        .replace("decibel", " ");
    let toks: Vec<&str> = normalized.split(|c: char| c.is_whitespace()).collect();
    // The window to search: everything after the LAST "to"/"at" target word when
    // present, so a channel number before it ("input 1 to -12") is excluded.
    let start = toks
        .iter()
        .rposition(|w| {
            let t = w.trim_matches(|c: char| !c.is_alphanumeric());
            t == "to" || t == "at"
        })
        .map(|i| i + 1)
        .unwrap_or(0);
    for tok in &toks[start..] {
        let t = tok.trim_matches(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.' || c == '+'));
        if t.is_empty() || t == "-" || t == "+" || t == "." {
            continue;
        }
        if let Ok(n) = t.parse::<f64>() {
            return Some(n);
        }
    }
    None
}

/// Extract the preset name from a "load the <name> preset" / "load preset
/// <name>" phrase. Returns the content word adjacent to "preset" (the token
/// before it, or after it when "preset" leads), stripped of the article. The
/// name is forwarded verbatim in the op — Nexus resolves it against its
/// presets/ directory (and rejects an unknown one cleanly). None when no name
/// can be isolated.
fn extract_preset_name(lower: &str) -> Option<String> {
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|w| !w.is_empty())
        .collect();
    let pos = words.iter().position(|w| *w == "preset")?;
    // Command/filler words that are never a preset name.
    let is_name = |w: &str| {
        !matches!(
            w,
            "the" | "a" | "an" | "my" | "load" | "recall" | "apply" | "preset"
                | "presets" | "please" | "to" | "for" | "me" | "up"
        )
    };
    // WHAT WENT WRONG: this preferred the token AFTER "preset" UNCONDITIONALLY,
    // and `is_name` only rejects a short command/politeness list — so any trailing
    // word became the preset name. "load the vocal preset now" loaded a preset
    // called "now", "…preset again" -> "again", "…preset thanks" -> "thanks",
    // "load the podcast preset darwin" -> "darwin". Nexus then rejects the unknown
    // name, but `handle_nexus` has already said "Forwarded that to the mixer", so
    // the user believes the vocal preset is live while the routing/gain is
    // unchanged — and if a preset ever WERE named after a filler word, the wrong
    // one would load and rewrite the whole matrix.
    //
    // The doc above always described the right rule: take the following token
    // only when "preset" LEADS ("load preset vocal"). Otherwise the name is the
    // token BEFORE it, which is what "load the <name> preset" means. `pos == 0`
    // is not enough on its own — "load preset vocal" has "load" in front — so
    // "leads" means: nothing but command/filler words precede it.
    let preset_leads = words[..pos].iter().all(|w| !is_name(w));
    if preset_leads {
        if let Some(after) = words.get(pos + 1) {
            if is_name(after) {
                return Some((*after).to_string());
            }
        }
    }
    if pos > 0 {
        // Walk back over articles to the name token ("load the vocal preset").
        let mut idx = pos - 1;
        loop {
            let w = words[idx];
            if is_name(w) {
                return Some(w.to_string());
            }
            if idx == 0 {
                break;
            }
            idx -= 1;
        }
    }
    None
}

// The op-string builders — EXACT Nexus OpDispatcher wire form (bare `{"op":...}`,
// NOT the Vision `{"type":"op"}` envelope). serde_json builds each so a preset
// name with a quote can never break the JSON framing.

fn op_gain_mute(channel: u32, mute: bool, stage: &str) -> String {
    json!({"op": "gain.set", "channel": channel, "mute": mute, "stage": stage}).to_string()
}
fn op_gain_set(channel: u32, gain_db: f64, stage: &str) -> String {
    json!({"op": "gain.set", "channel": channel, "gain_db": gain_db, "stage": stage}).to_string()
}
fn op_route_set(input: u32, output: u32, gain_db: f64) -> String {
    // -inf clears the crosspoint (SPEC §5); JSON has no infinity literal, so the
    // string sentinel "-inf" is forwarded — the Python _route_set maps it back to
    // float("-inf") verbatim (it special-cases the "-inf" string).
    let gain: serde_json::Value = if gain_db.is_infinite() && gain_db.is_sign_negative() {
        serde_json::Value::String("-inf".to_string())
    } else {
        json!(gain_db)
    };
    json!({"op": "route.set", "in": input, "out": output, "gain_db": gain}).to_string()
}
fn op_monitor_set(input: u32, output: u32, on: bool) -> String {
    json!({"op": "monitor.set", "in": input, "out": output, "on": on}).to_string()
}
fn op_preset_load(name: &str) -> String {
    json!({"op": "preset.load", "name": name}).to_string()
}
fn op_state_get() -> String {
    json!({"op": "state.get"}).to_string()
}

// ===========================================================================
// Mark-Forge voice control (SPEC §7 — the daemon forwards STRUCTURED ops ONLY;
// the Mark-Forge engine never parses natural language).
//
// Mark-Forge (apps/mark-forge) is a BINARY micro-app: a deterministic CPU/f64
// rigid-body physics engine. Its HOST -> APP op wire form is the BARE
// `{"op":"<name>", ...}` object (NOT the Vision `{"type":"op",...}` envelope) —
// its `parse_command` (apps/mark-forge/src/ipc.rs) reads `obj["op"]` and the
// `#[serde(tag = "op")]` Op enum dispatches on the dotted name. The op-string
// builders below produce that EXACT wire shape, matching the SPEC §7 op table
// and the app's own `op_deserializes_with_dotted_names` /
// `body_spawn_deserializes_with_optional_fields` round-trip tests verbatim:
//   world.reset {"op":"world.reset"}
//   body.spawn  {"op":"body.spawn","shape":{"kind":"cuboid","half_extents":[..]},"pos":[x,y,z]}
//   world.step  {"op":"world.step","n":N}
//   set.gravity {"op":"set.gravity","x":F,"y":F,"z":F}
//   state.get   {"op":"state.get"}
// serde_json builds each line so no field can break the JSON framing. The
// classifier is checked alongside the Silicon Canvas / Vision / Nexus seams,
// before the generic local handlers, so a precise physics-control phrase is
// handled deterministically and never lands on the cloud/LLM.
//
// The R3F render is DEVICE-GATED and is NEVER touched here: these ops are
// control-plane messages to the headless engine; whether the HUD is rendering is
// the HUD's concern. The daemon only classifies the utterance and forwards the
// structured op — it opens no GPU device and renders nothing.
// ===========================================================================

/// The Mark-Forge micro-app's registered name (its manifest `[app].name` and the
/// key into the app registry / its socket).
pub const MARK_FORGE_APP: &str = "mark-forge";

/// Where a freshly-dropped body appears: a few metres above the origin so it
/// falls onto the ground plane under gravity (the canonical "drop" gesture). The
/// engine resolves the rest via its integrator; the daemon only seeds the spawn.
const MARK_FORGE_DROP_HEIGHT: f64 = 5.0;
/// Half-extent of a dropped cuboid (a 1m unit cube) and radius of a dropped
/// sphere — sane defaults the user never has to speak. Forwarded verbatim in the
/// spawn op; the engine derives mass/inertia from the shape.
const MARK_FORGE_BOX_HALF_EXTENT: f64 = 0.5;
const MARK_FORGE_SPHERE_RADIUS: f64 = 0.5;
/// Default dynamic-body mass for a dropped shape. `Some(mass)` (> 0) makes it
/// dynamic; the engine would treat `None`/`<= 0` as a STATIC body (SpawnSpec),
/// which would never fall — so a "drop" must carry a positive mass.
const MARK_FORGE_DROP_MASS: f64 = 1.0;
/// Lunar surface gravity (m/s², downward) for "set gravity to the moon". A fixed
/// physical constant — never an RNG/wall-clock read — so the op stays
/// deterministic.
const MARK_FORGE_MOON_GRAVITY: f64 = -1.62;
/// Earth surface gravity (m/s², downward) for "set gravity to earth" / "normal
/// gravity". Matches the engine's own default so "reset gravity" restores it.
const MARK_FORGE_EARTH_GRAVITY: f64 = -9.81;
/// Mars surface gravity (m/s², downward) for "set gravity to mars".
const MARK_FORGE_MARS_GRAVITY: f64 = -3.72;
/// Zero gravity for "turn off gravity" / "set gravity to zero" / "space".
const MARK_FORGE_ZERO_GRAVITY: f64 = 0.0;

/// What a Mark-Forge voice command resolves to: LAUNCH the app, or forward a
/// STRUCTURED op line to the already-running engine. The op body is opaque to the
/// daemon (built to match apps/mark-forge/src/ipc.rs's `Op` wire form).
#[derive(Debug, Clone, PartialEq)]
pub enum MarkForgeCommand {
    /// "open the physics sandbox" — start the micro-app.
    Launch,
    /// A complete JSON op line (one line) to forward verbatim, e.g.
    /// `{"op":"world.reset"}`.
    Op(String),
}

/// Whether the utterance names the Mark-Forge app / capability itself ("mark
/// forge", "the physics sandbox", "the simulation", "the sandbox"). Used to gate
/// the bare launch verb so an unrelated "open safari" is never captured, and to
/// disambiguate "reset"/"pause" so they only fire in a physics context.
/// The BARE spawn idiom's closed vocabulary — "drop a box", "throw a ball",
/// "spawn a marble", "add a crate". The whole utterance must be drawn from this
/// list (numbers allowed, for "drop two boxes"). See the spawn branch in
/// [`mark_forge_command`] for why a co-occurring verb + shape noun was not
/// enough. Deliberately holds no verb/noun that is not part of the idiom itself:
/// the point is that it CANNOT be satisfied by adding words.
const MARK_FORGE_BARE_SPAWN_VOCAB: &[&str] = &[
    "drop", "drops", "spawn", "spawns", "add", "adds", "throw", "throws", "toss", "tosses",
    "a", "an", "the", "another", "one", "two", "three", "four", "five", "more", "some", "couple",
    "of", "in", "into", "onto", "on", "here", "there",
    "ball", "balls", "sphere", "spheres", "marble", "marbles",
    "box", "boxes", "cube", "cubes", "crate", "crates", "block", "blocks",
    "please", "darwin", "hey", "ok", "okay", "now", "just", "can", "could", "would", "you",
    "and", "for", "me", "us",
];

fn mentions_mark_forge(lower: &str) -> bool {
    names_mark_forge(lower) || lower.contains("the simulation") || lower.contains("the sandbox")
}

/// The half of [`mentions_mark_forge`] that actually NAMES this engine.
///
/// `mentions_mark_forge` also admits the bare nouns "the simulation" / "the
/// sandbox", which are ordinary English about anything anyone models or any
/// walled-off environment ("reset the sandbox account", "the simulation of my
/// expectations"). That is fine for a branch that has ANOTHER anchor, and not
/// fine for one where the co-word IS the anchor — see the world-reset branch.
fn names_mark_forge(lower: &str) -> bool {
    lower.contains("mark forge")
        || lower.contains("mark-forge")
        || lower.contains("markforge")
        || lower.contains("physics sandbox")
        || lower.contains("physics sim")
        || lower.contains("physics engine")
        || lower.contains("rigid body")
        || lower.contains("rigid-body")
}

/// The bare world-reset idiom — the WHOLE utterance is nothing but "reset the
/// world" / "clear the simulation" / "wipe the scene". Same closed-vocabulary
/// shape the spawn branch and the Nexus bare idioms use, and chosen for the same
/// reason: it cannot be satisfied by ADDING words, so one content word from
/// outside this list ("pandemic", "breakup", "police", "account", "expectations")
/// and it is somebody talking about their life, not wiping a physics scene.
const MARK_FORGE_BARE_RESET_VOCAB: &[&str] = &[
    "reset", "resets", "clear", "clears", "wipe", "wipes",
    "the", "a", "an", "this", "my", "all", "everything", "in", "out",
    "world", "scene", "bodies", "body", "simulation", "sandbox", "physics", "sim",
    "please", "darwin", "hey", "ok", "okay", "now", "just", "can", "could", "would", "you",
    "and", "for", "me", "us",
];

/// Whether the utterance carries an open-class launch verb.
///
/// WHOLE WORDS. Under `contains`, "start" matched "started" and "show" matched
/// "showed"/"showing", so a past-tense sentence that merely mentioned "the
/// simulation" was a LAUNCH: "they cleared the simulation results and started
/// over" opened Mark-Forge. A narration is not an instruction.
fn mentions_mark_forge_launch_verb(lower: &str) -> bool {
    crate::utterance::mentions_any_word(lower, &["open", "launch", "start", "show"])
        || lower.contains("bring up")
        || lower.contains("fire up")
}

/// Map a spoken utterance to a Mark-Forge command, or None when it is not a
/// physics-control phrase (the turn then falls through to normal routing).
/// Deterministic and pure so the mapping is unit-tested without a socket, a
/// running engine, or the classifier. Order matters: the specific ops (spawn,
/// reset, gravity, step/pause) are matched before the broad "open the physics
/// sandbox" launch so a control phrase that also says "open" is never mistaken
/// for a launch.
///
/// Recognized phrases (all case-insensitive, whole lowercased utterance):
///   - "drop/spawn/add a box|cube"                  -> body.spawn {cuboid}
///   - "drop/spawn/add a ball|sphere"               -> body.spawn {sphere}
///   - "reset/clear the simulation|sandbox|world"   -> world.reset
///   - "set gravity to the moon|mars|earth|zero" /
///     "turn off gravity"                           -> set.gravity {x,y,z}
///   - "step" / "step <N> frames" / "advance"       -> world.step {n>=1}
///   - "pause" / "hold" / "freeze"                  -> world.step {n:0}
///   - "open/launch/start the physics sandbox"      -> Launch
pub fn mark_forge_command(text: &str) -> Option<MarkForgeCommand> {
    let lower = text.to_lowercase();

    // --- spawn (drop/add a box or ball) ------------------------------------
    // The spawn verb plus a shape noun. Checked first so "drop a box" is never
    // read as anything else. A ball/sphere noun -> sphere; otherwise a box/cube
    // noun -> cuboid. The verb alone with no shape noun is NOT a spawn (it falls
    // through), so "drop it" / "drop everything" never spawns a phantom body.
    let spawn_verb = lower.contains("drop")
        || lower.contains("spawn")
        || lower.contains("add a ")
        || lower.contains("add an ")
        || lower.contains("throw");
    // WHAT WENT WRONG: this branch — checked FIRST, and terminal, because route()
    // dispatches through an else-if chain — was the ONLY Mark-Forge branch with no
    // physics-context gate. Reset needs a world/scene/bodies co-word, gravity needs
    // `gravity_commanded`, step/pause need `physics_ctx`; spawn needed nothing but
    // a verb substring and a shape noun ANYWHERE in the sentence. So:
    //   "can you add a block to my calendar at 3"        -> body.spawn{cuboid}
    //   "did you drop the boxes off at the post office"  -> body.spawn{cuboid}
    //   "throw the ball for the dog"                     -> body.spawn{sphere}
    //   "what time does the ball drop on new year's eve" -> body.spawn{sphere}
    // Each of those never reaches its real handler; with the sandbox closed the
    // user gets "I couldn't reach the physics sandbox: … Open it first, sir", and
    // with it open a 1 kg body is silently dropped into their scene. The existing
    // negative tests only covered spawn verbs with NO shape noun ("drop me an
    // email"), so the co-occurrence hole was untested.
    //
    // Two ways in now, both of which the shipped phrasings satisfy: the utterance
    // NAMES a physics context ("drop a box in the sandbox"), or the WHOLE
    // utterance is nothing but the bare spawn idiom ("drop a box", "throw a
    // ball") — the same closed-vocabulary shape the Nexus bare idioms use, chosen
    // because it cannot be satisfied by adding words. One content word from
    // outside the list ("calendar", "post", "office", "dog", "eve") and it is
    // somebody talking about their life, not driving a physics engine.
    let spawn_context =
        mentions_mark_forge(&lower) || nexus_closed_vocabulary(&lower, MARK_FORGE_BARE_SPAWN_VOCAB, true);
    if spawn_verb && spawn_context {
        if mentions_word(&lower, "ball")
            || mentions_word(&lower, "balls")
            || mentions_word(&lower, "sphere")
            || mentions_word(&lower, "spheres")
            || mentions_word(&lower, "marble")
        {
            return Some(MarkForgeCommand::Op(op_spawn_sphere()));
        }
        if mentions_word(&lower, "box")
            || mentions_word(&lower, "boxes")
            || mentions_word(&lower, "cube")
            || mentions_word(&lower, "cubes")
            || mentions_word(&lower, "crate")
            || mentions_word(&lower, "block")
        {
            return Some(MarkForgeCommand::Op(op_spawn_box()));
        }
    }

    // --- world reset -------------------------------------------------------
    // "reset/clear the simulation|world|sandbox|scene|bodies". Gated on a
    // physics co-word (or a Mark-Forge mention) so a bare "reset" in another
    // context never wipes the world. "reset gravity" is NOT a reset (it is a
    // gravity op) — handled by requiring a world/scene noun and excluding the
    // gravity case, which the gravity branch below also catches first if it has
    // a target.
    //
    // WHAT WENT WRONG: the co-word WAS the whole gate, and every co-word on that
    // list is ordinary English. Any sentence carrying one of them plus the word
    // reset/clear/wipe ANYWHERE wiped the physics world:
    //   "the world reset itself after the pandemic in a lot of ways"
    //   "reset the simulation of my expectations for this quarter"
    //   "clear the scene before the police get here"
    //   "I need to reset my whole world after that breakup"
    //   "the bodies of water in this county are all clear now"
    //   "wipe the scene from your memory it was embarrassing"
    //   "clear the world of that idea please"  /  "reset the sandbox account"
    //   "my world reset when the baby arrived"
    // The SIBLING spawn branch above already carries the answer and this branch
    // never got it: the utterance must NAME the engine, or be nothing BUT the
    // bare reset idiom. `mentions_mark_forge`'s loose halves ("the simulation",
    // "the sandbox") do NOT count as naming it here — they are exactly what
    // "reset the simulation of my expectations" walked in through, so the
    // context test uses `names_mark_forge`.
    let reset_context = names_mark_forge(&lower)
        || nexus_closed_vocabulary(&lower, MARK_FORGE_BARE_RESET_VOCAB, false);
    if crate::utterance::mentions_any_word(&lower, &["reset", "clear", "wipe"])
        && !lower.contains("gravity")
        && (mentions_mark_forge(&lower)
            || mentions_word(&lower, "world")
            || mentions_word(&lower, "scene")
            || mentions_word(&lower, "bodies"))
        && reset_context
    {
        return Some(MarkForgeCommand::Op(op_world_reset()));
    }

    // --- gravity -----------------------------------------------------------
    // "set gravity to the moon|mars|earth|zero", "turn off gravity", "moon
    // gravity". Requires the word "gravity" so it never fires on an unrelated
    // "moon"/"mars". The target body picks the constant; an unrecognized target
    // with a bare "set gravity" falls through (the daemon won't guess a vector).
    // GRAVITY MUST BE COMMANDED, NOT MENTIONED. `contains("gravity")` plus a
    // target word was enough, so "gravity is what, mass warping space" — a
    // sentence ABOUT gravity, in which "space" is the target — set the world's
    // gravity to zero. A statement is not an instruction.
    //
    // Accepts the documented forms: a setting verb ("set gravity to the moon",
    // "turn off gravity"), a named sandbox, or the bare adjacency "moon gravity"
    // / "zero gravity" where the target sits immediately before the noun.
    let gravity_commanded = crate::utterance::mentions_any_word(
        &lower,
        &["set", "turn", "make", "change", "switch", "put", "use", "give", "raise", "lower"],
    ) || mentions_mark_forge(&lower)
        || lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| {
                w[1] == "gravity"
                    && matches!(
                        w[0],
                        "moon" | "lunar" | "mars" | "martian" | "earth" | "zero" | "no" | "normal"
                    )
            });
    if lower.contains("gravity") && gravity_commanded {
        if let Some(y) = gravity_target(&lower) {
            return Some(MarkForgeCommand::Op(op_set_gravity(y)));
        }
    }

    // --- step / pause ------------------------------------------------------
    // The engine has no free-running loop: it advances ONLY on world.step{n}.
    // "step"/"advance" -> step exactly N frames (N from the utterance, default
    // 1); "pause"/"freeze"/"hold" -> world.step{n:0}, a deterministic zero-frame
    // step that advances no simulated time (an honest "pause" for an engine that
    // is already paused between steps). Both are gated on a physics context so a
    // bare "pause" elsewhere is untouched.
    let physics_ctx = mentions_mark_forge(&lower)
        || mentions_word(&lower, "simulation")
        || mentions_word(&lower, "sim")
        || mentions_word(&lower, "world")
        || mentions_word(&lower, "physics")
        // PLURAL ONLY. "advance 5 frames" is a step count; "the picture FRAME my
        // aunt got won't hold a charge" is a photograph, and the singular used to
        // make it a physics context — which then let "hold" pause the world.
        || mentions_word(&lower, "frames");
    // WHAT WENT WRONG: this had two substring escape hatches beside the real
    // gate, and both fired on ordinary speech.
    //
    //   contains("step the")  matches "step THERE" — "probe the snow with a pole
    //                         before you step there" advanced the simulation.
    //   contains("frame")     matches "picture FRAME" — "the picture frame my
    //                         aunt got won't hold a charge" paused the world.
    //
    // Neither bought anything: "step the simulation" and "advance 5 frames" are
    // already admitted by `physics_ctx`, which matches the same words WHOLE.
    if crate::utterance::mentions_any_word(&lower, &["step", "steps", "advance"]) && physics_ctx {
        let n = extract_step_count(&lower).unwrap_or(1);
        return Some(MarkForgeCommand::Op(op_world_step(n)));
    }
    // "hold" was here and is ordinary English — "won't hold a charge", "hold on
    // a second". `physics_ctx` alone did not save it, because "frame" used to be
    // a physics word.
    if (mentions_word(&lower, "pause")
        || mentions_word(&lower, "freeze")
        || mentions_word(&lower, "hold")
        || mentions_word(&lower, "halt"))
        && physics_ctx
    {
        return Some(MarkForgeCommand::Op(op_world_step(0)));
    }

    // --- state read (READ-ONLY snapshot) -----------------------------------
    // "what's the physics state" / "what's the state of the sandbox".
    //
    // MEASURED RECALL MISS: the Mark-Forge app has implemented `state.get`
    // (apps/mark-forge/src/ipc.rs `Op::StateGet`) since it shipped, and NO router
    // branch ever emitted it — the op was unreachable from voice entirely, which
    // is the strongest form of a recall gap. Read-only: it returns a snapshot and
    // mutates nothing.
    //
    // Each phrase BINDS the state noun to the physics sandbox, so a bare "what's
    // the state" (of anything at all) is not taken.
    // MEASURED HIJACK (adversary pass): "state of the simulation" / "simulation
    // state" are NOT bound to this sandbox — a simulation is anything anyone
    // models. They captured "the state of the simulation in that paper was
    // unclear" and "what's the state of the simulation they ran", both CLEAN at
    // HEAD, and the shipped probe ("what's the physics state") never needed
    // them. Only the phrases that name THIS app or its physics remain.
    if lower.contains("physics state")
        || lower.contains("state of the physics")
        || lower.contains("sandbox state")
        || lower.contains("state of the sandbox")
        || lower.contains("mark forge state")
        || lower.contains("mark-forge state")
    {
        return Some(MarkForgeCommand::Op(op_mark_forge_state_get()));
    }

    // --- launch ------------------------------------------------------------
    // Only when the utterance actually names Mark-Forge / the sandbox AND carries
    // an open-class verb. Last so a control phrase that also says "open" was
    // already handled above.
    if mentions_mark_forge(&lower) && mentions_mark_forge_launch_verb(&lower) {
        return Some(MarkForgeCommand::Launch);
    }

    None
}

/// Whole-word token check for Mark-Forge phrases (reuses the same boundary rule
/// as the other seams' `contains_word`): `word` matches only as a standalone
/// alnum token, so "box" never fires inside "boxer" and "sim" never inside
/// "simple".
use crate::utterance::mentions_word;

/// The downward gravity magnitude a "set gravity to <target>" phrase selects, or
/// None when no recognized target is named (a bare "set gravity" with no body
/// then falls through rather than the daemon guessing a vector). The targets are
/// fixed physical constants; "off"/"zero"/"space" -> 0, "moon"/"mars"/"earth" ->
/// their surface gravity.
fn gravity_target(lower: &str) -> Option<f64> {
    if mentions_word(lower, "off")
        || mentions_word(lower, "zero")
        || mentions_word(lower, "none")
        || mentions_word(lower, "space")
        || lower.contains("turn off")
        || lower.contains("no gravity")
        || lower.contains("zero g")
        || lower.contains("weightless")
    {
        return Some(MARK_FORGE_ZERO_GRAVITY);
    }
    if mentions_word(lower, "moon") || mentions_word(lower, "lunar") {
        return Some(MARK_FORGE_MOON_GRAVITY);
    }
    if mentions_word(lower, "mars") || mentions_word(lower, "martian") {
        return Some(MARK_FORGE_MARS_GRAVITY);
    }
    if mentions_word(lower, "earth")
        || mentions_word(lower, "normal")
        || mentions_word(lower, "default")
    {
        return Some(MARK_FORGE_EARTH_GRAVITY);
    }
    // An explicit NUMERIC target: "set gravity to -9.8". MEASURED RECALL MISS —
    // every named body was here and the one form a physics user actually speaks,
    // the number itself, was not.
    //
    // Read ONLY from after an explicit "to"/"at", so a number elsewhere in the
    // sentence ("the 3 boxes fell") can never become a gravity vector, and
    // clamped to a sane magnitude so a misheard figure cannot launch the world.
    numeric_gravity(lower)
}

/// Parse an explicit numeric gravity from "gravity to|at <number>". Pure. `None`
/// unless the target word sits IMMEDIATELY AFTER the noun "gravity". Clamped to
/// +-100 m/s^2.
///
/// The anchor is "gravity to"/"gravity at" and NOT a free-floating "to"/"at",
/// which was the first cut of this function and was measurably wrong: "the
/// gravity of the situation set in at about 3" has the word gravity, a setting
/// verb, and a number after "at" — and would have written a 3.0 gravity vector
/// into the world. Binding the preposition to the noun is what refuses it.
fn numeric_gravity(lower: &str) -> Option<f64> {
    const MAX_ABS_GRAVITY: f64 = 100.0;
    let normalized = lower.replace("minus ", "-").replace("negative ", "-");
    // "gravity is <n>" is deliberately absent: it is a DECLARATIVE, not an order.
    let after = ["gravity to ", "gravity at "]
        .iter()
        .find_map(|a| normalized.split_once(a).map(|(_, r)| r))?;
    let toks: Vec<&str> = after.split_whitespace().collect();
    for tok in &toks {
        let t = tok.trim_matches(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.' || c == '+'));
        if t.is_empty() || t == "-" || t == "+" || t == "." {
            continue;
        }
        if let Ok(n) = t.parse::<f64>() {
            if n.is_finite() {
                return Some(n.clamp(-MAX_ABS_GRAVITY, MAX_ABS_GRAVITY));
            }
        }
    }
    None
}

/// Extract the frame count from "step <N> frames" / "advance <N>" / "step N".
/// Returns the first standalone integer token, or None (the caller then defaults
/// to a single frame). Caps at a sane bound so a misheard huge number cannot ask
/// the engine to advance millions of frames in one synchronous call.
fn extract_step_count(lower: &str) -> Option<u32> {
    const MAX_STEP_FRAMES: u32 = 10_000;
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .find_map(|w| w.parse::<u32>().ok())
        .map(|n| n.clamp(1, MAX_STEP_FRAMES))
}

// The op-string builders — EXACT Mark-Forge `Op` wire form (bare `{"op":...}`,
// the `#[serde(tag = "op")]` dotted names). serde_json builds each so no field
// can break the JSON framing. The shape sub-object is tagged on `kind`
// (snake_case) and `pos`/`half_extents` serialize as `[x,y,z]` arrays — exactly
// what apps/mark-forge/src/ipc.rs deserializes (verified by its own round-trip
// tests).

fn op_world_reset() -> String {
    json!({"op": "world.reset"}).to_string()
}
/// The EXACT wire form apps/mark-forge/src/ipc.rs deserializes as `Op::StateGet`
/// (`#[serde(rename = "state.get")]`, no fields).
fn op_mark_forge_state_get() -> String {
    json!({"op": "state.get"}).to_string()
}
fn op_world_step(n: u32) -> String {
    json!({"op": "world.step", "n": n}).to_string()
}
fn op_set_gravity(y: f64) -> String {
    json!({"op": "set.gravity", "x": 0.0, "y": y, "z": 0.0}).to_string()
}
fn op_spawn_box() -> String {
    json!({
        "op": "body.spawn",
        "shape": {
            "kind": "cuboid",
            "half_extents": [
                MARK_FORGE_BOX_HALF_EXTENT,
                MARK_FORGE_BOX_HALF_EXTENT,
                MARK_FORGE_BOX_HALF_EXTENT
            ]
        },
        "pos": [0.0, MARK_FORGE_DROP_HEIGHT, 0.0],
        "mass": MARK_FORGE_DROP_MASS
    })
    .to_string()
}
fn op_spawn_sphere() -> String {
    json!({
        "op": "body.spawn",
        "shape": {"kind": "sphere", "radius": MARK_FORGE_SPHERE_RADIUS},
        "pos": [0.0, MARK_FORGE_DROP_HEIGHT, 0.0],
        "mass": MARK_FORGE_DROP_MASS
    })
    .to_string()
}

#[cfg(test)]
mod tests {

    /// SOMEBODY ELSE'S DISPLAY IS NOT THE OWNER'S SCREEN — asserted on BOTH gates
    /// that reach the capture, because they overlap and closing one alone only
    /// moves the hijack. MEASURED: with only Lumen narrowed, "read the display on
    /// the thermostat" and "read the screen on the treadmill" still captured the
    /// screen through Vision, so the owner would have seen no change.
    ///
    /// A screen read surfaces on-screen passwords/messages (`is_screen_read` says
    /// so) and is read back ALOUD. Every sentence here is about an appliance.
    #[test]
    fn a_read_aimed_at_another_devices_display_never_captures_the_screen() {
        for u in [
            "what buttons are on the display of the microwave",
            "what controls are on the display of the oven",
            "read the display on the thermostat",
            "what buttons are on the dashboard display of the car",
            "what fields are on the display of the printer",
            "read the screen on the treadmill",
            "what buttons are on the screen of the coffee machine",
            "list the controls on the elevator display",
        ] {
            assert!(
                super::lumen_command(u).is_none(),
                "{u:?} reached a Lumen screen read — the owner asked about an appliance"
            );
            assert!(
                super::screen_read_op(&u.to_lowercase()).is_none(),
                "{u:?} reached a Vision screen read — the owner asked about an appliance"
            );
            assert!(
                !super::is_screen_read(u),
                "{u:?} is still classified as a screen read"
            );
        }
    }

    /// ...AND EVERY REAL SCREEN READ MUST SURVIVE ON BOTH GATES. The possessor
    /// allowlist has to cover this machine AND the surfaces that live on it, or
    /// the veto eats the capability it is protecting.
    #[test]
    fn the_other_device_veto_leaves_every_real_screen_read_working() {
        for u in [
            "what buttons are on this screen",
            "read the controls on this screen",
            "read my screen",
            "what's on my screen",
            "read me the screen",
            "read the screen",
            "what is on my screen",
            "read the buttons on the settings page",
            "read the controls on the login screen",
            "read the screen on my mac",
            // FOUND BY MEASUREMENT, NOT BY INSPECTION. Each of these reached a
            // Lumen read at 7731042 and reached NOTHING once the veto landed,
            // because the possessor allowlist omitted five of the eight nouns in
            // `lumen_mentions_control_noun` — the very list that makes an
            // utterance a Lumen read. The guard was refusing its own capability.
            "read the label on the button",
            "read the labels on the checkboxes",
            "read the labels on the icons",
            "read me the labels on the tabs",
            "read the options on the dropdown menu",
        ] {
            assert_eq!(
                super::lumen_command(u),
                Some(super::LumenCommand::Read),
                "{u:?} is a real screen read and stopped working"
            );
        }
    }

    /// RE-PROVING THE APPLIANCE SIDE AFTER THE ALLOWLIST WAS WIDENED. Adding the
    /// control nouns puts a word on the ALLOW side that an appliance sentence can
    /// legitimately carry ("read the labels on the BUTTONS of the microwave"), so
    /// the veto has to be re-proved rather than assumed still sound: widening one
    /// end of a guard makes a NEW guard.
    ///
    /// The veto survives because it fires when ANY possessor names something that
    /// is not one of this machine's surfaces — so a sentence may satisfy the
    /// allowlist at its first possessor and still be refused at its second, which
    /// is exactly the shape an appliance sentence takes. Every sentence here puts
    /// a NEWLY-restored word in the first possessor and the device in the second;
    /// without the second-possessor rule they would all capture the screen.
    ///
    /// AND EVERY ONE WAS MEASURED FIRING AT 7731042 — a negative probe chosen to
    /// pass is not a probe. The first draft of this test carried "what buttons are
    /// on the control panel of the washing machine", which reached NOTHING at HEAD
    /// (no read verb, no screen word), so asserting it was refused asserted
    /// nothing; it was replaced with "read the buttons on the control panel of the
    /// washing machine", which DID reach a Lumen read. The isolating mutation is
    /// recorded too: adding "washing" to the allowlist above fails THIS test while
    /// leaving the eight-appliance test green.
    #[test]
    fn a_restored_control_noun_does_not_carry_an_appliance_past_the_veto() {
        for u in [
            "read the labels on the buttons of the microwave",
            "read the buttons on the control panel of the washing machine",
            "read the icons on the display of the oven",
            "read the labels on the buttons of the thermostat",
            "read the label on the button of the microwave",
            "read the links on the page of the treadmill",
        ] {
            assert!(
                super::lumen_command(u).is_none(),
                "{u:?} reached a Lumen screen read — a restored control noun carried \
                 an appliance past the veto"
            );
            assert!(
                super::screen_read_op(&u.to_lowercase()).is_none(),
                "{u:?} reached a Vision screen read — a restored control noun carried \
                 an appliance past the veto"
            );
            assert!(
                !super::is_screen_read(u),
                "{u:?} is still classified as a screen read"
            );
        }
    }

    /// A PLAYLIST IS A CURATION REQUEST, AND THIS BRANCH EGRESSES. With
    /// `[voice].cloud_music` shipping true, reaching the composer POSTs the
    /// owner's sentence to ElevenLabs and spends generation credit — for a request
    /// DARWIN cannot satisfy at all (the only music capability is COMPOSE; "play
    /// some lo-fi music" is a recorded NO-GO). MEASURED at HEAD: six of ten
    /// sentences probed in this shape composed music.
    #[test]
    fn a_curation_request_never_reaches_the_cloud_composer() {
        for u in [
            "generate a playlist of ambient music for the drive",
            "make a playlist of lo-fi music for studying",
            "produce a playlist of background music for the party",
            "write a playlist of instrumental music for dinner",
            "generate a list of ambient music artists",
            "make me a mixtape of ambient music",
            "compose a playlist for the drive",
            "queue up some background music",
        ] {
            assert!(
                super::classify_music_intent(u).is_none(),
                "{u:?} was sent to the cloud composer — the owner asked for a set of \
                 existing tracks, which is a recorded NO-GO"
            );
        }
        // ...and composition itself is untouched.
        for u in [
            "make me a song about the rain",
            "generate some background music",
            "compose something calm for me",
            "make a jingle for the podcast",
            "produce some instrumental music",
        ] {
            assert!(super::classify_music_intent(u).is_some(), "{u:?} stopped composing");
        }
    }

    /// A SPREADSHEET CELL IS NOT A REFERENCE DESIGNATOR. The bare-designator
    /// idiom's doc calls it "CLOSED-VOCABULARY"; the shape test was "1-3 letters
    /// then 1-4 digits", which is also every spreadsheet cell and every chess
    /// square. MEASURED at HEAD: eight of the nine cell/square utterances probed
    /// reached `select.component`.
    #[test]
    fn a_spreadsheet_cell_is_not_a_bare_reference_designator() {
        for u in [
            "select a1", "highlight b2", "highlight a5", "select e4", "highlight g7",
            "select h8", "highlight aa1", "select b12",
        ] {
            assert!(
                super::silicon_canvas_command(u).is_none(),
                "{u:?} was forwarded to Silicon Canvas as a component selection"
            );
        }
        // The refdes idiom itself is untouched — these are the shipped probes, and
        // dropping single-letter prefixes wholesale would have taken all of them.
        for (u, want) in [
            ("select r14", "R14"),
            ("highlight u3", "U3"),
            ("isolate c5", "C5"),
            ("probe q2", "Q2"),
        ] {
            match super::silicon_canvas_command(u) {
                Some(super::SiliconCanvasCommand::Op(line)) => {
                    let v: serde_json::Value = serde_json::from_str(&line).expect("wire json");
                    assert_eq!(v["op"], "select.component", "{u:?}");
                    assert_eq!(v["name"], want, "{u:?}");
                }
                other => panic!("{u:?} stopped selecting: {other:?}"),
            }
        }
    }

    /// REGRESSION (router-recall miss list): eight app-command phrasings reached
    /// NOTHING. Each assertion below names the utterance the printed miss list
    /// carried; each negative is a sentence the widening could otherwise have
    /// swallowed. Grouped because they all live in router.rs's classifiers.
    #[test]
    fn recall_miss_app_phrasings_reach_their_op_and_ordinary_speech_does_not() {
        // MUSIC — "generate some background music": OBJECTS held every word for a
        // PIECE of music and not "music" itself. The bare word is still refused;
        // a MEMO request is vetoed outright (that is how notes used to be lost).
        assert!(super::classify_music_intent("generate some background music").is_some());
        assert!(super::classify_music_intent("produce some instrumental music").is_some());
        for u in [
            "make a note about the background music in that film",
            "write down some music recommendations for the drive",
            "what's the background music in this show",
            "play some jazz",
        ] {
            assert!(super::classify_music_intent(u).is_none(), "not a composition: {u:?}");
        }

        // DESCRIBE — "tell me what you see on my screen" went to the OCR path, and
        // "look at <file>.png and tell me what it shows" reached nothing at all.
        assert!(matches!(
            super::describe_command("tell me what you see on my screen"),
            Some(super::DescribeRequest::Screen { .. })
        ));
        assert!(matches!(
            super::describe_command("look at ~/Desktop/diagram.png and tell me what it shows"),
            Some(super::DescribeRequest::Image { .. })
        ));
        // The bare "look at <file>" form too — without this assertion the "look
        // at" cue is unproven (the sentence above also carries "tell me what it
        // shows", so it passes on that cue alone; a mutation that deleted "look
        // at" SURVIVED until this line existed).
        assert!(matches!(
            super::describe_command("look at ~/Desktop/diagram.png"),
            Some(super::DescribeRequest::Image { .. })
        ));
        // Without an image FILE, "look at" is not a describe request.
        assert!(super::describe_command("look at the sunset over the bay").is_none());

        // LUMEN — "what buttons are on this screen": the control-kind question.
        // The SCREEN is required, so a sewing question is not a control read.
        assert!(matches!(
            super::lumen_command("what buttons are on this screen"),
            Some(super::LumenCommand::Read)
        ));
        for u in ["what buttons should i sew on this coat", "what fields did they plant this year"] {
            assert!(super::lumen_command(u).is_none(), "{u:?}");
        }

        // SILICON CANVAS — "select r14": a bare reference designator, with no
        // "component" noun anywhere.
        // Asserted on the PARSED wire fields, not on a string the same builder
        // produced — a byte comparison against `op_select_component` would only
        // prove the builder equals itself.
        match super::silicon_canvas_command("select r14") {
            Some(super::SiliconCanvasCommand::Op(line)) => {
                let v: serde_json::Value = serde_json::from_str(&line).expect("wire json");
                assert_eq!(v["op"], "select.component");
                assert_eq!(v["name"], "R14");
            }
            other => panic!("expected a select.component op, got {other:?}"),
        }
        for u in ["select all the rows in that spreadsheet", "highlight the important parts for me"] {
            assert!(super::silicon_canvas_command(u).is_none(), "{u:?}");
        }

        // VISION — "turn the camera off" (stop, never start) and "what is the
        // vision app doing" (status).
        assert_eq!(
            super::vision_command("turn the camera off"),
            Some(super::VisionCommand::Op(super::op_watch_stop()))
        );
        assert_eq!(
            super::vision_command("what is the vision app doing"),
            Some(super::VisionCommand::Op(super::op_status()))
        );
        // The OFF path must never become an ON path, and a past-tense report is
        // not an order.
        assert!(!super::camera_off_command("turn on the camera"));
        for u in [
            "i turned the camera off after the show",
            "the camera off the coast picked up the storm",
        ] {
            assert!(!super::camera_off_command(u), "{u:?}");
        }
        assert!(super::vision_command("my vision has been blurry since the surgery").is_none());

        // NEXUS — "set channel 1 to -6 db" (gain on a bare numbered channel) and
        // "what's the mixer state" (the read-only snapshot).
        assert_eq!(
            super::nexus_command("set channel 1 to -6 db"),
            Some(super::NexusCommand::Op(super::op_gain_set(1, -6.0, "input")))
        );
        assert_eq!(
            super::nexus_command("what's the mixer state"),
            Some(super::NexusCommand::Op(super::op_state_get()))
        );
        // The DECIBEL UNIT is the gate: without it a channel number is a TV channel.
        for u in ["set channel 1 to the news at six", "we set channel 3 to record the game"] {
            assert!(super::nexus_command(u).is_none(), "{u:?}");
        }

        // MARK-FORGE — "set gravity to -9.8" (a numeric target) and "what's the
        // physics state" (state.get, which NO router branch ever emitted).
        assert_eq!(
            super::mark_forge_command("set gravity to -9.8"),
            Some(super::MarkForgeCommand::Op(super::op_set_gravity(-9.8)))
        );
        assert_eq!(
            super::mark_forge_command("what's the physics state"),
            Some(super::MarkForgeCommand::Op(super::op_mark_forge_state_get()))
        );
        // The preposition must bind to the NOUN: a sentence about the gravity of a
        // situation, with a number after "at", must not write a gravity vector.
        for u in ["the gravity of the situation set in at about 3", "set the alarm to 7 in the morning"] {
            assert!(super::mark_forge_command(u).is_none(), "{u:?}");
        }
    }
    use super::{
        ambient_monitor_should_start, arg_str, classify_app_request, clear_roll_call_interrupt,
        cloud_model, conversation_brain, describe_command, describe_confined_path, enforce_tool,
        local_model_for_turn, local_sub_for_turn,
        extract_app_name, extract_content_words, extract_image_path, extract_sensitivity,
        extract_web_query,
        extract_image_prompt, generate_image_command, handle_describe, handle_generate_image,
        vqa_question,
        handle_identify_sound, identify_sound_clip_or_request,
        interrupt_roll_call, is_describe_request, is_generate_image_request,
        is_identify_sound_request, is_screen_read,
        is_uncertain_fallback, lumen_command, mark_forge_command, nexus_command, op_classify_sound,
        recent_replies, ui_actuate_input,
        select_agent, silicon_canvas_command, sound_clip_path, suggests_web, utterance_wants_open,
        vision_command, wants_cloud, wants_quit, AppRequest, ConversationBrain, DescribeRequest,
        GenerateImageRequest, IdentifySoundRequest, LumenCommand, MarkForgeCommand, NexusCommand,
        SiliconCanvasCommand, VisionCommand,
        MARK_FORGE_APP, NEXUS_APP, ROLL_CALL_CANCEL, SILICON_CANVAS_APP, VISION_APP,
    };
    use crate::agents::AgentRegistry;
    use crate::config::Config;
    use crate::inference::{Classification, InferenceClient};
    use serde_json::json;
    use std::sync::atomic::Ordering;

    /// A classifier verdict for routing-decision tests: confident enough to
    /// stay local unless `complexity` forces cloud.
    fn classification(intent: &str, complexity: &str, confidence: f64) -> Classification {
        Classification {
            intent: intent.to_string(),
            complexity: complexity.to_string(),
            confidence,
            args: serde_json::Value::Null,
        }
    }

    /// CONTRACT B routing-decision table (the conversation-specific brain), now
    /// resolved through the model-tier layer. With NO override, the decision table
    /// is preserved at the config default for a HEAVY turn (the tier that keeps the
    /// configured default): cloud_heavy -> Opus, cloud_fast -> Haiku, no key/local/
    /// unknown -> local. Pure — no live cloud call, no inference client. (The auto
    /// step-down for a trivial turn and the override precedence are covered in
    /// model_tier.rs's own tests and the new conversation tests below.)
    #[test]
    fn conversation_brain_decision_table() {
        let _guard = crate::model_tier::OverrideGuard::force(None);
        let mut cfg = Config::default();
        // heavy_model/fast_model are the shipped contract ids.
        assert_eq!(cfg.cloud.heavy_model, "claude-opus-5");
        assert_eq!(cfg.cloud.fast_model, "claude-haiku-4-5");
        // A heavy, confident conversation turn keeps the configured default tier.
        let heavy = classification("conversation", "heavy", 0.95);

        // Default route is cloud_heavy: with a key, a heavy turn -> Opus cloud.
        assert_eq!(cfg.router.conversation_route, "cloud_heavy");
        assert_eq!(
            conversation_brain(&cfg, true, &heavy).0,
            ConversationBrain::Cloud("claude-opus-5".to_string())
        );
        // No key: even cloud_heavy degrades to the local 4B (Fallback).
        assert_eq!(conversation_brain(&cfg, false, &heavy).0, ConversationBrain::Local);

        // cloud_fast + key -> Haiku cloud; no key -> local.
        cfg.router.conversation_route = "cloud_fast".to_string();
        // A LIGHT turn under cloud_fast stays Fast (Haiku); a heavy one escalates
        // to Heavy. Use a light turn here to lock the cloud_fast -> Haiku mapping.
        let light = classification("conversation", "light", 0.95);
        assert_eq!(
            conversation_brain(&cfg, true, &light).0,
            ConversationBrain::Cloud("claude-haiku-4-5".to_string())
        );
        assert_eq!(conversation_brain(&cfg, false, &light).0, ConversationBrain::Local);

        // Explicit local: the resident 4B regardless of the key.
        cfg.router.conversation_route = "local".to_string();
        assert_eq!(conversation_brain(&cfg, true, &heavy).0, ConversationBrain::Local);
        assert_eq!(conversation_brain(&cfg, false, &heavy).0, ConversationBrain::Local);

        // Unknown value falls back to the safe, always-available local path.
        cfg.router.conversation_route = "wat".to_string();
        assert_eq!(conversation_brain(&cfg, true, &heavy).0, ConversationBrain::Local);
    }

    /// THRESHOLD finding 1 — GUEST = LOCAL-ONLY. A guest turn must NEVER reach the
    /// owner's PAID cloud (a cloud call appends an obol spend row + bumps the owner's
    /// daily budget — a durable, owner-readable trace — and egresses the guest's turn
    /// under the owner's API key). The fix forces a guest local at the SAME two seams
    /// vault uses; proven here at the cloud-vs-local decision the seams compute, with
    /// the composition `guest OR vault -> local`. The owner path is byte-for-byte
    /// unchanged (still cloud by default).
    #[test]
    fn a_guest_turn_is_forced_local_only_never_the_owners_paid_cloud() {
        let _guard = crate::model_tier::OverrideGuard::force(None);
        let cfg = Config::default(); // conversation_route defaults to cloud_heavy
        assert_eq!(cfg.router.conversation_route, "cloud_heavy");
        let heavy = classification("conversation", "heavy", 0.95);

        // OWNER path (no guest scope): a reachable-cloud turn is UNCHANGED — the seam
        // composition passes it through, and the conversation brain picks the cloud.
        assert!(
            crate::threshold::deny_cloud(crate::vault::deny_cloud(true)),
            "owner: a reachable cloud turn is passed through unchanged"
        );
        let owner_reachable = crate::threshold::deny_cloud(crate::vault::deny_cloud(true));
        assert!(
            matches!(conversation_brain(&cfg, owner_reachable, &heavy).0, ConversationBrain::Cloud(_)),
            "the owner still uses the paid cloud brain by default"
        );

        // GUEST: the SAME seam composition forces LOCAL — no cloud call, hence no obol
        // spend row / no budget bump / no owner-key egress on a bystander's turn.
        let guest = crate::threshold::guest_from(
            &crate::threshold::Scope::owner(vec!["*".to_string()], crate::focus::FocusProfile::Default),
            &crate::focus::FocusProfile::DeepFocus,
        );
        let _o = crate::threshold::ScopeOverride::guest(guest);
        assert!(crate::threshold::is_guest_turn());
        // SEAM 1 (cloud_reachable) is forced off even with the cloud reachable + vault off.
        assert!(
            !crate::threshold::deny_cloud(crate::vault::deny_cloud(true)),
            "guest: seam 1 (cloud_reachable) is forced local"
        );
        let guest_reachable = crate::threshold::deny_cloud(crate::vault::deny_cloud(true));
        assert_eq!(
            conversation_brain(&cfg, guest_reachable, &heavy).0,
            ConversationBrain::Local,
            "a guest conversation is answered by the on-device brain, never the paid cloud"
        );
        // SEAM 2 (the actuating tool-loop `to_cloud`) is likewise forced off for a guest.
        assert!(
            !crate::threshold::deny_cloud(crate::vault::deny_cloud(true)),
            "guest: seam 2 (to_cloud) is forced local"
        );
    }

    /// MODEL TIER wired into the conversation brain: an explicit override beats the
    /// config default, and an offline override forces the local path with NO cloud
    /// model — even when the cloud is reachable. Auto (no override) preserves the
    /// config-default behavior. This is the router-level proof that the swap is
    /// MODEL-only and that "offline" means no cloud call.
    #[test]
    fn conversation_brain_honors_model_override() {
        let _guard = crate::model_tier::OverrideGuard::force(None);
        let cfg = Config::default(); // cloud_heavy default
        let heavy = classification("conversation", "heavy", 0.95);

        // No override -> Auto -> Heavy/Opus (config default preserved).
        let (brain, _tier, reason) = conversation_brain(&cfg, true, &heavy);
        assert_eq!(brain, ConversationBrain::Cloud("claude-opus-5".to_string()));
        assert_eq!(reason, crate::model_tier::Reason::Auto);

        // Offline override -> Local, NO cloud model, even with cloud reachable.
        crate::model_tier::set_override(Some(crate::model_tier::Tier::Local));
        let (brain, tier, reason) = conversation_brain(&cfg, true, &heavy);
        assert_eq!(brain, ConversationBrain::Local);
        assert_eq!(tier, crate::model_tier::Tier::Local);
        assert_eq!(reason, crate::model_tier::Reason::Override);

        // Fast override -> Haiku cloud regardless of the heavy difficulty.
        crate::model_tier::set_override(Some(crate::model_tier::Tier::Fast));
        let (brain, _tier, reason) = conversation_brain(&cfg, true, &heavy);
        assert_eq!(brain, ConversationBrain::Cloud("claude-haiku-4-5".to_string()));
        assert_eq!(reason, crate::model_tier::Reason::Override);

        // Clear back to Auto.
        crate::model_tier::set_override(None);
    }

    /// MULTI-RESIDENT LOCAL sub-tier wired into the router (task #17). Under the
    /// CONSERVATIVE default (single-resident) the daemon sends NO local_model — the
    /// wire is identical to today and the base answers every local turn. With a
    /// multi-resident warm-set configured, a trivial turn threads the warm fast
    /// model id while a hard turn keeps the base (None). PURE — no inference call.
    #[tokio::test]
    async fn local_model_for_turn_is_none_under_single_resident_default() {
        let cfg = Config::default(); // empty warm-set, 0 budget => single-resident
        // Neither difficulty changes anything: single-resident => no local_model.
        assert_eq!(
            local_model_for_turn(&cfg, &classification("conversation", "light", 0.95)).await,
            None
        );
        assert_eq!(
            local_model_for_turn(&cfg, &classification("conversation", "heavy", 0.95)).await,
            None
        );
    }

    #[tokio::test]
    async fn local_model_for_turn_threads_fast_model_on_trivial_turn_when_multi_resident() {
        let mut cfg = Config::default();
        cfg.models.llm = "base-4b-4bit".to_string(); // ~2.4 GiB
        cfg.models.local_warm = vec!["fast-0.6b-4bit".to_string()]; // ~0.36 GiB
        cfg.models.local_budget_gib = 3.0; // admits the fast extra -> multi-resident

        // A trivial, confident turn -> the warm fast model is threaded.
        assert_eq!(
            local_model_for_turn(&cfg, &classification("conversation", "light", 0.95)).await,
            Some("fast-0.6b-4bit".to_string())
        );
        // A heavy turn keeps the capable base => None (no id on the wire; the
        // server answers on the base). No silent downgrade of a hard turn.
        assert_eq!(
            local_model_for_turn(&cfg, &classification("conversation", "heavy", 0.95)).await,
            None
        );
        // A low-confidence light turn is treated as hard -> base => None.
        assert_eq!(
            local_model_for_turn(&cfg, &classification("conversation", "light", 0.3)).await,
            None
        );
    }

    #[tokio::test]
    async fn local_model_for_turn_stays_none_when_budget_too_small() {
        // A multi-resident warm-set CONFIGURED but a budget too small to admit the
        // extra (or below the base estimate) stays single-resident => always None.
        let mut cfg = Config::default();
        cfg.models.llm = "base-4b-4bit".to_string();
        cfg.models.local_warm = vec!["fast-0.6b-4bit".to_string()];
        cfg.models.local_budget_gib = 1.0; // below the base estimate -> single
        assert_eq!(
            local_model_for_turn(&cfg, &classification("conversation", "light", 0.95)).await,
            None
        );
    }

    /// The HUD's per-turn local sub-choice label (FAST/CAPABLE/none) emitted in the
    /// `model.tier` payload. Under single-resident it is None (no indicator, the
    /// base answers); multi-resident reports the model that ACTUALLY answered —
    /// `fast` for a trivial/confident turn, `capable` when the base handled a
    /// hard/low-confidence turn. PURE — no inference. Matches local_model_for_turn.
    #[tokio::test]
    async fn local_sub_for_turn_reports_the_active_warm_choice() {
        // Single-resident default => no sub-choice (HUD indicator stays empty).
        let single = Config::default();
        assert_eq!(
            local_sub_for_turn(&single, &classification("conversation", "light", 0.95)).await,
            None
        );
        assert_eq!(
            local_sub_for_turn(&single, &classification("conversation", "heavy", 0.95)).await,
            None
        );

        // Multi-resident: a trivial confident turn answered on the fast model.
        let mut multi = Config::default();
        multi.models.llm = "base-4b-4bit".to_string();
        multi.models.local_warm = vec!["fast-0.6b-4bit".to_string()];
        multi.models.local_budget_gib = 3.0;
        assert_eq!(
            local_sub_for_turn(&multi, &classification("conversation", "light", 0.95)).await,
            Some("fast")
        );
        // A hard turn kept the capable base => CAPABLE (not a phantom fast pick).
        assert_eq!(
            local_sub_for_turn(&multi, &classification("conversation", "heavy", 0.95)).await,
            Some("capable")
        );
        // A low-confidence light turn is treated as hard => CAPABLE.
        assert_eq!(
            local_sub_for_turn(&multi, &classification("conversation", "light", 0.3)).await,
            Some("capable")
        );
    }

    /// CONTRACT B: the existing heavy/low-confidence cloud routing is
    /// UNCHANGED. Heavy -> cloud (Opus); a confident light action intent stays
    /// local; a low-confidence light turn still goes cloud (fast model). This
    /// applies to every intent — conversation_route does not touch it.
    #[test]
    fn heavy_and_action_routing_is_unchanged() {
        let cfg = Config::default(); // threshold 0.6

        // Heavy conversation -> cloud, Opus (heavy path, unchanged).
        let heavy = classification("conversation", "heavy", 0.95);
        assert!(wants_cloud(&heavy, &cfg), "heavy must route to cloud");
        assert_eq!(cloud_model(true, &cfg), "claude-opus-5", "heavy -> opus");

        // Confident light action intent -> local (unchanged: not heavy, high
        // confidence). conversation_route is irrelevant for action intents.
        let action = classification("app.launch", "light", 0.95);
        assert!(!wants_cloud(&action, &cfg), "confident action stays local");

        // Confident light conversation -> not cloud by the heavy/low-confidence
        // rule; the conversation-specific brain (above) decides cloud-vs-local.
        let chat = classification("conversation", "light", 0.95);
        assert!(!wants_cloud(&chat, &cfg));

        // Low-confidence light turn still goes cloud on the fast model
        // (unchanged low-confidence path).
        let unsure = classification("file.op", "light", 0.4);
        assert!(wants_cloud(&unsure, &cfg), "low confidence -> cloud");
        assert_eq!(cloud_model(false, &cfg), "claude-haiku-4-5", "light cloud -> haiku");
    }

    /// RC-6: an UNCERTAIN FALLBACK (low-confidence conversation — the garbled-
    /// echo shape CLASSIFY_FALLBACK produces) is recognized so the router can
    /// keep it OUT of the actuating cloud tool loop. A confident conversation
    /// turn, and any non-conversation intent (a real action, even low
    /// confidence), are NOT fallbacks and keep their existing routing.
    #[test]
    fn uncertain_fallback_is_only_low_confidence_conversation() {
        let cfg = Config::default(); // cloud_confidence_threshold 0.6

        // The exact CLASSIFY_FALLBACK shape: conversation / 0.3 -> fallback.
        assert!(is_uncertain_fallback(&classification("conversation", "heavy", 0.3), &cfg));
        // Low-confidence conversation generally -> fallback (no actuation).
        assert!(is_uncertain_fallback(&classification("conversation", "light", 0.5), &cfg));

        // A CONFIDENT conversation turn is NOT a fallback.
        assert!(!is_uncertain_fallback(&classification("conversation", "light", 0.95), &cfg));
        // A low-confidence ACTION intent is a real (weakly recognized) action,
        // NOT a fallback — its existing cloud tool routing is untouched.
        assert!(!is_uncertain_fallback(&classification("web.open", "light", 0.3), &cfg));
        assert!(!is_uncertain_fallback(&classification("app.launch", "heavy", 0.4), &cfg));
        // Exactly at the threshold is confident enough (not below it).
        assert!(!is_uncertain_fallback(&classification("conversation", "light", 0.6), &cfg));
    }

    /// Darwin-Prime delegation via the router wrapper: the offline-survival route
    /// fires exactly when the cloud is unreachable.
    ///
    /// WHAT WENT WRONG — AND WHAT THIS TEST USED TO ASSERT: `select_agent` took a
    /// `to_cloud` flag and OR-ed it into reachability, and this test pinned
    /// `(cloud_reachable=false, to_cloud=true) -> darwin` with the comment "the
    /// cloud is reachable for it". It is not. `to_cloud` comes from
    /// `wants_cloud(class, cfg)`, which reads only the classifier's
    /// complexity/confidence, while `cloud_reachable` is "an API key resolves".
    /// On a KEYLESS install every heavy conversation turn is exactly that pair —
    /// so hulk, the offline-survival agent, was never selected on the one install
    /// where it is the whole point, and route() went on to make a
    /// guaranteed-to-fail cloud call first. The test encoded the defect: it passed
    /// `to_cloud` as a literal and never exercised the caller's computation.
    #[test]
    fn select_agent_gates_offline_route_on_cloud_reachability() {
        let reg = AgentRegistry::canonical();
        // Cloud up: conversational turn is the orchestrator's.
        assert_eq!(select_agent(&reg, "conversation", "tell me about mars", true).name, "darwin");
        // Cloud down: hulk survives — INCLUDING the heavy/uncertain turns the old
        // `|| to_cloud` used to steal back for darwin.
        assert_eq!(select_agent(&reg, "conversation", "tell me about mars", false).name, "hulk");
        // Local action intents are unaffected by cloud state.
        assert_eq!(select_agent(&reg, "app.launch", "open safari", false).name, "oracle");
    }

    /// The CALLER'S computation, not a literal: on a keyless install a heavy
    /// conversation turn really does produce `to_cloud = true` while
    /// `cloud_reachable = false`, and hulk must still be chosen. This is the half
    /// the old test never covered.
    #[test]
    fn a_heavy_conversation_turn_without_a_cloud_key_still_reaches_hulk() {
        let cfg = Config::default();
        let reg = AgentRegistry::canonical();
        let heavy = classification("conversation", "heavy", 0.9);
        // PRECONDITION: this really is the cloud-bound shape the caller computes.
        assert!(
            super::wants_cloud(&heavy, &cfg),
            "precondition: a heavy conversation turn is cloud-bound by classification"
        );
        // …and with no key resolving, reachability is false. The two are
        // independent, which is the whole finding.
        assert_eq!(
            select_agent(&reg, &heavy.intent, "explain how photosynthesis works in detail", false)
                .name,
            "hulk",
            "with no cloud key the offline-survival agent must handle the turn"
        );
    }

    /// Tool-allowlist isolation at the router boundary: an agent that lacks the
    /// intent's tool is replaced by the tool's owner; an agent that holds it is
    /// kept. friday (intel) cannot run app.launch — that is oracle's.
    #[test]
    fn enforce_tool_reroutes_out_of_domain_intents() {
        let reg = AgentRegistry::canonical();
        let friday = reg.get("friday").unwrap();
        // friday does not own app.launch -> handed to oracle (the owner).
        let acting = enforce_tool(&reg, friday, "app.launch");
        assert_eq!(acting.name, "oracle");
        // oracle owns app.launch -> kept.
        let oracle = reg.get("oracle").unwrap();
        assert_eq!(enforce_tool(&reg, oracle, "app.launch").name, "oracle");
        // friday owns system.query -> kept.
        assert_eq!(enforce_tool(&reg, friday, "system.query").name, "friday");
        // darwin (wildcard) keeps anything.
        let darwin = reg.get("darwin").unwrap();
        assert_eq!(enforce_tool(&reg, darwin, "web.open").name, "darwin");
    }

    /// Roll-call interrupt mechanics: the cancel flag toggles cleanly and a
    /// fresh roll-call would clear it (the flag is process-wide and idempotent).
    /// Serialized via the flag's own reset so concurrent tests don't collide.
    /// Roll-call interrupt lifecycle (RC-9). interrupt_roll_call() SETS the
    /// cancel flag; clear_roll_call_interrupt() (called from
    /// speech::clear_barge_in at each new turn) RESETS it, so a barge over an
    /// unrelated reply can no longer leave a roll-call abort latched. Both
    /// mutators of the process-global ROLL_CALL_CANCEL are exercised in ONE test
    /// so they can never race each other on a parallel runner — the flag is a
    /// single shared global, and two separate tests mutating it would collide.
    #[test]
    fn roll_call_interrupt_lifecycle() {
        // Set, then read back.
        interrupt_roll_call();
        assert!(ROLL_CALL_CANCEL.load(Ordering::Relaxed), "interrupt must set the cancel flag");
        // Clear resets it (the RC-9 fix's mechanism).
        clear_roll_call_interrupt();
        assert!(!ROLL_CALL_CANCEL.load(Ordering::Relaxed), "clear must reset the cancel flag");
        // Idempotent: clearing again is harmless.
        clear_roll_call_interrupt();
        assert!(!ROLL_CALL_CANCEL.load(Ordering::Relaxed));
        // Leave the flag CLEAR so the live roll-call (which also clears it at
        // start) is unaffected.
        clear_roll_call_interrupt();
    }

    #[test]
    fn app_name_extraction_takes_words_after_the_trigger_verb() {
        assert_eq!(extract_app_name("darwin please open up google chrome"), "google chrome");
        assert_eq!(extract_app_name("launch the calculator app for me"), "calculator");
        assert_eq!(extract_app_name("quit safari"), "safari");
        assert_eq!(extract_app_name("close safari"), "safari");
        assert_eq!(extract_app_name("start photo booth now"), "photo booth");
    }

    /// Audit regression: quit-class utterances must NEVER reach the
    /// launcher — "quit safari" used to OPEN Safari ("Opened Safari.").
    #[test]
    fn quit_and_close_never_route_to_the_launcher() {
        for text in ["quit safari", "close safari", "exit chrome", "kill the music app"] {
            assert!(wants_quit(text), "missed quit verb in: {text}");
            let extracted = extract_app_name(text);
            assert_eq!(
                classify_app_request("app.launch", text, &extracted),
                AppRequest::Quit,
                "would have launched: {text}"
            );
            assert_eq!(
                classify_app_request("app.control", text, &extracted),
                AppRequest::Quit,
                "would have launched: {text}"
            );
        }
        assert!(!wants_quit("open safari"));
        assert!(!wants_quit("darwin please open up google chrome"));
    }

    /// Belt-and-suspenders reroute: an app.launch whose remainder smells of
    /// the web goes to the web.open handling — the original failing case
    /// must trigger it even if the classifier says app.launch.
    #[test]
    fn web_flavored_launches_reroute_to_web_open() {
        let text = "open the official apple website on safari";
        let extracted = extract_app_name(text);
        assert_eq!(
            classify_app_request("app.launch", text, &extracted),
            AppRequest::Web
        );
        // Bare-domain and scheme flavors trigger too.
        for text in [
            "open apple.com",
            "open up rust-lang.org for me",
            "open https://apple.com",
            "open the anthropic web page",
            "open that site again",
        ] {
            let extracted = extract_app_name(text);
            assert_eq!(
                classify_app_request("app.launch", text, &extracted),
                AppRequest::Web,
                "should reroute to web: {text}"
            );
        }
        // Plain app launches stay launches.
        for text in ["open safari", "launch the calculator app for me", "start photo booth"] {
            let extracted = extract_app_name(text);
            assert_eq!(
                classify_app_request("app.launch", text, &extracted),
                AppRequest::Launch,
                "should stay a launch: {text}"
            );
        }
        // The reroute is app.launch-only per contract.
        assert_eq!(
            classify_app_request("app.control", "open apple.com", "apple.com"),
            AppRequest::Launch
        );
    }

    #[test]
    fn web_markers_are_words_or_domain_fragments() {
        assert!(suggests_web("official apple website"));
        assert!(suggests_web("apple.com"));
        assert!(suggests_web("wikipedia.org"));
        assert!(suggests_web("https://apple.com"));
        assert!(suggests_web("the web"));
        assert!(!suggests_web("safari"));
        assert!(!suggests_web("google chrome"));
        assert!(!suggests_web("communications app")); // no false substring hits
        assert!(!suggests_web(""));
    }

    #[test]
    fn web_query_drops_command_and_web_noise() {
        assert_eq!(
            extract_web_query("search the web for rust async tutorials"),
            "rust async tutorials"
        );
        assert_eq!(extract_web_query("google the weather in tokyo"), "weather tokyo");
    }

    #[test]
    fn arg_str_reads_only_non_empty_strings() {
        let args = json!({"url": "apple.com", "browser": "  ", "n": 4});
        assert_eq!(arg_str(&args, "url"), Some("apple.com"));
        assert_eq!(arg_str(&args, "browser"), None); // blank -> absent
        assert_eq!(arg_str(&args, "n"), None); // wrong type -> absent
        assert_eq!(arg_str(&args, "missing"), None);
        // Old servers: args is Null, every lookup is None.
        assert_eq!(arg_str(&serde_json::Value::Null, "url"), None);
    }

    #[test]
    fn app_name_extraction_is_empty_without_a_trigger_verb() {
        // The router then feeds the whole utterance to the fuzzy matcher.
        assert_eq!(extract_app_name("could you get safari going"), "");
        assert_eq!(extract_app_name("open"), ""); // verb with nothing after it
    }

    #[test]
    fn content_words_drop_the_command_vocabulary() {
        assert_eq!(
            extract_content_words("find my budget spreadsheet file"),
            "budget spreadsheet"
        );
        assert_eq!(
            extract_content_words("look for the document called tax-report.pdf"),
            "tax-report.pdf"
        );
        assert_eq!(extract_content_words("find my files"), "");
    }

    #[test]
    fn open_detection_is_a_plain_substring_check() {
        assert!(utterance_wants_open("find and open the budget file"));
        assert!(!utterance_wants_open("find the budget file"));
    }

    /// CONTRACT B: the router passes DARWIN's most-recent replies as the cloud
    /// conversation anti-repeat `avoid` list. History is oldest-first; the
    /// freshest replies come back first, blanks are dropped, and the list is
    /// capped at n (the prompt-level lever Opus needs since it has no
    /// temperature). Empty history -> empty list (a first turn dodges nothing).
    #[test]
    fn recent_replies_takes_the_freshest_darwin_replies() {
        let history = vec![
            ("hi".to_string(), "Hello, sir. Good to have you back.".to_string()),
            ("hi".to_string(), "Welcome back, sir.".to_string()),
            ("hi".to_string(), "  ".to_string()), // blank reply dropped
            ("hi".to_string(), "Ah, there you are, sir.".to_string()),
        ];
        let avoid = recent_replies(&history, 4);
        // Freshest first, blank dropped: 3 non-blank replies, newest leading.
        assert_eq!(
            avoid,
            vec![
                "Ah, there you are, sir.".to_string(),
                "Welcome back, sir.".to_string(),
                "Hello, sir. Good to have you back.".to_string(),
            ]
        );
        // Cap is honoured.
        assert_eq!(recent_replies(&history, 1), vec!["Ah, there you are, sir.".to_string()]);
        // First turn: nothing to dodge.
        assert!(recent_replies(&[], 4).is_empty());
    }

    // ---- Silicon Canvas voice control (SPEC §6) ----

    /// Helper: assert the utterance maps to an Op carrying EXACTLY this JSON
    /// wire string (the form Silicon Canvas's ops.rs deserializes verbatim).
    fn assert_op(text: &str, expected_json: &str) {
        match silicon_canvas_command(text) {
            Some(SiliconCanvasCommand::Op(line)) => {
                // Compare as parsed JSON so key order is irrelevant; the exact
                // op-tag + fields are what the contract pins.
                let got: serde_json::Value = serde_json::from_str(&line).unwrap();
                let want: serde_json::Value = serde_json::from_str(expected_json).unwrap();
                assert_eq!(got, want, "for utterance {text:?}");
            }
            other => panic!("expected an Op for {text:?}, got {other:?}"),
        }
    }

    /// "open silicon canvas" (and its open-class variants) is a LAUNCH; the
    /// app name is the manifest name the registry keys on.
    #[test]
    fn silicon_canvas_launch_phrases() {
        assert_eq!(SILICON_CANVAS_APP, "silicon-canvas");
        for text in [
            "open silicon canvas",
            "launch silicon canvas",
            "bring up silicon canvas",
            "darwin, show me silicon canvas",
            "open the schematic",
            "bring up the board view",
        ] {
            assert_eq!(
                silicon_canvas_command(text),
                Some(SiliconCanvasCommand::Launch),
                "should launch: {text:?}"
            );
        }
    }

    /// "show me the <X> net" / "highlight the <X> net" -> select.net {name},
    /// with the net name forwarded verbatim (uppercased to KiCad convention).
    #[test]
    fn silicon_canvas_net_selection_maps_to_select_net() {
        assert_op("show me the 3V3 net", r#"{"op":"select.net","name":"3V3"}"#);
        assert_op("highlight the gnd net", r#"{"op":"select.net","name":"GND"}"#);
        assert_op("select the vbus net", r#"{"op":"select.net","name":"VBUS"}"#);
        // The net name rides through even with extra words around it.
        assert_op("can you show me the sda net please", r#"{"op":"select.net","name":"SDA"}"#);
    }

    /// Trace mode: start / step / stop map to the three trace ops, and the
    /// specific step/stop verbs are matched before the broad "trace" -> start.
    #[test]
    fn silicon_canvas_trace_mode_ops() {
        assert_op("trace this net", r#"{"op":"trace.start"}"#);
        assert_op("start tracing", r#"{"op":"trace.start"}"#);
        assert_op("begin the trace", r#"{"op":"trace.start"}"#);
        // Step (advance) — must NOT be read as start.
        assert_op("next trace step", r#"{"op":"trace.step"}"#);
        assert_op("step the trace", r#"{"op":"trace.step"}"#);
        assert_op("advance the trace", r#"{"op":"trace.step"}"#);
        // Stop/exit.
        assert_op("stop tracing", r#"{"op":"trace.stop"}"#);
        assert_op("exit trace mode", r#"{"op":"trace.stop"}"#);
    }

    /// "run ERC" and the spelled-out electrical-rule-check phrasing -> erc.run.
    #[test]
    fn silicon_canvas_erc_maps_to_erc_run() {
        assert_op("run erc", r#"{"op":"erc.run"}"#);
        assert_op("run the ERC", r#"{"op":"erc.run"}"#);
        assert_op("run the electrical rule check", r#"{"op":"erc.run"}"#);
        assert_op("check the electrical rules", r#"{"op":"erc.run"}"#);
    }

    /// Component selection and view fit.
    #[test]
    fn silicon_canvas_component_and_view_ops() {
        assert_op("select component u3", r#"{"op":"select.component","name":"U3"}"#);
        assert_op("show component r12", r#"{"op":"select.component","name":"R12"}"#);
        // A bare "component" with no ref token is NOT a select.component.
        assert!(silicon_canvas_command("tell me about the component").is_none());
        // View fit.
        assert_op("fit the board", r#"{"op":"view.set","mode":"fit","target":"all"}"#);
        assert_op("show the whole board", r#"{"op":"view.set","mode":"fit","target":"all"}"#);
        assert_op("fit all", r#"{"op":"view.set","mode":"fit","target":"all"}"#);
    }

    /// The classifier does NOT capture unrelated utterances: a plain "open
    /// safari" or a greeting falls through to normal routing (None), so the
    /// Silicon Canvas pre-check never shadows the macOS launcher or chat.
    #[test]
    fn silicon_canvas_command_ignores_unrelated_utterances() {
        for text in [
            "open safari",
            "hello darwin how are you",
            "what's the weather",
            "find my budget spreadsheet",
            "open apple.com",
            "play some music",
            "i read the network news",   // "net" only as a substring of network/news
            "open the calculator",
        ] {
            assert_eq!(
                silicon_canvas_command(text),
                None,
                "must not capture an unrelated utterance: {text:?}"
            );
        }
    }

    /// Whole-word "net": "network"/"netflix" never trigger select.net (the
    /// extractor splits on word boundaries and requires the standalone token).
    #[test]
    fn silicon_canvas_net_is_whole_word_only() {
        assert!(silicon_canvas_command("check the network settings").is_none());
        assert!(silicon_canvas_command("open netflix").is_none());
        // But a real net selection still fires.
        assert_op("show me the clk net", r#"{"op":"select.net","name":"CLK"}"#);
    }

    /// THE ORDINARY-SPEECH CORPUS, IN TEST FORM. Every utterance here was
    /// captured by the shipped classifier out of a 1,897-line corpus of
    /// ordinary speech (or is the minimal shape of one that was). None of them
    /// is about a circuit board; each one was an answer the user did not get,
    /// because router.rs:1953 is an else-if chain and a captured turn never
    /// reaches conversation.
    ///
    /// Mutation-proof: delete any single gate helper's condition and lines here
    /// go red. The four classes are labelled with the substring that did it.
    #[test]
    fn silicon_canvas_does_not_capture_ordinary_speech() {
        for text in [
            // "erc" inside percent / mercy / merchant / commerce — 80 captures.
            "what percent of my paycheck goes to taxes",
            "have mercy, this curry is spicy",
            "the chamber of commerce sent me a membership bill",
            "the merchant charged me twice",
            "we need to check her erc grant application",
            "i bought erc 721 nfts last year",
            "can you check the erc refund status",
            // "trace" as ordinary English — 31 captures.
            "is there any trace of gluten in this bread",
            "the recipe calls for a trace of nutmeg at the end",
            "they found trace amounts of lead",
            "i had to retrace my steps to find my keys",
            "there's no trace",
            // ...and the shape that survives mere ADJACENCY of a lifecycle verb.
            "please stop the trace on my credit report",
            "resume the trace of the phone call",
            "restart the trace on the shipment",
            "i want to begin a trace on the missing package",
            "let's cancel the trace request with the bank",
            "they will exit the trace program next year",
            "enter the trace number on the website",
            "stop tracing the outline and color it in",
            "step by step trace the recipe with me",
            "the trace mode on my fitness watch is broken",
            "begin to trace the payments tomorrow",
            "can you trace this charge on my card",
            "can they trace it back to me",
            "please trace my package",
            // "net" as ordinary English — 20 captures.
            "my net calories were way under yesterday",
            "what's the net weight of a can of chickpeas",
            "nothing but net",
            "how much do you take home in net pay",
            "show me the mosquito net options",
            "the safety net plan is generous",
            "i watched a tennis net repair video",
            "can you go to the store and get a net",
            "go grab the net",
            // "fit"/"board"/"all" as substrings, and as whole words in
            // non-board senses — 1 capture plus its neighbours.
            "how many bodies can that minivan actually fit with luggage",
            "the fit of these boards is off",
            "the whole board of directors approved the budget",
            "show me the whole board game collection",
            "i told the whole board about it",
            "will it all fit in the car",
            "i fit all my clothes in one bag",
            // A component reference that is not a component.
            "component 5 is backordered",
            "show me component 4 of the essay",
            "find component 7 of the rubric",
            // The electrician's question the ERC gate must not steal.
            "the electrical rules for a subpanel on this board",
        ] {
            assert_eq!(
                silicon_canvas_command(text),
                None,
                "ordinary speech must not be captured: {text:?}"
            );
        }
    }

    /// ...and the real commands, which the gate above must NOT cost. Every one
    /// of these is either asserted elsewhere in this file, printed in the doc
    /// comment on `silicon_canvas_command`, listed in apps/silicon-canvas/SPEC.md,
    /// or the way a person actually phrases one of those out loud. A previous
    /// attempt at this fix drove captures to zero and broke a third of them; a
    /// zero-capture number on its own is not a result.
    #[test]
    fn silicon_canvas_still_takes_the_real_commands() {
        // Launch, including the phrasings with a trailing locus.
        for text in [
            "start silicon canvas",
            "darwin open up silicon canvas please",
            "can you open silicon canvas for me",
            "show me the schematic",
            "open the schematic on my screen",
            "open silicon canvas in the sandbox",
            "launch the board view",
        ] {
            assert_eq!(
                silicon_canvas_command(text),
                Some(SiliconCanvasCommand::Launch),
                "should still launch: {text:?}"
            );
        }
        // Net selection: label names need no verb; a trailing locus, a
        // "please" and a "for me" do not end the command.
        assert_op("the 3v3 net", r#"{"op":"select.net","name":"3V3"}"#);
        assert_op("what's on the 3v3 net", r#"{"op":"select.net","name":"3V3"}"#);
        assert_op(
            "darwin show me the 3v3 net on my screen",
            r#"{"op":"select.net","name":"3V3"}"#,
        );
        assert_op(
            "could you highlight the vbus net for me",
            r#"{"op":"select.net","name":"VBUS"}"#,
        );
        assert_op(
            "can you highlight the clk net in the sandbox",
            r#"{"op":"select.net","name":"CLK"}"#,
        );
        assert_op("select the ground net", r#"{"op":"select.net","name":"GROUND"}"#);
        assert_op("show me the in net", r#"{"op":"select.net","name":"IN"}"#);
        // A non-label name still works when a select verb names it.
        assert_op("show me the batt net", r#"{"op":"select.net","name":"BATT"}"#);
        assert_op("go to the batt net", r#"{"op":"select.net","name":"BATT"}"#);
        // Trace: the object may continue the phrase, and the lifecycle verb may
        // sit anywhere a speaker puts it.
        assert_op("trace this connection", r#"{"op":"trace.start"}"#);
        assert_op("trace the gnd net", r#"{"op":"trace.start"}"#);
        assert_op("trace this net on my screen", r#"{"op":"trace.start"}"#);
        assert_op("can you trace this net for me", r#"{"op":"trace.start"}"#);
        assert_op("enter trace mode", r#"{"op":"trace.start"}"#);
        assert_op("trace it", r#"{"op":"trace.start"}"#);
        assert_op("next step in the trace", r#"{"op":"trace.step"}"#);
        assert_op("advance the trace one step", r#"{"op":"trace.step"}"#);
        assert_op("step the trace forward", r#"{"op":"trace.step"}"#);
        assert_op("keep tracing, next segment", r#"{"op":"trace.step"}"#);
        assert_op("step to the next trace segment", r#"{"op":"trace.step"}"#);
        assert_op("okay stop the trace now", r#"{"op":"trace.stop"}"#);
        assert_op("that's enough, stop tracing", r#"{"op":"trace.stop"}"#);
        assert_op("cancel the trace", r#"{"op":"trace.stop"}"#);
        // ERC: verb, ERC noun, or object position.
        assert_op("re-run erc", r#"{"op":"erc.run"}"#);
        assert_op("rerun the erc", r#"{"op":"erc.run"}"#);
        assert_op("can you run erc on the board", r#"{"op":"erc.run"}"#);
        assert_op("show me the erc errors", r#"{"op":"erc.run"}"#);
        assert_op("any erc violations", r#"{"op":"erc.run"}"#);
        assert_op("show me the erc", r#"{"op":"erc.run"}"#);
        assert_op(
            "run an electrical rule check on the schematic",
            r#"{"op":"erc.run"}"#,
        );
        // Component: a designator needs no verb; a bare number needs a verb AND
        // object position.
        assert_op("component r12", r#"{"op":"select.component","name":"R12"}"#);
        assert_op(
            "select component u7 on my screen",
            r#"{"op":"select.component","name":"U7"}"#,
        );
        assert_op("select component 5", r#"{"op":"select.component","name":"5"}"#);
        // View fit.
        let fit = r#"{"op":"view.set","mode":"fit","target":"all"}"#;
        assert_op("fit the board on my screen", fit);
        assert_op("zoom to fit the board", fit);
        assert_op("can you fit the whole board", fit);
        assert_op("fit everything on the board", fit);
        assert_op("fit the board view", fit);
        assert_op("show the entire board", fit);
    }

    /// The three real commands this gate DOES cost, pinned so the trade is
    /// visible rather than discovered. Each is a phrase the shipped classifier
    /// accepted and this one does not, and each is the price of a specific
    /// ordinary-speech capture listed above:
    ///   - "the whole board" with no view verb, because "i told the whole
    ///     board" and "we presented to the whole board" have the same shape;
    ///   - "trace this charge"-shaped objects, i.e. any trace object that is
    ///     not PCB material;
    ///   - an "<X> net" with a non-label name and no select verb, because
    ///     "nothing but net" has exactly that shape.
    ///
    /// If a user reports one of these missing, the fix is to name the board or
    /// add the verb — not to widen the gate without a corpus re-run.
    #[test]
    fn silicon_canvas_documented_trade_offs() {
        assert!(silicon_canvas_command("the whole board").is_none());
        assert!(silicon_canvas_command("trace this charge").is_none());
        assert!(silicon_canvas_command("the batt net").is_none());
    }

    // ======================================================================
    // Vision voice control. Mirrors the Silicon Canvas tests above. The wire
    // form pinned here is the FROZEN Op.swift envelope: every op carries
    // {"type":"op","op":...} — these exact lines appear in the Vision app's own
    // IPCTests, so a pass here proves the daemon emits what the app accepts.
    // ======================================================================

    /// Assert the utterance maps to a Vision Op carrying EXACTLY this JSON wire
    /// string (compared as parsed JSON so key order is irrelevant).
    fn assert_vision_op(text: &str, expected_json: &str) {
        match vision_command(text) {
            Some(VisionCommand::Op(line)) => {
                let got: serde_json::Value = serde_json::from_str(&line).unwrap();
                let want: serde_json::Value = serde_json::from_str(expected_json).unwrap();
                assert_eq!(got, want, "for utterance {text:?}");
            }
            other => panic!("expected a Vision Op for {text:?}, got {other:?}"),
        }
    }

    /// "open/launch/start vision" is a LAUNCH keyed on the manifest name.
    #[test]
    fn vision_launch_phrases() {
        assert_eq!(VISION_APP, "vision");
        for text in [
            "open vision",
            "launch vision",
            "start vision",
            "darwin, bring up vision",
            "fire up the camera feed",
        ] {
            assert_eq!(
                vision_command(text),
                Some(VisionCommand::Launch),
                "{text:?} should be a Vision launch"
            );
        }
        // "vision" must be a whole word — never inside "television"/"revision".
        assert!(vision_command("open the television").is_none());
        assert!(vision_command("start the revision").is_none());
    }

    // ===== LUMEN (#45) dispatch ==========================================

    /// "read me the screen / the buttons / what's on screen" classify as a Lumen
    /// READ; "click/press/tap the <ordinal|name>" as a Lumen ACT carrying the
    /// phrase.
    #[test]
    fn lumen_read_and_act_phrases_route_correctly() {
        for text in [
            "read me the screen",
            "read the screen",
            "read me the buttons",
            "read the controls",
            "narrate the screen",
            "list the buttons",
            "what's on screen",
            "what is on my screen",
            "what are the buttons",
        ] {
            assert_eq!(lumen_command(text), Some(LumenCommand::Read), "{text:?} -> READ");
        }
        for (text, want) in [
            ("click the third button", "click the third button"),
            ("press the second button", "press the second button"),
            ("tap Submit", "tap submit"),
            ("click Sign in", "click sign in"),
            ("click the 2nd link", "click the 2nd link"),
        ] {
            assert_eq!(
                lumen_command(text),
                Some(LumenCommand::Act(want.to_string())),
                "{text:?} -> ACT (lowercased phrase)"
            );
        }
    }

    /// The classifier is CONSERVATIVE: ordinary speech and the more-specific
    /// Vision phrasings never over-trigger a Lumen read/act.
    #[test]
    fn lumen_does_not_over_trigger_on_unrelated_speech() {
        for text in [
            // No UI-actuation verb / no screen-or-controls read anchor.
            "read me the news",
            "what's on my plate today",
            "what do you see",
            "press play",           // press + no control noun/ordinal
            "push harder",          // push + no control noun/ordinal
            "let's press on",       // press + no control noun/ordinal
            "is the tap water safe", // REGRESSION: "tap" mid-sentence is not a command
            "i'll be on tap all night",
            "he had to tap out of the match",
            "select all my emails", // "select" is NOT a Lumen act verb
            "choose a restaurant",  // "choose" is NOT a Lumen act verb
            // These belong to the more-specific Vision ops (deferred by Lumen).
            "where's the submit button",
            "locate the settings icon",
            "watch the screen",
            "scan this document",
            "read this handwriting",
            "describe my screen",
        ] {
            assert_eq!(lumen_command(text), None, "{text:?} must NOT trigger Lumen");
        }
    }

    /// A Lumen READ is a screen read (surfaces on-screen control labels), so it is
    /// unioned into `is_screen_read` for TRANSIENCE; a Lumen ACT is NOT a read.
    #[test]
    fn lumen_read_is_transient_but_act_is_not() {
        assert!(is_screen_read("read me the buttons"), "a lumen read is transient");
        assert!(is_screen_read("what are the controls"), "a lumen read is transient");
        assert!(!is_screen_read("click the third button"), "an actuation is not a screen read");
    }

    /// The ACT arm builds the `ui_actuate` tool input in the EXACT `UiActuateArgs`
    /// shape a live tool call carries — a single click at the resolved point, with
    /// `confirm` OMITTED (never self-set, so it can only ever PARK).
    #[test]
    fn ui_actuate_input_is_the_capstone_tool_shape() {
        let req = crate::ui_automation::ActuationRequest {
            action: crate::ui_automation::Action::Click { x: 300, y: 200 },
            target_desc: "Cancel".to_string(),
        };
        let input = ui_actuate_input(&req);
        assert_eq!(input["action"], "click");
        assert_eq!(input["target"], "Cancel");
        assert_eq!(input["x"], 300);
        assert_eq!(input["y"], 200);
        assert!(input.get("confirm").is_none(), "confirm is never set by Lumen: {input}");
    }

    /// THE SAFETY ASSERTION: the ACT path builds an ActuationRequest via the pure
    /// selector and flows it through the UNCHANGED capstone (`execute_tool`,
    /// the SAME entry a live tool call uses) — and the capstone NEVER auto-executes
    /// it. `resolve_voice_action` -> `ui_actuate_input` -> `execute_tool` under the
    /// ui_actuate-owning agent's allowlist: nothing is performed and nothing
    /// self-authorizes (with the master switch off — the default — the gate is a
    /// DryRun even with confirm; in this headless build the deny-leaning display
    /// bound also refuses the click pre-actuation, so nothing is even parked). NO
    /// real AX/OCR/actuate runs. `plan_actuation` against a real bound proves the
    /// request is a valid SINGLE actuation (never a batch).
    #[tokio::test]
    async fn lumen_act_flows_through_the_unchanged_capstone_and_never_auto_executes() {
        use crate::ui_automation::{Action, ScreenBounds};
        // A located control list, exactly as a prior read would have produced.
        let controls = vec![
            crate::lumen::NarratableElement {
                label: "Submit".into(),
                role: crate::lumen::ElementRole::Button,
                center: Some((100, 200)),
            },
            crate::lumen::NarratableElement {
                label: "Cancel".into(),
                role: crate::lumen::ElementRole::Button,
                center: Some((300, 200)),
            },
        ];
        // Pure selection -> the ONE target's actuation request (never a batch).
        let req = crate::lumen::resolve_voice_action("click the second button", &controls).unwrap();
        assert!(matches!(req.action, Action::Click { x: 300, y: 200 }));
        assert!(
            crate::ui_automation::plan_actuation(&req, ScreenBounds { width: 4000, height: 4000 }).is_ok(),
            "the request is a valid, bounded, single actuation"
        );
        // The gate never auto-executes on Lumen's say-so: with the master switch
        // off (default), even a confirm is a DryRun (parks/previews, never fires).
        assert_eq!(
            crate::integrations::gate(true),
            crate::integrations::ActionMode::DryRun,
            "confirm alone can never execute — the action parks/previews"
        );

        // Flow the SAME request through the UNCHANGED capstone entry.
        let db = TempDb::new("lumen-act");
        let mem = Memory::open(&db.0).unwrap();
        let reg = AgentRegistry::canonical();
        let actuator = reg.owner_of("ui_actuate").expect("an agent owns ui_actuate");
        assert!(actuator.may_use("ui_actuate"), "the owner may use the capstone");
        let input = ui_actuate_input(&req);
        let (outcome, _is_error) = crate::anthropic::execute_tool(
            "ui_actuate",
            &input,
            &mem,
            &actuator.tools,
            &actuator.namespace,
            true,
            true, // context_trusted: mirrors the attended live-actuation production call
            &mut crate::anthropic::ToolEffect::DryRun,
        )
        .await;
        assert!(
            !outcome.to_lowercase().contains("i performed"),
            "the capstone must NEVER auto-execute a Lumen actuation: {outcome}"
        );
        // Nothing self-authorized: no parked-then-executed action left the slot
        // holding an executed effect (the deny-leaning bound refused it pre-park).
        assert!(
            crate::confirm::peek_pending(std::time::Instant::now()).is_none(),
            "a Lumen actuation never self-parks an executed action"
        );
    }

    /// The READ arm forwards the READ-ONLY Vision `read.screen` locate through the
    /// speech path (llm_voice) — honest when Vision isn't reachable, never a
    /// fabricated readout, and it actuates nothing.
    #[tokio::test]
    async fn lumen_read_arm_forwards_the_readonly_locate_through_speech() {
        let apps = std::sync::Arc::new(crate::apps::AppRegistry::discover(std::path::Path::new(
            "/nonexistent",
        )));
        let reg = AgentRegistry::canonical();
        let out = super::handle_lumen(
            LumenCommand::Read,
            &Memory::open(&TempDb::new("lumen-read").0).unwrap(),
            &apps,
            reg.orchestrator(),
        )
        .await;
        assert!(out.llm_voice, "the read acknowledgment is persona-voiced");
        // Vision isn't running here, so it says so HONESTLY (never a fake readout).
        assert!(out.data.to_lowercase().contains("screen"), "{}", out.data);
        assert!(!out.data.to_lowercase().contains("i performed"), "read actuates nothing");
    }

    /// "what do you see" / "who is there" -> the generic presence STATUS
    /// snapshot. DEFENSIVE-ONLY: "who is there" is presence, NOT an identity
    /// query — it maps to the SAME status op as "what do you see"; there is no
    /// name/face op anywhere.
    #[test]
    fn vision_presence_queries_map_to_status_not_identity() {
        let status = r#"{"type":"op","op":"status"}"#;
        assert_vision_op("what do you see", status);
        assert_vision_op("darwin, what can you see right now", status);
        assert_vision_op("who is there", status);
        assert_vision_op("who's there", status);
        assert_vision_op("is anyone there", status);
        assert_vision_op("is somebody there", status);
        // The op body NEVER contains a name/identity field — presence only.
        if let Some(VisionCommand::Op(line)) = vision_command("who is there") {
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert!(v.get("name").is_none(), "presence status must carry no identity");
            assert!(v.get("person").is_none());
            assert_eq!(v["op"], "status");
        } else {
            panic!("expected a status op");
        }
    }

    /// "watch the door|room|camera" -> watch.start {camera}; "watch the
    /// screen|display" -> watch.start {screen}.
    #[test]
    fn vision_watch_picks_camera_or_screen_source() {
        assert_vision_op(
            "watch the door",
            r#"{"type":"op","op":"watch.start","source":"camera"}"#,
        );
        assert_vision_op(
            "watch the room",
            r#"{"type":"op","op":"watch.start","source":"camera"}"#,
        );
        assert_vision_op(
            "keep watching the front camera",
            r#"{"type":"op","op":"watch.start","source":"camera"}"#,
        );
        assert_vision_op(
            "watch the screen",
            r#"{"type":"op","op":"watch.start","source":"screen"}"#,
        );
        assert_vision_op(
            "watch my display",
            r#"{"type":"op","op":"watch.start","source":"screen"}"#,
        );
    }

    /// "stop watching" -> watch.stop (checked before the broad watch.start so a
    /// stop verb is never mistaken for a start).
    #[test]
    fn vision_stop_watching_maps_to_watch_stop() {
        let stop = r#"{"type":"op","op":"watch.stop"}"#;
        assert_vision_op("stop watching", stop);
        assert_vision_op("stop watching the door", stop);
        assert_vision_op("end the watch", stop);
        assert_vision_op("cancel watching the screen", stop);
    }

    /// "analyze <name>.mp4" forwards the path verbatim; a bare "analyze this
    /// video" forwards an EMPTY path the app reports cleanly (Op.swift rejects an
    /// empty path -> .unknown -> a clean vision.error, never a crash).
    #[test]
    fn vision_analyze_file_forwards_path_or_empty() {
        assert_vision_op(
            "analyze front_door.mp4",
            r#"{"type":"op","op":"analyze.file","path":"front_door.mp4"}"#,
        );
        assert_vision_op(
            "analyze the clip porch-cam.mov please",
            r#"{"type":"op","op":"analyze.file","path":"porch-cam.mov"}"#,
        );
        // Bare "analyze this video" -> analyze.file with an empty path.
        assert_vision_op(
            "analyze this video",
            r#"{"type":"op","op":"analyze.file","path":""}"#,
        );
    }

    /// "set sensitivity to <X>" -> set.sensitivity with a clamped 0..=1 value;
    /// words/percent/float forms all resolve.
    #[test]
    fn vision_sensitivity_maps_to_set_sensitivity() {
        // Percent and bare float both normalize to 0..=1.
        match vision_command("set the sensitivity to 70 percent") {
            Some(VisionCommand::Op(line)) => {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(v["op"], "set.sensitivity");
                assert!((v["value"].as_f64().unwrap() - 0.7).abs() < 1e-9);
            }
            other => panic!("expected set.sensitivity, got {other:?}"),
        }
        match vision_command("set sensitivity to 0.3") {
            Some(VisionCommand::Op(line)) => {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert!((v["value"].as_f64().unwrap() - 0.3).abs() < 1e-9);
            }
            other => panic!("expected set.sensitivity, got {other:?}"),
        }
        // Word form clamps into range.
        match vision_command("set sensitivity to high") {
            Some(VisionCommand::Op(line)) => {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                let val = v["value"].as_f64().unwrap();
                assert!((0.0..=1.0).contains(&val) && val > 0.5);
            }
            other => panic!("expected set.sensitivity, got {other:?}"),
        }
    }

    /// An EXPLICIT sensitivity value wins over the verb that carries a word form.
    ///
    /// WHAT WENT WRONG: `extract_sensitivity` ran its word arms first and matched
    /// with `contains`, so "lower" satisfied `contains("low")` and returned 0.25
    /// before the number was ever parsed. "lower the sensitivity to 0.1" wrote
    /// 0.25 — 2.5x the requested threshold, and in the OPPOSITE direction from the
    /// request — on the one state-mutating op in the Vision seam, with an
    /// acknowledgment that never names the value, so the user could not notice.
    #[test]
    fn a_spoken_sensitivity_number_beats_the_verbs_own_word_form() {
        let value_of = |text: &str| -> f64 {
            match vision_command(text) {
                Some(VisionCommand::Op(line)) => {
                    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(v["op"], "set.sensitivity", "for {text:?}");
                    v["value"].as_f64().unwrap()
                }
                other => panic!("expected set.sensitivity for {text:?}, got {other:?}"),
            }
        };
        for (text, want) in [
            ("lower the sensitivity to 0.1", 0.1),
            ("lower the sensitivity to 30 percent", 0.3),
            ("raise the sensitivity to 0.9", 0.9),
            ("set the sensitivity to 0.3", 0.3),
        ] {
            let got = value_of(text);
            assert!(
                (got - want).abs() < 1e-9,
                "{text:?} must write {want}, wrote {got}"
            );
        }
        // No number: the word form still supplies the value, both plain and
        // comparative ("set the sensitivity higher" is a supported phrasing).
        assert!((value_of("set the sensitivity to low") - 0.25).abs() < 1e-9);
        assert!((value_of("set the sensitivity higher") - 0.85).abs() < 1e-9);
        assert!((value_of("set the sensitivity to medium") - 0.5).abs() < 1e-9);

        // The extractor's own three passes, exercised directly (the gate above
        // does not admit every phrasing the extractor must still handle):
        //   pass 1 — a connector-introduced number beats a word form anywhere;
        //   pass 2 — whole-word forms, so "below"/"allow"/"slow" supply nothing;
        //   pass 3 — the connector-free number, unchanged from before.
        assert_eq!(extract_sensitivity("lower it to 0.1"), Some(0.1));
        assert_eq!(extract_sensitivity("set it to high on camera 2"), Some(0.85));
        assert_eq!(extract_sensitivity("sensitivity 0.4"), Some(0.4));
        assert_eq!(extract_sensitivity("keep it below the alarm"), None);
        assert_eq!(extract_sensitivity("allow the slow flowchart"), None);
        assert_eq!(extract_sensitivity("to 70 percent"), Some(0.7));
    }

    /// "what's on my screen" / "read my screen" / "read this" -> the read.screen
    /// OCR op, on-wire byte-identical to the FROZEN default the Swift
    /// testFrozenOpWireNamesUnchanged pins ({"type":"op","op":"read.screen"}, no
    /// explicit source — the default .screen). A plain read carries NO query.
    #[test]
    fn vision_read_screen_maps_to_read_screen_op() {
        let read = r#"{"type":"op","op":"read.screen"}"#;
        for text in [
            "what's on my screen",
            "what is on my screen",
            "what's on screen right now",
            "read my screen",
            "read the screen",
            "read what's on my screen",
            "darwin, read this",
            "read that for me",
        ] {
            assert_vision_op(text, read);
            // The default read carries no query field and no source field — the
            // FROZEN default op shape, unchanged.
            if let Some(VisionCommand::Op(line)) = vision_command(text) {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert!(v.get("query").is_none(), "plain read carries no query: {text:?}");
                assert!(v.get("source").is_none(), "default read omits source: {text:?}");
                assert_eq!(v["op"], "read.screen");
            } else {
                panic!("expected a read.screen op for {text:?}");
            }
        }
    }

    /// "read that back to me" is the REPEAT-WHAT-I-SAID idiom, NOT a screen OCR.
    ///
    /// WHAT WENT WRONG: `reads_this_or_that`'s own doc block listed "can you read
    /// that back to me" among the sentences its two locks had closed — and it had
    /// not. The modal "can" lets `vision_verb_in_command_position` forgive the
    /// bare subject "you", and "back" sat in OK_TAIL, so both locks passed. The
    /// user dictates something, asks DARWIN to read it back, and gets a
    /// whole-screen ScreenCaptureKit capture plus "Reading your screen now, sir…"
    /// — and, because the vision arm captured the turn, no answer to the actual
    /// request.
    #[test]
    fn read_that_back_to_me_is_the_repeat_idiom_not_a_screen_capture() {
        for text in [
            "can you read that back to me",
            "could you read that back to me",
            "read that back to me please",
            "read this back",
            "read that back",
        ] {
            assert!(
                !is_screen_read(text),
                "{text:?} must not be treated as a screen read"
            );
            assert_eq!(
                vision_command(text),
                None,
                "{text:?} must not become a Vision op"
            );
        }
        // PRECONDITION + narrowness: the genuine bare/directional forms still
        // route, so this fix did not close the seam it was protecting.
        for text in ["read this", "read that for me", "read this out", "read that to me"] {
            assert!(is_screen_read(text), "{text:?} must still be a screen read");
        }
    }

    /// "where's the <X> button" / "find the <X> button" / "locate the <X>" -> a
    /// read.screen op carrying the control phrase as `query`. READ-ONLY: this
    /// LOCATES a control (the app returns its box/center); the daemon never emits
    /// a click op — there is no click op anywhere in the contract.
    #[test]
    fn vision_where_is_a_control_maps_to_read_screen_with_query() {
        let cases = [
            ("where's the submit button", "submit"),
            ("where is the sign in button", "sign in"),
            ("find the save button", "save"),
            ("locate the settings icon", "settings"),
            ("where is the search field", "search"),
        ];
        for (text, want_query) in cases {
            match vision_command(text) {
                Some(VisionCommand::Op(line)) => {
                    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(v["op"], "read.screen", "for {text:?}");
                    assert_eq!(v["query"], want_query, "for {text:?}");
                    // READ-ONLY: never a click/actuate field.
                    assert!(v.get("click").is_none(), "where-is must never click: {text:?}");
                    assert!(v.get("tap").is_none());
                    assert!(v.get("actuate").is_none());
                }
                other => panic!("expected a read.screen query op for {text:?}, got {other:?}"),
            }
        }
    }

    /// A continuous "watch the screen" is STILL a watch.start (not an OCR read):
    /// the watch lifecycle is matched before the screen-read seam, so the two
    /// never collide.
    #[test]
    fn vision_watch_the_screen_is_not_a_screen_read() {
        assert_vision_op(
            "watch the screen",
            r#"{"type":"op","op":"watch.start","source":"screen"}"#,
        );
        // And a screen-read phrase never collides with the watch op.
        assert!(!is_screen_read("watch the screen"));
        assert!(is_screen_read("read my screen"));
    }

    /// PRIVACY PIN: `is_screen_read` agrees with the routing — anything that maps
    /// to a read.screen op is flagged transient, and nothing else is. main.rs
    /// gates fact extraction on this, so a screen read can never seed a durable
    /// fact / optimizer trace. The recognized text itself never reaches this path
    /// (it rides the vision.screen telemetry event); this pins the UTTERANCE +
    /// acknowledgment out of persistence too.
    #[test]
    fn screen_read_utterances_are_flagged_transient_and_others_are_not() {
        for text in [
            "what's on my screen",
            "read my screen",
            "read this",
            "where's the submit button",
            "find the save button",
        ] {
            assert!(is_screen_read(text), "{text:?} must be flagged transient");
            // Consistency: a transient utterance is exactly a read.screen op.
            match vision_command(text) {
                Some(VisionCommand::Op(line)) => {
                    assert!(line.contains("read.screen"), "{text:?} -> read.screen");
                }
                other => panic!("expected a read.screen op for {text:?}, got {other:?}"),
            }
        }
        // NON screen-read turns are NOT transient (they learn normally).
        for text in [
            "what do you see",          // presence status, not OCR
            "watch the screen",         // continuous watch, not OCR
            "remember my birthday is may third",
            "open vision",
            "what's the weather",
        ] {
            assert!(!is_screen_read(text), "{text:?} must NOT be transient");
        }
    }

    // ----- #28 HANDWRITING read / #29 DOCUMENT scan ----------------------------

    /// "read this handwriting" / "read the whiteboard" -> the read.handwriting op
    /// (#28). The default source is .camera (the line omits `source`, mirroring the
    /// Swift Op.swift default). An explicit "on screen" stamps the screen source.
    #[test]
    fn vision_read_handwriting_maps_to_read_handwriting_op() {
        for text in [
            "read this handwriting",
            "read the handwritten note",
            "read the whiteboard",
            "transcribe the whiteboard",
            "what does this handwriting say",
            "what's written on the whiteboard",
        ] {
            match vision_command(text) {
                Some(VisionCommand::Op(line)) => {
                    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(v["op"], "read.handwriting", "for {text:?}");
                    // Default source omitted -> the app's .camera default.
                    assert!(v.get("source").is_none(), "default handwriting read omits source: {text:?}");
                    // READ-ONLY: never a click/actuate field.
                    assert!(v.get("click").is_none() && v.get("actuate").is_none());
                }
                other => panic!("expected a read.handwriting op for {text:?}, got {other:?}"),
            }
        }
        // An explicit "on screen" handwriting read stamps the screen source.
        match vision_command("read the handwriting on screen") {
            Some(VisionCommand::Op(line)) => {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(v["op"], "read.handwriting");
                assert_eq!(v["source"], "screen");
            }
            other => panic!("expected a read.handwriting screen op, got {other:?}"),
        }
    }

    /// "scan this document" / "scan the page" / "scan this receipt" -> the
    /// scan.document op (#29). Default source .camera (omitted); "on screen" stamps
    /// the screen source. READ-ONLY: never a click/actuate field.
    #[test]
    fn vision_scan_document_maps_to_scan_document_op() {
        for text in [
            "scan this document",
            "scan the page",
            "scan this receipt",
            "scan the paper",
            "scan this form",
            "scan the invoice",
        ] {
            match vision_command(text) {
                Some(VisionCommand::Op(line)) => {
                    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(v["op"], "scan.document", "for {text:?}");
                    assert!(v.get("source").is_none(), "default scan omits source: {text:?}");
                    assert!(v.get("click").is_none() && v.get("actuate").is_none());
                }
                other => panic!("expected a scan.document op for {text:?}, got {other:?}"),
            }
        }
        // "scan the document on screen" stamps the screen source.
        match vision_command("scan the document on screen") {
            Some(VisionCommand::Op(line)) => {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(v["op"], "scan.document");
                assert_eq!(v["source"], "screen");
            }
            other => panic!("expected a scan.document screen op, got {other:?}"),
        }
    }

    /// DISTINCTNESS: handwriting (#28), document scan (#29), and the plain on-
    /// screen OCR read are three separate intents that never collide. A
    /// handwriting/document phrase must NOT fall into the generic read.screen op,
    /// and "read my screen" must NOT become a handwriting/scan op.
    #[test]
    fn handwriting_scan_and_plain_screen_read_are_distinct() {
        // Handwriting -> read.handwriting (NOT read.screen).
        match vision_command("read this handwriting") {
            Some(VisionCommand::Op(line)) => {
                assert!(line.contains("read.handwriting"));
                assert!(!line.contains("read.screen"), "handwriting must not be a plain screen read");
            }
            other => panic!("got {other:?}"),
        }
        // Scan -> scan.document (NOT read.screen).
        match vision_command("scan this document") {
            Some(VisionCommand::Op(line)) => {
                assert!(line.contains("scan.document"));
                assert!(!line.contains("read.screen"));
            }
            other => panic!("got {other:?}"),
        }
        // Plain on-screen OCR stays read.screen (NOT handwriting/scan).
        match vision_command("read my screen") {
            Some(VisionCommand::Op(line)) => {
                assert!(line.contains("read.screen"));
                assert!(!line.contains("read.handwriting") && !line.contains("scan.document"));
            }
            other => panic!("got {other:?}"),
        }
    }

    /// PRIVACY PIN: a handwriting read (#28) and a document scan (#29) BOTH surface
    /// sensitive recognized text (a handwritten note / a scanned page can carry
    /// private content), so both are flagged TRANSIENT — consistent with the
    /// routing (anything mapping to read.handwriting/scan.document is transient).
    #[test]
    fn handwriting_and_scan_utterances_are_flagged_transient() {
        for text in [
            "read this handwriting",
            "read the whiteboard",
            "scan this document",
            "scan the receipt",
        ] {
            assert!(is_screen_read(text), "{text:?} must be flagged transient (sensitive recognized text)");
            match vision_command(text) {
                Some(VisionCommand::Op(line)) => {
                    assert!(
                        line.contains("read.handwriting") || line.contains("scan.document"),
                        "{text:?} -> a handwriting/scan op"
                    );
                }
                other => panic!("expected a handwriting/scan op for {text:?}, got {other:?}"),
            }
        }
    }

    // ----- VLM DESCRIBE (task #2) — DISTINCT from the OCR read.screen path -----

    /// "describe my screen" / "what am I looking at" route to a VLM SCREEN
    /// describe — DISTINCT from the OCR read.screen path. The describe verb maps
    /// to DescribeRequest::Screen, never to a read.screen op. A BARE describe
    /// (no specific question) carries `question: None` (a generic caption).
    #[test]
    fn describe_screen_phrases_map_to_a_screen_describe() {
        for text in [
            "describe my screen",
            "describe what's on my screen",
            "what am I looking at",
            "what do you make of my screen",
            "describe the display",
        ] {
            assert_eq!(
                describe_command(text),
                Some(DescribeRequest::Screen { question: None }),
                "{text:?} must be a generic VLM screen describe (no specific question)"
            );
        }
    }

    /// VQA (task #2, build 2/2): a SPECIFIC visual question about the screen is
    /// threaded to the VLM as `question`, so the model answers THAT rather than
    /// emitting a generic caption. Two routes: the explicit "ask my screen …"
    /// trigger, and a describe verb carrying a substantive question.
    #[test]
    fn screen_vqa_threads_the_specific_question() {
        // Explicit "ask (about) my/the screen <q>" — the prefix is stripped.
        assert_eq!(
            describe_command("ask my screen which button rebuilds"),
            Some(DescribeRequest::Screen { question: Some("which button rebuilds".to_string()) })
        );
        assert_eq!(
            describe_command("ask about my screen: what is the error?"),
            Some(DescribeRequest::Screen { question: Some("what is the error?".to_string()) })
        );
        // A describe verb PLUS a substantive question -> the whole utterance is the
        // VQA prompt (the VLM reads the intent from the user's own words).
        assert_eq!(
            describe_command("describe my screen, is there a build error?"),
            Some(DescribeRequest::Screen {
                question: Some("describe my screen, is there a build error?".to_string())
            })
        );
        // "ask <a person> about the screen" is NOT a screen VQA (it does not begin
        // with an "ask <the screen>" prefix) — a message-a-contact intent is never
        // poached into the VLM.
        assert_eq!(describe_command("ask sarah about the screen resolution"), None);
    }

    /// "describe this image <path>" / "what's in <path>" route to a VLM IMAGE
    /// describe carrying the RAW candidate path (confined later by the handler).
    /// A bare describe carries `question: None`; a specific question is threaded.
    #[test]
    fn describe_image_phrases_carry_the_named_path() {
        assert_eq!(
            describe_command("describe this image /Users/me/pics/cat.png"),
            Some(DescribeRequest::Image {
                path: "/Users/me/pics/cat.png".to_string(),
                question: None
            })
        );
        assert_eq!(
            describe_command("what's in photo.jpg"),
            Some(DescribeRequest::Image { path: "photo.jpg".to_string(), question: None })
        );
        // Case of the path survives (file systems are case-sensitive).
        assert_eq!(
            describe_command("describe the picture MyPhoto.JPEG"),
            Some(DescribeRequest::Image { path: "MyPhoto.JPEG".to_string(), question: None })
        );
        // A specific question about the file -> threaded as VQA, with the path
        // token stripped out of the prompt (a file path never leaks to the VLM).
        assert_eq!(
            describe_command("describe cat.png — is the dog asleep?"),
            Some(DescribeRequest::Image {
                path: "cat.png".to_string(),
                question: Some("describe — is the dog asleep?".to_string())
            })
        );
        // The extractor finds an image extension token, nothing else.
        assert_eq!(extract_image_path("describe /tmp/a.png now"), Some("/tmp/a.png".to_string()));
        assert_eq!(extract_image_path("describe this image"), None);
    }

    /// vqa_question is PURE: a remnant made only of describe/scaffolding vocab is a
    /// generic caption (None); any substantive token makes it a specific question.
    #[test]
    fn vqa_question_distinguishes_generic_from_specific() {
        // Generic describe scaffolding -> None (the op uses its default prompt).
        for generic in ["describe my screen", "what am i looking at", "describe it", "describe the display"] {
            assert_eq!(vqa_question(generic, None), None, "{generic:?} is a generic caption");
        }
        // Substantive question -> Some(verbatim).
        assert_eq!(
            vqa_question("what's the error on my screen", None),
            Some("what's the error on my screen".to_string())
        );
        // Path is stripped before the generic/specific decision AND out of the
        // returned prompt (a file path never leaks to the VLM).
        assert_eq!(vqa_question("describe this image cat.png", Some("cat.png")), None);
        assert_eq!(
            vqa_question("what breed is the dog in cat.png", Some("cat.png")),
            Some("what breed is the dog in".to_string())
        );
    }

    /// PANIC PIN (no-regression): vqa_question / describe_command must NEVER panic
    /// on an offset-shifting-lowercase utterance — a char like `İ` whose lowercase
    /// is a different byte length — that also names an image path. The path-strip
    /// must not index a byte offset derived from a lowercased copy onto the
    /// original text (that lands mid-char and panics replace_range). Mirrors the
    /// extract_image_prompt offset-shift panic pin.
    #[test]
    fn vqa_and_describe_never_panic_on_offset_shifting_lowercase() {
        for text in [
            "İ describe a.png",
            "describe İcafé.png what İis on it",
            "İİİ what is in /tmp/İ.png please",
            "ẞ describe photo.PNG İ",
            "what İs in \u{0130}\u{0130}.jpeg",
            "ask my screen İ what İs the error",
            "ask about my display \u{0130}",
        ] {
            let _ = describe_command(text);
            let _ = vqa_question(text, extract_image_path(text).as_deref());
        }
    }

    /// CONTRACT PIN (no-regression): the OCR read.screen path is NOT poached by
    /// the VLM describe path. "read my screen" / "what's on my screen" stay OCR
    /// (a read.screen op, NOT a describe), and the describe phrases are NOT OCR.
    /// The two intents are mutually exclusive by construction.
    #[test]
    fn ocr_read_screen_and_vlm_describe_are_distinct_intents() {
        // OCR read verbs -> read.screen op, and NOT a describe request.
        for ocr in ["read my screen", "what's on my screen", "read this", "read the screen"] {
            assert!(is_screen_read(ocr), "{ocr:?} must stay an OCR read");
            assert_eq!(describe_command(ocr), None, "{ocr:?} must NOT be a VLM describe");
            match vision_command(ocr) {
                Some(VisionCommand::Op(line)) => assert!(line.contains("read.screen")),
                other => panic!("expected a read.screen op for {ocr:?}, got {other:?}"),
            }
        }
        // VLM describe verbs -> describe request, and NOT an OCR read.
        for vlm in ["describe my screen", "what am I looking at", "describe this image a.png"] {
            assert!(describe_command(vlm).is_some(), "{vlm:?} must be a VLM describe");
            assert!(!is_screen_read(vlm), "{vlm:?} must NOT be an OCR read");
        }
    }

    /// PRIVACY PIN: a VLM describe is flagged transient (it can surface sensitive
    /// VISUAL content), exactly like an OCR screen read — so main.rs keeps its
    /// utterance + acknowledgment out of lifelong memory / optimizer traces.
    #[test]
    fn describe_requests_are_flagged_transient() {
        for text in [
            "describe my screen",
            "what am I looking at",
            "describe this image cat.png",
            "ask my screen what is the error",
        ] {
            assert!(is_describe_request(text), "{text:?} must be flagged transient");
        }
        // Unrelated turns are not describe requests (they learn normally).
        for text in ["what's the weather", "open vision", "remember my birthday is may third"] {
            assert!(!is_describe_request(text), "{text:?} must NOT be a describe");
        }
    }

    /// GATE + FALLBACK (honesty-first): [vision] ships ON (full-power default) but
    /// INERT WITHOUT A MODEL (model="") — an IMAGE describe NEVER calls the VLM op and
    /// NEVER fabricates a description; it returns an honest gate line and emits the
    /// vision.describe telemetry as unavailable. Hermetic: no real model, no socket
    /// touched (the empty-model gate short-circuits before any op call), an empty app
    /// registry.
    #[tokio::test]
    async fn describe_image_gate_inert_without_model_falls_back_honestly_no_op_call() {
        let cfg = Config::default(); // [vision] enabled=true but model="" => inert
        assert!(cfg.vision.enabled, "precondition: VLM ships ON (full-power default)");
        assert!(cfg.vision.model.trim().is_empty(), "precondition: no VLM model configured (inert)");
        let registry = crate::apps::AppRegistry::discover(std::path::Path::new("/nonexistent"));
        // A lazy client pointed at a socket that does not exist; the gate path
        // must NOT reach it (proving no op is called when off).
        let mut infer = InferenceClient::new(std::path::PathBuf::from("/nonexistent/inference.sock"));
        let out = handle_describe(
            DescribeRequest::Image { path: "anything.png".to_string(), question: None },
            &cfg,
            &mut infer,
            &registry,
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(out.llm_voice, "the describe reply is persona-voiced");
        let low = out.data.to_lowercase();
        assert!(
            low.contains("on-device") && (low.contains("isn't downloaded") || low.contains("turned off") || low.contains("vision-language")),
            "off-gate copy must be honest about the on-device, not-set-up VLM: {:?}",
            out.data
        );
        // CRUCIAL: it is NOT a fabricated description (no invented scene content).
        assert!(!low.contains("i can see"), "must never fabricate a description: {:?}", out.data);
    }

    /// PATH CONFINEMENT (no escape): describe_confined_path REJECTS a path that
    /// resolves OUTSIDE the allowed root BEFORE any op call — a `..` traversal, an
    /// absolute-elsewhere path, and a nonexistent path all return an honest Err
    /// (never a description, never sent to the op). Hermetic: the reject happens
    /// before infer is ever touched. Mirrors the docsearch::confine red-team pin.
    #[tokio::test]
    async fn describe_path_confinement_rejects_escapes_before_any_op() {
        let root = std::env::temp_dir().join(format!("darwin-vlm-confine-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // A real allowed root with one real image inside it.
        let inside = root.join("ok.png");
        std::fs::write(&inside, b"\x89PNG\r\n\x1a\n").unwrap();

        let mut infer = InferenceClient::new(std::path::PathBuf::from("/nonexistent/inference.sock"));

        // 1) Absolute-elsewhere (outside the root) -> REJECTED.
        let r = describe_confined_path(
            std::path::Path::new("/etc/hosts"),
            None,
            &mut infer,
            &root,
            "image",
        )
        .await;
        assert!(r.is_err(), "an absolute-elsewhere path must be rejected");
        assert!(r.unwrap_err().to_lowercase().contains("allowed"), "honest reject reason");

        // 2) `..` traversal escaping the root -> REJECTED.
        let escape = root.join("..").join("escape.png");
        let r = describe_confined_path(&escape, None, &mut infer, &root, "image").await;
        assert!(r.is_err(), "a `..` escape must be rejected");

        // 3) A nonexistent path (cannot canonicalize) -> REJECTED.
        let r = describe_confined_path(
            &root.join("does-not-exist.png"),
            None,
            &mut infer,
            &root,
            "image",
        )
        .await;
        assert!(r.is_err(), "a nonexistent path must be rejected (never sent)");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// ROUTING PIN: the describe intent re-pins the active agent to VISION (the
    /// vision owner). The route does `agents.get(VISION_APP)`; this pins that the
    /// Vision agent exists in the canonical roster and is resolvable by that key,
    /// so a describe turn is owned by Vision (the HUD + persona track it).
    #[test]
    fn describe_routes_to_the_vision_agent() {
        let reg = AgentRegistry::canonical();
        let vision = reg.get(VISION_APP).expect("the Vision agent must be in the roster");
        assert_eq!(vision.name, "vision");
        assert_eq!(vision.namespace, "agent.vision");
        // And every describe phrase is a describe request that triggers the re-pin.
        for text in ["describe my screen", "what am I looking at", "describe this image x.png"] {
            assert!(describe_command(text).is_some(), "{text:?} drives the Vision re-pin");
        }
    }

    /// An EMPTY reindex must not be blamed on the on-device embedder.
    ///
    /// WHAT WENT WRONG: the reply had two arms — "on-device embeddings" when
    /// `embedded_chunks == chunks && chunks > 0`, else "the on-device embedder was
    /// unavailable". `reindex` returns whole-store counts, so a root with nothing
    /// indexable gives 0/0/0: `0 == 0` holds but `chunks > 0` does not, and the
    /// empty case fell into the embedder-failure arm. The user was handed a
    /// specific, false diagnosis about an embedder that was never asked, and went
    /// off to debug the inference server instead of the allowlist.
    #[test]
    fn an_empty_reindex_does_not_blame_the_embedder() {
        let empty = super::reindex_reply(0, 0, 0);
        assert!(
            !empty.contains("embedder was unavailable"),
            "an empty index is not an embedder failure: {empty}"
        );
        assert!(
            empty.contains("docsearch].roots"),
            "the honest reply must point at the allowlist: {empty}"
        );

        // The two real arms are unchanged.
        let all = super::reindex_reply(12, 340, 340);
        assert!(all.contains("on-device embeddings"), "{all}");
        let partial = super::reindex_reply(12, 340, 100);
        assert!(partial.contains("embedder was unavailable"), "{partial}");
        assert!(partial.contains("lexical BM25"), "{partial}");
    }

    // ----- IMAGE GENERATION (task #18) — on-device text->image, OFF/opt-in ----

    /// "generate/make/draw/create an image of X" maps to a GenerateImageRequest
    /// carrying the extracted PROMPT (the subject after the connector). PURE — no
    /// socket, no model, no classifier.
    #[test]
    fn generate_image_phrases_carry_the_prompt() {
        let cases = [
            ("generate an image of a red bicycle", "a red bicycle"),
            ("make a picture of an astronaut riding a horse", "an astronaut riding a horse"),
            ("draw a drawing of a cat in a hat", "a cat in a hat"),
            ("create an illustration showing a sunset over mountains", "a sunset over mountains"),
            ("paint a painting depicting a stormy sea", "a stormy sea"),
        ];
        for (text, want_prompt) in cases {
            let req = generate_image_command(text)
                .unwrap_or_else(|| panic!("{text:?} must be an image-generation request"));
            assert_eq!(req.prompt, want_prompt, "prompt extraction for {text:?}");
        }
        // The subject extractor keeps the full tail (an "of X with Y" stays whole).
        assert_eq!(
            extract_image_prompt("draw a picture of a dog with a hat").as_deref(),
            Some("a dog with a hat")
        );

        // WHAT WENT WRONG: the extractor scanned the WHOLE utterance for the
        // earliest connector, while the caller's comment promised "the first such
        // connector AFTER an image noun". A connector before the image noun
        // therefore captured the prompt: "instead of a photo, make a drawing of
        // the house" handed the diffusion model "a photo, make a drawing of the
        // house" and the user got an image of something they never asked for.
        assert_eq!(
            extract_image_prompt("instead of a photo, make a drawing of the house").as_deref(),
            Some("the house")
        );
        assert_eq!(
            generate_image_command("instead of a photo, make a drawing of the house")
                .expect("still an image request")
                .prompt,
            "the house"
        );
        // With NO image noun before any connector the behaviour is unchanged.
        assert_eq!(
            extract_image_prompt("draw a picture of a red bicycle").as_deref(),
            Some("a red bicycle")
        );
    }

    /// BOTH CLOUD EARLY-RETURNS ARE UNCONDITIONAL, AND THAT IS LOAD-BEARING.
    ///
    /// A hoist of the four non-capture app gates — suppressing these two returns
    /// for the turns silicon / nexus / markforge / genimage claim — was built,
    /// measured and REFUSED: it gives those gates FIRST REFUSAL on everything
    /// that reaches cloud conversation, and six of their branches are not precise
    /// enough to be first (the block above `needs_deep_reasoning` in `route()`
    /// names each branch and the ordinary sentence it swallows).
    ///
    /// Nothing else can catch a re-land. `route()` is async over a Memory, an
    /// InferenceClient and an AppRegistry, so no unit test reaches these two
    /// lines; adding one `&& !hoisted` to either would make 48 measured ordinary
    /// sentences actuate an app on the shipped config while every other test in
    /// this file stayed green. So this reads the source, with the two rules that
    /// class of guard needs:
    ///   * BOUNDED AT BOTH ENDS — from the refusal block to the local seam's
    ///     first classifier call, so it cannot reach this test module and
    ///     SELF-MATCH on the needles written right here;
    ///   * `expect` on every needle, never `unwrap_or`, so a moved anchor is a
    ///     loud failure and not a silently-true assertion.
    ///
    /// If you are re-landing the hoist: harden the six branches first, re-prove
    /// each against sentences written for the NEW rule, then change this guard
    /// deliberately — it is the record that the work was not skipped.
    #[test]
    fn both_cloud_returns_are_unconditional() {
        let src = include_str!("router.rs");
        let start = src
            .find("// A HOIST OF THE FOUR NON-CAPTURE APP GATES WAS MEASURED HERE AND REFUSED.")
            .expect("route()'s hoist-refusal record is gone; re-point this guard");
        let rest = &src[start..];
        let end = rest
            .find("let describe = describe_command(text);")
            .expect("route()'s local app seam moved; re-point this guard");
        let window = &rest[..end];
        assert!(
            !window.contains("fn both_cloud_returns_are_unconditional"),
            "the window swallowed this test — it would pass on its own source"
        );
        let tool_loop = window
            .find("let actuating_cloud = to_cloud && !is_uncertain_fallback(class, cfg);")
            .expect(
                "the cloud TOOL LOOP gate is no longer exactly \
                 `to_cloud && !is_uncertain_fallback(class, cfg)` — an app gate has \
                 been given first refusal over it. Harden the six branches named \
                 above `needs_deep_reasoning` before allowing that.",
            );
        let conversation = window
            .find("if class.intent == \"conversation\" {")
            .expect(
                "the CONVERSATION branch is no longer entered unconditionally for a \
                 conversation intent — an app gate has been given first refusal \
                 over it, which is the measured hijack this guard exists to catch",
            );
        assert!(
            tool_loop < conversation,
            "the two cloud returns are out of order; re-read the seam before \
             trusting this guard"
        );
    }

    /// PRECISION PIN: ordinary speech must never reach the on-device diffusion
    /// model.
    ///
    /// The gate used to be two independent `contains` scans (any generate verb as
    /// a SUBSTRING anywhere, any image noun as a SUBSTRING anywhere) with nothing
    /// tying them together. Every sentence below satisfied both halves and
    /// rendered a picture instead of answering. They were invisible because
    /// `route()` consults this gate only BELOW its two cloud early-returns, so on
    /// the shipped cloud config the conversation branch answers them first — the
    /// precision hole is real the whole time on the offline / vault / guest path,
    /// and costing it out for a HOIST is what surfaced it. The hoist itself was
    /// measured and refused (see the block above `needs_deep_reasoning`); these
    /// sentences are a live defect either way, which is why the fix landed.
    #[test]
    fn ordinary_speech_never_generates_an_image() {
        for u in [
            // substring verb / substring noun
            "the photosynthesis chapter makes more sense with the diagram of the leaf",
            "art therapy makes a difference with kids who have trouble talking",
            "remake that photo album of the wedding with the newer prints",
            // narration: an inflected verb is somebody ELSE drawing
            "my daughter created a drawing of a dinosaur with a crayon",
            "that painting of the harbor is my favourite thing in the house",
            "she makes a picture of health with all that hiking",
            "he drew a picture of what the neighbourhood used to look like",
            "the painter rendered a portrait of my grandmother with oils",
            "he painted a picture of a company that was already failing",
            "the article draws a picture of a city with no water left",
            "I painted the picture of the room with a roller and it took forever",
            "the drawing of the boundaries with the neighbours got ugly",
            // the noun is not the verb's object
            "the big picture of this quarter is that we make less with more effort",
            "make an effort with the picture of professionalism you project",
            // These two specifically exercise the OBJECT relation's near edge: a
            // content word sits between the verb and the image noun, INSIDE the
            // two-determiner window. Without the "one content word ends it" rule
            // they both render a picture.
            "make sure the picture of the receipt is clear",
            "draw up the drawing of the extension with the builder",
            // "paint a picture of <clause>" is the English idiom for EXPLAIN
            "let us paint a picture of what next year looks like with the new budget",
            "paint me a picture of how the meeting actually went",
            "can you draw me a picture of why that matters",
            "try to create a picture of what the customer actually wants",
            "make a picture of how bad it got and you will understand",
            // "imagine" is not a render verb
            "I cannot imagine the pressure of taking a photo with a broken lens",
            "imagine the artwork of a whole generation with no galleries left",
        ] {
            assert!(
                generate_image_command(u).is_none(),
                "{u:?} is ordinary speech and must not generate an image: {:?}",
                generate_image_command(u)
            );
        }
        // ...and every real request still works, prompt intact.
        for (u, want) in [
            ("draw me a picture of a lighthouse", "a lighthouse"),
            ("generate an image of a red bicycle", "a red bicycle"),
            ("make me a picture of a mountain at sunset", "a mountain at sunset"),
            ("create an illustration of a fox in the snow", "a fox in the snow"),
            ("sketch a drawing of a sailboat", "a sailboat"),
            ("paint a painting depicting a stormy sea", "a stormy sea"),
        ] {
            let req = generate_image_command(u)
                .unwrap_or_else(|| panic!("{u:?} is a real image request and must still work"));
            assert_eq!(req.prompt, want, "prompt for {u:?}");
        }
    }

    /// PANIC PIN (no-regression): extract_image_prompt must never panic on an STT
    /// transcript whose lowercase form is NOT byte-length-preserving. The dotted
    /// capital 'İ' (U+0130, 2 bytes) lowercases to "i̇" (3 bytes), so the old
    /// `lower.find()` byte offset was wrong for the ORIGINAL `text` and slicing it
    /// landed mid-codepoint or past the end — panicking the always-on daemon
    /// (transcripts are untrusted multilingual input awaited inline in main's
    /// event loop). The fix scans `text`'s own char boundaries, so `start` is
    /// always valid IN `text`. These inputs reproduced the pre-fix panic.
    #[test]
    fn extract_image_prompt_never_panics_on_offset_shifting_lowercase() {
        // Each call must return (Some/None) without panicking on a char boundary.
        // 'İ' before/around the matched connector is the offset-shift trigger.
        for text in [
            "draw İ a photo of İcat",
            "İ art of 🐱",
            "İİİ picture of x",
            "draw a picture İ of İ a cat",
            "İ",                  // lone offset-shifter, no connector
            "İ of İ",             // connector flanked by shifters
        ] {
            // Just exercising the extractor — the contract is "no panic".
            let _ = extract_image_prompt(text);
            // And the public entry point that flows from the live transcript.
            let _ = generate_image_command(text);
            let _ = is_generate_image_request(text);
        }
        // Subject after the connector survives intact even with a leading 'İ'.
        assert_eq!(
            extract_image_prompt("draw İ a photo of İcat").as_deref(),
            Some("İcat"),
            "tail after the connector is preserved (original case + multibyte char)"
        );
        // No connector -> None (no guess), even when the only content is a shifter.
        assert_eq!(extract_image_prompt("İ").as_deref(), None);
    }

    /// CONTRACT PIN (no-regression): image GENERATION and VLM DESCRIBE are DISTINCT
    /// intents and never poach each other. A describe verb ("describe", "what's
    /// in") is NEVER an image-generation request; a generate verb ("draw an image
    /// of X") is NEVER a describe request. Mutually exclusive by construction. Also:
    /// a non-image "make me a sandwich" is NOT image generation (needs an image
    /// noun), and a bare "generate an image" with no subject yields no prompt.
    #[test]
    fn generate_image_and_describe_are_distinct_and_well_scoped() {
        // Describe verbs -> describe, NOT generate.
        for d in ["describe my screen", "what am I looking at", "describe this image cat.png", "what's in photo.jpg"] {
            assert!(describe_command(d).is_some(), "{d:?} must stay a VLM describe");
            assert!(generate_image_command(d).is_none(), "{d:?} must NOT be image generation");
        }
        // Generate verbs -> generate, NOT describe.
        for g in ["generate an image of a dog", "draw a picture of a house", "make an illustration of a robot"] {
            assert!(generate_image_command(g).is_some(), "{g:?} must be image generation");
            assert!(describe_command(g).is_none(), "{g:?} must NOT be a VLM describe");
        }
        // A non-image "make" request needs an image NOUN — never poached.
        for not_img in ["make me a sandwich", "draw the curtains", "what's the weather", "open vision"] {
            assert!(generate_image_command(not_img).is_none(), "{not_img:?} must NOT be image generation");
        }
        // A bare generate with no subject -> no prompt -> not a request (no guess).
        assert!(generate_image_command("generate an image").is_none(), "no subject => no prompt");
    }

    /// PRIVACY PIN: an image-generation turn is flagged transient (its prompt +
    /// the generated image can be personal, and both stay on-device) — so main.rs
    /// keeps its utterance + acknowledgment out of lifelong memory / optimizer
    /// traces, exactly like the VLM describe / OCR reads.
    #[test]
    fn generate_image_requests_are_flagged_transient() {
        for text in ["generate an image of a dog", "draw a picture of my house", "make an illustration of a robot"] {
            assert!(is_generate_image_request(text), "{text:?} must be flagged transient");
        }
        for text in ["what's the weather", "describe my screen", "remember my birthday is may third"] {
            assert!(!is_generate_image_request(text), "{text:?} must NOT be an image-generation turn");
        }
    }

    /// GATE + FALLBACK (honesty-first): [image] ships ON (full-power default) but
    /// INERT WITHOUT A MODEL (model=""), an image-generation request NEVER calls the
    /// generate_image op and NEVER fabricates an image — it returns an honest "not set
    /// up" line and emits the image.generated telemetry as unavailable. CRUCIALLY there
    /// is NO cloud fallback. Hermetic: no real model, no socket touched (the
    /// empty-model gate short-circuits before any op call) — the client points at a
    /// nonexistent socket to prove no op is reached.
    #[tokio::test]
    async fn generate_image_gate_inert_without_model_reports_honestly_no_op_no_cloud() {
        let cfg = Config::default(); // [image] enabled=true but model="" => inert
        assert!(cfg.image.enabled, "precondition: image generation ships ON (full-power default)");
        assert!(cfg.image.model.trim().is_empty(), "precondition: no image model configured (inert)");
        // A lazy client pointed at a socket that does not exist; the gate path must
        // NOT reach it (proving no op is called when off).
        let mut infer = InferenceClient::new(std::path::PathBuf::from("/nonexistent/inference.sock"));
        let out = handle_generate_image(
            GenerateImageRequest { prompt: "a red bicycle".to_string() },
            &cfg,
            &mut infer,
        )
        .await;
        assert!(out.llm_voice, "the image reply is persona-voiced");
        let low = out.data.to_lowercase();
        assert!(
            low.contains("on-device") && (low.contains("isn't set up") || low.contains("turned off") || low.contains("image model")),
            "off-gate copy must be honest about the on-device, not-set-up image model: {:?}",
            out.data
        );
        // CRUCIAL: it never fabricates an image and never mentions a cloud fallback.
        assert!(!low.contains("here is"), "must never claim a fabricated image: {:?}", out.data);
        assert!(
            low.contains("won't") || low.contains("cloud") || low.contains("on-device"),
            "must be honest there is no cloud fallback: {:?}",
            out.data
        );
    }

    /// ROUTING PIN: the image-generation intent re-pins the active agent to VISION
    /// (the visual-capability owner, same as describe). The route does
    /// `agents.get(VISION_APP)`; this pins that the Vision agent is resolvable by
    /// that key, so an image-generation turn is owned by Vision (the HUD + persona
    /// track it).
    #[test]
    fn generate_image_routes_to_the_vision_agent() {
        let reg = AgentRegistry::canonical();
        let vision = reg.get(VISION_APP).expect("the Vision agent must be in the roster");
        assert_eq!(vision.name, "vision");
        for text in ["generate an image of a dog", "draw a picture of a house"] {
            assert!(generate_image_command(text).is_some(), "{text:?} drives the Vision re-pin");
        }
    }

    // ----- AUDIO SCENE UNDERSTANDING (task #15) ------------------------------

    /// The "identify this sound" intent recognizes the sound-scene phrasings and
    /// is DISTINCT from STT (speech transcription): a "what did X say" / transcribe
    /// phrasing must NEVER be read as a sound-identify request (it falls through to
    /// the speech path). PURE — no socket, no mic, no app.
    #[test]
    fn identify_sound_intent_recognizes_sound_queries_and_is_distinct_from_stt() {
        // SOUND-scene queries -> identify-sound.
        for q in [
            "what was that sound",
            "what was that noise",
            "what's that sound",
            "identify that sound",
            "name that sound",
            "what am I hearing",
            "what do you hear",
            "what kind of sound was that",
        ] {
            assert!(is_identify_sound_request(q), "should be a sound-identify: {q:?}");
        }
        // STT / speech-transcription phrasings -> NOT identify-sound (stay distinct).
        for q in [
            "what did I say",
            "what did he say",
            "what did she say",
            "transcribe that",
            "transcribe what I said",
            "what were the words",
            "what did you hear me say",
        ] {
            assert!(
                !is_identify_sound_request(q),
                "a speech/transcription phrasing must NOT be a sound-identify (STT stays distinct): {q:?}"
            );
        }
        // Plain/unrelated turns are not sound-identify requests.
        for q in ["what's the weather", "open vision", "play some music", "what time is it"] {
            assert!(!is_identify_sound_request(q), "{q:?} must NOT be a sound-identify");
        }
    }

    /// The intent supplies the clip the daemon ALREADY captured (caller-provided),
    /// NEVER a user-named path and NEVER a fresh capture. When the intent fires but
    /// there is no clip, it STILL routes (clip=None) so the handler answers
    /// honestly — it does not fall through to a generic answer or open the mic.
    #[test]
    fn identify_sound_uses_the_already_captured_clip_never_opens_the_mic() {
        let clip = std::path::Path::new("/tmp/darwin/state/tmp/sound-clip.wav");
        // Intent + a captured clip available -> route with that exact clip.
        let req = identify_sound_clip_or_request("what was that sound", Some(clip))
            .expect("a sound-identify with a clip routes");
        assert_eq!(req.clip.as_deref(), Some(clip), "the daemon's captured clip is supplied verbatim");

        // Intent fires but NO clip captured -> still routes, with clip=None (the
        // handler reports it honestly; the mic is never opened to make one).
        let req = identify_sound_clip_or_request("identify that noise", None)
            .expect("a sound-identify still routes with no clip");
        assert_eq!(req.clip, None, "no clip => clip:None, never a fabricated/fresh capture");

        // Not a sound-identify -> None (falls through to normal routing) regardless
        // of whether a clip exists.
        assert!(identify_sound_clip_or_request("what's the weather", Some(clip)).is_none());
        assert!(identify_sound_clip_or_request("what did I say", Some(clip)).is_none());
    }

    /// The classify.sound op line is EXACTLY the Swift Op.swift wire form: the
    /// `{"type":"op"}` envelope, op "classify.sound", a REQUIRED `path` (mirrors
    /// describe.capture). serde_json frames it so a path with a quote can't break it.
    #[test]
    fn op_classify_sound_matches_the_swift_wire_form() {
        let line = op_classify_sound("/tmp/state/tmp/sound-clip.wav");
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid JSON op line");
        assert_eq!(v["type"], "op");
        assert_eq!(v["op"], "classify.sound");
        assert_eq!(v["path"], "/tmp/state/tmp/sound-clip.wav", "path is required + verbatim");
        // A path with a quote stays valid JSON (no framing break).
        let q = op_classify_sound("/tmp/a\"b.wav");
        let v: serde_json::Value = serde_json::from_str(&q).expect("a quote in the path can't break framing");
        assert_eq!(v["path"], "/tmp/a\"b.wav");
    }

    /// The clip path the daemon supplies for a one-shot classification is under the
    /// project state dir (the allowlisted root) — the same place the VAD/cpal
    /// capture writes its utterance WAVs — so no new microphone is opened.
    #[test]
    fn sound_clip_path_is_under_the_state_tmp_dir() {
        let p = sound_clip_path(std::path::Path::new("/srv/darwin"));
        assert_eq!(p, std::path::Path::new("/srv/darwin/state/tmp/sound-clip.wav"));
    }

    /// HERMETIC ROUTING: the identify-sound handler over a CONFINED, real clip
    /// INVOKES apps::send_op for the VISION app (it is the only call that can
    /// produce the "not running" outcome). With the Vision app registered but NOT
    /// running, the handler reaches send_op, which rejects — proving the op was
    /// dispatched to Vision (the classify.sound wire form is pinned separately by
    /// `op_classify_sound_matches_the_swift_wire_form`). No socket is bound and no
    /// child is spawned: the registry's running flag is the gate, exactly like
    /// apps.rs's `send_op_rejects_unknown_and_not_running_apps`.
    #[tokio::test]
    async fn identify_sound_handler_invokes_classify_sound_via_send_op() {
        use crate::apps::AppRegistry;

        let root = std::env::temp_dir().join(format!(
            "darwin-idsound-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        // A real clip under the allowed root so confinement PASSES (so the test
        // exercises the send_op call, not the confinement reject).
        let clip_dir = root.join("state").join("tmp");
        std::fs::create_dir_all(&clip_dir).unwrap();
        let clip = clip_dir.join("sound-clip.wav");
        std::fs::write(&clip, b"RIFF....WAVEfmt ").unwrap();

        // Register the VISION app name (the handler hard-codes VISION_APP). It is
        // discovered but NOT running — so send_op rejects with "not running",
        // proving the handler dispatched the op to Vision (never to anything else,
        // never a fabricated label). No socket bound, no child spawned.
        let app_dir = root.join("apps").join(VISION_APP);
        std::fs::create_dir_all(&app_dir).unwrap();
        let manifest = format!(
            r#"
            [app]
            name = "{VISION_APP}"
            version = "0.1.0"
            description = "hermetic test stand-in for the vision app"
            entry = "apps/{VISION_APP}/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = ["state/apps/{VISION_APP}"]
            [ui]
            surface = "panel"
            telemetry_topics = ["vision.sound"]
        "#
        );
        std::fs::write(app_dir.join("manifest.toml"), manifest).unwrap();

        let registry = AppRegistry::discover(&root);

        let out = handle_identify_sound(Some(clip.clone()), &registry, &root).await;
        assert!(out.llm_voice, "the reply is persona-voiced");
        let low = out.data.to_lowercase();
        // The op reached send_op for VISION (registered but not running) -> the
        // honest "couldn't reach Vision / open it first" copy. This is the
        // not-running send_op outcome — proof the classify.sound op was dispatched
        // to the Vision app (a confinement reject or a no-clip path would NOT say
        // "reach vision"). Never a fabricated sound class on this path either.
        assert!(
            low.contains("reach vision") || low.contains("open it first"),
            "the handler must INVOKE send_op for Vision (not-running outcome): {:?}",
            out.data
        );
        for invented in ["doorbell", "alarm", "glass", "music"] {
            assert!(!low.contains(invented), "must never fabricate a class on the transport path: {:?}", out.data);
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// HONESTY: with NO captured clip the handler says so plainly — it NEVER
    /// fabricates a sound class and NEVER opens the mic to make one. Hermetic: no
    /// running app needed (the None arm short-circuits before any op call).
    #[tokio::test]
    async fn identify_sound_with_no_clip_is_honest_never_fabricates() {
        let registry = crate::apps::AppRegistry::discover(std::path::Path::new("/nonexistent"));
        let out = handle_identify_sound(None, &registry, std::path::Path::new("/tmp")).await;
        assert!(out.llm_voice);
        let low = out.data.to_lowercase();
        assert!(
            low.contains("don't have") || low.contains("no ") || low.contains("recent sound clip"),
            "must honestly report no clip: {:?}",
            out.data
        );
        assert!(low.contains("never opens the mic") || low.contains("on-device"), "honest gate copy: {:?}", out.data);
        // CRUCIAL: never a fabricated class.
        for invented in ["doorbell", "alarm", "glass", "music", "i hear"] {
            assert!(!low.contains(invented), "must never fabricate a sound class ({invented}): {:?}", out.data);
        }
    }

    /// PATH CONFINEMENT: an identify-sound clip OUTSIDE the allowed root is REJECTED
    /// before any op — the handler refuses to classify it and never forwards a
    /// thing. Hermetic: no running app (the reject precedes the op call).
    #[tokio::test]
    async fn identify_sound_rejects_a_clip_outside_the_allowed_root() {
        let root = std::env::temp_dir().join(format!("darwin-idsound-confine-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let registry = crate::apps::AppRegistry::discover(std::path::Path::new("/nonexistent"));
        // An absolute-elsewhere clip (outside the root) -> REJECTED, never sent.
        let out = handle_identify_sound(
            Some(std::path::PathBuf::from("/etc/hosts")),
            &registry,
            &root,
        )
        .await;
        let low = out.data.to_lowercase();
        assert!(
            low.contains("allowed") || low.contains("won't classify"),
            "an out-of-root clip must be refused honestly: {:?}",
            out.data
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// MONITOR GATE: the ambient sound monitor's spawn gate is the pure
    /// `ambient_monitor_should_start(enabled)`. With the flag OFF the gate is false —
    /// the monitor never auto-starts. The shipped DEFAULT is now ON (full-power), but
    /// it is INERT WITHOUT MIC/TCC: even with the gate true the device-gated mic loop
    /// captures nothing without Microphone consent. This is the pure half of main.rs's
    /// spawn gate.
    #[test]
    fn ambient_monitor_gate_is_the_flag_and_default_is_on() {
        // Flag OFF -> the monitor must not start (the off path is intact).
        assert!(
            !ambient_monitor_should_start(false),
            "with the flag off the ambient monitor must NOT auto-start"
        );
        // The config default is now ON (full-power) — defense in depth: the gate
        // tracks the flag, and the mic loop is still TCC-gated at runtime.
        assert!(
            Config::default().audio.sound_monitor,
            "[audio].sound_monitor ships ON (full-power default; inert without mic/TCC)"
        );
        assert!(
            ambient_monitor_should_start(Config::default().audio.sound_monitor),
            "the default config arms the gate (the mic loop still needs TCC consent at runtime)"
        );
        // The flag is the only thing the pure gate reads.
        assert!(
            ambient_monitor_should_start(true),
            "an enabled flag opens the spawn gate"
        );
    }

    /// The IdentifySoundRequest is a small, comparable carrier; this pins its shape
    /// (clip Option) so the routing + handler agree on the contract.
    #[test]
    fn identify_sound_request_carries_only_the_clip() {
        let r = IdentifySoundRequest { clip: Some(std::path::PathBuf::from("/x/y.wav")) };
        assert_eq!(r.clip.unwrap().to_str(), Some("/x/y.wav"));
        let none = IdentifySoundRequest { clip: None };
        assert!(none.clip.is_none());
    }

    /// Unrelated utterances never produce a Vision command (so they fall through
    /// to normal routing) — including ones that merely share a stray keyword.
    /// A POSSESSIVE OWNER IS NOT PART OF THE TARGET.
    ///
    /// "baby's" tokenizes to baby / s / room, and the bare "s" took the modifier
    /// slot, so every "watch the baby's room" was refused. Fixing only the
    /// modifier left the OWNER in `head`, which the next word then shifted into
    /// the modifier slot — so "watch the baby's room" passed only because "baby"
    /// happens to be a target word, while "watch my son's room" and "watch the
    /// neighbor's driveway" stayed refused. Both slots have to clear.
    #[test]
    fn a_possessive_owner_does_not_block_the_target() {
        for u in [
            "watch the baby's room",
            "watch my son's room",
            "watch my daughter's room",
            "watch the kid's room",
            "watch the neighbor's driveway",
        ] {
            let got = vision_command(u).unwrap_or_else(|| panic!("{u:?} must watch"));
            let VisionCommand::Op(line) = got else { panic!("{u:?} must be an Op") };
            assert!(line.contains("watch.start"), "{u:?} -> {line}");
            assert!(line.contains("\"camera\""), "{u:?} names a camera target -> {line}");
        }
    }

    /// The room a person happens to have must not decide whether the command
    /// works. "watch the kitchen door" worked and "watch the bedroom door" did
    /// not. Same for the lens: "watch the doorbell camera" worked and "watch the
    /// doorbell cam" did not, because "cam" sat in the TAILS list and was skipped,
    /// making the modifier the head.
    #[test]
    fn room_kinds_and_short_lens_words_are_real_targets() {
        for u in [
            "watch the bedroom door",
            "watch the bathroom door",
            "watch the office door",
            "watch the kitchen door",
            "watch the doorbell cam",
            "watch the security cam",
            "watch the ring camera",
            "watch the nest camera",
        ] {
            let got = vision_command(u).unwrap_or_else(|| panic!("{u:?} must watch"));
            let VisionCommand::Op(line) = got else { panic!("{u:?} must be an Op") };
            assert!(line.contains("watch.start"), "{u:?} -> {line}");
        }
    }

    /// A leading quantifier is part of the determiner, not a closer — including
    /// the partitive "all OF the doors", which the `of` guard would otherwise
    /// refuse. And the PHRASAL continuation verbs are how people actually resume.
    #[test]
    fn quantifiers_and_phrasal_continuations_still_reach_the_camera() {
        for u in [
            "watch all the doors",
            "watch all the cameras",
            "watch all of the doors",
            "go back to watching the driveway",
            "carry on watching the front door",
            "get back to watching the front door",
        ] {
            let got = vision_command(u).unwrap_or_else(|| panic!("{u:?} must watch"));
            let VisionCommand::Op(line) = got else { panic!("{u:?} must be an Op") };
            assert!(line.contains("watch.start"), "{u:?} -> {line}");
        }
    }

    #[test]
    fn vision_command_ignores_unrelated_utterances() {
        for text in [
            "what's the weather",
            "open safari",
            "play some music",
            "what do you think about the market",
            "set a timer for ten minutes",
            "tell me a joke",
            // "watch" with no Vision sense + no Vision app mention is still a
            // watch verb; ensure a non-watch sentence doesn't trip it.
            "i'll be back in a minute",
        ] {
            assert_eq!(
                vision_command(text),
                None,
                "{text:?} must not be a Vision command"
            );
        }
    }

    /// An oversize / junk utterance is handled cleanly: vision_command returns
    /// None (no panic, no allocation blowup) so the turn falls through to normal
    /// routing — the daemon never forwards a malformed op, and the app's own
    /// Op.decode is the final total-decode backstop for anything that does
    /// reach it.
    #[test]
    fn vision_command_handles_oversize_and_junk_cleanly() {
        // A very long string with no Vision phrase -> None, no panic.
        let huge = "lorem ipsum ".repeat(5000);
        assert_eq!(vision_command(&huge), None);
        // Pure punctuation / empty -> None.
        assert_eq!(vision_command(""), None);
        assert_eq!(vision_command("??? --- ..."), None);
        // A Vision phrase buried in a huge string still resolves to a valid op
        // (and serde framing stays well-formed) rather than choking.
        let buried = format!("{huge} what do you see {huge}");
        assert_vision_op(&buried, r#"{"type":"op","op":"status"}"#);
    }

    // ======================================================================
    // Nexus voice control (SPEC §6). Mirrors the Silicon Canvas / Vision tests.
    // The wire form pinned here is the BARE `{"op":...}` object the Nexus
    // OpDispatcher (apps/nexus/main.py) reads — NOT the Vision `{"type":"op"}`
    // envelope. Each expected line matches the SPEC §5 op table and the Python
    // dispatch handlers verbatim, so a pass here proves the daemon emits ops the
    // Nexus control plane already accepts.
    // ======================================================================

    /// Assert the utterance maps to a Nexus Op carrying EXACTLY this JSON wire
    /// string (compared as parsed JSON so key order is irrelevant; the op-tag +
    /// fields are what the contract pins).
    fn assert_nexus_op(text: &str, expected_json: &str) {
        match nexus_command(text) {
            Some(NexusCommand::Op(line)) => {
                let got: serde_json::Value = serde_json::from_str(&line).unwrap();
                let want: serde_json::Value = serde_json::from_str(expected_json).unwrap();
                assert_eq!(got, want, "for utterance {text:?}");
            }
            other => panic!("expected a Nexus Op for {text:?}, got {other:?}"),
        }
    }

    /// "open/launch/start/bring up nexus" (and its capability aliases) is a
    /// LAUNCH; the app name is the manifest name the registry keys on.
    #[test]
    fn nexus_launch_phrases() {
        assert_eq!(NEXUS_APP, "nexus");
        for text in [
            "open nexus",
            "launch nexus",
            "start nexus",
            "darwin, bring up nexus",
            "bring up the routing matrix",
            "open the mixer",
        ] {
            assert_eq!(
                nexus_command(text),
                Some(NexusCommand::Launch),
                "{text:?} should be a Nexus launch"
            );
        }
        // "nexus" must be a whole word — never inside another token.
        assert!(nexus_command("open the connexus dashboard").is_none());
    }

    /// "mute the mic" -> gain.set {mute:true} on the default mic input (0),
    /// input stage; "unmute input 2" -> gain.set {mute:false} on channel 2.
    #[test]
    fn nexus_mute_maps_to_gain_set_mute() {
        assert_nexus_op(
            "mute the mic",
            r#"{"op":"gain.set","channel":0,"mute":true,"stage":"input"}"#,
        );
        assert_nexus_op(
            "mute the microphone",
            r#"{"op":"gain.set","channel":0,"mute":true,"stage":"input"}"#,
        );
        // An explicit channel overrides the mic default.
        assert_nexus_op(
            "mute input 2",
            r#"{"op":"gain.set","channel":2,"mute":true,"stage":"input"}"#,
        );
        // Unmute flips the boolean (and never reads as a fresh mute).
        assert_nexus_op(
            "unmute the mic",
            r#"{"op":"gain.set","channel":0,"mute":false,"stage":"input"}"#,
        );
        assert_nexus_op(
            "unmute input 1",
            r#"{"op":"gain.set","channel":1,"mute":false,"stage":"input"}"#,
        );

        // WHAT WENT WRONG: the mute op hard-coded `stage: "input"` while
        // NEXUS_BARE_MUTE_VOCAB deliberately admits the OUTPUT-side nouns, so
        // "mute the speakers" muted the SM7dB MICROPHONE and the speakers kept
        // playing — an unrequested mic mute reached through a legitimate command.
        // `set_output_mute` was unreachable from voice entirely.
        for u in [
            "mute the speakers",
            "mute the monitor",
            "mute the output",
            "mute the headphones",
        ] {
            assert_nexus_op(u, r#"{"op":"gain.set","channel":0,"mute":true,"stage":"output"}"#);
        }
        assert_nexus_op(
            "unmute the speakers",
            r#"{"op":"gain.set","channel":0,"mute":false,"stage":"output"}"#,
        );
        // An explicit output channel still overrides the monitor default.
        assert_nexus_op(
            "mute output 2",
            r#"{"op":"gain.set","channel":2,"mute":true,"stage":"output"}"#,
        );
    }

    /// "set input gain to -18" -> gain.set {gain_db:-18, stage:input}; an output
    /// phrasing targets the output stage; the spoken sign word is handled.
    #[test]
    fn nexus_gain_set_maps_to_gain_set_value() {
        assert_nexus_op(
            "set input gain to -18",
            r#"{"op":"gain.set","channel":0,"gain_db":-18.0,"stage":"input"}"#,
        );
        // "minus" spelled out (STT) + a dB suffix.
        assert_nexus_op(
            "set the input gain to minus 6 db",
            r#"{"op":"gain.set","channel":0,"gain_db":-6.0,"stage":"input"}"#,
        );
        // Explicit input channel.
        assert_nexus_op(
            "set the gain on input 1 to -12",
            r#"{"op":"gain.set","channel":1,"gain_db":-12.0,"stage":"input"}"#,
        );
        // Output stage (named output channel).
        assert_nexus_op(
            "set output 1 gain to -3",
            r#"{"op":"gain.set","channel":1,"gain_db":-3.0,"stage":"output"}"#,
        );
        // WHAT WENT WRONG: `mentions_output`'s doc listed "monitor" as naming the
        // output side and the body never checked it, while NEXUS_GAIN_NOUNS
        // deliberately lists "monitor"/"monitors" so the monitor side can be
        // named. "set the monitor gain to -12 db" therefore attenuated the SM7dB
        // MICROPHONE by 12 dB: the user turns down what they hear and their own
        // voice goes quiet to everyone else — the same sentence with "speaker"
        // did the right thing.
        assert_nexus_op(
            "set the monitor gain to -12 db",
            r#"{"op":"gain.set","channel":0,"gain_db":-12.0,"stage":"output"}"#,
        );
        assert_nexus_op(
            "turn the monitor gain down to -12 db",
            r#"{"op":"gain.set","channel":0,"gain_db":-12.0,"stage":"output"}"#,
        );
        // The forms that were already right must stay right.
        assert_nexus_op(
            "set the speaker gain to -12 db",
            r#"{"op":"gain.set","channel":0,"gain_db":-12.0,"stage":"output"}"#,
        );
        assert_nexus_op(
            "set the headphone gain to -6 db",
            r#"{"op":"gain.set","channel":0,"gain_db":-6.0,"stage":"output"}"#,
        );
        // "the gain" with no number is NOT a gain.set (no dB value -> falls
        // through to normal routing).
        assert!(nexus_command("turn up the gain").is_none());
    }

    /// "route input 1 to the monitor" -> route.set on the monitor bus (output 0)
    /// at unity; an explicit output is honored; "unroute" clears (-inf sentinel).
    #[test]
    fn nexus_route_maps_to_route_set() {
        // "to the monitor" -> the monitor bus output (0), 0 dB unity.
        assert_nexus_op(
            "route input 1 to the monitor",
            r#"{"op":"route.set","in":1,"out":0,"gain_db":0.0}"#,
        );
        // Explicit input + output.
        assert_nexus_op(
            "route input 2 to output 3",
            r#"{"op":"route.set","in":2,"out":3,"gain_db":0.0}"#,
        );
        // Unroute clears the crosspoint with the "-inf" string sentinel that
        // Nexus's _route_set maps back to float("-inf").
        assert_nexus_op(
            "unroute input 1 from output 3",
            r#"{"op":"route.set","in":1,"out":3,"gain_db":"-inf"}"#,
        );
    }

    /// "monitor input 1" -> monitor.set {on:true}; "stop monitoring" -> off. The
    /// monitor toggle is distinct from a generic crosspoint route.set.
    #[test]
    fn nexus_monitor_maps_to_monitor_set() {
        assert_nexus_op(
            "monitor input 1",
            r#"{"op":"monitor.set","in":1,"out":0,"on":true}"#,
        );
        assert_nexus_op(
            "stop monitoring",
            r#"{"op":"monitor.set","in":0,"out":0,"on":false}"#,
        );
        assert_nexus_op(
            "turn off the monitor",
            r#"{"op":"monitor.set","in":0,"out":0,"on":false}"#,
        );
    }

    /// "load the <name> preset" / "load preset <name>" -> preset.load {name},
    /// forwarded verbatim (Nexus resolves it against presets/).
    #[test]
    fn nexus_preset_load_maps_to_preset_load() {
        assert_nexus_op(
            "load the vocal preset",
            r#"{"op":"preset.load","name":"vocal"}"#,
        );
        assert_nexus_op(
            "load preset podcast",
            r#"{"op":"preset.load","name":"podcast"}"#,
        );
        assert_nexus_op(
            "recall the streaming preset",
            r#"{"op":"preset.load","name":"streaming"}"#,
        );
        // A preset name with a hyphen survives the tokenizer.
        assert_nexus_op(
            "load the voice-over preset",
            r#"{"op":"preset.load","name":"voice-over"}"#,
        );
        // "load a preset" with no name -> not actionable, falls through.
        assert!(nexus_command("load a preset").is_none());

        // WHAT WENT WRONG: the token AFTER "preset" was preferred
        // UNCONDITIONALLY, so any trailing word became the preset NAME — "load
        // the vocal preset now" loaded a preset called "now". Nexus rejects the
        // unknown name, but handle_nexus has already said "Forwarded that to the
        // mixer", so the user believes the vocal preset is live while nothing
        // changed.
        for (utter, want) in [
            ("load the vocal preset now", "vocal"),
            ("load the vocal preset again", "vocal"),
            ("recall the streaming preset thanks", "streaming"),
            ("load the podcast preset darwin", "podcast"),
            ("load the vocal preset please", "vocal"),
        ] {
            assert_nexus_op(
                utter,
                &format!(r#"{{"op":"preset.load","name":"{want}"}}"#),
            );
        }
        // The "preset LEADS" form still takes the FOLLOWING token, which is the
        // whole reason that branch exists.
        assert_nexus_op(
            "load preset voice-over now",
            r#"{"op":"preset.load","name":"voice-over"}"#,
        );
    }

    /// "what are the levels" / "show me the meters" / "what's the routing state"
    /// -> state.get (a read-only snapshot request).
    #[test]
    fn nexus_levels_query_maps_to_state_get() {
        let state = r#"{"op":"state.get"}"#;
        assert_nexus_op("what are the levels", state);
        assert_nexus_op("show me the meters", state);
        assert_nexus_op("what's the routing state", state);
        assert_nexus_op("read out the matrix", state);
        assert_nexus_op("what is currently routed", state);
    }

    /// Unrelated utterances never produce a Nexus command (so they fall through
    /// to normal routing) — including ones that merely share a stray keyword, and
    /// the other apps' control phrases (no cross-app capture).
    /// EVERY OFF-VERB THE TOGGLE ACCEPTS MUST TURN THE MONITOR OFF.
    ///
    /// The toggle has two lists: one deciding whether the branch FIRES, and one
    /// deciding on-vs-off. A verb admitted to the first without the second makes
    /// the branch fire, find no off-word it recognizes, and default to ON — so
    /// "quit monitoring" and "pause monitoring" OPENED A LIVE MIC-TO-MONITOR
    /// crosspoint on a request to close one. That is the worst possible direction
    /// for this particular op to be wrong in.
    ///
    /// This asserts the two lists agree, which is the invariant; the counts alone
    /// would not catch a verb added to one and not the other.
    #[test]
    fn every_monitor_off_verb_actually_turns_it_off() {
        for u in [
            "stop monitoring",
            "quit monitoring",
            "pause monitoring",
            "end monitoring",
            "switch off the monitor",
            "shut off the monitor",
            "turn the monitor off",
            "turn off the monitor",
            "disable the monitor",
            "kill the monitor",
        ] {
            let got = nexus_command(u).unwrap_or_else(|| panic!("{u:?} must reach the monitor toggle"));
            let NexusCommand::Op(line) = got else { panic!("{u:?} must be an Op") };
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["op"], "monitor.set", "{u:?}");
            assert_eq!(
                v["on"], false,
                "{u:?} asked to STOP monitoring and this opens a live mic instead"
            );
        }
        // ...and the on-direction still works, so the fix is not "always off".
        for u in ["start monitoring", "turn the monitor on", "enable the monitor"] {
            let got = nexus_command(u).unwrap_or_else(|| panic!("{u:?} must reach the toggle"));
            let NexusCommand::Op(line) = got else { panic!("{u:?} must be an Op") };
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["on"], true, "{u:?} asked to START monitoring");
        }
    }

    /// A CHANNEL NUMBER IS PART OF THE COMMAND.
    ///
    /// The mute vocabulary listed "channel" but rejected the digit beside it, so
    /// "mute channel 3" and "unmute channel 3" were dropped while "mute input 2"
    /// worked. The user says a number; the classifier has to be able to hear one.
    #[test]
    fn mute_accepts_a_spoken_channel_number() {
        for (u, muted) in [
            ("mute channel 3", true),
            ("unmute channel 3", false),
            ("mute input 2", true),
            ("mute the mic", true),
        ] {
            let got = nexus_command(u).unwrap_or_else(|| panic!("{u:?} must reach the mute branch"));
            let NexusCommand::Op(line) = got else { panic!("{u:?} must be an Op") };
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["mute"], muted, "{u:?}");
        }
    }

    /// The read idiom people actually speak. "are the levels clipping" is the
    /// most natural question anyone asks a meter and it died on the word
    /// "clipping"; "again" was in the mute and monitor vocabularies and missing
    /// only from this one — the signature of a list fitted to a corpus rather
    /// than to speech.
    #[test]
    fn the_levels_read_admits_ordinary_wrapper_words() {
        for u in [
            "what are the levels",
            "show me the meters",
            "show me the levels again",
            "are the levels clipping",
            "are the meters moving",
            "read the levels to me",
        ] {
            let got = nexus_command(u).unwrap_or_else(|| panic!("{u:?} must read the levels"));
            let NexusCommand::Op(line) = got else { panic!("{u:?} must be an Op") };
            assert!(line.contains("state.get"), "{u:?} -> {line}");
        }
    }

    #[test]
    fn nexus_command_ignores_unrelated_utterances() {
        for text in [
            "what's the weather",
            "open safari",
            "play some music",
            "tell me a joke",
            "find my budget spreadsheet",
            "open apple.com",
            // Silicon Canvas / Vision phrases must NOT be captured by Nexus.
            "show me the 3V3 net",
            "what do you see",
            "run erc",
            // "matrix" inside an unrelated open verb context is a launch-class
            // word, not a state query — but with no Nexus mention it is nothing.
            "tell me about the matrix movie",
        ] {
            assert_eq!(
                nexus_command(text),
                None,
                "{text:?} must not be a Nexus command"
            );
        }
    }

    /// An oversize / junk utterance is handled cleanly: nexus_command returns
    /// None (no panic) so the turn falls through to normal routing, and a Nexus
    /// phrase buried in a huge string still resolves to a well-formed op.
    #[test]
    fn nexus_command_handles_oversize_and_junk_cleanly() {
        let huge = "lorem ipsum ".repeat(5000);
        assert_eq!(nexus_command(&huge), None);
        assert_eq!(nexus_command(""), None);
        assert_eq!(nexus_command("??? --- ..."), None);
        let buried = format!("{huge} mute the mic {huge}");
        assert_nexus_op(
            &buried,
            r#"{"op":"gain.set","channel":0,"mute":true,"stage":"input"}"#,
        );
    }

    /// REGRESSION: ordinary speech must never MUTATE the audio matrix. Every
    /// utterance here comes from the 1,897-utterance everyday corpus this
    /// classifier was measured against, and every one of them wrote to the
    /// mixer before the gates above existed: a sentence about someone's COMMUTE
    /// muted the owner's microphone (`contains("mute")`), and 25 sentences
    /// about blood pressure, credit and computer displays rewrote a monitor
    /// crosspoint (`contains("monitor")`, no gate at all).
    #[test]
    fn ordinary_speech_never_mutates_the_audio_matrix() {
        for text in [
            "my commute was a nightmare this morning",
            "I need to monitor my blood pressure twice a day",
            "the bank offers free credit monitoring",
            "is a curved monitor worth the extra money",
            "the rangers monitor the snowpack all winter",
            "I don't want to monitor my teenager's every move",
            "the bank told me to stop monitoring my credit so obsessively",
            "I need to trim the mic budget by 5 percent to 2 people",
        ] {
            assert_eq!(nexus_command(text), None, "{text:?} must not touch the mixer");
        }
        // PRECONDITION: the trigger letters really are in these utterances, so a
        // future edit that simply stopped looking for them could not make this
        // test pass vacuously.
        assert!("my commute was a nightmare this morning".contains("mute"));
        assert!("is a curved monitor worth the extra money".contains("monitor"));
        assert!("I need to trim the mic budget by 5 percent to 2 people".contains("trim"));
        // ...and the real commands built from the same words still work.
        assert_nexus_op(
            "mute the mic",
            r#"{"op":"gain.set","channel":0,"mute":true,"stage":"input"}"#,
        );
        assert_nexus_op(
            "stop monitoring",
            r#"{"op":"monitor.set","in":0,"out":0,"on":false}"#,
        );
    }

    /// REGRESSION: naming a mic is not permission to REWIRE one, and neither is
    /// naming one NEXT TO a routing verb. route.set is the only op that can
    /// write -inf and DESTROY a crosspoint, so its gate is the narrowest in the
    /// file, and it took three separate bindings to get there — each of these
    /// groups is a design that was measured and found to write crosspoints on
    /// utterances the original code ignored:
    ///   * ordinary-English verbs ("send", "disconnect") need an explicitly
    ///     NUMBERED channel — otherwise "send the microphone back to amazon"
    ///     routes and "send a clear photo of the microphone to the seller"
    ///     CLEARS;
    ///   * a routing verb plus an audio noun is not enough — "the mic patch
    ///     cable is broken" and "there's a patch for the preamp driver" have
    ///     both, and a patch CABLE is a parcel. A destination is required;
    ///   * verb + noun + destination is STILL only co-occurrence — "route the
    ///     kids from the mic stand to the door" has all three. The verb must
    ///     take the signal chain as its OBJECT, and a mic STAND is furniture.
    #[test]
    fn sending_something_that_merely_mentions_a_mic_is_not_a_route_write() {
        for text in [
            "send the microphone back to amazon",
            "send me the open mic night lineup",
            "send a clear photo of the microphone to the seller",
            "can you send the mic specs to the AV vendor",
            "disconnect the mic and send it back",
            "did you send the preamp invoice yet",
            "the mic patch cable is broken",
            "I need a longer patch cable for the mic",
            "did the mic firmware patch land",
            "there's a patch for the preamp driver",
            "can you patch the mixer software tonight",
            "where did I put the mic patch bay diagram",
            "the fader cap fell off, order a patch kit",
            "is there a patch note for the crosspoint bug",
            "my running route goes past the microphone store",
            "the mixer is on the delivery route today",
            "route the kids from the mic stand to the door",
        ] {
            assert_eq!(nexus_command(text), None, "{text:?} is a parcel, not a patch");
        }
        // PRECONDITION: these really do carry a routing verb AND an audio noun,
        // so the test cannot pass just because the branch stopped looking.
        assert!("the mic patch cable is broken".contains("patch"));
        assert!("route the kids from the mic stand to the door".contains("mic"));
        // The real routing commands built from the same words are unchanged.
        assert_nexus_op(
            "route the mic to the monitor",
            r#"{"op":"route.set","in":0,"out":0,"gain_db":0.0}"#,
        );
        assert_nexus_op(
            "patch the mic into the monitor",
            r#"{"op":"route.set","in":0,"out":0,"gain_db":0.0}"#,
        );
        assert_nexus_op(
            "unpatch the mic from the monitor",
            r#"{"op":"route.set","in":0,"out":0,"gain_db":"-inf"}"#,
        );
        assert_nexus_op(
            "send input 1 to output 2",
            r#"{"op":"route.set","in":1,"out":2,"gain_db":0.0}"#,
        );
        assert_nexus_op(
            "disconnect input 2 from output 3",
            r#"{"op":"route.set","in":2,"out":3,"gain_db":"-inf"}"#,
        );
    }

    /// REGRESSION: "level" and "meter" are two of the most ordinary nouns in
    /// English, and a bare `contains()` answered 93 of the 1,897 everyday
    /// utterances with a mixer snapshot instead of a real answer — including
    /// ones where the word was not even a word ("thermometer", "levelled").
    /// "matrix" carried the same disease one level down: `contains("rout")` is
    /// a FRAGMENT and fires inside "routine".
    ///
    /// The read idiom accepts an adjective between the article and the noun
    /// ("the PEAK levels"), so the last three negatives matter: that adjective
    /// must not become a way back in for a sentence about a reservoir.
    #[test]
    fn an_ordinary_level_or_meter_is_not_a_mixer_read() {
        for text in [
            "my stress levels have been through the roof this week",
            "sea level is rising a little every year",
            "I got a parking meter ticket, can I contest it",
            "do I own a meat thermometer or just a candy meter",
            "how many meters is a lap in that pool",
            "my whole routine is a matrix of pills and timers",
            "can you check my levels",
            "the volume level is way too low on this video",
            "the peak levels of tourism in july were insane",
            "what are the current levels of the reservoir",
            "the audio levels at the concert were painful",
        ] {
            assert_eq!(nexus_command(text), None, "{text:?} is not a mixer read");
        }
        // The read idioms — bare ("the levels"/"the meters" with nothing but
        // function words and audio adjectives around them) and channel-named.
        let state = r#"{"op":"state.get"}"#;
        assert_nexus_op("what are the levels", state);
        assert_nexus_op("show me the meters on my screen", state);
        assert_nexus_op("what do the meters say", state);
        assert_nexus_op("how are the levels looking", state);
        assert_nexus_op("what are my input levels", state);
        assert_nexus_op("show me the peak levels", state);
        assert_nexus_op("what are the audio levels", state);
        assert_nexus_op("what are the meters showing", state);
    }

    /// REGRESSION: a QUESTION about the routing is a READ. Both of the first two
    /// wrote a crosspoint before this — the route branch matched "route" inside
    /// "routed" and took the bare word "output"/"input" as its target, so asking
    /// what was patched PATCHED SOMETHING. The apostrophe-free "whats" is here
    /// because STT emits it constantly and a whole-word "what" test silently
    /// dropped it back into the WRITING branch — a contraction must never be the
    /// difference between reading a crosspoint and rewiring one.
    #[test]
    fn asking_about_the_routing_reads_it_and_never_writes_it() {
        let state = r#"{"op":"state.get"}"#;
        assert_nexus_op("what's routed to output 1", state);
        assert_nexus_op("whats routed to output 1", state);
        assert_nexus_op("what inputs are routed right now", state);
        assert_nexus_op("what is currently routed", state);
    }

    /// REGRESSION: "trim" is a VERB, and it is the SPEC's own word for input
    /// gain staging (§"Gain staging policy": the interface preamp is *trimmed*
    /// so speech peaks hit -18 dBFS). An earlier gate recognized only the NOUN
    /// form ("the input trim", "the trim on input 1") because the command list
    /// it was tuned against happened to contain only that form, and it killed
    /// the entire verb family — including utterances naming a NUMBERED channel,
    /// which is this classifier's strongest safety signal. Same blindness in
    /// the other direction: "set the gain TO -6 DB ON INPUT 1" puts the dB
    /// target between the head and its channel, and the adjacency walk stopped
    /// dead at "to".
    #[test]
    fn a_trim_is_a_trim_when_it_is_a_verb_too() {
        assert_nexus_op(
            "trim input 1 to -6 db",
            r#"{"op":"gain.set","channel":1,"gain_db":-6.0,"stage":"input"}"#,
        );
        assert_nexus_op(
            "trim output 1 to -3 db",
            r#"{"op":"gain.set","channel":1,"gain_db":-3.0,"stage":"output"}"#,
        );
        assert_nexus_op(
            "trim the mic to -6",
            r#"{"op":"gain.set","channel":0,"gain_db":-6.0,"stage":"input"}"#,
        );
        assert_nexus_op(
            "trim the input to -6 db",
            r#"{"op":"gain.set","channel":0,"gain_db":-6.0,"stage":"input"}"#,
        );
        assert_nexus_op(
            "set the gain to -6 db on input 1",
            r#"{"op":"gain.set","channel":1,"gain_db":-6.0,"stage":"input"}"#,
        );
        assert_nexus_op(
            "set the gain to -12 on the mic",
            r#"{"op":"gain.set","channel":0,"gain_db":-12.0,"stage":"input"}"#,
        );
        // The NOUN form is untouched, and so is the bare idiom with a trailing
        // phrase on it (a closed vocabulary that has not heard of "right now"
        // throws away a real command).
        assert_nexus_op(
            "set the input trim to -18 db",
            r#"{"op":"gain.set","channel":0,"gain_db":-18.0,"stage":"input"}"#,
        );
        assert_nexus_op(
            "set the gain to -6 right now",
            r#"{"op":"gain.set","channel":0,"gain_db":-6.0,"stage":"input"}"#,
        );
        // What the verb form must NOT take: the mic as a direct object with no
        // dB target is somebody talking about a budget, not a gain stage.
        assert_eq!(
            nexus_command("I need to trim the mic budget by 5 percent to 2 people"),
            None
        );
        assert_eq!(nexus_command("we should trim the mic budget to 2 people"), None);
        // No dB value at all is still not a gain.set.
        assert_eq!(nexus_command("turn up the gain"), None);
    }

    /// REGRESSION: an utterance that says the user STOPPED monitoring must never
    /// TURN THE MONITOR ON. Making the off-words whole-word stopped them
    /// matching the past and progressive forms that `contains("stop")` and
    /// `contains("disable")` used to catch, and the result was not a missed
    /// command but a FLIP into the device-activating direction: each of these
    /// went on:false -> on:TRUE, opening a live mic-to-monitor-bus crosspoint
    /// where the code being replaced closed one. "kill the monitor" is the same
    /// class — it reaches this branch through the bare-toggle vocabulary and
    /// turned the monitor ON.
    #[test]
    fn past_tense_off_speech_never_opens_the_monitor() {
        let off = r#"{"op":"monitor.set","in":0,"out":0,"on":false}"#;
        assert_nexus_op("I stopped monitoring the mic last week", off);
        assert_nexus_op("I've stopped monitoring my mic levels", off);
        assert_nexus_op("they disabled the mic monitoring already", off);
        assert_nexus_op("kill the monitor", off);
        assert_nexus_op("turn the monitor off please", off);
        assert_nexus_op(
            "we stopped monitoring input 1 months ago",
            r#"{"op":"monitor.set","in":1,"out":0,"on":false}"#,
        );
        // PRECONDITION: these are the INFLECTED forms, not the bare ones — if a
        // future edit reverted to whole-word "stop"/"disable" only, the
        // assertions above would fail rather than pass vacuously.
        assert!(!"I stopped monitoring the mic last week".contains("stop "));
        assert!(!"they disabled the mic monitoring already".contains("disable "));
        // The ON direction is untouched.
        let on = r#"{"op":"monitor.set","in":0,"out":0,"on":true}"#;
        assert_nexus_op("turn on the monitor", on);
        assert_nexus_op("turn the monitor back on", on);
        assert_nexus_op(
            "monitor input 1",
            r#"{"op":"monitor.set","in":1,"out":0,"on":true}"#,
        );
    }

    // ======================================================================
    // Mark-Forge voice control (SPEC §7). Mirrors the Silicon Canvas / Vision /
    // Nexus tests. The wire form pinned here is the BARE `{"op":...}` object the
    // Mark-Forge engine (apps/mark-forge/src/ipc.rs) deserializes via its
    // `#[serde(tag = "op")]` Op enum — NOT the Vision `{"type":"op"}` envelope.
    // Each expected line matches the SPEC §7 op table and the app's own
    // round-trip tests verbatim (op_deserializes_with_dotted_names,
    // body_spawn_deserializes_with_optional_fields), so a pass here proves the
    // daemon emits ops the engine already accepts.
    // ======================================================================

    /// Assert the utterance maps to a Mark-Forge Op carrying EXACTLY this JSON
    /// wire string (compared as parsed JSON so key order is irrelevant; the
    /// op-tag + fields are what the contract pins).
    fn assert_mark_forge_op(text: &str, expected_json: &str) {
        match mark_forge_command(text) {
            Some(MarkForgeCommand::Op(line)) => {
                let got: serde_json::Value = serde_json::from_str(&line).unwrap();
                let want: serde_json::Value = serde_json::from_str(expected_json).unwrap();
                assert_eq!(got, want, "for utterance {text:?}");
            }
            other => panic!("expected a Mark-Forge Op for {text:?}, got {other:?}"),
        }
    }

    /// "open/launch/start the physics sandbox" (and its aliases) is a LAUNCH;
    /// the app name is the manifest name the registry keys on.
    #[test]
    fn mark_forge_launch_phrases() {
        assert_eq!(MARK_FORGE_APP, "mark-forge");
        for text in [
            "open the physics sandbox",
            "launch the physics sandbox",
            "start mark forge",
            "darwin, bring up the physics sandbox",
            "open mark-forge",
            "fire up the physics engine",
            "show me the sandbox",
        ] {
            assert_eq!(
                mark_forge_command(text),
                Some(MarkForgeCommand::Launch),
                "{text:?} should be a Mark-Forge launch"
            );
        }
        // "sandbox"/"sim" must be a whole word / real mention — never inside
        // another token, and a bare open verb with no Mark-Forge mention falls
        // through to the macOS launcher.
        assert!(mark_forge_command("open safari").is_none());
    }

    /// "drop a box|cube" -> body.spawn of a dynamic cuboid a few metres up; the
    /// shape is tagged on `kind` and the vectors serialize as `[x,y,z]` arrays
    /// (exactly the SpawnSpec wire form the engine deserializes).
    #[test]
    fn mark_forge_drop_box_maps_to_body_spawn_cuboid() {
        let want = r#"{"op":"body.spawn","shape":{"kind":"cuboid","half_extents":[0.5,0.5,0.5]},"pos":[0.0,5.0,0.0],"mass":1.0}"#;
        assert_mark_forge_op("drop a box", want);
        assert_mark_forge_op("drop a cube", want);
        assert_mark_forge_op("spawn a box", want);
        assert_mark_forge_op("add a crate", want);
        assert_mark_forge_op("darwin, drop a box in the sandbox", want);
        // The spawned body carries a POSITIVE mass so it is dynamic and actually
        // falls (a None/<=0 mass would be a static body that never moves).
        if let Some(MarkForgeCommand::Op(line)) = mark_forge_command("drop a box") {
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert!(v["mass"].as_f64().unwrap() > 0.0, "a dropped box must be dynamic");
        } else {
            panic!("expected a body.spawn op");
        }
    }

    /// "drop a ball|sphere" -> body.spawn of a dynamic sphere.
    #[test]
    fn mark_forge_drop_ball_maps_to_body_spawn_sphere() {
        let want = r#"{"op":"body.spawn","shape":{"kind":"sphere","radius":0.5},"pos":[0.0,5.0,0.0],"mass":1.0}"#;
        assert_mark_forge_op("drop a ball", want);
        assert_mark_forge_op("drop a sphere", want);
        assert_mark_forge_op("spawn a marble", want);
        // A ball noun wins over a box noun when both are absent of the other —
        // "drop a ball" is a sphere, not a cuboid.
        assert_mark_forge_op("throw a ball", want);
    }

    /// "reset/clear the simulation" -> world.reset, gated on a physics context so
    /// a bare "reset" elsewhere never wipes the world.
    #[test]
    fn mark_forge_reset_maps_to_world_reset() {
        let want = r#"{"op":"world.reset"}"#;
        assert_mark_forge_op("reset the simulation", want);
        assert_mark_forge_op("reset the physics sandbox", want);
        assert_mark_forge_op("clear the world", want);
        assert_mark_forge_op("wipe the scene", want);
        assert_mark_forge_op("reset everything in the sandbox", want);
        // "reset" with no physics context falls through to normal routing.
        assert!(mark_forge_command("reset my password").is_none());
        // "reset gravity" is NOT a world reset (it is a gravity op / falls
        // through) — a reset must never be triggered by the gravity word.
        assert!(
            !matches!(
                mark_forge_command("reset the gravity"),
                Some(MarkForgeCommand::Op(ref l)) if l.contains("world.reset")
            ),
            "reset gravity must not wipe the world"
        );
    }

    /// "set gravity to the moon|mars|earth|zero" / "turn off gravity" ->
    /// set.gravity with the matching fixed constant on the downward (y) axis.
    #[test]
    fn mark_forge_gravity_targets_map_to_set_gravity() {
        assert_mark_forge_op(
            "set gravity to the moon",
            r#"{"op":"set.gravity","x":0.0,"y":-1.62,"z":0.0}"#,
        );
        assert_mark_forge_op(
            "set gravity to mars",
            r#"{"op":"set.gravity","x":0.0,"y":-3.72,"z":0.0}"#,
        );
        assert_mark_forge_op(
            "set gravity back to earth",
            r#"{"op":"set.gravity","x":0.0,"y":-9.81,"z":0.0}"#,
        );
        assert_mark_forge_op(
            "set gravity to normal",
            r#"{"op":"set.gravity","x":0.0,"y":-9.81,"z":0.0}"#,
        );
        // Zero-g variants.
        assert_mark_forge_op(
            "turn off gravity",
            r#"{"op":"set.gravity","x":0.0,"y":0.0,"z":0.0}"#,
        );
        assert_mark_forge_op(
            "set gravity to zero",
            r#"{"op":"set.gravity","x":0.0,"y":0.0,"z":0.0}"#,
        );
        // "gravity" with no recognized target falls through (the daemon won't
        // guess a vector).
        assert!(mark_forge_command("what is gravity").is_none());
        // A bare "moon" / "mars" with no "gravity" word never fires.
        assert!(mark_forge_command("tell me about the moon").is_none());
    }

    /// "step" / "advance" -> world.step{n>=1} (N from the utterance, default 1);
    /// "pause"/"freeze" -> world.step{n:0}. Both gated on a physics context.
    #[test]
    fn mark_forge_step_and_pause_map_to_world_step() {
        // Single step (default 1 frame).
        assert_mark_forge_op("step the simulation", r#"{"op":"world.step","n":1}"#);
        assert_mark_forge_op("advance the physics", r#"{"op":"world.step","n":1}"#);
        assert_mark_forge_op("step the sandbox", r#"{"op":"world.step","n":1}"#);
        // An explicit frame count is honored.
        assert_mark_forge_op("step the simulation 10 frames", r#"{"op":"world.step","n":10}"#);
        assert_mark_forge_op("advance 5 frames", r#"{"op":"world.step","n":5}"#);
        // Pause -> a zero-frame step (advances no simulated time).
        assert_mark_forge_op("pause the simulation", r#"{"op":"world.step","n":0}"#);
        assert_mark_forge_op("freeze the physics", r#"{"op":"world.step","n":0}"#);
        assert_mark_forge_op("hold the simulation", r#"{"op":"world.step","n":0}"#);
        // A bare "pause" / "step" with no physics context falls through.
        assert!(mark_forge_command("pause the music").is_none());
        assert!(mark_forge_command("step outside for a minute").is_none());
    }

    /// A misheard huge step count is clamped to a sane bound so the engine is
    /// never asked to advance millions of frames synchronously.
    #[test]
    fn mark_forge_step_count_is_clamped() {
        if let Some(MarkForgeCommand::Op(line)) =
            mark_forge_command("step the simulation 99999999 frames")
        {
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["n"].as_u64().unwrap(), 10_000, "huge step count must clamp");
        } else {
            panic!("expected a world.step op");
        }
    }

    /// Unrelated utterances never produce a Mark-Forge command (so they fall
    /// through to normal routing) — including ones that share a stray keyword,
    /// and the other apps' control phrases (no cross-app capture).
    /// A DESCRIPTION MUST NEVER BE HEARD AS A WORLD WIPE.
    ///
    /// The reset verbs were matched with `contains`, so "clear" inside "unclear"
    /// paired with an ordinary "world"/"scene" mention destroyed every body in the
    /// simulation. The nouns beside them already used whole-word matching; the
    /// verbs — the half that actually does the damage — did not.
    #[test]
    fn a_clear_shaped_word_does_not_wipe_the_world() {
        for text in [
            "the physics in this world is unclear",
            "the scene has nuclear reactors in it",
            "clearance between the bodies looks tight",
        ] {
            let got = mark_forge_command(text);
            assert!(
                !matches!(got, Some(MarkForgeCommand::Op(_))),
                "{text:?} describes the scene; wiping it here destroys every body \
                 the user built (got {got:?})"
            );
        }
        // A real reset still resets.
        assert!(
            mark_forge_command("clear the world").is_some(),
            "an actual reset command must still reset"
        );
    }

    /// ORDINARY SPEECH MUST NOT REACH THE DESTRUCTIVE OPS.
    ///
    /// Four ordinary sentences reached mark-forge, two of them destructive. Each
    /// came in through a different hole, and three were substring bugs sitting
    /// beside a correct whole-word gate:
    ///
    ///   "does the second book reset everything that happened" -> world.RESET
    ///       "everything" was listed as a physics noun. It is ordinary English.
    ///   "gravity is what, mass warping space"                 -> set.GRAVITY 0
    ///       contains("gravity") + a target word was enough, so a sentence ABOUT
    ///       gravity, in which "space" is the target, zeroed the world's gravity.
    ///   "probe the snow with a pole before you step there"    -> world.step
    ///       contains("step the") matches "step THERE".
    ///   "the picture frame my aunt got won't hold a charge"   -> world.step 0
    ///       contains("frame") matches "picture FRAME", making it a physics
    ///       context, which then let the ordinary verb "hold" pause the world.
    #[test]
    fn ordinary_speech_never_resets_or_regravitates_the_world() {
        for u in [
            "does the second book reset everything that happened",
            "gravity is what, mass warping space",
            "probe the snow with a pole before you step there",
            "the picture frame my aunt got won't hold a charge",
            "clear everything off the kitchen table",
            "let me step there for a second",
            "the picture frame is crooked",
            "gravity is the weakest of the four forces",
            // WHAT WENT WRONG (world.reset): the co-word WAS the whole gate, and
            // "world" / "scene" / "bodies" / "the simulation" / "the sandbox" are
            // ordinary English. Every one of these WIPED THE PHYSICS WORLD. The
            // sibling spawn branch had already been given the closed-vocabulary
            // gate that closes them; this branch had not.
            "the world reset itself after the pandemic in a lot of ways",
            "reset the simulation of my expectations for this quarter",
            "clear the scene before the police get here",
            "I need to reset my whole world after that breakup",
            "wipe the scene from your memory it was embarrassing",
            "the bodies of water in this county are all clear now",
            "clear the world of that idea please",
            "reset the sandbox account before the demo",
            "my world reset when the baby arrived",
            // ...and the LAUNCH verbs were `contains`, so "started" was "start"
            // and any past-tense sentence mentioning "the simulation" opened the
            // engine.
            "they cleared the simulation results and started over",
        ] {
            assert!(
                mark_forge_command(u).is_none(),
                "{u:?} is ordinary speech and must not reach the physics sandbox: {:?}",
                mark_forge_command(u)
            );
        }
    }

    /// ...and every real command still works, including the two the first cut of
    /// the above broke: "advance 5 frames" (PLURAL frames is a step count, the
    /// singular is a photograph) and "hold the simulation".
    #[test]
    fn the_real_physics_commands_all_survive() {
        for (u, op) in [
            ("reset the simulation", "world.reset"),
            ("clear the world", "world.reset"),
            ("wipe the scene", "world.reset"),
            ("step the simulation", "world.step"),
            ("advance 5 frames", "world.step"),
            ("advance the physics", "world.step"),
            ("pause the simulation", "world.step"),
            ("hold the simulation", "world.step"),
            ("freeze the physics", "world.step"),
            ("set gravity to the moon", "set.gravity"),
            ("turn off gravity", "set.gravity"),
            ("moon gravity", "set.gravity"),
        ] {
            let got = mark_forge_command(u)
                .unwrap_or_else(|| panic!("{u:?} is a real command and must still work"));
            let MarkForgeCommand::Op(line) = got else { panic!("{u:?} must be an Op") };
            assert!(line.contains(op), "{u:?} -> {line}, expected {op}");
        }
    }

    #[test]
    fn mark_forge_command_ignores_unrelated_utterances() {
        for text in [
            "what's the weather",
            "open safari",
            "play some music",
            "tell me a joke",
            "find my budget spreadsheet",
            "open apple.com",
            "drop me an email",            // "drop" without a shape noun
            "drop everything and call me", // "drop" without a shape noun
            // WHAT WENT WRONG: the spawn branch was the ONE Mark-Forge branch
            // with no physics-context gate, so a spawn verb CO-OCCURRING with a
            // shape noun anywhere in the sentence was enough. Each of these was
            // answered by the physics sandbox (terminal — route()'s else-if chain
            // never reached the real handler) and, with the sandbox open, silently
            // dropped a 1 kg body into the user's scene.
            "can you add a block to my calendar at 3",
            "did you drop the boxes off at the post office",
            "drop the kids off at the block party",
            "throw the ball for the dog",
            "what time does the ball drop on new year's eve",
            "remind me to drop the boxes off at the post office",
            "pause the music",             // pause outside a physics context
            "reset my password",           // reset outside a physics context
            // Other apps' phrases must NOT be captured by Mark-Forge.
            "show me the 3V3 net",
            "what do you see",
            "mute the mic",
            "tell me about the moon landing", // "moon" with no gravity word
        ] {
            assert_eq!(
                mark_forge_command(text),
                None,
                "{text:?} must not be a Mark-Forge command"
            );
        }
    }

    /// An oversize / junk utterance is handled cleanly: mark_forge_command
    /// returns None (no panic) so the turn falls through to normal routing, and a
    /// Mark-Forge phrase buried in a huge string still resolves to a well-formed
    /// op.
    ///
    /// THIS TEST ASSERTED THE BUG. Its buried case was `"{huge} reset the
    /// simulation {huge}"` — 120,000 characters of lorem ipsum with a reset
    /// phrase in the middle — and it PINNED that as a world.reset. That is not
    /// robustness, it is the co-word hijack stated as a contract: it is the same
    /// shape as "reset the simulation of my expectations for this quarter" and
    /// "the world reset itself after the pandemic", only larger. The robustness
    /// contract (no panic, a well-formed op, a fall-through on junk) is kept; the
    /// buried case is re-anchored on the arm that legitimately survives dilution —
    /// an utterance that NAMES the engine — and the co-word form is now pinned
    /// the other way.
    #[test]
    fn mark_forge_command_handles_oversize_and_junk_cleanly() {
        let huge = "lorem ipsum ".repeat(5000);
        assert_eq!(mark_forge_command(&huge), None);
        assert_eq!(mark_forge_command(""), None);
        assert_eq!(mark_forge_command("??? --- ..."), None);
        // Names the engine -> still resolves, still well-formed, however diluted.
        let buried = format!("{huge} reset the physics sandbox {huge}");
        assert_mark_forge_op(&buried, r#"{"op":"world.reset"}"#);
        // A BARE CO-WORD diluted in junk is not a command. "simulation" and
        // "world" are ordinary English; only the whole-utterance idiom or the
        // engine's own name carries a reset.
        for co_word in ["reset the simulation", "clear the world", "wipe the scene"] {
            assert_eq!(
                mark_forge_command(&format!("{huge} {co_word} {huge}")),
                None,
                "{co_word:?} buried in unrelated text must not wipe the world"
            );
            // ...while the same phrase ON ITS OWN still does.
            assert!(
                mark_forge_command(co_word).is_some(),
                "{co_word:?} on its own is still a real command"
            );
        }
    }

    // ===== CAPABILITY SELECTOR — end-to-end with the SHIPPED scorer ==========
    // These exercise the exact wiring route() uses: crate::selector::classify_mode
    // with the production LexicalAgentScorer (no mock). They pin that the headline
    // cases route to the right capability, and that BOTH rails hold with the real
    // scorer — the selector never silently arms autonomy or a consequential action.

    use crate::agents::LexicalAgentScorer;
    use crate::selector::{classify_mode, Mode, Selection};

    /// "every morning brief me" -> standing (PROPOSED — it parks for confirm, the
    /// router maps Standing to propose_standing_mission; never silently created).
    #[test]
    fn selector_routes_recurring_request_to_standing_with_shipped_scorer() {
        assert_eq!(
            classify_mode("every morning brief me on my deadlines", &LexicalAgentScorer),
            Selection::Route(Mode::Standing)
        );
        assert_eq!(
            classify_mode("from now on keep watching the launch project", &LexicalAgentScorer),
            Selection::Route(Mode::Standing)
        );
    }

    /// "what's the status of the launch project" -> world_query (read-only).
    #[test]
    fn selector_routes_state_question_to_world_query_with_shipped_scorer() {
        assert_eq!(
            classify_mode("what's the status of the launch project", &LexicalAgentScorer),
            Selection::Route(Mode::WorldQuery)
        );
    }

    /// "the launch slipped to next Tuesday" -> world_update (shared tier only).
    #[test]
    fn selector_routes_stated_fact_to_world_update_with_shipped_scorer() {
        assert_eq!(
            classify_mode("the launch slipped to next Tuesday", &LexicalAgentScorer),
            Selection::Route(Mode::WorldUpdate)
        );
    }

    /// "plan and kick off the migration" -> mission (FURY).
    #[test]
    fn selector_routes_multistep_now_to_mission_with_shipped_scorer() {
        assert_eq!(
            classify_mode("plan and kick off the migration", &LexicalAgentScorer),
            Selection::Route(Mode::Mission)
        );
    }

    /// A plain action / normal question -> one_shot, UNCHANGED. The selector must
    /// not hijack the existing fast-cue routing for plain commands.
    #[test]
    fn selector_leaves_plain_requests_one_shot_with_shipped_scorer() {
        for q in [
            "open safari",
            "what time is it",
            "what's the weather",
            "play some jazz",
            "set a timer for ten minutes",
            "hi darwin",
        ] {
            assert_eq!(
                classify_mode(q, &LexicalAgentScorer),
                Selection::Route(Mode::OneShot),
                "plain request must stay one_shot (existing routing unchanged): {q}"
            );
        }
    }

    /// RAIL 1 with the real scorer: a genuinely ambiguous "look after my stuff"
    /// (no hard cue) must NEVER silently establish a standing mission or any
    /// consequential mode — it stays one_shot (safe-default) or clarifies, never
    /// Route(Standing).
    #[test]
    fn selector_never_silently_arms_autonomy_on_ambiguous_with_shipped_scorer() {
        for q in [
            "look after my deadlines for me",
            "handle my stuff",
            "deal with things",
            "take care of it",
        ] {
            let sel = classify_mode(q, &LexicalAgentScorer);
            assert_ne!(
                sel,
                Selection::Route(Mode::Standing),
                "an ambiguous request must never silently route to standing: {q} -> {sel:?}"
            );
            // It is either the safe default or an explicit clarify — never a
            // consequential mode arrived at by a guess.
            match sel {
                Selection::Route(m) => assert!(
                    !m.is_consequential(),
                    "ambiguous request reached a consequential mode {m:?}: {q}"
                ),
                Selection::Clarify(_) => {} // asking is allowed and safe.
            }
        }
    }

    // ---- ROUTER DISPATCH: notebook + life-log utterances route end-to-end ---
    // These exercise the EXACT composition the route() handler runs for these two
    // intents — classify the utterance, then dispatch it against the real store —
    // so the wiring is proven at the router layer without spinning up the live
    // InferenceClient / ReplySession / AppRegistry the full route() needs. Fully
    // hermetic: a temp Db + a SYNTHETIC last research run; NO fetch/model/network.

    use crate::memory::Memory;
    use crate::research::{Claim, ResearchReport, Source};
    use std::path::PathBuf;

    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("darwin-router-dispatch-{}-{}.db", std::process::id(), tag));
            let _ = std::fs::remove_file(&path);
            TempDb(path)
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut p = self.0.clone().into_os_string();
                p.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(p));
            }
        }
    }

    /// A synthetic report whose ONLY grounded source is #1 (a phantom 999 + an
    /// uncited 0 are present, so the save must keep just the grounded one).
    fn synthetic_report() -> ResearchReport {
        ResearchReport {
            question: "what is X".into(),
            sources: vec![Source {
                id: 1,
                url: "https://a.test".into(),
                title: "Real A".into(),
                excerpt: "e".into(),
            }],
            claims: vec![Claim::new("a grounded point", 1), Claim::new("phantom", 999)],
            planned_subqueries: 1,
            pursued_subqueries: 1,
            truncated: false,
        }
    }

    #[tokio::test]
    async fn memory_store_keeps_every_distinct_note_never_clobbering_the_last() {
        // Regression (full-OS sweep): memory.store used the FIXED key
        // "<ns>.note", so a second note silently overwrote the first. Notes are
        // now content-keyed; identical text stays a no-growth upsert.
        let db = TempDb::new("note-clobber");
        let mem = Memory::open(&db.0).unwrap();
        let reg = AgentRegistry::canonical();
        let agent = reg.orchestrator();
        let apps = crate::apps::AppRegistry::discover(std::path::Path::new("/nonexistent"));
        let apps = std::sync::Arc::new(apps);

        for text in ["the wifi password is hidden", "buy oat milk", "buy oat milk"] {
            super::handle_local("memory.store", &serde_json::Value::Null, text, &mem, &apps, agent).await;
        }
        let facts = mem.agent_scoped_facts(&agent.namespace, 50).await.unwrap();
        let notes: Vec<&str> = facts
            .iter()
            .filter(|(k, _)| k.starts_with(&format!("{}.note.", agent.namespace)))
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(notes.len(), 2, "two distinct notes kept, duplicate deduped: {notes:?}");
        assert!(notes.contains(&"the wifi password is hidden"));
        assert!(notes.contains(&"buy oat milk"));
    }

    // =====================================================================
    // THRESHOLD — GUEST MODE: the structured-intent FAST PATH is gated
    // =====================================================================

    fn guest_scope_fixture() -> crate::threshold::Scope {
        crate::threshold::guest_from(
            &crate::threshold::Scope::owner(vec!["*".to_string()], crate::focus::FocusProfile::Default),
            &crate::focus::FocusProfile::DeepFocus,
        )
    }

    #[tokio::test]
    async fn guest_handle_local_refuses_owner_data_and_write_intents_but_allows_conversation_and_status() {
        // FINDING 3: handle_local is the structured-intent FAST PATH — it bypasses
        // the tool-loop + recall gates. For a GUEST it must DENY BY DEFAULT: refuse
        // memory.recall (reads owner facts), memory.store (WRITES owner memory),
        // app.control, web.open, and anything else not in the non-personal set.
        let db = TempDb::new("guest-handle-local");
        let mem = Memory::open(&db.0).unwrap();
        let reg = AgentRegistry::canonical();
        let agent = reg.orchestrator();
        let apps = std::sync::Arc::new(crate::apps::AppRegistry::discover(std::path::Path::new("/nonexistent")));

        // Seed an owner fact so we can prove memory.recall never speaks it.
        mem.upsert_fact("user.name", "Darwin").await.unwrap();
        mem.upsert_fact("agent.darwin.secret_note", "the owner's private note").await.unwrap();

        let _o = crate::threshold::ScopeOverride::guest(guest_scope_fixture());

        // memory.recall — REFUSED, and speaks NO owner fact.
        let out = super::handle_local("memory.recall", &serde_json::Value::Null, "what do you remember", &mem, &apps, agent).await;
        assert!(!out.llm_voice, "a refusal is spoken verbatim, not sent to the LLM");
        assert!(out.data.contains("guest mode"), "memory.recall is refused in guest mode: {}", out.data);
        assert!(!out.data.contains("Darwin"), "no owner fact leaks via memory.recall: {}", out.data);
        assert!(!out.data.contains("secret_note"), "no owner private fact leaks: {}", out.data);

        // memory.store — REFUSED, and performs NO write to the owner namespace.
        let out = super::handle_local("memory.store", &serde_json::Value::Null, "my card PIN is 1234", &mem, &apps, agent).await;
        assert!(out.data.contains("guest mode"), "memory.store is refused in guest mode: {}", out.data);
        let facts = mem.agent_scoped_facts(&agent.namespace, 100).await.unwrap();
        assert!(
            !facts.iter().any(|(_, v)| v.contains("1234")),
            "a guest memory.store must perform NO write to the owner's memory: {facts:?}"
        );

        // app.control + web.open — REFUSED.
        for intent in ["app.control", "app.launch", "web.open", "web.search", "file.op"] {
            let out = super::handle_local(intent, &serde_json::Value::Null, "do the thing", &mem, &apps, agent).await;
            assert!(out.data.contains("guest mode"), "{intent} must be refused for a guest: {}", out.data);
            assert!(!out.llm_voice, "{intent} refusal is spoken verbatim");
        }

        // conversation + system.query — ALLOWED (fall through / non-personal status).
        let out = super::handle_local("conversation", &serde_json::Value::Null, "hello there", &mem, &apps, agent).await;
        assert!(!out.data.contains("guest mode"), "conversation is allowed for a guest: {}", out.data);
        let out = super::handle_local("system.query", &serde_json::Value::Null, "how are you running", &mem, &apps, agent).await;
        assert!(!out.data.contains("guest mode"), "non-personal system status is allowed for a guest: {}", out.data);
    }

    #[tokio::test]
    async fn owner_handle_local_is_unchanged_no_guest_gate() {
        // OWNER RAIL: with NO guest scope, handle_local performs its normal work —
        // memory.store WRITES the note (byte-for-byte today's behavior).
        let db = TempDb::new("owner-handle-local");
        let mem = Memory::open(&db.0).unwrap();
        let reg = AgentRegistry::canonical();
        let agent = reg.orchestrator();
        let apps = std::sync::Arc::new(crate::apps::AppRegistry::discover(std::path::Path::new("/nonexistent")));

        let _o = crate::threshold::ScopeOverride::owner();
        let out = super::handle_local("memory.store", &serde_json::Value::Null, "buy oat milk", &mem, &apps, agent).await;
        assert!(!out.data.contains("guest mode"), "owner path never sees the guest refusal: {}", out.data);
        let facts = mem.agent_scoped_facts(&agent.namespace, 100).await.unwrap();
        assert!(facts.iter().any(|(_, v)| v == "buy oat milk"), "owner memory.store writes the note (unchanged)");
    }

    #[test]
    fn guest_denied_fast_path_catches_owner_data_and_consequential_classifiers() {
        // The route() fast-path gate: every owner-data / consequential specialized
        // classifier is refused for a guest, while plain conversation / translation /
        // status falls through (None).
        let cfg = Config::default();
        // Owner-DATA readers + owner CONTROLS / consequential actions -> Some(reason).
        for u in [
            "why do you think i like tea",     // user_model mirror (owner profile)
            "what was i doing an hour ago",    // aperture (activity timeline)
            "what did i copy earlier",         // pasteboard
            "save this research",              // notebook
            "go dark",                         // vault control
            "replay the macro morning",        // macro replay (consequential)
            "undo that",                       // journal undo (consequential)
            "always allow the gmail_send action", // policy control (pure classify, no write)
            "use the local model",             // model swap control
            "roll call",                       // agent roster (finding 2)
            "who's on the team",               // agent roster (finding 2)
            "list my agents",                  // agent query (finding 2)
            "what agents do you have",         // agent query (finding 2)
            // RUNBOOKS — the arm this mirror was MISSING, and the only route()
            // fast path that executes tools (handle_runbook_command ->
            // runbook::run -> route_step -> execute_tool under the orchestrator's
            // ["*"] allowlist). A guest could drive the owner's automation DAG.
            "run the runbook morning",
            "run runbook morning",
            "execute the runbook deploy",
            // PLAN is read-only but still leaks the owner's runbook structure.
            "plan the runbook morning",
            "preview runbook deploy",
        ] {
            assert!(
                super::guest_denied_fast_path(u, &cfg).is_some(),
                "{u:?} must be refused for a guest (owner-data or consequential fast path)"
            );
        }
        // PRECONDITION for the runbook cases above: they really do classify as
        // runbook commands (so the assertion is testing the mirror, not a typo
        // that happens to be caught by some other arm).
        assert!(
            crate::runbook::classify_runbook_command("run the runbook morning").is_some(),
            "precondition: this utterance must reach the runbook arm in route()"
        );
        assert!(
            crate::macros::classify_macro_command("run the runbook morning").is_none(),
            "precondition: the macro classifier does NOT already cover runbooks"
        );
        // Guest-safe turns -> None (they flow to the guest-gated conversational path).
        for u in [
            "hello, how are you",
            "translate good morning into french",
            "what's the weather like",
            "tell me a joke",
        ] {
            assert!(
                super::guest_denied_fast_path(u, &cfg).is_none(),
                "{u:?} is guest-safe and must fall through"
            );
        }
    }

    #[test]
    fn guest_denied_fast_path_does_not_mutate_policy() {
        // REGRESSION: the fast-path gate uses the PURE `classify_policy_command`, NOT
        // `handle_user_policy_text` (which APPLIES the rule). Probing a guest's policy
        // utterance must classify it as denied WITHOUT writing any policy.
        let cfg = Config::default();
        assert!(
            super::guest_denied_fast_path("always allow the shell_run action", &cfg).is_some(),
            "a policy utterance is refused for a guest"
        );
        // The pure classifier used by the gate matches; the mutating handler was never
        // called (nothing to assert on global policy here beyond no panic / no write —
        // the point is the gate never routes through the applying path).
    }

    #[tokio::test]
    async fn guest_recall_and_history_feeds_are_empty_owner_feeds_are_full() {
        // FINDING 1 (feeds): a GUEST turn's auto RAG feed AND conversation history are
        // WITHHELD entirely — a bystander's prompt carries none of the owner's stored
        // facts or prior dialogue. The owner path is byte-for-byte today's.
        let db = TempDb::new("guest-feeds");
        let mem = Memory::open(&db.0).unwrap();
        mem.upsert_fact("user.name", "Darwin").await.unwrap();
        mem.upsert_fact("user.model.diet", "vegetarian").await.unwrap();
        mem.record_transcript(None, "what's my name", "conversation", "local", Some("You're Darwin."))
            .await
            .unwrap();

        // GUEST: both feeds are empty.
        {
            let _o = crate::threshold::ScopeOverride::guest(guest_scope_fixture());
            assert!(super::agent_facts(&mem, "agent.darwin").await.is_empty(), "guest RAG feed is empty");
            assert!(super::fetch_history(&mem).await.is_empty(), "guest history feed is empty");
        }
        // OWNER: both feeds carry the owner's data (unchanged).
        {
            let _o = crate::threshold::ScopeOverride::owner();
            let facts = super::agent_facts(&mem, "agent.darwin").await;
            assert!(facts.iter().any(|(k, _)| k == "user.name"), "owner RAG feed carries facts");
            assert!(!super::fetch_history(&mem).await.is_empty(), "owner history feed carries the exchange");
        }
    }

    #[tokio::test]
    async fn router_notebook_utterance_saves_then_revisits_the_real_run() {
        let db = TempDb::new("notebook");
        let mem = Memory::open(&db.0).unwrap();
        let ns = "agent.darwin";

        // A real SAGE run just completed (the live path records exactly this).
        let _g = crate::notebook::LastRunGuard::stage(Some(crate::notebook::LastResearchRun {
            topic: "the JWST".into(),
            report: synthetic_report(),
            synthesized: "On the JWST [1]".into(),
        }));

        // "save this research" -> classify -> dispatch (the route() composition).
        let intent = crate::notebook::classify_notebook_intent("save this research")
            .expect("an explicit save utterance classifies as a notebook intent");
        let out = crate::notebook::dispatch(&mem, ns, intent).await.unwrap();
        assert_eq!(out.verb, "saved", "the utterance persisted the real last run");

        // "show my research notebook on the JWST" -> revisit returns it, citing the
        // real grounded source ONLY (never the phantom).
        let intent = crate::notebook::classify_notebook_intent(
            "show my research notebook on the JWST",
        )
        .expect("a revisit utterance classifies");
        let out = crate::notebook::dispatch(&mem, ns, intent).await.unwrap();
        assert_eq!(out.verb, "revisit");
        assert!(out.reply.contains("https://a.test"), "the real source surfaces: {}", out.reply);
        assert!(!out.reply.contains("999"), "a fabricated citation must never surface: {}", out.reply);
    }

    #[tokio::test]
    async fn router_report_utterance_builds_from_the_saved_cited_runs() {
        // The route() composition for #40: an explicit "generate a report on X"
        // utterance classifies, and dispatch (with the op enabled) pulls the
        // agent-scoped saved cited runs on X and assembles a bounded report citing
        // ONLY their real grounded sources. Hermetic: temp Db + a synthetic run.
        let db = TempDb::new("report");
        let mem = Memory::open(&db.0).unwrap();
        let ns = "agent.darwin";
        // A real cited run is already saved on the topic (the notebook path enforces
        // that only the grounded source #1 persists — never the phantom 999).
        crate::notebook::save_run(&mem, ns, "the JWST", &synthetic_report(), "On the JWST [1]")
            .await
            .unwrap();

        let intent = crate::report::classify_report_intent("generate a report on the JWST")
            .expect("an explicit report utterance classifies");
        let on = crate::report::ReportConfig { enabled: true };
        let out = crate::report::dispatch(&mem, ns, intent, &on).await.unwrap();
        assert_eq!(out.verb, "report", "the report was built from the saved run");
        // The markdown cites ONLY the real grounded source, never the phantom. The
        // title is the normalized (lowercased) topic the intent carried.
        assert!(out.markdown.contains("# the jwst"), "title rendered: {}", out.markdown);
        assert!(out.markdown.contains("https://a.test"), "the real source surfaces: {}", out.markdown);
        assert!(!out.markdown.contains("999"), "a fabricated citation must never surface: {}", out.markdown);
        let report = out.report.unwrap();
        assert_eq!(report.all_citations.len(), 1, "only the grounded citation");
    }

    #[tokio::test]
    async fn router_report_when_disabled_declines_and_reads_nothing() {
        // With the op explicitly DISABLED (an operator override; the shipped default
        // is ON) dispatch declines honestly and reads nothing.
        let db = TempDb::new("report-off");
        let mem = Memory::open(&db.0).unwrap();
        let ns = "agent.darwin";
        let intent = crate::report::classify_report_intent("generate a report on anything")
            .expect("classifies");
        let off = crate::report::ReportConfig { enabled: false };
        assert!(!off.enabled, "explicitly disabled");
        let out = crate::report::dispatch(&mem, ns, intent, &off).await.unwrap();
        assert_eq!(out.verb, "report_off", "the disabled op declines");
        assert!(out.report.is_none(), "nothing was built");
        assert!(out.markdown.to_lowercase().contains("off"), "{}", out.markdown);
    }

    #[tokio::test]
    async fn router_report_unknown_topic_is_honest_empty() {
        let db = TempDb::new("report-empty");
        let mem = Memory::open(&db.0).unwrap();
        let ns = "agent.darwin";
        let intent = crate::report::classify_report_intent("write a report on a topic never researched")
            .expect("classifies");
        let on = crate::report::ReportConfig { enabled: true };
        let out = crate::report::dispatch(&mem, ns, intent, &on).await.unwrap();
        assert_eq!(out.verb, "report_empty", "no saved cited run -> honest empty");
        assert!(out.markdown.to_lowercase().contains("no sources to report on"), "{}", out.markdown);
    }

    #[test]
    fn classify_music_intent_extracts_the_prompt_on_creation_requests() {
        use super::classify_music_intent as c;
        // The flagship: "compose" anchors alone (no "song" noun). The verb +
        // leading article are stripped, leaving the cleaned prompt.
        assert_eq!(
            c("DARWIN, compose an 8-bit happy birthday").as_deref(),
            Some("8-bit happy birthday")
        );
        assert_eq!(c("compose an 8-bit happy birthday").as_deref(), Some("8-bit happy birthday"));
        // "about/of" tails unwrap to the descriptor.
        assert_eq!(c("compose a song about the rain").as_deref(), Some("the rain"));
        assert_eq!(c("write me a tune about my dog").as_deref(), Some("my dog"));
        assert_eq!(c("generate a beat of pure 90s house").as_deref(), Some("pure 90s house"));
        // Broad verbs WITH a music object noun match.
        assert!(c("make me a jingle for my coffee shop").is_some());
        assert_eq!(c("make me a jingle for my coffee shop").as_deref(), Some("my coffee shop"));
        assert!(c("produce a melody that goes da da dum").is_some());
        // "play me a <object>" is a creation ask.
        assert!(c("play me a track in the style of lo-fi").is_some());
        // A bare creation request with nothing described falls back to a non-empty
        // generic prompt (never an empty string the op can't compose).
        let bare = c("compose a song").expect("bare compose still matches");
        assert!(!bare.is_empty(), "bare compose must yield a non-empty prompt");
        // REGRESSION: a music-object noun that is only a PREFIX of a longer word must
        // NOT be stripped — "beatles" must survive (the bug stripped the bare "beat"
        // lead -> "les song").
        assert_eq!(c("compose a beatles song").as_deref(), Some("beatles song"));
    }

    #[test]
    fn classify_music_intent_rejects_non_music_speech() {
        use super::classify_music_intent as c;
        // No creation verb -> not music (the critical anti-over-trigger case).
        assert!(c("play some jazz").is_none());
        assert!(c("play the latest taylor swift").is_none());
        assert!(c("turn up the music").is_none());
        assert!(c("what's the time").is_none());
        assert!(c("what's the cpu usage").is_none());
        assert!(c("how's the weather today").is_none());
        // Broad creation verbs WITHOUT a music object are NOT music.
        assert!(c("make me a sandwich").is_none());
        assert!(c("write me an email to my boss").is_none());
        assert!(c("generate a report on the JWST").is_none());
        assert!(c("produce the quarterly numbers").is_none());
        // "play me ..." without a music object noun is not music.
        assert!(c("play me the news").is_none());
        // Casual mention of a song without a creation verb is not music.
        assert!(c("i love that song").is_none());
        assert!(c("what song is this").is_none());
        // Empty / whitespace.
        assert!(c("").is_none());
        assert!(c("   ").is_none());
        // REGRESSION: the OBJECT side used to be a bare `contains`, so an ordinary
        // request that merely CONTAINS a music noun inside a longer word — or uses
        // one as a compound modifier — was swallowed by JEROME. "write down my
        // tracking number" was composed as a song called "down my tracking number"
        // and the note was silently lost.
        assert!(c("write down my tracking number").is_none());
        assert!(c("make a note about the heartbeat monitor").is_none());
        assert!(c("write a note about my track record").is_none());
        assert!(c("generate the beaten path itinerary").is_none());
        assert!(c("make a note of the fortune 500 list").is_none());
        assert!(c("write down the tuner settings").is_none());
        assert!(c("write up the sprint retrospective").is_none());
    }

    #[test]
    fn router_chart_intent_emits_the_exact_snapshot_points() {
        // The route() composition for #41: a "chart this" utterance classifies, the
        // latest REAL snapshot becomes a ChartSpec of the EXACT cpu/mem values, and
        // emit_chart publishes the chart.data envelope. Hermetic: an injected
        // snapshot-shaped spec + the test telemetry seam (no WS client, no network).
        assert!(crate::chart::classify_chart_intent("chart this").is_some());
        let snap = crate::telemetry::SystemSnapshot {
            cpu_percent: 25.0,
            mem_used_bytes: 2_000_000_000,
            mem_total_bytes: 8_000_000_000,
            disk_free_bytes: None,
            disk_total_bytes: None,
            uptime_secs: 10,
        };
        let spec = crate::chart::chart_from_snapshot(Some(snap));
        let mut rx = crate::telemetry::subscribe_for_test();
        // WHAT WENT WRONG: the drain below used to break on the FIRST `chart.data`
        // frame, and its comment justified that with "the only chart.data emitter
        // in a test run". That was false — chart.rs's
        // `emit_chart_publishes_a_chart_data_envelope` publishes a 3-point
        // `line_spec()` on this same process-global bus, and under `cargo test
        // chart` that sibling's frame could be buffered ahead of ours. The test
        // then asserted against a DIFFERENT test's chart: it failed with
        // "left: 3, right: 2" on a defect that did not exist, and — far worse —
        // could silently PASS on a foreign frame, so a real regression in the
        // router's chart composition would go unnoticed.
        //
        // Rather than leave that to scheduling luck, plant the decoy ourselves:
        // emit a frame shaped exactly like the sibling's BEFORE our own, so the
        // drain is always forced to skip a foreign `chart.data`. The loop now
        // identifies OUR frame by the snapshot chart's title/label (never by the
        // points, which are the thing under test).
        crate::chart::emit_chart(&crate::chart::ChartSpec::new(
            crate::chart::ChartKind::Line,
            vec![crate::chart::ChartSeries::new(
                "cpu",
                vec![(0.0, 12.0), (1.0, 30.5), (2.0, 18.0)],
            )],
            "t (s)",
            "cpu %",
            "CPU over time",
        ));
        crate::chart::emit_chart(&spec);
        // The telemetry hub is a SHARED broadcast bus, so under parallel test load
        // OTHER tests' frames interleave into this receiver. Drain and pick OUR
        // `chart.data` frame — the snapshot chart, identified by its title and
        // series label — instead of assuming it arrives first.
        let mut env: Option<serde_json::Value> = None;
        for _ in 0..512 {
            match rx.try_recv() {
                Ok(raw) => {
                    let e: serde_json::Value = serde_json::from_str(&raw).unwrap();
                    if e["event"] == "chart.data"
                        && e["data"]["title"] == "System load"
                        && e["data"]["series"][0]["label"] == "load %"
                    {
                        env = Some(e);
                        break;
                    }
                }
                // A lagged receiver dropped some frames under load — keep draining.
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                // Empty or Closed: our synchronous emit is already buffered, so if we
                // reach here without finding it that IS a real failure.
                Err(_) => break,
            }
        }
        let env = env.expect("a chart.data envelope was published");
        assert_eq!(env["event"], "chart.data");
        let pts = env["data"]["series"][0]["points"].as_array().unwrap();
        // EXACTLY the two real metrics: cpu 25 at x=0, mem 25% at x=1.
        assert_eq!(pts.len(), 2, "exactly the snapshot metrics: {pts:?}");
        assert_eq!(pts[0], serde_json::json!([0.0, 25.0]));
        assert_eq!(pts[1], serde_json::json!([1.0, 25.0]));
    }

    #[tokio::test]
    async fn router_lifelog_utterance_builds_the_real_digest() {
        let db = TempDb::new("lifelog");
        let mem = Memory::open(&db.0).unwrap();
        let ns = "agent.darwin";
        crate::episodic::record_episode(
            &Config::default(),
            &mem,
            ns,
            "worked on the rocket engine design",
            "ok",
            "code",
            false,
            crate::episodic::VoiceGate { enabled: false, enrolled: false, owner_verified: false },
        )
        .await
        .unwrap();

        // "what did I do this week" -> classify -> dispatch (the route() composition).
        let intent = crate::lifelog::classify_lifelog_intent("what did I do this week")
            .expect("a life-log utterance classifies");
        let reply = crate::lifelog::dispatch(&mem, ns, intent).await;
        assert!(reply.contains("1 recorded turn"), "names the real count: {reply}");
        assert!(reply.contains("rocket"), "names a real theme from the episode: {reply}");

        // An EMPTY store yields an HONEST empty digest (never a fabricated event) —
        // a fresh Db with nothing logged.
        let empty_db = TempDb::new("lifelog-empty");
        let empty_mem = Memory::open(&empty_db.0).unwrap();
        let intent = crate::lifelog::classify_lifelog_intent("what did I do today").unwrap();
        let reply = crate::lifelog::dispatch(&empty_mem, ns, intent).await;
        assert!(reply.to_lowercase().contains("nothing logged"), "honest empty: {reply}");
    }

#[cfg(test)]
mod describe_capture_tests {
    /// capture_screen_frame must WAIT for the Vision app and must not accept a frame
    /// the app did not just write.
    ///
    /// The shipped version did a fire-and-forget send_op and then checked
    /// frame.exists() on the very next line with no await between them. The first
    /// screen question therefore always failed — with the untrue reason "Screen
    /// Recording consent is needed on-device" — and every later one could describe the
    /// leftover frame from a previous capture as though it were the current screen.
    /// The function's CODE, with comment lines stripped.
    ///
    /// Stripping matters: this function's comments discuss the very bug being pinned
    /// ("this used to be a fire-and-forget send_op"), so a naive substring check
    /// reports the fixed code as still broken. An assertion that cannot tell prose
    /// from code is not an assertion.
    fn body() -> String {
        let src = include_str!("router.rs");
        let i = src
            .find("async fn capture_screen_frame(")
            .expect("capture_screen_frame moved; re-point this guard");
        let rest = &src[i..];
        // End at the function's own closing brace at column 0.
        let end = rest.find("\n}\n").map(|e| e + 2).unwrap_or(rest.len());
        rest[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_capture_waits_for_the_frame_it_asked_for() {
        // The first version fired and forgot, then checked existence on the next line.
        // The SECOND version waited on apps::request_op — which only resolves on a
        // {"type":"result","id":...} line, and the Vision app's RelayType is
        // items|status|log|modules with no result case at all. So every screen question
        // stalled for the whole APP_REQUEST_TIMEOUT and then failed: a fast wrong
        // answer turned into a slow one. The app cannot reply, but the FRAME is the
        // result, so we poll for it under a deadline.
        let b = body();
        assert!(
            b.contains("CAPTURE_FRAME_TIMEOUT"),
            "the capture does not bound its wait; a Vision app that never writes the \
             frame would hang the request"
        );
        assert!(
            !b.contains("apps::request_op("),
            "capture_screen_frame waits on request_op, which the Vision app has no \
             message type to satisfy — every screen question stalls until timeout"
        );
        assert!(
            b.contains("apps::send_op("),
            "the op must still be forwarded to the app"
        );
    }

    #[test]
    fn the_frame_lives_inside_the_apps_write_grant() {
        // apps/vision/manifest.toml grants fs_write = ["state/tmp/vision"], and
        // generate_sbpl emits exactly one (allow file-write* (subpath ...)) per entry.
        // The frame path was state/vision/, outside that subpath, so the app was
        // seatbelt-denied and could never have written it on any run.
        let manifest = include_str!("../../apps/vision/manifest.toml");
        let grant = manifest
            .lines()
            .find(|l| l.trim_start().starts_with("fs_write"))
            .expect("apps/vision/manifest.toml lost its fs_write line");
        assert!(
            grant.contains("state/tmp/vision"),
            "the Vision write grant moved; re-point the frame path: {grant}"
        );
        let b = body();
        assert!(
            b.contains(r#".join("tmp")"#),
            "the describe frame is written outside the Vision app's only fs_write \
             grant, so the capture is seatbelt-denied"
        );
    }

    #[test]
    fn a_stale_frame_is_removed_before_the_capture() {
        let b = body();
        let removed = b
            .find("remove_file")
            .expect("the previous frame is not deleted, so a stale screen can be \
                    described as the current one");
        let checked = b.find("frame.exists()").expect("existence check vanished");
        assert!(
            removed < checked,
            "the stale frame must be removed BEFORE the freshness check, or the check \
             passes on the old file"
        );
    }

    #[test]
    fn the_reply_is_awaited_before_the_existence_check() {
        let b = body();
        let sent = b.find("apps::send_op(").unwrap();
        let exists = b.find("frame.exists()").unwrap();
        assert!(
            sent < exists,
            "the frame is checked before the app is asked to write it"
        );
        assert!(
            b[sent..exists].contains(".await"),
            "the send is not awaited before the wait loop, so the race is back"
        );
        assert!(
            b.contains("CAPTURE_FRAME_POLL"),
            "the wait must actually sleep between checks rather than spin"
        );
    }
}
}
#[cfg(test)]
mod budget_and_audit_tests {
    use super::*;
    use crate::obol::Pressure;

    /// The dollar cap applied to CHAT and not to ACTIONS — the turns that spend most.
    #[test]
    fn the_actuating_path_steps_down_under_budget_pressure() {
        let cfg = Config::default();
        // No cap configured (the shipped default): unchanged routing.
        assert_eq!(
            cloud_model_under_budget(true, &cfg, Pressure::None),
            cfg.cloud.heavy_model,
            "with no cap the heavy turn must still buy the heavy model"
        );
        // Ease: a heavy turn steps down.
        assert_eq!(
            cloud_model_under_budget(true, &cfg, Pressure::Ease),
            cfg.cloud.fast_model,
            "at the ease shoulder a heavy ACTION turn must step down, as chat does"
        );
        // Floor: pinned to the cheapest cloud brain.
        assert_eq!(
            cloud_model_under_budget(true, &cfg, Pressure::Floor),
            cfg.cloud.fast_model,
            "over the cap a heavy ACTION turn must not still buy the heavy model"
        );
    }

    #[test]
    fn a_light_turn_is_unaffected_by_pressure() {
        let cfg = Config::default();
        for p in [Pressure::None, Pressure::Ease, Pressure::Floor] {
            assert_eq!(cloud_model_under_budget(false, &cfg, p), cfg.cloud.fast_model);
        }
    }

    /// Pressure is REDUCE-ONLY: it may never pick a more expensive model.
    #[test]
    fn pressure_never_upgrades_the_model() {
        let cfg = Config::default();
        let none = cloud_model_under_budget(true, &cfg, Pressure::None);
        for p in [Pressure::Ease, Pressure::Floor] {
            let under = cloud_model_under_budget(true, &cfg, p);
            assert!(
                under == cfg.cloud.fast_model || under == none,
                "budget pressure selected a model that is not a step down"
            );
        }
    }

    /// A DENIAL is a decision and must be recorded. The audit log stopped at "parked",
    /// so a reader could not tell a refused action from one never answered.
    ///
    /// AMENDMENT — THE SECOND GUARD WITH `emit_payloads`'s SHAPE, and the worse of
    /// the two. The residual sweep that hardened `confirm.rs`'s webhook park guard
    /// reported that exactly ONE other guard in the tree read a first occurrence and
    /// windowed a fixed number of bytes after it. This is the other one, and unlike
    /// confirm.rs's it did not merely go quiet on a second site — it FAILED OPEN on
    /// the very edit it exists to catch.
    ///
    /// The old form searched the WHOLE file for `Resolution::Cancelled(ack) =>`.
    /// That needle occurs TWICE in router.rs: at the arm it guards, and on this
    /// test's own `.find(..)` line ~13,700 lines below it. The 900-byte window opened
    /// at the SECOND one contains this test's own
    /// `arm.contains("audit::Outcome::Denied")`. MEASURED: rename the production
    /// arm's binding (`Cancelled(ack)` -> `Cancelled(reason)`) and the old form still
    /// PASSES — `find` slides onto the test's own source, the `expect` never fires,
    /// and the guard reports green while the audit record it protects is gone. Two
    /// of this campaign's standing traps at once: a first-occurrence window, and a
    /// source-anchored guard that self-matches.
    ///
    /// Bounded at BOTH ends: the window is router.rs up to its FIRST `#[cfg(test)]`
    /// (line 9705, ~4,200 lines above this test), so this test's own text is out of
    /// scope and cannot stand in for the arm; both halves are floored so neither a
    /// marker at byte 0 nor one at EOF can make this vouch for nothing. And EVERY arm
    /// in that window is checked, not the first — a second cancel arm that forgot the
    /// audit record is exactly the site the old form was blind to.
    #[test]
    fn the_cancel_path_records_a_denial() {
        let src = include_str!("router.rs");
        let cut = src.find("#[cfg(test)]").expect("router.rs has a test seam");
        let prod = &src[..cut];
        assert!(
            prod.len() > 100_000,
            "the router.rs production window collapsed: {}",
            prod.len()
        );
        assert!(
            src.len() - cut > 10_000,
            "the #[cfg(test)] marker matched too late — the window swallowed the tests"
        );
        let sites: Vec<usize> = prod
            .match_indices("Resolution::Cancelled(ack) =>")
            .map(|(i, _)| i)
            .collect();
        assert!(
            !sites.is_empty(),
            "the cancel arm left router.rs's production half; re-point this guard \
             rather than letting it pass over zero arms"
        );
        for i in sites {
            let arm = &prod[i..(i + 900).min(prod.len())];
            assert!(
                arm.contains("audit::Outcome::Denied"),
                "a denied consequential action leaves no audit entry (router.rs:{})",
                prod[..i].lines().count()
            );
        }
    }
}
