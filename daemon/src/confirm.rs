//! Cross-turn SPOKEN confirmation gate for consequential actions.
//!
//! This is the SECOND factor that sits ABOVE the armed-by-default master switch
//! ([`crate::integrations::consequential_allowed`], which ships ON). The master switch decides
//! whether outward actions are permitted AT ALL; this module decides whether a
//! specific, named action the model proposed is actually allowed to fire — and
//! the ONLY thing that lets it fire is a real human saying "yes" on a LATER
//! turn. The model's own `confirm` flag no longer executes anything; it is the
//! spoken human affirmation, classified here, that does.
//!
//! ## Shape
//!
//!  * [`classify_confirmation`] — a PURE classifier mapping a free-form spoken
//!    utterance to [`ConfirmReply`] (`Affirm` / `Deny` / `Unrelated`). It is
//!    deliberately CONSERVATIVE: any sign the user is qualifying, hedging,
//!    changing, or retracting the action ("yes but change the title", "no wait
//!    yes") classifies as `Unrelated`/`Deny`, NEVER a clean affirm — a stale
//!    action the user is trying to modify must never be executed as-is.
//!
//!  * [`PendingConfirmation`] + a process-global single-slot store. When a
//!    consequential tool is invoked while the master switch is ON, the action
//!    is NOT executed: it is PARKED here together with the faithful dry-run
//!    PREVIEW and the invoking agent's allowlist, and the model is handed a
//!    confirmation prompt as the tool outcome. A new consequential invocation
//!    REPLACES any existing pending (single slot — only the most recent
//!    proposal can be confirmed).
//!
//!  * The router pre-check (in [`crate::router`]) consults [`take_live`] at the
//!    top of each turn: an `Affirm` REPLAYS the EXACT parked `{tool,input}` in
//!    Execute mode (re-deriving nothing from the new utterance, still honoring
//!    the agent allowlist + master switch); a `Deny` cancels; an `Unrelated`
//!    cancels (so a stray later command can never be mistaken as confirming a
//!    stale action) and falls through to normal routing.
//!
//! Everything here is HERMETIC: the store is in-process, the clock is
//! injectable for expiry tests, and nothing touches the network, the brain, or
//! a real client. The actuation itself happens back in `anthropic::execute_tool`
//! via the injectable replay seam, never inside this module.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// How long a parked confirmation stays live. After this, a "yes" no longer
/// confirms it — the user has to ask again. Long enough for a natural pause and
/// a re-read of the preview; short enough that an action can't lurk for minutes
/// waiting to be accidentally triggered by an unrelated affirmation.
pub const PENDING_TTL: Duration = Duration::from_secs(120);

/// The classification of a spoken reply, in the context of a live pending
/// confirmation. Only [`ConfirmReply::Affirm`] ever executes the parked action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmReply {
    /// A clean, unqualified yes to THIS action ("yes", "do it", "ship it").
    Affirm,
    /// An explicit no / stop / wait / retract ("no", "cancel", "not yet").
    Deny,
    /// Anything else — a new request, a question, a qualified/hedged reply, or
    /// silence-of-intent. Treated as "drop the unconfirmed action and route
    /// this utterance normally": NEVER as a confirmation.
    Unrelated,
}

/// A consequential action awaiting a spoken human yes. Single-slot: at most one
/// of these is live at a time (see the module-global store below).
#[derive(Debug, Clone)]
pub struct PendingConfirmation {
    /// The agent (memory namespace, e.g. "agent.pepper") that proposed the
    /// action — carried so the replay records under the same namespace and the
    /// telemetry/transcript stays per-agent.
    pub agent: String,
    /// The tool the action would invoke (e.g. "gmail_send").
    pub tool: String,
    /// The EXACT arguments to replay. The replay runs these, never anything
    /// re-derived from the confirming utterance — so "yes" can only ever fire
    /// precisely what was previewed.
    pub input: Value,
    /// The agent's allowlist at park time. The replay re-checks this, so an
    /// action can never be confirmed into existence for an agent that may not
    /// use the tool (defense in depth: `execute_tool` checks it again too).
    pub allowed: Vec<String>,
    /// The faithful dry-run preview shown to the user (names repo/recipient/
    /// amount/device precisely). Replayed into the spoken confirmation prompt.
    pub preview: String,
    /// When this was parked, for TTL expiry (injectable clock in tests).
    pub created_at: Instant,
    /// Stable, content-derived id minted at park time. It is what the
    /// authenticated-local command channel (`command.rs`) names when it
    /// `confirm {id}` / `deny {id}` a parked action — the equivalent of the
    /// spoken "confirm", but addressed to a SPECIFIC parked action by id so a
    /// stale or fabricated id can never fire anything. Derived from the action's
    /// own bytes (agent || tool || canonical(input)) so it is unforgeable
    /// without already knowing the exact parked action, and stable across a
    /// `pending` listing and the follow-up `confirm`.
    pub id: String,
    /// PLAN-APPLY (plan.rs): the structured, STATE-BOUND diff computed at park time
    /// for a tool with a per-tool planner (connector_add / standing_create), or
    /// `None` for a tool that falls back to the text `preview` (unchanged path).
    /// When present it carries the `state_hash` of the relevant state at park time;
    /// the confirm path (`anthropic::replay_confirmed_action`) recomputes that hash
    /// and RE-PARKS on drift, so a plan is a purely ADDITIVE precondition — it never
    /// replaces the voice-id / master-switch / allowlist checks below. BOXED so a
    /// plan-carrying pending stays small in the single-slot store and the enums that
    /// wrap it (`ByIdConfirm`, `Resolution`) keep their tight variant sizes.
    pub plan: Option<Box<crate::plan::Plan>>,
}

/// Derive the stable content id for a parked action: a short hex prefix of
/// SHA-256 over `agent || NUL || tool || NUL || canonical(input)`. Pure — the
/// command channel computes the SAME id when it lists `pending`, so the id the
/// HUD echoes back on `confirm {id}` matches iff it names the genuinely-parked
/// action. Re-deriving from content (not a random counter) means an attacker
/// would have to already know the exact agent+tool+input to name a valid id,
/// and the by-id confirm still re-checks the slot, so the id is an addressing
/// label, never an authorization.
pub fn derive_pending_id(agent: &str, tool: &str, input: &Value) -> String {
    let mut h = Sha256::new();
    h.update(agent.as_bytes());
    h.update([0u8]);
    h.update(tool.as_bytes());
    h.update([0u8]);
    // serde_json's Value Serialize is deterministic for a given Value (object
    // keys are emitted in their stored order); two identical parked actions
    // produce the same canonical bytes and hence the same id.
    h.update(input.to_string().as_bytes());
    let digest = h.finalize();
    // 16 hex chars (64 bits) — ample to address the single live slot without a
    // collision and short enough to speak/echo.
    hex::encode(&digest[..8])
}


/// Process-global single slot. `None` = nothing awaiting confirmation. A plain
/// `Mutex<Option<_>>` (held only across a clone/replace) mirrors the daemon's
/// other small bits of shared state (e.g. `actions::APP_CACHE`).
static PENDING: Mutex<Option<PendingConfirmation>> = Mutex::new(None);

/// Test-only: one shared serialization lock for EVERY test (in any module) that
/// touches the process-global `PENDING` slot. Cargo runs tests across modules
/// concurrently, so a lock private to one module cannot stop a `command::tests`
/// case from racing a `confirm::tests` case on the single global slot. Both
/// modules acquire THIS lock (poison-tolerant) so such tests serialize.
#[cfg(test)]
pub(crate) static PENDING_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Lock the store, recovering from a poisoned mutex rather than panicking — a
/// parked confirmation must never wedge the daemon.
fn lock() -> std::sync::MutexGuard<'static, Option<PendingConfirmation>> {
    PENDING.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The id of the action whose prompt was last handed to a turn — i.e. the one
/// the user was actually asked about. Kept separate from [`PENDING`] so a
/// background parker overwriting the slot cannot also rewrite what the user was
/// shown. Poison-tolerant like the slot itself.
static PROMPTED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn prompted_lock() -> std::sync::MutexGuard<'static, Option<String>> {
    PROMPTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The complete set of CONSEQUENTIAL (side-effecting) tools — every tool whose
/// `execute_tool` arm routes through `integrations::gate(confirm)`. This is the
/// single source of truth for "does this invocation need a spoken yes"; the
/// `consequential_registry_is_complete_and_exact` test pins this set (exactly 22
/// entries, no dupes) so it stays in lockstep with the gate call sites and a
/// newly-gated tool can't silently skip the confirmation layer.
///
/// `standing_create` is gated here for a DIFFERENT reason than the integration
/// tools: it does not POST or SPEND, but ESTABLISHING a standing mission spawns
/// recurring autonomy, which is itself a consequential decision DARWIN must never
/// make on a low-confidence guess. Routing it through the SAME cross-turn gate
/// means a create PARKS for a spoken human "yes" instead of silently creating a
/// recurring mission. (`standing_list` / `standing_cancel` are read-only /
/// reversible and are NOT gated.)
///
/// `shell_run` (the sandboxed shell / terminal #43) is the MOST consequential tool
/// of all — arbitrary command execution — so it is gated here unconditionally: an
/// un-confirmed shell command PARKS and NEVER auto-runs. (A destructive/denylisted
/// command is refused PRE-exec by `shell::classify_shell_command` and never even
/// reaches the park; the actual exec only happens under the master switch +
/// confirm + voice-id + !lockdown, inside a deny-default sandbox-exec profile.)
///
/// `ui_actuate` (gated UI automation #44, the CAPSTONE) is the single most
/// DANGEROUS tool — it physically ACTUATES the macOS UI (a click / type / key) —
/// so it is gated here PER ACTION: ONE confirm authorizes EXACTLY ONE actuation.
/// An un-confirmed actuation PARKS and NEVER auto-runs; a SECOND actuation re-parks
/// for its OWN spoken yes (the first confirm does NOT carry over) — there is no
/// path that batches actuations or loops autonomously. The actual CGEvent/AX post
/// only happens under the master switch + confirm + voice-id + !lockdown, AND the
/// device Accessibility-TCC consent (runtime user consent, not SBPL-grantable).
pub const CONSEQUENTIAL_TOOLS: &[&str] = &[
    "github_comment_issue",
    "github_open_pr",
    "slack_post_message",
    "gcal_create_event",
    "gmail_send",
    "gdrive_upload_text",
    "x_post",
    "linkedin_post",
    "gads_pause_campaign",
    "gads_enable_campaign",
    "gads_set_budget",
    "meta_pause_campaign",
    "meta_resume_campaign",
    "meta_set_budget",
    "dume_control",
    // Establishing a standing mission = spawning recurring autonomy -> gated.
    "standing_create",
    // The sandboxed shell: arbitrary command execution, the MOST consequential
    // tool. It ALWAYS parks for a spoken yes; it never auto-runs.
    "shell_run",
    // Gated UI automation (#44, the CAPSTONE): physically ACTUATING the macOS UI
    // (click/type/key) — the single most DANGEROUS tool. It parks PER ACTION (ONE
    // confirm = ONE actuation; a second re-parks); it never auto-runs, never
    // batches, never loops. The actuation itself is device-gated (Accessibility
    // TCC consent + a real display) and built-not-run.
    "ui_actuate",
    // Adding an MCP connector = a persistent mutation of the machine's tool
    // surface (a vetted [[mcp.servers]] entry written to darwin.toml). It ALWAYS
    // parks for a spoken yes on the exact spec; it never auto-applies. It handles
    // NO secret (the token goes to the Keychain out-of-band) and the connector is
    // added INERT (agents=[], every tool gated).
    "connector_add",
    // Guided remediation for the Inbound Exposure Auditor: it deep-links to ONE
    // allowlisted System Settings pane so the USER can flip a switch. It changes
    // no setting itself and only ever OPENS an allowlisted `x-apple.system
    // preferences:` pane — but opening a settings pane on the user's behalf is a
    // visible UI side effect, so it is gated PER ACTION like every other actuator:
    // it parks for a spoken yes and never auto-opens.
    "open_settings_pane",
    // Semantic Pasteboard SET (pasteboard.rs): pasteboard_put places text on the
    // macOS clipboard. BENIGN by construction — a pasteboard SET only, never a
    // keystroke, file mutation, or network call — but it is a VISIBLE side effect
    // on the user's clipboard, so it is gated PER ACTION: it parks for a spoken
    // yes and never auto-copies (one confirm authorizes exactly one set).
    "pasteboard_put",
    // Self-Forge: authoring a micro-app from a goal, then COMPILING AND RUNNING the
    // authored sources to validate them (cargo check + cargo test, or py_compile).
    // `cargo test` builds and EXECUTES model-written code, and cargo runs any
    // build.rs the model emitted. That is arbitrary code execution from a CLOUD
    // model's output — the same capability shell_run is gated for, arriving by a
    // path the user never typed.
    //
    // It was the only tool of that class that never parked, so a prompt-injected
    // tool continuation could reach it directly. The validation now also runs under
    // the deny-default seatbelt (forge.rs), but a human authorising the forge is the
    // gate that closes the injection path itself.
    "forge_app",
];

/// Whether a tool name is consequential (side-effecting) and therefore must be
/// parked for a spoken confirmation rather than executed on first call.
pub fn is_consequential_tool(name: &str) -> bool {
    CONSEQUENTIAL_TOOLS.contains(&name)
}

/// Park `pending` as THE single live confirmation, replacing any prior one.
/// Returns the spoken confirmation prompt to hand back as the tool outcome.
///
/// The stable content [`id`](PendingConfirmation::id) is (re)derived here from
/// the action's own bytes, so every parked action carries a correct id whatever
/// the caller put in the field — the command channel's `confirm {id}` / `deny
/// {id}` address the slot by exactly this id.
/// `reaches_user` says whether THIS park's prompt is actually spoken to the operator
/// this turn. Only such a park may arm PROMPTED.
///
/// THE GUARD USED TO BE A TAUTOLOGY. The comment below claimed a background parker
/// "can never be fired by that yes" — but park_ctx(true, ) armed PROMPTED unconditionally, so
/// every later parker simply re-pointed it at its own id and take_live's `p.id == id`
/// compared the slot against itself. The only code that modelled a background park was
/// a `#[cfg(test)]` function, so the guard was exercised solely through a test-only
/// entry point while production had the hole wide open: the user is prompted for
/// action ALPHA, a standing tick or overnight agent parks BETA (unattended runs are
/// deliberately downgraded Always->Ask, so they DO park), and the user's "confirm"
/// fires BETA — different agent, different arguments, never shown to them.
/// `reaches_user` is `context_trusted` at the tool paths: true across a live
/// interactive turn, false for a standing tick / tripwire / durable-mission resume /
/// inbound webhook / injected-content sub-task. It is exactly "did a human hear this".
///
/// It is the FIRST parameter, and there is no one-argument form, so a new call site
/// cannot park without answering the question.
pub fn park_ctx(reaches_user: bool, pending: PendingConfirmation) -> String {
    park_inner(pending, reaches_user)
}

fn park_inner(mut pending: PendingConfirmation, reaches_user: bool) -> String {
    pending.id = derive_pending_id(&pending.agent, &pending.tool, &pending.input);
    let prompt = confirmation_prompt(&pending.preview);
    // Remember WHICH action this prompt describes. The spoken "yes" is matched
    // against this id (see `take_live`), so a LATER parker — a standing order,
    // an overnight agent, any background turn — that overwrites the slot after
    // the user was prompted can never be fired by that "yes".
    if reaches_user {
        *prompted_lock() = Some(pending.id.clone());
    }
    *lock() = Some(pending);
    prompt
}


/// TEST-ONLY: park an action WITHOUT recording it as the prompted one — models a
/// background turn whose prompt never reached the user. Production reaches the same
/// state via `park_ctx(false, pending)` — two arguments, the reaches-user flag FIRST
/// (this used to name a nonexistent three-argument form with the flag INVERTED, i.e.
/// the PROMPTED-arming call that is the exact opposite of this state).
#[cfg(test)]
pub fn park_background_for_test(pending: PendingConfirmation) -> String {
    park_inner(pending, false)
}

/// Build the spoken confirmation prompt from a faithful dry-run preview. The
/// preview already names the action precisely; this appends the explicit
/// yes/no instruction the user answers on the NEXT turn.
///
/// The dry-run previews are authored for the OFF-switch path, so each carries a
/// `[dry run] ` lead-in and a trailing `Enable consequential actions and confirm
/// to <verb>.` hint. A parked confirmation, by contrast, is only ever reached
/// when the master switch is ALREADY on, so both of those are false/redundant
/// here: we strip them so the spoken prompt is just the faithful action
/// description plus the one say-confirm clause the user actually answers.
pub fn confirmation_prompt(preview: &str) -> String {
    // Drop the OFF-mode "[dry run] " lead-in if present.
    let preview = preview.strip_prefix("[dry run] ").unwrap_or(preview);
    // Drop the OFF-mode enablement hint ("...Enable consequential actions and
    // confirm to <verb>.") — it tells the user to flip a switch already flipped
    // and double-states the confirm instruction we append below. We split on the
    // boilerplate sentence rather than the verb so any phrasing is handled.
    let preview = preview
        .split(" Enable consequential actions")
        .next()
        .unwrap_or(preview);
    let preview = preview.trim_end_matches(['.', ' ']);
    format!("{preview} — say 'confirm' to proceed or 'cancel' to drop it.")
}

/// Is a (non-expired) confirmation currently parked? Side-effect-free except
/// that it clears a pending that has aged past [`PENDING_TTL`] (so a stale slot
/// never lingers). `now` is injectable for tests. Public predicate for reuse
/// (e.g. a future HUD "awaiting confirmation" indicator); the router itself
/// consumes via [`take_live`].
#[allow(dead_code)] // public predicate + expiry-peek; exercised by the unit tests
pub fn is_live(now: Instant) -> bool {
    let mut guard = lock();
    match guard.as_ref() {
        Some(p) if now.duration_since(p.created_at) <= PENDING_TTL => true,
        Some(_) => {
            // Aged out — drop it so a later "yes" can never revive it, AND drop the
            // prompt that named it (see `retire_slot`). Note this arm ONLY: on the
            // LIVE arm above PROMPTED must survive, because `run_pipeline` calls this
            // predicate on every single utterance.
            retire_slot(&mut guard);
            false
        }
        None => false,
    }
}

/// EMPTY THE SLOT AND THE PROMPT TOGETHER. The one place a pending is retired
/// outside a successful [`take_live`].
///
/// THE PAIRING IS THE INVARIANT, AND IT USED TO HOLD IN ONLY ONE DIRECTION.
/// [`take_live`]'s guard reasons that "nothing nulls PROMPTED without also nulling
/// the slot" — true, and it is what makes `slot == Some && PROMPTED == None`
/// diagnostic of an unattended park. But the CONVERSE was false: four paths emptied
/// the SLOT and left PROMPTED pointing at the retired action — [`is_live`] and
/// [`peek_pending`] on expiry, and [`confirm_by_id`] / [`deny_by_id`] on both their
/// matched and expired arms. `is_live` runs on EVERY utterance, so a prompt the user
/// simply never answered left a permanent stale PROMPTED id behind.
///
/// A stale PROMPTED is live authorization for one exact content id. Ids are derived
/// from `agent || tool || canonical(input)`, and a standing mission re-proposes the
/// SAME action verbatim on its next tick — so the background park that follows
/// (correctly `park_ctx(false, ..)`, arming nothing) matched the leftover id anyway,
/// and the operator's next "yes" to anything at all fired it. That is precisely the
/// hijack `take_live`'s `p.id == id` check exists to stop, reached through the
/// leftover instead of through PROMPTED being armed.
///
/// Clearing both is RESTRICT-ONLY: it can only turn a would-be match into a refusal.
/// A park the user was genuinely prompted with re-arms PROMPTED at `park_inner`.
fn retire_slot(guard: &mut std::sync::MutexGuard<'_, Option<PendingConfirmation>>) {
    **guard = None;
    // Same nesting order `take_live` already uses (PENDING outer, PROMPTED inner).
    *prompted_lock() = None;
}

/// ATOMICALLY take the live pending confirmation, clearing the slot. Returns
/// `None` when nothing is parked OR the parked action has expired (an expired
/// pending is dropped here too). The caller decides what to do based on the
/// classified reply — but whatever the reply, the slot is now empty, so a stale
/// action can never be confirmed twice or linger. `now` is injectable.
pub fn take_live(now: Instant) -> Option<PendingConfirmation> {
    // The id the user was actually prompted with. A slot holding a DIFFERENT
    // action (parked by a background turn after the prompt) is left INTACT and
    // not confirmed — the spoken "yes" answers the question that was asked, and
    // the background action keeps waiting for its own confirmation.
    let prompted = prompted_lock().clone();
    let mut guard = lock();
    let matches_prompt = match (guard.as_ref(), prompted.as_deref()) {
        (Some(p), Some(id)) => p.id == id,
        // NO PROMPT ON RECORD -> REFUSE. This used to fall through to `true`
        // ("take whatever is parked"), with a comment reasoning about "a
        // pre-existing park". There is no such state.
        //
        // PENDING is written in exactly ONE place (`park_inner`, below), and
        // PROMPTED is armed there too — but ONLY `if reaches_user`. Nothing nulls
        // PROMPTED without also nulling the slot: `clear()` nulls both, and a
        // successful `take_live` nulls both. So `slot == Some && PROMPTED == None`
        // holds if and only if the park DECLARED IT DID NOT REACH THE USER.
        //
        // The CONVERSE now holds too, and it did not always: every other path that
        // empties the slot (expiry in `is_live`/`peek_pending`, a by-id confirm or
        // deny) goes through `retire_slot`, which nulls PROMPTED with it. Without
        // that, a prompt the operator never answered outlived its action and
        // authorized the next park carrying the same content id — see `retire_slot`.
        //
        // Which means the fallback fired exactly and only in the case the guard
        // exists to catch. A standing-mission tick, tripwire, durable-mission
        // resume or runbook step parks something consequential — gmail_send,
        // shell_run — with arguments the operator was never read, and the
        // operator's next "yes" to anything at all executed it. The module header
        // and this function's own doc both state that cannot happen.
        //
        // Refusing here does not strand a real confirmation: a park the user was
        // actually prompted with always arms PROMPTED, so it matches on the arm
        // above. An unprompted park keeps waiting for its own explicit confirm.
        (Some(_), None) => false,
        _ => false,
    };
    if !matches_prompt {
        return None;
    }
    let taken = guard.take();
    *prompted_lock() = None;
    match taken {
        Some(p) if now.duration_since(p.created_at) <= PENDING_TTL => Some(p),
        // expired (or none): slot already cleared by `take()`.
        _ => None,
    }
}

/// Unconditionally clear any pending confirmation. Called from the shared
/// barge / roll-call-cancel lifecycle so an interrupted turn never leaves an
/// action armed.
pub fn clear() {
    *lock() = None;
    *prompted_lock() = None;
}

/// A faithful, read-only snapshot of the live pending action for the command
/// channel's `pending` listing: the addressing id, the agent/tool, and the
/// human-readable preview. Carries NO replay material (no input args) — the
/// channel never re-derives an action from a listing, it can only `confirm
/// {id}` the slot itself. Returns `None` when nothing is parked or the slot has
/// aged past the TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingView {
    pub id: String,
    pub agent: String,
    pub tool: String,
    pub preview: String,
}

/// Read-only peek at the live pending (if any, non-expired) for the `pending`
/// listing. Side-effect-free except that it drops an expired slot (same expiry
/// discipline as [`is_live`]). The injectable clock keeps the tests hermetic.
pub fn peek_pending(now: Instant) -> Option<PendingView> {
    let mut guard = lock();
    match guard.as_ref() {
        Some(p) if now.duration_since(p.created_at) <= PENDING_TTL => Some(PendingView {
            id: p.id.clone(),
            agent: p.agent.clone(),
            tool: p.tool.clone(),
            preview: p.preview.clone(),
        }),
        Some(_) => {
            retire_slot(&mut guard); // aged out — slot AND prompt
            None
        }
        None => None,
    }
}

/// The result of an authenticated-local `confirm {id}` over the command channel.
/// This is the channel's analogue of the spoken `Affirm` — but addressed to a
/// SPECIFIC parked action by id, so it can NEVER fabricate or replay an
/// arbitrary action.
#[derive(Debug)]
pub enum ByIdConfirm {
    /// The id named the genuinely-parked, non-expired action: here it is,
    /// already TAKEN from the slot (so it can't be confirmed twice). The caller
    /// replays it through the SAME `replay_confirmed_action` the spoken path
    /// uses — which re-checks the agent allowlist AND the master switch, so a
    /// switch-OFF confirm only previews and fires nothing.
    Matched(PendingConfirmation),
    /// No live pending, the slot expired, OR the id did not match the parked
    /// action. NOTHING is taken and NOTHING fires — an unknown/stale id is inert
    /// (the slot, if any, is left intact for the real id, unless it had expired,
    /// in which case the expired slot is dropped).
    NoMatch,
}

/// Authenticated-local confirm-by-id: if a non-expired action is parked AND its
/// content id equals `id`, TAKE it from the slot and return it for replay;
/// otherwise leave the (live) slot untouched and return [`ByIdConfirm::NoMatch`].
///
/// This is the ONLY by-id path that can lead to a fire, and it can fire ONLY the
/// EXACT parked action whose id was named — never a re-derived or fabricated
/// one. The actual execution (and the master-switch re-check) still happens in
/// `replay_confirmed_action`, exactly as for the spoken `Affirm`. `now` is
/// injectable for TTL tests.
pub fn confirm_by_id(id: &str, now: Instant) -> ByIdConfirm {
    let mut guard = lock();
    match guard.as_ref() {
        // Live + id matches: take it (clearing the slot) and hand it back. The
        // PROMPT that named it goes with it (see `retire_slot`) — this action is
        // answered, so nothing may still be authorized to fire under its id.
        Some(p) if now.duration_since(p.created_at) <= PENDING_TTL && p.id == id => {
            let taken = guard.take().expect("just matched Some");
            *prompted_lock() = None;
            ByIdConfirm::Matched(taken)
        }
        // Live but the id does NOT match: leave the real pending intact, fire
        // nothing. A wrong/fabricated id is a no-op, never a replay. PROMPTED is
        // left alone too — the real pending is still awaiting its own answer.
        Some(p) if now.duration_since(p.created_at) <= PENDING_TTL => {
            let _ = p;
            ByIdConfirm::NoMatch
        }
        // Expired: drop the stale slot AND its prompt, nothing to confirm.
        Some(_) => {
            retire_slot(&mut guard);
            ByIdConfirm::NoMatch
        }
        None => ByIdConfirm::NoMatch,
    }
}

/// Authenticated-local deny-by-id: if a non-expired action is parked AND its id
/// equals `id`, CLEAR it and report `true` (the action was dropped); otherwise
/// leave a live, non-matching slot intact and report `false`. Like the spoken
/// `Deny`, this only ever clears — it never fires anything. `now` is injectable.
pub fn deny_by_id(id: &str, now: Instant) -> bool {
    let mut guard = lock();
    match guard.as_ref() {
        Some(p) if now.duration_since(p.created_at) <= PENDING_TTL && p.id == id => {
            retire_slot(&mut guard);
            true
        }
        // Live but non-matching id: leave it alone (a wrong id must not clear
        // the genuine pending out from under the user), PROMPTED included.
        Some(p) if now.duration_since(p.created_at) <= PENDING_TTL => {
            let _ = p;
            false
        }
        // Expired or none: nothing to deny; drop a stale slot AND its prompt.
        Some(_) => {
            retire_slot(&mut guard);
            false
        }
        None => false,
    }
}

/// What the router should do with this turn given a live pending and the
/// classified spoken reply. Returned by [`resolve_reply`] so the routing
/// decision is a PURE function of (reply, pending) — unit-testable without the
/// router, the brain, or any client.
#[derive(Debug)]
pub enum Resolution {
    /// The human affirmed: REPLAY this exact parked action (Execute mode). The
    /// router calls the actuator with `pending.{tool,input,allowed,agent}` —
    /// never anything re-derived from the utterance.
    Replay(PendingConfirmation),
    /// The human declined: speak this acknowledgement, run nothing.
    Cancelled(String),
    /// Neither yes nor no: the pending is dropped; route THIS utterance
    /// normally (the router falls through). Nothing fired.
    PassThrough,
}

/// PURE routing decision for a live pending + a spoken utterance: classify the
/// utterance and decide whether to replay (Affirm), cancel (Deny), or pass
/// through (Unrelated). The pending has ALREADY been taken from the slot by the
/// caller ([`take_live`]), so whatever this returns, no stale action lingers.
/// `cancel_phrase` renders the Deny acknowledgement (so the wording stays in the
/// router where the tool->phrase map lives). This is the testable heart of the
/// cross-turn gate: a `Replay` ALWAYS carries the EXACT parked action.
pub fn resolve_reply(
    pending: PendingConfirmation,
    utterance: &str,
    cancel_phrase: impl FnOnce(&str) -> String,
) -> Resolution {
    match classify_confirmation(utterance) {
        ConfirmReply::Affirm => Resolution::Replay(pending),
        ConfirmReply::Deny => Resolution::Cancelled(cancel_phrase(&pending.tool)),
        ConfirmReply::Unrelated => Resolution::PassThrough,
    }
}

/// PURE affirmation classifier. Maps a free-form spoken utterance to a
/// [`ConfirmReply`], robust to case, surrounding punctuation, and extra words.
///
/// SAFETY-FIRST rules (in priority order):
///  1. Any DENY/RETRACT marker present -> never an affirm. "no", "stop",
///     "wait", "hold on", "not yet", "cancel", "abort", "nevermind", "scratch
///     that", and the negations "don't"/"do not" all force `Deny` — even when an
///     affirm word is also present ("no wait yes" is conservative `Deny`).
///  2. A CLEAN affirm (every token is a yes-word or harmless filler, no deny
///     marker) -> `Affirm`. Checked before the qualifier branch so a fixed
///     affirm idiom like "make it so" is not tripped by a "make it" marker.
///  3. A yes carrying a MODIFY/QUALIFY marker ("but", "change", "instead",
///     "actually", ...) means the user is altering the proposal, not cleanly
///     accepting it -> `Unrelated` ("yes but change the title" never executes
///     the stale action).
///  4. Everything else -> `Unrelated` (a new command, a question, chit-chat).
pub fn classify_confirmation(utterance: &str) -> ConfirmReply {
    // Normalize: lowercase; DROP apostrophes so contractions stay whole
    // ("don't" -> "dont", "let's" -> "lets"); turn every other non-alphanumeric
    // into a space; collapse whitespace.
    let normalized: String = utterance
        .chars()
        .filter(|c| *c != '\'' && *c != '\u{2019}')
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect();
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    if tokens.is_empty() {
        return ConfirmReply::Unrelated;
    }
    let joined = tokens.join(" ");
    let has = |needle: &str| contains_phrase(&tokens, needle);

    // -- (1) DENY / RETRACT: any of these forces Deny, even alongside a yes. ---
    // Negations + explicit stop/cancel/hold words. "do not" is checked as a
    // phrase; "dont"/"don" cover the apostrophe-stripped "don't".
    const DENY_PHRASES: &[&str] = &[
        "no",
        "nope",
        "nah",
        "cancel",
        "stop",
        "dont",
        "do not",
        "nevermind",
        "never mind",
        "not yet",
        "hold on",
        "hold off",
        "abort",
        "scratch that",
        "forget it",
        "drop it",
        "negative",
        // "undo" while an action is still PARKED means "don't do it" — the
        // fail-safe reading. (Undoing an already-EXECUTED action is the
        // journal's job, in the router arm — journal.rs — which only runs when
        // no confirmation is pending.)
        "undo",
    ];
    if DENY_PHRASES.iter().any(|p| has(p)) {
        return ConfirmReply::Deny;
    }
    // "wait" alone (no other clear signal) is a retraction; with a modify word
    // it falls through to the qualifier branch. Treat a bare "wait" as Deny so
    // "wait" / "hold on a sec" never confirm.
    if has("wait") && !is_clean_affirm(&joined) {
        return ConfirmReply::Deny;
    }

    // -- (2) CLEAN AFFIRM: the whole utterance is yes-words + filler. ----------
    // Checked BEFORE the qualifier branch so a fixed affirm idiom like
    // "make it so" is not tripped by the "make it" qualify marker.
    if is_clean_affirm(&joined) {
        return ConfirmReply::Affirm;
    }

    // -- (3) MODIFY / QUALIFY: a yes carrying a change is NOT a clean accept. --
    // We only reach here when the utterance is NOT a clean affirm, so an affirm
    // word PLUS a qualifier means "yes, but do it differently" — the parked
    // action is stale and must not fire.
    const QUALIFY_PHRASES: &[&str] = &[
        "but", "change", "instead", "except", "however", "actually", "rather",
        "different", "edit", "make it", "use", "to", "add",
    ];
    let affirm_present = AFFIRM_PHRASES.iter().any(|p| has(p));
    if affirm_present && QUALIFY_PHRASES.iter().any(|p| has(p)) {
        return ConfirmReply::Unrelated;
    }

    // -- (4) Everything else: route normally, drop the pending. ----------------
    ConfirmReply::Unrelated
}

/// The affirmation vocabulary. A clean affirm = the utterance is built ENTIRELY
/// of these (plus filler) with no deny/modify marker. Multi-word entries are
/// matched as contiguous phrases.
const AFFIRM_PHRASES: &[&str] = &[
    "yes", "yeah", "yep", "yup", "ya", "yse", "confirm", "confirmed",
    "do it", "go ahead", "go for it", "proceed", "send it", "ship it",
    "send", "post it", "post", "affirmative", "sure", "ok", "okay", "k",
    "please do", "please", "absolutely", "definitely", "fire it", "approved",
    "approve", "let's go", "lets go", "make it so",
];

/// Is the utterance a CLEAN affirmation — every token is part of an affirm
/// phrase or harmless filler, with no deny/modify marker? This is what keeps
/// "yes please do it" an affirm while "yes send a different one" is not.
fn is_clean_affirm(joined: &str) -> bool {
    let tokens: Vec<&str> = joined.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }
    // Filler words that may surround an affirm without changing its meaning.
    const FILLER: &[&str] = &["it", "that", "now", "then", "and", "go", "just", "right", "all"];
    // Walk left-to-right, consuming the longest affirm phrase at each position;
    // any token that is neither part of an affirm phrase nor filler means the
    // utterance carries extra intent and is NOT a clean affirm.
    let mut i = 0;
    let mut matched_any = false;
    while i < tokens.len() {
        if let Some(len) = longest_affirm_at(&tokens[i..]) {
            i += len;
            matched_any = true;
            continue;
        }
        if FILLER.contains(&tokens[i]) {
            i += 1;
            continue;
        }
        return false;
    }
    matched_any
}

/// Length (in tokens) of the longest affirm phrase that starts at `tokens[0]`,
/// or `None` if no affirm phrase starts there.
fn longest_affirm_at(tokens: &[&str]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for phrase in AFFIRM_PHRASES {
        let parts: Vec<&str> = phrase.split_whitespace().collect();
        if parts.len() <= tokens.len() && tokens[..parts.len()] == parts[..] {
            best = Some(best.map_or(parts.len(), |b: usize| b.max(parts.len())));
        }
    }
    best
}

/// Does the token stream contain `needle` (a single token or a contiguous
/// multi-token phrase) as a whole-word match? Trailing-space entries like
/// "to " are normalized away by the caller's tokenization, so we match on the
/// trimmed phrase.
fn contains_phrase(tokens: &[&str], needle: &str) -> bool {
    let parts: Vec<&str> = needle.split_whitespace().collect();
    if parts.is_empty() {
        return false;
    }
    tokens.windows(parts.len()).any(|w| w == parts.as_slice())
}

#[cfg(test)]
mod tests {

    /// REGRESSION (sweep HIGH): the spoken "yes" must answer the question the
    /// user was ACTUALLY asked. `PENDING` is a single process-global slot and
    /// `park_ctx(true, )` overwrites it unconditionally, so a background parker (a
    /// standing order, an overnight agent, any concurrent turn) landing between
    /// the prompt and the reply used to make the user's "yes" fire an action
    /// they were never shown.
    /// AN ACTION THE USER WAS NEVER PROMPTED WITH CANNOT BE CONFIRMED BY "YES".
    ///
    /// `take_live`'s prompt guard fell through to "take whatever is parked"
    /// whenever PROMPTED was None, reasoning about "a pre-existing park". No such
    /// state exists: PENDING is written in exactly one place, PROMPTED is armed
    /// there too but only `if reaches_user`, and nothing nulls PROMPTED without
    /// also nulling the slot. So Some/None held IF AND ONLY IF the park declared
    /// it did not reach the user — the fallback fired exactly and only in the case
    /// the guard exists to catch.
    ///
    /// Reachable from production: a standing-mission tick parks with
    /// `context_trusted: false`, which does not arm PROMPTED. The operator's next
    /// "yes" — to anything — executed it, with arguments they were never read.
    ///
    /// The sibling test above covers a background park CLOBBERING a prompted one.
    /// This covers a background park standing ALONE, which is the reachable case
    /// (PROMPTED is None at daemon start, after any successful take_live, and
    /// after every clear() — which barge-in and panic both call).
    #[test]
    fn a_lone_unprompted_park_is_not_taken_by_a_spoken_yes() {
        let _lock = PENDING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear();

        let mut b = sample("shell_run");
        b.input = json!({"cmd": "rm -rf ~/research"});
        let _ = park_background_for_test(b);
        assert!(peek_pending(Instant::now()).is_some(), "the action is parked");

        assert!(
            take_live(Instant::now()).is_none(),
            "a spoken yes must NOT confirm an action the user was never prompted with"
        );
        assert!(
            peek_pending(Instant::now()).is_some(),
            "the unprompted action must remain parked, not silently dropped"
        );

        // The prompted case still works, or this would just break confirmation.
        clear();
        let _ = park_ctx(true, sample("gmail_send"));
        assert!(
            take_live(Instant::now()).is_some(),
            "an action the user WAS prompted with is still confirmable"
        );
        clear();
    }

    #[test]
    fn a_background_parker_cannot_hijack_the_spoken_confirm() {
        // These drive the PROCESS-GLOBAL slot, so they must hold PENDING_TEST_LOCK
        // like every other slot-touching test. Without it they raced the guarded
        // tests (their `clear()` emptied the slot a guarded test had just parked),
        // making `park_then_take_live_round_trips_and_clears` fail intermittently.
        let _g = store_guard();
        clear();
        // 1. The user is prompted about action A.
        let _prompt = park_ctx(true, sample("gmail_send"));
        // 2. A BACKGROUND turn parks a DIFFERENT action B, clobbering the slot
        //    without the user ever being prompted about it.
        let mut b = sample("x_post");
        b.input = json!({"text": "public post"});
        let _ = park_background_for_test(b);
        // 3. The user says "yes" — answering the prompt about A. B must NOT fire.
        let taken = take_live(Instant::now());
        assert!(
            taken.is_none(),
            "a spoken yes must never confirm an action the user was not shown: {taken:?}"
        );
        // B is still parked, awaiting its own confirmation — not silently lost.
        assert!(
            peek_pending(Instant::now()).is_some(),
            "the background action stays parked"
        );
        clear();
    }

    /// The normal path is unaffected: prompt then confirm fires the SAME action.
    #[test]
    fn the_prompted_action_still_confirms_normally() {
        // Slot-touching -> serialize on PENDING_TEST_LOCK (see above).
        let _g = store_guard();
        clear();
        let _prompt = park_ctx(true, sample("gmail_send"));
        let taken = take_live(Instant::now()).expect("the prompted action confirms");
        assert_eq!(taken.tool, "gmail_send");
        clear();
    }
    use super::*;
    use serde_json::json;

    // -- (1) classify_confirmation truth table --------------------------------

    #[test]
    fn affirmations_classify_as_affirm() {
        for u in [
            "yes",
            "Yes.",
            "YES!",
            "yeah",
            "yep",
            "yup",
            "confirm",
            "Confirmed.",
            "do it",
            "go ahead",
            "go ahead.",
            "proceed",
            "send it",
            "ship it",
            "affirmative",
            "sure",
            "ok",
            "okay",
            "ok do it",
            "please do",
            "yes please",
            "yes, do it now",
            "go for it",
            "absolutely",
            "approved",
            "make it so",
        ] {
            assert_eq!(
                classify_confirmation(u),
                ConfirmReply::Affirm,
                "{u:?} should be a clean affirm"
            );
        }
    }

    #[test]
    fn denials_classify_as_deny() {
        for u in [
            "no",
            "No.",
            "NOPE",
            "cancel",
            "stop",
            "don't",
            "do not",
            "do not send it",
            "nevermind",
            "never mind",
            "not yet",
            "wait",
            "hold on",
            "hold on a second",
            "abort",
            "scratch that",
            "forget it",
            "negative",
            "nah",
            // "undo" against a PARKED (not-yet-executed) action is a retraction
            // — never a confirmation, never a journal undo (nothing executed).
            "undo",
            "undo that",
            "yes but undo that first",
        ] {
            assert_eq!(
                classify_confirmation(u),
                ConfirmReply::Deny,
                "{u:?} should be a denial"
            );
        }
    }

    #[test]
    fn unrelated_classifies_as_unrelated() {
        for u in [
            "what time is it",
            "open safari",
            "tell me a joke",
            "what would it say",
            "remind me later",
            "send a tweet about cats", // a NEW request, not a confirmation
            "",
            "   ",
            "the weather is nice",
        ] {
            assert_eq!(
                classify_confirmation(u),
                ConfirmReply::Unrelated,
                "{u:?} should be unrelated"
            );
        }
    }

    /// The crux safety cases: anything that mixes a yes with a retraction or a
    /// modification must NEVER be a clean affirm of the parked action.
    #[test]
    fn mixed_and_qualified_replies_never_affirm() {
        // Deny wins over a co-present yes (conservative).
        assert_eq!(classify_confirmation("no wait yes"), ConfirmReply::Deny);
        assert_eq!(classify_confirmation("yes no"), ConfirmReply::Deny);
        assert_eq!(classify_confirmation("actually no"), ConfirmReply::Deny);
        assert_eq!(classify_confirmation("hold on, no"), ConfirmReply::Deny);

        // A qualified accept is NOT a clean affirm of THIS action.
        for u in [
            "yes but change the title",
            "yes, but use a different repo",
            "sure but make it shorter",
            "ok actually change the recipient",
            "yeah, instead send it to bob",
            "yes change the amount",
        ] {
            assert_ne!(
                classify_confirmation(u),
                ConfirmReply::Affirm,
                "{u:?} must NOT cleanly affirm the parked action"
            );
        }
    }

    // -- store lifecycle ------------------------------------------------------
    // These all touch the process-global single slot, so they must not run
    // concurrently. The crate-wide `PENDING_TEST_LOCK` serializes them
    // (poison-tolerant) against EVERY module's slot-touching tests, and each
    // clear()s the slot on entry so one test never leaks state to the next.
    fn store_guard() -> std::sync::MutexGuard<'static, ()> {
        let g = super::PENDING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear();
        g
    }

    fn sample(tool: &str) -> PendingConfirmation {
        PendingConfirmation {
            agent: "agent.pepper".into(),
            tool: tool.into(),
            input: json!({"to": "a@b.com", "subject": "Hi", "body": "x"}),
            allowed: vec!["gmail_send".into()],
            preview: "Would send an email to a@b.com".into(),
            created_at: Instant::now(),
            // park_ctx(true, ) (re)derives this; the field is set so direct-store tests
            // that bypass park_ctx(true, ) still have a well-formed id.
            id: String::new(),
            // No structured plan on this generic sample (text-preview path).
            plan: None,
        }
    }

    /// park stores, take_live returns the EXACT parked record and empties the
    /// slot; a second take_live is None.
    #[test]
    fn park_then_take_live_round_trips_and_clears() {
        let _g = store_guard();
        let prompt = park_ctx(true, sample("gmail_send"));
        assert!(prompt.contains("confirm"), "prompt invites a yes: {prompt}");
        assert!(prompt.contains("cancel"), "prompt invites a no: {prompt}");
        let now = Instant::now();
        let taken = take_live(now).expect("a live pending");
        assert_eq!(taken.tool, "gmail_send");
        assert_eq!(taken.input["to"], "a@b.com");
        assert!(take_live(now).is_none(), "slot emptied after take");
        clear();
    }

    /// A new park REPLACES the prior one (single slot).
    #[test]
    fn single_slot_replacement() {
        let _g = store_guard();
        let _ = park_ctx(true, sample("gmail_send"));
        let _ = park_ctx(true, sample("slack_post_message"));
        let taken = take_live(Instant::now()).expect("a live pending");
        assert_eq!(taken.tool, "slack_post_message", "newest proposal wins");
        clear();
    }

    /// A pending older than the TTL is treated as expired: is_live is false and
    /// take_live returns None (and clears the slot). Clock is injected.
    #[test]
    fn expiry_clears_a_stale_pending() {
        let _g = store_guard();
        let mut p = sample("gmail_send");
        let base = Instant::now();
        p.created_at = base;
        let _ = park_ctx(true, p);
        let future = base + PENDING_TTL + Duration::from_secs(1);
        assert!(!is_live(future), "aged pending is not live");
        // is_live cleared it; a take at the same instant finds nothing.
        assert!(take_live(future).is_none(), "expired pending is gone");
        clear();
    }

    // -- by-id confirmation (the authenticated-local command-channel path) -----

    /// park stamps a stable content id; peek_pending surfaces it (replay-free);
    /// the same id is reproducible from the action's bytes via derive_pending_id.
    #[test]
    fn park_stamps_a_stable_content_id_that_peek_surfaces() {
        let _g = store_guard();
        let s = sample("gmail_send");
        let want = derive_pending_id(&s.agent, &s.tool, &s.input);
        let _ = park_ctx(true, s);
        let view = peek_pending(Instant::now()).expect("a live pending");
        assert_eq!(view.id, want, "peek surfaces the stable content id");
        assert_eq!(view.tool, "gmail_send");
        // peek is read-only: the slot is still live after peeking.
        assert!(peek_pending(Instant::now()).is_some(), "peek does not consume");
        clear();
    }

    /// confirm_by_id with the GENUINE id takes the exact parked action; a SECOND
    /// confirm of the same id is NoMatch (consumed). A wrong id is a no-op that
    /// leaves the real pending intact.
    #[test]
    fn confirm_by_id_takes_only_the_named_action() {
        let _g = store_guard();
        let _ = park_ctx(true, sample("gmail_send"));
        let id = peek_pending(Instant::now()).unwrap().id;
        let now = Instant::now();

        // A wrong id: nothing taken, the real pending stays.
        assert!(matches!(confirm_by_id("deadbeefdeadbeef", now), ByIdConfirm::NoMatch));
        assert!(peek_pending(now).is_some(), "wrong id left the pending intact");

        // The real id: the EXACT parked action comes back, and the slot clears.
        match confirm_by_id(&id, now) {
            ByIdConfirm::Matched(p) => {
                assert_eq!(p.tool, "gmail_send");
                assert_eq!(p.input["to"], "a@b.com");
            }
            ByIdConfirm::NoMatch => panic!("the genuine id must match"),
        }
        assert!(peek_pending(now).is_none(), "matched id consumed the slot");
        // A second confirm of the same id fires nothing (already consumed).
        assert!(matches!(confirm_by_id(&id, now), ByIdConfirm::NoMatch), "no re-fire");
        clear();
    }

    /// confirm_by_id on an EXPIRED pending is NoMatch (and drops the stale slot)
    /// even when the id is correct — a stale action can never be confirmed.
    #[test]
    fn confirm_by_id_refuses_an_expired_action() {
        let _g = store_guard();
        let mut p = sample("gmail_send");
        let base = Instant::now();
        p.created_at = base;
        let _ = park_ctx(true, p);
        let id = peek_pending(base).unwrap().id;
        let future = base + PENDING_TTL + Duration::from_secs(1);
        assert!(matches!(confirm_by_id(&id, future), ByIdConfirm::NoMatch), "expired id never fires");
        assert!(peek_pending(future).is_none(), "the stale slot was dropped");
        clear();
    }

    /// deny_by_id clears only the named action; a wrong id leaves it parked.
    #[test]
    fn deny_by_id_clears_only_the_named_action() {
        let _g = store_guard();
        let _ = park_ctx(true, sample("gmail_send"));
        let id = peek_pending(Instant::now()).unwrap().id;
        let now = Instant::now();
        assert!(!deny_by_id("00000000ffffffff", now), "wrong id is a no-op");
        assert!(peek_pending(now).is_some(), "still parked after a wrong deny");
        assert!(deny_by_id(&id, now), "the real id clears it");
        assert!(peek_pending(now).is_none(), "denied action gone");
        clear();
    }

    /// Two DIFFERENT parked actions produce DIFFERENT ids; an id from one never
    /// confirms the other (single slot, but the id is content-bound).
    #[test]
    fn distinct_actions_have_distinct_ids() {
        let _g = store_guard();
        let a = sample("gmail_send");
        let mut b = sample("slack_post_message");
        b.input = json!({"channel": "#ops", "text": "hi"});
        let id_a = derive_pending_id(&a.agent, &a.tool, &a.input);
        let id_b = derive_pending_id(&b.agent, &b.tool, &b.input);
        assert_ne!(id_a, id_b, "different actions hash to different ids");
        // Park b; confirming with a's id is a no-op.
        let _ = park_ctx(true, b);
        assert!(matches!(confirm_by_id(&id_a, Instant::now()), ByIdConfirm::NoMatch));
        assert!(peek_pending(Instant::now()).is_some(), "b stays parked");
        clear();
    }

    /// Within the TTL the pending is still live and takeable.
    #[test]
    fn within_ttl_is_live() {
        let _g = store_guard();
        let mut p = sample("x_post");
        let base = Instant::now();
        p.created_at = base;
        let _ = park_ctx(true, p);
        let soon = base + Duration::from_secs(5);
        assert!(is_live(soon), "fresh pending is live");
        assert!(take_live(soon).is_some());
        clear();
    }

    /// clear() empties the slot (the barge / roll-call lifecycle).
    #[test]
    fn clear_empties_the_slot() {
        let _g = store_guard();
        let _ = park_ctx(true, sample("dume_control"));
        clear();
        assert!(take_live(Instant::now()).is_none(), "clear drops the pending");
    }

    // -- consequential registry pinned to the gate call sites -----------------

    #[test]
    fn consequential_registry_is_complete_and_exact() {
        // Every gated tool is recognized; a read-only tool is not.
        for t in CONSEQUENTIAL_TOOLS {
            assert!(is_consequential_tool(t), "{t} must be consequential");
        }
        for t in ["github_list_prs", "gmail_list_recent", "edith_brief", "midas_balances"] {
            assert!(!is_consequential_tool(t), "{t} is read-only, not consequential");
        }
        // standing_create is gated (establishing recurring autonomy needs a yes);
        // standing_list / standing_cancel are NOT (read-only / reversible).
        assert!(is_consequential_tool("standing_create"), "standing_create must be gated");
        assert!(!is_consequential_tool("standing_list"), "standing_list is read-only");
        assert!(!is_consequential_tool("standing_cancel"), "standing_cancel is reversible");
        // shell_run (the sandboxed shell #43) is the MOST consequential tool —
        // arbitrary execution — so it MUST be gated (it always parks, never auto-runs).
        assert!(is_consequential_tool("shell_run"), "shell_run must be gated (it parks, never auto-runs)");
        // ui_actuate (gated UI automation #44, the CAPSTONE) is the single most
        // DANGEROUS tool — it physically actuates the UI — so it MUST be gated PER
        // ACTION (it always parks; one confirm = one actuation; it never auto-runs).
        assert!(is_consequential_tool("ui_actuate"), "ui_actuate must be gated per-action (it parks, never auto-runs)");
        // connector_add adds an MCP connector (a persistent config mutation) — it
        // MUST be gated (it always parks for a spoken yes; it never auto-applies).
        assert!(is_consequential_tool("connector_add"), "connector_add must be gated (it parks, never auto-applies)");
        // open_settings_pane opens an allowlisted System Settings pane (a visible UI
        // side effect) — it MUST be gated per action (it parks; it never auto-opens).
        assert!(is_consequential_tool("open_settings_pane"), "open_settings_pane must be gated (it parks, never auto-opens)");
        // pasteboard_put SETS the macOS clipboard (a visible side effect) — benign
        // (a pasteboard set only, never a keystroke/file/network) but MUST be gated
        // per action (it parks for a spoken yes; it never auto-copies).
        assert!(is_consequential_tool("pasteboard_put"), "pasteboard_put must be gated (it parks, never auto-copies)");
        // forge_app COMPILES AND RUNS code a cloud model wrote (cargo check + cargo
        // test, plus any build.rs it emitted). That is arbitrary code execution
        // arriving by a path the user never typed, so it MUST be gated exactly like
        // shell_run — it was the only tool of that class that never parked, which
        // left it reachable from a prompt-injected tool continuation.
        assert!(is_consequential_tool("forge_app"), "forge_app must be gated (it compiles and RUNS model-authored code)");
        // Exactly the 22 gate-routed tools, no dupes.
        assert_eq!(CONSEQUENTIAL_TOOLS.len(), 22, "expected 22 consequential tools");
        let mut sorted = CONSEQUENTIAL_TOOLS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 22, "no duplicate consequential tool names");
    }

    #[test]
    fn confirmation_prompt_is_clean() {
        // Trailing period on the preview is not doubled.
        let p = confirmation_prompt("Would open a PR titled 'Fix bug'.");
        assert_eq!(p, "Would open a PR titled 'Fix bug' — say 'confirm' to proceed or 'cancel' to drop it.");
    }

    #[test]
    fn confirmation_prompt_strips_off_mode_boilerplate() {
        // A real integration dry-run preview carries a "[dry run] " lead-in and a
        // trailing "Enable consequential actions and confirm to <verb>." hint —
        // both authored for the OFF-switch path. A parked confirmation is only
        // ever reached with the master switch already ON, so the spoken prompt
        // must NOT tell the user to enable a switch they've already enabled, nor
        // double-state the confirm instruction. Verify both are stripped while
        // the faithful action description (recipient + subject) survives.
        let preview = "[dry run] Would send an email to bob@x.com with subject \
                       \"Hi\" (begins: \"hey\"). Enable consequential actions and \
                       confirm to send.";
        let p = confirmation_prompt(preview);
        assert_eq!(
            p,
            "Would send an email to bob@x.com with subject \"Hi\" (begins: \"hey\") \
             — say 'confirm' to proceed or 'cancel' to drop it."
        );
        // The now-false enablement hint never reaches the user.
        assert!(
            !p.contains("Enable consequential actions"),
            "spoken prompt must not embed the OFF-mode enablement hint: {p}"
        );
        assert!(!p.contains("[dry run]"), "spoken prompt must not leak the dry-run lead-in: {p}");
    }

    #[test]
    fn parked_confirmation_prompt_omits_enablement_hint() {
        // End-to-end: park_ctx(true, ) runs the real preview through confirmation_prompt,
        // and the returned spoken prompt must be free of the OFF-mode hint.
        let _g = store_guard();
        let mut pending = sample("gmail_send");
        pending.preview = "[dry run] Would send an email to a@b.com with subject \"Hi\". \
                           Enable consequential actions and confirm to send."
            .into();
        let prompt = park_ctx(true, pending);
        assert!(
            !prompt.contains("Enable consequential actions"),
            "parked prompt must not embed the enablement hint: {prompt}"
        );
        assert!(!prompt.contains("[dry run]"), "parked prompt must not leak the dry-run lead-in: {prompt}");
        assert!(prompt.contains("say 'confirm'"), "parked prompt keeps the say-confirm clause: {prompt}");
        assert!(
            prompt.contains("a@b.com"),
            "parked prompt keeps the faithful recipient: {prompt}"
        );
    }

    // -- resolve_reply: the cross-turn decision, proven with a mock executor ---
    // The router calls resolve_reply(pending, utterance, ...) and, on a Replay,
    // hands the returned pending to the actuator. Here we stand in for the
    // actuator with a recording mock that captures EXACTLY what fired, so we can
    // prove a spoken "yes" replays the PARKED tool+input (Execute mode), and that
    // Deny / Unrelated fire NOTHING.

    /// A recording stand-in for the actuator: records (tool, input, executed?)
    /// so a test can assert the EXACT args fired, once, only on an Affirm.
    #[derive(Default)]
    struct MockActuator {
        fired: std::cell::RefCell<Vec<(String, Value)>>,
    }
    impl MockActuator {
        /// Mimics replay_confirmed_action: it would force confirm=true and run
        /// the parked tool+input. We assert on what it was handed.
        fn execute(&self, p: &PendingConfirmation) {
            self.fired
                .borrow_mut()
                .push((p.tool.clone(), p.input.clone()));
        }
        fn calls(&self) -> Vec<(String, Value)> {
            self.fired.borrow().clone()
        }
    }

    fn cancel_phrase(tool: &str) -> String {
        format!("Cancelled. I won't {tool}.")
    }

    /// PARK-THEN-AFFIRM: a spoken "yes" REPLAYS the EXACT parked {tool,input},
    /// firing the actuator EXACTLY ONCE with the parked args — NOT anything from
    /// the (here, affirming) utterance.
    #[test]
    fn affirm_replays_the_exact_parked_action_once() {
        let _g = store_guard();
        let parked = PendingConfirmation {
            agent: "agent.pepper".into(),
            tool: "gmail_send".into(),
            input: json!({"to": "alice@example.com", "subject": "Q3", "body": "draft"}),
            allowed: vec!["gmail_send".into()],
            preview: "Would send an email to alice@example.com".into(),
            created_at: Instant::now(),
            id: String::new(),
            plan: None,
        };
        let _ = park_ctx(true, parked);

        // New turn: the human says yes. Router takes the live pending and
        // resolves the reply.
        let pending = take_live(Instant::now()).expect("a live pending");
        let mock = MockActuator::default();
        match resolve_reply(pending, "yes, do it", cancel_phrase) {
            Resolution::Replay(p) => mock.execute(&p),
            other => panic!("expected Replay, got {other:?}"),
        }

        let calls = mock.calls();
        assert_eq!(calls.len(), 1, "the action fires EXACTLY once");
        assert_eq!(calls[0].0, "gmail_send", "the PARKED tool fired");
        // The PARKED input fired verbatim — not re-derived from "yes, do it".
        assert_eq!(calls[0].1["to"], "alice@example.com");
        assert_eq!(calls[0].1["subject"], "Q3");
        assert_eq!(calls[0].1["body"], "draft");
    }

    /// PARK-THEN-DENY: a spoken "cancel" runs NOTHING and acknowledges.
    #[test]
    fn deny_cancels_and_executes_nothing() {
        let _g = store_guard();
        let _ = park_ctx(true, sample("slack_post_message"));
        let pending = take_live(Instant::now()).expect("a live pending");
        let mock = MockActuator::default();
        match resolve_reply(pending, "no, cancel that", cancel_phrase) {
            Resolution::Cancelled(ack) => assert!(ack.contains("Cancelled")),
            other => panic!("expected Cancelled, got {other:?}"),
        }
        assert!(mock.calls().is_empty(), "Deny must fire nothing");
        // And the slot is empty: a later "yes" cannot revive it.
        assert!(take_live(Instant::now()).is_none());
    }

    /// PARK-THEN-UNRELATED: a new command drops the pending, fires NOTHING, and
    /// passes through (the router then routes the new utterance normally).
    #[test]
    fn unrelated_drops_and_passes_through() {
        let _g = store_guard();
        let _ = park_ctx(true, sample("x_post"));
        let pending = take_live(Instant::now()).expect("a live pending");
        let mock = MockActuator::default();
        match resolve_reply(pending, "what's the weather", cancel_phrase) {
            Resolution::PassThrough => {}
            other => panic!("expected PassThrough, got {other:?}"),
        }
        assert!(mock.calls().is_empty(), "Unrelated must fire nothing");
        // The stale action is gone — a stray later "yes" cannot confirm it.
        assert!(take_live(Instant::now()).is_none());
    }

    /// A QUALIFIED yes ("yes but change the recipient") is NOT a clean affirm:
    /// it passes through (drops the stale action) and fires nothing — the user is
    /// modifying the proposal, so the previewed action must never execute.
    #[test]
    fn qualified_yes_does_not_replay() {
        let _g = store_guard();
        let _ = park_ctx(true, sample("gmail_send"));
        let pending = take_live(Instant::now()).expect("a live pending");
        let mock = MockActuator::default();
        match resolve_reply(pending, "yes but change the recipient", cancel_phrase) {
            Resolution::PassThrough => {}
            other => panic!("a qualified yes must not Replay; got {other:?}"),
        }
        assert!(mock.calls().is_empty(), "a qualified yes fires nothing");
    }
}
#[cfg(test)]
mod hijack_tests {
    use super::*;

    /// `PENDING` is PROCESS-GLOBAL and cargo runs test modules concurrently, so
    /// every test here that parks or takes must serialize on the crate-wide lock —
    /// the same discipline `tests::store_guard` follows. Without it these raced the
    /// tool-loop park tests in `anthropic::tests`: a `clear()` there emptied a slot
    /// parked here, and `an_unattended_tool_park_does_not_arm_the_prompt` failed
    /// intermittently on a defect that was not there.
    fn slot_guard() -> std::sync::MutexGuard<'static, ()> {
        let g = super::PENDING_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        clear();
        g
    }

    fn pending(agent: &str, tool: &str, arg: &str) -> PendingConfirmation {
        PendingConfirmation {
            agent: agent.to_string(),
            tool: tool.to_string(),
            input: serde_json::json!({ "arg": arg }),
            preview: format!("{tool} with {arg}"),
            id: String::new(),
            allowed: Vec::new(),
            created_at: Instant::now(),
            plan: None,
        }
    }

    /// THE HOLE THE GUARD WAS SUPPOSED TO CLOSE, reached through PRODUCTION code.
    ///
    /// park() armed PROMPTED unconditionally, so every later parker re-pointed it at
    /// its own id and take_live's `p.id == id` compared the slot against itself. The
    /// only thing that modelled a background park was a #[cfg(test)] helper — so the
    /// guard was exercised solely through a test-only entry point while production had
    /// the hole open. This test uses park(.., false), which is what an unattended turn
    /// now calls.
    #[test]
    fn a_background_park_cannot_steal_the_users_confirm() {
        let _guard = slot_guard();
        park_ctx(true, pending("orchestrator", "gmail_send", "ALPHA"));
        // A standing tick / overnight agent / inbound webhook parks its own action.
        park_ctx(false, pending("scribe", "x_post", "BETA"));

        let taken = take_live(Instant::now());
        assert!(
            taken.is_none(),
            "the spoken confirm fired {:?} — an action the user was never shown",
            taken.map(|p| p.tool)
        );
        // ...and BETA is still parked, waiting for its own confirmation.
        assert!(lock().is_some(), "the background action was consumed as collateral");
        let _ = take_live(Instant::now());
    }

    #[test]
    fn the_users_own_action_still_confirms() {
        let _guard = slot_guard();
        park_ctx(true, pending("orchestrator", "gmail_send", "ALPHA"));
        let taken = take_live(Instant::now()).expect("the user's own park must confirm");
        assert_eq!(taken.tool, "gmail_send");
    }

    /// A PROMPT THAT WAS NEVER ANSWERED MUST NOT AUTHORIZE THE NEXT IDENTICAL PARK.
    ///
    /// The slot and the prompt used to come apart. `is_live` — which `run_pipeline`
    /// calls on EVERY utterance for the self-echo and wake-word guards — dropped an
    /// EXPIRED pending but left PROMPTED pointing at it. Ids are content-derived
    /// (`agent || tool || canonical(input)`), and a standing mission re-proposes the
    /// same action verbatim on its next tick, so the background park that followed —
    /// correctly declaring `park_ctx(false, ..)` and arming nothing — matched the
    /// leftover id anyway. `take_live` then handed it over on the operator's next
    /// "yes" to anything at all: the exact hijack the `p.id == id` check exists to
    /// stop, reached through the leftover instead of through PROMPTED being armed.
    ///
    /// Drives the REAL clock: the pending is backdated past PENDING_TTL rather than
    /// mocked, so the expiry branch under test is the one production takes.
    #[test]
    fn an_expired_prompt_does_not_authorize_an_identical_background_park() {
        let _guard = slot_guard();

        // The operator IS prompted for ALPHA, and never answers.
        let mut alpha = pending("orchestrator", "gmail_send", "ALPHA");
        alpha.created_at = Instant::now() - (PENDING_TTL + Duration::from_secs(1));
        park_ctx(true, alpha);
        // PRECONDITION: the slot really did age out, and `is_live` really is the
        // thing that retires it (this is the call run_pipeline makes every turn).
        assert!(!is_live(Instant::now()), "the park must be past its TTL");
        assert!(lock().is_none(), "is_live must have dropped the expired slot");

        // A standing tick re-proposes the SAME action verbatim — same agent, tool
        // and input, hence the SAME content id — and honestly declares that nobody
        // was prompted for it.
        park_ctx(false, pending("orchestrator", "gmail_send", "ALPHA"));

        assert!(
            take_live(Instant::now()).is_none(),
            "a stale prompt authorized an unattended re-park: the operator's next \
             'yes' fires an action they were never read"
        );
        // And the background park is still parked, waiting for its own confirm.
        assert!(lock().is_some(), "the unattended action was consumed as collateral");
        let _ = take_live(Instant::now());
    }

    /// The by-id channel answers a prompt too, so it must retire it too.
    ///
    /// `confirm_by_id` / `deny_by_id` emptied the slot and left PROMPTED behind — so
    /// after the operator resolved ALPHA from the HUD, the next background park of an
    /// identical ALPHA inherited that authorization. Both verbs are covered; the
    /// non-matching-id arms must still leave a live prompt alone.
    #[test]
    fn resolving_by_id_retires_the_prompt_it_answered() {
        for resolve_by_id in [true, false] {
            let _guard = slot_guard();
            park_ctx(true, pending("orchestrator", "gmail_send", "ALPHA"));
            let id = peek_pending(Instant::now()).expect("parked").id;
            if resolve_by_id {
                assert!(
                    matches!(confirm_by_id(&id, Instant::now()), ByIdConfirm::Matched(_)),
                    "the id names the parked action"
                );
            } else {
                assert!(deny_by_id(&id, Instant::now()), "the id names the parked action");
            }
            assert!(lock().is_none(), "the slot is empty after a by-id resolution");

            // The same action comes back from an unattended tick.
            park_ctx(false, pending("orchestrator", "gmail_send", "ALPHA"));
            assert!(
                take_live(Instant::now()).is_none(),
                "an already-answered prompt (via {}) still authorized the next \
                 unattended park of the same action",
                if resolve_by_id { "confirm_by_id" } else { "deny_by_id" }
            );
            let _ = take_live(Instant::now());
        }
    }

    /// RESTRICT-ONLY CHECK: retiring the prompt alongside the slot must not break the
    /// ordinary flow. A live (non-expired) `is_live` / `peek_pending` — both of which
    /// run on every turn — must LEAVE the prompt armed, and a wrong id must clear
    /// neither, so the operator's real "confirm" still lands.
    #[test]
    fn peeking_at_a_live_pending_leaves_the_prompt_armed() {
        let _guard = slot_guard();
        park_ctx(true, pending("orchestrator", "gmail_send", "ALPHA"));
        assert!(is_live(Instant::now()), "still within the TTL");
        assert!(peek_pending(Instant::now()).is_some());
        // A fabricated id must not disarm the real prompt either.
        assert!(matches!(confirm_by_id("deadbeefdeadbeef", Instant::now()), ByIdConfirm::NoMatch));
        assert!(!deny_by_id("deadbeefdeadbeef", Instant::now()));
        let taken = take_live(Instant::now()).expect("the operator's own confirm must still land");
        assert_eq!(taken.tool, "gmail_send");
    }

    /// A user-facing re-park (the plan-drift path) legitimately re-arms PROMPTED.
    #[test]
    fn a_user_facing_repark_rearms_the_prompt() {
        let _guard = slot_guard();
        park_ctx(true, pending("orchestrator", "gmail_send", "ALPHA"));
        park_ctx(true, pending("orchestrator", "gmail_send", "ALPHA-REVISED"));
        let taken = take_live(Instant::now()).expect("the re-prompted action must confirm");
        assert_eq!(taken.preview, "gmail_send with ALPHA-REVISED");
    }

    /// park_ctx is what the tool paths call, threading context_trusted.
    #[test]
    fn an_unattended_tool_park_does_not_arm_the_prompt() {
        let _guard = slot_guard();
        park_ctx(true, pending("orchestrator", "gmail_send", "ALPHA"));
        park_ctx(false, pending("scribe", "x_post", "BETA"));
        assert!(
            take_live(Instant::now()).is_none(),
            "an unattended park armed the prompt and stole the confirm"
        );
        let _ = take_live(Instant::now());
    }

    /// Parks that CANNOT have reached the user must pass false. Checking only the call
    /// FORM is not enough — the first version of this guard accepted park_ctx(true, ..)
    /// from the webhook path, which is precisely the background parker the whole
    /// mechanism exists to stop.
    ///
    /// EVERY webhook park site, not the first one. This guard used to read
    /// `src.find("confirm::park_ctx(")` and check the ~40 bytes after it — the
    /// first-occurrence-only shape that let a `mission.resumed` SETTLE emit ship
    /// unguarded in anthropic.rs (a topic with two emit sites whose guard windowed on
    /// site one and silently vouched for site two). webhooks.rs has exactly ONE park
    /// site today, so the old form was CORRECT and not a live bug — it was a guard
    /// that would go quiet the day someone added a second one, which is the day it is
    /// needed. `every_park_call_site_states_whether_it_reaches_the_user` would not
    /// have caught that either: it ACCEPTS `true,` as a classified argument, and
    /// "this file's parks must be `false`" is the rule only stated here. The site
    /// count is floored so a rotted needle fails instead of passing vacuously.
    #[test]
    fn background_only_parkers_declare_that_no_one_was_prompted() {
        // An inbound webhook fires on its own schedule; nobody was prompted this turn.
        let src = include_str!("webhooks.rs");
        let mut sites = 0usize;
        for (n, line) in src.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            let Some(at) = t.find("confirm::park_ctx(") else { continue };
            sites += 1;
            let call = &t[at..];
            assert!(
                call.contains("park_ctx(false"),
                "webhooks.rs:{}: the webhook path claims its prompt reached the user, so \
                 it arms PROMPTED and can steal a confirm the operator spoke for \
                 something else: {call}",
                n + 1
            );
        }
        assert!(
            sites >= 1,
            "webhooks.rs no longer parks (or the needle rotted); re-point this guard \
             rather than letting it pass over zero sites"
        );
    }

    /// A PARKED PREVIEW IS A QUESTION, NOT A RESULT — it must never be cached.
    ///
    /// tool_loop's per-turn dedup ledger stored any non-error outcome, and a park is
    /// non-error. So if the model re-requested an identical call later in the same
    /// turn, AFTER parking a different consequential action in between, the dedup
    /// branch replayed the stored ALPHA prompt verbatim without re-parking. The model
    /// asked the user about ALPHA while the armed slot held BETA, and the spoken
    /// "confirm" fired arguments the user was never read. Same end state as the
    /// hijack above, reached through the cache instead of the slot.
    ///
    /// Checked at the source, because tool_loop needs a live model to drive: BOTH
    /// ledger inserts must be gated on the call having actually run.
    ///
    /// This guard used to pin the literal `if !parked {`, computed from
    /// `is_parked_consequential`. That predicate was itself the bug — it re-derived
    /// "did this park?" from the master switch and the consequential registry while
    /// ignoring the per-action policy, so an `Always` rule that FIRED was labeled
    /// parked and its insert was skipped, double-firing the actuator on a repeated
    /// tool_use block. `execute_tool` now reports a `ToolEffect` and both loops gate
    /// on `Executed`. What the effect means in each regime is asserted behaviorally
    /// in anthropic.rs's
    /// `execute_tool_reports_park_and_execute_apart_under_the_same_master_switch`;
    /// this guard only holds the wiring — that no insert is ever unguarded.
    #[test]
    fn neither_tool_loop_caches_a_call_that_did_not_run() {
        let src = include_str!("anthropic.rs");
        let inserts: Vec<_> = src.match_indices("seen.insert(signature,").collect();
        assert_eq!(
            inserts.len(),
            2,
            "expected exactly the cloud + offline tool-loop ledger inserts; a new one \
             appeared (or one moved) and is not covered by this guard"
        );
        for (at, _) in inserts {
            // Walk back to the nearest enclosing `if` LINE (skipping comments, so
            // prose containing "if" cannot stand in for the real guard) and require
            // it to test the reported effect. A bare `if !is_error {` is what
            // shipped the defect.
            let guard = src[..at]
                .lines()
                .rev()
                .map(str::trim)
                .find(|l| l.starts_with("if ") && !l.starts_with("//"))
                .unwrap_or("<no enclosing if>");
            // Either the effect is matched inline, or the guard is a boolean bound
            // from it one line earlier — resolve that binding rather than trusting
            // the name, so an `if did_execute {` whose `did_execute` quietly went
            // back to meaning something else still fails here.
            let bound_from_effect = guard
                .strip_prefix("if ")
                .and_then(|r| r.strip_suffix(" {"))
                .is_some_and(|ident| {
                    src.contains(&format!(
                        "let {ident} = matches!(effect, ToolEffect::Executed);"
                    ))
                });
            assert!(
                guard.contains("ToolEffect::Executed") || bound_from_effect,
                "a ledger insert is guarded by `{guard}`, which is not the effect the \
                 call reported; caching a parked preview replays a stale confirmation \
                 prompt, and not caching a real execution lets a repeated tool_use \
                 block fire the actuator twice"
            );
        }
    }

    /// No park call site may leave "did this prompt reach the user?" unclassified.
    ///
    /// WHAT WENT WRONG: this guard scanned for the literal `confirm::park(` — a
    /// spelling that has not existed since `park_ctx` replaced it. `"confirm::park_ctx("`
    /// does NOT contain `"confirm::park("` (the `k` is followed by `_`, not `(`), so the
    /// needle matched exactly ONE line in the entire tree: this assertion's own string
    /// literal, in a file the loop does not read. The assertion evaluated to
    /// `!false || _` on every input — it could not fail. Injecting the exact regression
    /// its own docstring names (`park_ctx(true, ..)` on the webhook path) left it green.
    ///
    /// It now reads the ARGUMENT, not the spelling: every `park_ctx(` call site must
    /// pass one of a small allowlist of context expressions, so a new call site cannot
    /// arm PROMPTED by accident — it has to be classified here first. The floor on the
    /// site count is the anti-rot guard: if the needle stops matching again, the test
    /// fails instead of passing vacuously.
    #[test]
    fn every_park_call_site_states_whether_it_reaches_the_user() {
        const FILES: &[(&str, &str)] = &[
            ("anthropic.rs", include_str!("anthropic.rs")),
            ("command.rs", include_str!("command.rs")),
            ("lockdown.rs", include_str!("lockdown.rs")),
            ("main.rs", include_str!("main.rs")),
            ("standing.rs", include_str!("standing.rs")),
            ("webhooks.rs", include_str!("webhooks.rs")),
        ];
        // The ONLY accepted first arguments. `true` = the prompt is spoken to the
        // operator on THIS turn; `false` = a background parker (nobody was prompted);
        // `context_trusted` = the tool loop's per-turn flag. Anything else has to be
        // added here deliberately — that IS the guard.
        const ACCEPTED: &[&str] = &["true,", "false,", "context_trusted,"];
        let mut sites = 0usize;
        for (file, src) in FILES {
            let lines: Vec<&str> = src.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let t = line.trim_start();
                if t.starts_with("//") {
                    continue;
                }
                let Some(at) = t.find("park_ctx(") else { continue };
                let mut rest = t[at + "park_ctx(".len()..].to_string();
                if rest.trim().is_empty() {
                    // A multi-line call: the context argument is on the next line.
                    rest = lines.get(i + 1).map(|l| l.trim_start()).unwrap_or("").to_string();
                }
                sites += 1;
                assert!(
                    ACCEPTED.iter().any(|a| rest.starts_with(a)),
                    "{file}:{} parks with an unclassified context argument `{}` — the \
                     call site must say whether its prompt reached the user, because an \
                     unattended park that arms PROMPTED steals the operator's spoken \
                     confirm",
                    i + 1,
                    rest.chars().take(40).collect::<String>()
                );
            }
        }
        assert!(
            sites >= 10,
            "the guard found only {sites} park_ctx call sites; the needle has rotted \
             again and this test is passing vacuously"
        );
    }


}
