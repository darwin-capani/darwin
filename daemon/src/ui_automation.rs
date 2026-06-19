//! GATED UI AUTOMATION (#44) — the CAPSTONE, and the SINGLE MOST DANGEROUS
//! capability JARVIS has: actually ACTUATING the macOS UI (posting a synthetic
//! mouse click, typing keystrokes, sending a key combo) on the user's behalf. It
//! is built to the same honesty-first, maximally-gated, deny-default contract as
//! the sandboxed shell (#43) — but stricter still, because an actuation is an
//! IRREVERSIBLE physical effect on a live machine. It has, in order, a stack of
//! independent layers an instruction must clear before a single CGEvent fires,
//! and a final — the actuation itself — that is DEVICE-GATED (built here, NEVER
//! invoked under `cargo test`):
//!
//!   1. PURE SINGLE-ACTION PLANNER ([`plan_actuation`]) — maps one instruction +
//!      target to exactly ONE [`ActuationPlan`] holding exactly ONE [`Action`]
//!      (a single `Click` / `Type` / `Key`). It VALIDATES + BOUNDS the action
//!      (a click coordinate must lie inside the supplied screen bounds; an empty
//!      type / key is refused; a degenerate/empty instruction is refused). By
//!      CONSTRUCTION the type holds ONE action — there is no `Vec<Action>`, no
//!      sequence, no batch field — so a plan can NEVER encode more than one
//!      actuation. ONE plan = ONE actuation.
//!
//!   2. CONFIG GATE ([`ui_automation_permitted`]) — `[ui_automation].enabled`
//!      ships **false**. With it off the actuate intent is never classified and
//!      the `ui_actuate` tool is inert (an honest "off" reply); nothing is
//!      planned, parked, or actuated.
//!
//!   3. GATE ROUTING (the safety spine, wired in `anthropic`/`confirm`) — the
//!      actuate tool (`ui_actuate`) is in [`crate::confirm::CONSEQUENTIAL_TOOLS`],
//!      so `execute_tool` PARKS it for a cross-turn spoken human "yes" — PER
//!      ACTION. It only ever ACTUATES under `gate(confirm) == Execute`, i.e. the
//!      master switch `[integrations].allow_consequential` is ON **and** the
//!      human confirmed **and** `!is_locked_down()` **and** the voice-id owner
//!      gate passed. ONE confirm authorizes EXACTLY ONE actuation: a second
//!      actuation re-parks for its OWN spoken yes — there is no path that batches
//!      actuations, pre-approves a plan of several, or loops autonomously.
//!
//!   4. ACTUATION SEAM ([`do_actuate`], DEVICE-gated) — would post the CGEvent /
//!      Accessibility (AX) action for the ONE planned action, guarded by an
//!      Accessibility-TCC check ([`accessibility_permission_granted`]) that
//!      honestly reports "accessibility permission not granted" when absent. It
//!      is WIRED behind the gate + `[ui_automation].enabled` but is the
//!      device-gated precedent (vision-capture / apply-heal / shell-exec): it is
//!      BUILT, NOT invoked in any test. No test ever posts a real event.
//!
//! HONESTY: the planner + the gate routing are proven HERMETICALLY (pure
//! functions, no event post, no display, no daemon). The real actuation is
//! DEVICE-gated (it needs the Accessibility TCC consent — runtime user consent,
//! NOT SBPL-grantable — and a real display) and is NOT claimed proven here. An
//! actuation NEVER auto-runs (it always parks PER-ACTION for a spoken confirm)
//! and is NEVER batched or autonomous. The Vision app stays READ-ONLY (it
//! LOCATES a control; this is a SEPARATE, maximally-gated actuate op). An
//! actuation result is NEVER fabricated.

// ---------------------------------------------------------------------------
// (2) CONFIG GATE — may UI automation actuate at all? Mirrors shell::
// shell_permitted: the master `[ui_automation].enabled` switch (ships false).
// With it off the feature is inert.
// ---------------------------------------------------------------------------

/// Whether gated UI automation may actuate: the `[ui_automation].enabled` switch
/// is on. With it false (the shipped default) the actuate intent is never
/// classified and the `ui_actuate` tool is inert — exactly like
/// `shell::shell_permitted` / `code::code_permitted`. This is the CONFIG gate; it
/// is independent of (and ANDed beneath) the master switch + confirm + voice-id +
/// lockdown gates the gate routing enforces, AND the device Accessibility-TCC
/// gate the actuation seam enforces.
pub fn ui_automation_permitted(enabled: bool) -> bool {
    enabled
}

// ---------------------------------------------------------------------------
// (1) PURE SINGLE-ACTION PLANNER — plan_actuation(instruction|target) ->
// ActuationPlan { ONE Action, target_desc }. ONE plan = ONE actuation. The type
// holds EXACTLY one action by construction (no Vec, no sequence) so a plan can
// NEVER carry a batch. Validates + bounds the action; refuses a degenerate one.
// ---------------------------------------------------------------------------

/// The pixel bounds of the display an actuation targets. A planned `Click` must
/// land strictly inside `[0, width) x [0, height)` — a coordinate outside the
/// real screen is a degenerate plan and is refused, so a fabricated/off-screen
/// click can never be planned. Supplied by the caller (on-device, from the live
/// display geometry); in tests a fixed bound makes the planner hermetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenBounds {
    pub width: i32,
    pub height: i32,
}

impl ScreenBounds {
    /// Is `(x, y)` a real on-screen pixel: `0 <= x < width` and `0 <= y < height`?
    /// Strict upper bound — a click at exactly `width`/`height` is off the last
    /// pixel and is refused. A non-positive screen (no display) accepts nothing.
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < self.width && y < self.height
    }
}

/// EXACTLY ONE UI actuation. This enum is the WHOLE action surface — a plan holds
/// one of these and no more. There is deliberately NO variant that wraps a
/// sequence/batch of actions: by construction a single actuation is the most a
/// plan can ever represent, so the per-action gate is structurally guaranteed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A single synthetic mouse click at one on-screen pixel (bounded to the
    /// real display by the planner).
    Click { x: i32, y: i32 },
    /// Typing ONE run of text (a single actuation — the whole string is one
    /// `type` op, not a batch of per-key actions the gate could be tricked into
    /// treating as one confirm-per-keystroke). Non-empty.
    Type { text: String },
    /// A single key combo (e.g. "cmd+s", "return", "escape"). Non-empty.
    Key { combo: String },
}

impl Action {
    /// A short, human-readable verb for the spoken preview / telemetry ("click",
    /// "type", "key"). Never leaks more than the action class.
    pub fn verb(&self) -> &'static str {
        match self {
            Action::Click { .. } => "click",
            Action::Type { .. } => "type",
            Action::Key { .. } => "key",
        }
    }
}

/// A validated, BOUNDED plan for EXACTLY ONE UI actuation. It carries the single
/// [`Action`] and a faithful human-readable `target_desc` (what the user named —
/// e.g. "the Send button"). ONE `ActuationPlan` = ONE actuation: there is no
/// field that could hold a second action, so a plan can never pre-approve a
/// batch. The struct is constructed ONLY by [`plan_actuation`] (the validating
/// planner) — there is no public way to build an unbounded one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActuationPlan {
    /// The SINGLE action to perform. Exactly one — never a list.
    action: Action,
    /// A faithful description of the target, for the preview / spoken confirm /
    /// telemetry (the secret-free summary). Never empty.
    target_desc: String,
}

impl ActuationPlan {
    /// The one planned action (read-only). The seam actuates exactly this and
    /// nothing else.
    pub fn action(&self) -> &Action {
        &self.action
    }

    /// The faithful target description for the preview / telemetry.
    pub fn target_desc(&self) -> &str {
        &self.target_desc
    }

    /// A faithful one-line preview of the single actuation, for the dry-run /
    /// spoken-confirm path. Names the action class + the target — never claims it
    /// happened. PURE; the secret-free summary the audit log can carry.
    pub fn preview(&self) -> String {
        match &self.action {
            Action::Click { x, y } => format!(
                "click at ({x}, {y}) on \"{}\"",
                self.target_desc
            ),
            Action::Type { text } => format!(
                "type {} character(s) into \"{}\"",
                text.chars().count(),
                self.target_desc
            ),
            Action::Key { combo } => format!(
                "press the key combo \"{combo}\" on \"{}\"",
                self.target_desc
            ),
        }
    }
}

/// Why an instruction could not be planned into ONE valid, bounded actuation.
/// A refused instruction is NEVER actuated and NEVER parked — it is reported
/// honestly (the daemon arm renders a spoken refusal), exactly like the shell
/// denylist refusal happens PRE-park.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The instruction / target was empty or whitespace-only — nothing to do.
    Empty,
    /// A click coordinate fell OUTSIDE the real screen bounds (off-screen /
    /// fabricated). Refused: an actuation must land on a real pixel.
    OffScreen { x: i32, y: i32, bounds: ScreenBounds },
    /// The text to type / the key combo was empty — a degenerate no-op actuation.
    DegenerateAction,
}

impl PlanError {
    /// A faithful, honest one-line reason for the spoken refusal. Never fabricates
    /// success; states precisely why nothing will be actuated.
    pub fn reason(&self) -> String {
        match self {
            PlanError::Empty => "the instruction named no target to act on".to_string(),
            PlanError::OffScreen { x, y, bounds } => format!(
                "the target ({x}, {y}) is off-screen (the display is {}x{}), so I won't click there",
                bounds.width, bounds.height
            ),
            PlanError::DegenerateAction => {
                "the action was empty (nothing to type / no key to press)".to_string()
            }
        }
    }
}

/// What the planner is asked to actuate: exactly ONE action against a named
/// target. The caller builds this from the model's tool input (the located
/// control + the requested action) — the planner's job is to VALIDATE + BOUND it
/// into an [`ActuationPlan`] or REFUSE it. A request, like a plan, holds exactly
/// ONE action by construction — there is no list — so a batch can never enter
/// the planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActuationRequest {
    /// The single action requested.
    pub action: Action,
    /// A human-readable description of the target the user named (e.g. "the Send
    /// button"). Used as the plan's `target_desc`; refused if empty.
    pub target_desc: String,
}

/// PLAN exactly ONE UI actuation from a request + the live screen bounds. PURE —
/// no event post, no display, no I/O; the single source of truth for "is this a
/// valid, bounded, single actuation".
///
/// Validation / bounding (deny-leaning, by construction):
///   * a degenerate request (empty target description, empty type text, empty
///     key combo) is REFUSED — there is no plan to park or actuate;
///   * a `Click` coordinate MUST lie strictly inside the real screen bounds — an
///     off-screen / fabricated coordinate is REFUSED (an actuation must land on a
///     real pixel);
///   * the resulting [`ActuationPlan`] holds EXACTLY ONE action — by the type's
///     construction it can never hold a sequence/batch, so ONE plan is ONE
///     actuation and the per-action gate is structurally guaranteed.
pub fn plan_actuation(
    request: &ActuationRequest,
    bounds: ScreenBounds,
) -> Result<ActuationPlan, PlanError> {
    // The target description must name something — an empty target is a
    // degenerate instruction we refuse before any plan exists.
    let target = request.target_desc.trim();
    if target.is_empty() {
        return Err(PlanError::Empty);
    }

    // Validate + bound the single action.
    match &request.action {
        Action::Click { x, y } => {
            if !bounds.contains(*x, *y) {
                return Err(PlanError::OffScreen { x: *x, y: *y, bounds });
            }
        }
        Action::Type { text } => {
            if text.is_empty() {
                return Err(PlanError::DegenerateAction);
            }
        }
        Action::Key { combo } => {
            if combo.trim().is_empty() {
                return Err(PlanError::DegenerateAction);
            }
        }
    }

    Ok(ActuationPlan {
        action: request.action.clone(),
        target_desc: target.to_string(),
    })
}

// ---------------------------------------------------------------------------
// (4) ACTUATION SEAM (DEVICE-gated — built, NEVER invoked under cargo test). It
// would post the CGEvent / AX action for the ONE planned action, guarded by an
// Accessibility-TCC consent check. Mirrors shell::run_sandboxed + the vision-
// capture device-gated precedent. NO test calls do_actuate.
// ---------------------------------------------------------------------------

/// The faithful result of a single actuation. Carries ONLY that the one planned
/// action was posted (or honestly why it was not) — NEVER fabricated. The seam
/// returns this; the caller renders it into the spoken outcome / telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActuateResult {
    /// The verb of the single action that was actuated ("click"/"type"/"key").
    pub verb: &'static str,
    /// The faithful target description the action was performed against.
    pub target_desc: String,
}

/// Why a device-gated actuation could not be performed. Reported HONESTLY — the
/// daemon never claims an actuation happened when it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActuateError {
    /// The Accessibility TCC permission is not granted (runtime user consent, NOT
    /// SBPL-grantable). Without it macOS rejects synthetic events / AX actions, so
    /// the seam refuses honestly rather than pretending. This is the variant the
    /// device-gated seam returns when consent is absent.
    AccessibilityNotGranted,
    /// No usable display to actuate against (headless / locked screen). Constructed
    /// only on-device by the CGEvent/AX post path (built, never run in a test).
    #[allow(dead_code)] // on-device-only error arm; the post seam is device-gated
    NoDisplay,
    /// The underlying CGEvent / AX post failed on-device. Carries an honest detail.
    /// Constructed only on-device by the post path (built, never run in a test).
    #[allow(dead_code)] // on-device-only error arm; the post seam is device-gated
    PostFailed(String),
    /// There is no on-device CGEvent backend on THIS build (a non-macOS host), so
    /// NO synthetic event could be posted. On macOS the post path is now REAL
    /// (CGEvent); this variant remains the honest answer for a host that has no
    /// CGEvent surface at all — the daemon reports the TRUTH ("nothing was changed")
    /// instead of claiming a click/keystroke that never happened. (Constructed only on
    /// the non-macOS post arm; on macOS the post seams use `PostFailed` for any real
    /// post failure, so this is never hit there.)
    #[allow(dead_code)] // constructed only on the non-macOS post arm + asserted in tests
    BackendUnavailable,
}

impl ActuateError {
    /// A faithful, honest one-line reason — never fabricates success.
    pub fn reason(&self) -> String {
        match self {
            ActuateError::AccessibilityNotGranted => {
                "accessibility permission not granted (grant it in System Settings › Privacy & \
                 Security › Accessibility — it is runtime consent macOS will not let me self-grant)"
                    .to_string()
            }
            ActuateError::NoDisplay => "no usable display to act on".to_string(),
            ActuateError::PostFailed(d) => format!("the actuation failed on-device: {d}"),
            ActuateError::BackendUnavailable => {
                "UI actuation is not available on this build — the planner and the safety gates are \
                 real, but the on-device CGEvent backend is not implemented for this host (it is \
                 not macOS), so no action was performed"
                    .to_string()
            }
        }
    }
}

/// DEVICE-GATED check: is the Accessibility (TCC) permission granted to THIS
/// process? On macOS the answer comes from `AXIsProcessTrusted()` (exported by the
/// ApplicationServices framework). It is runtime USER consent (the user toggles it
/// in System Settings › Privacy & Security › Accessibility) — it is NOT
/// SBPL-grantable and JARVIS can never self-grant it. Without it, macOS silently
/// drops synthetic CGEvents and rejects AX actions, so the seam MUST refuse
/// honestly when it is absent rather than fabricate a click.
///
/// We resolve `AXIsProcessTrusted` DYNAMICALLY at runtime (`dlopen` the framework,
/// `dlsym` the symbol) rather than at LINK time, deliberately: the daemon is
/// otherwise framework-free and hermetic, and this seam is the device-gated
/// precedent — built, never invoked under `cargo test`. Dynamic resolution keeps
/// the hermetic build with no link-time framework dependency, and stays HONEST: if
/// the framework or symbol can't be resolved (a non-device / stripped host), we
/// return `false` and the seam refuses, never fabricating consent. It is NEVER
/// reached under cargo test (no test calls `do_actuate`, the only caller).
#[cfg(target_os = "macos")]
pub fn accessibility_permission_granted() -> bool {
    use std::os::raw::{c_char, c_int, c_void};
    extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
    const RTLD_NOW: c_int = 0x2;
    // The ApplicationServices umbrella framework exports AXIsProcessTrusted.
    let fw = b"/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices\0";
    let sym = b"AXIsProcessTrusted\0";
    // SAFETY: dlopen/dlsym are the standard runtime-link primitives; on success the
    // resolved symbol is the parameter-less, side-effect-free C query that returns a
    // Boolean reporting whether this process holds the Accessibility TCC grant. A
    // null handle/symbol (framework or symbol absent) is treated as "not granted".
    unsafe {
        let handle = dlopen(fw.as_ptr() as *const c_char, RTLD_NOW);
        if handle.is_null() {
            return false;
        }
        let ptr = dlsym(handle, sym.as_ptr() as *const c_char);
        if ptr.is_null() {
            return false;
        }
        let ax_is_process_trusted: extern "C" fn() -> bool = std::mem::transmute(ptr);
        ax_is_process_trusted()
    }
}

/// On a non-macOS host there is no Accessibility TCC / CGEvent surface at all, so
/// the permission can never be present — the seam refuses honestly.
#[cfg(not(target_os = "macos"))]
pub fn accessibility_permission_granted() -> bool {
    false
}

/// DEVICE-GATED ACTUATION SEAM. Posts the REAL CGEvent (a synthetic mouse `Click`,
/// a unicode `Type`, or a key `Key`) for the ONE [`ActuationPlan`]'s single action,
/// AFTER an Accessibility-TCC consent check. It performs EXACTLY ONE actuation —
/// the plan holds exactly one action by construction — and then returns; it NEVER
/// loops, NEVER batches, and NEVER re-derives a second action.
///
/// IT POSTS REAL EVENTS ON-DEVICE, but IS NEVER INVOKED IN ANY TEST. Like the
/// vision-capture / apply-heal / shell-exec device-gated precedent, the REAL
/// actuation only happens on-device behind the full gate ([`ui_automation_permitted`]
/// + the master switch + the spoken per-action confirm replay + voice-id +
/// `!lockdown`) AND the Accessibility TCC consent (a real display). The planner +
/// the gate routing are proven hermetically; the actuation itself is device-gated
/// (the synthetic CGEvent only lands when macOS has the runtime TCC grant and a live
/// display — neither can exist under `cargo test`). This function NEVER runs unless
/// the caller has already cleared every gate. It NEVER fabricates an actuation
/// result — when consent is absent it returns [`ActuateError::AccessibilityNotGranted`],
/// and on ANY post failure it returns [`ActuateError::PostFailed`] (an honest
/// detail), never a fake success.
///
/// Preconditions the caller MUST have established before calling this:
///   1. [`ui_automation_permitted`] is true (`[ui_automation].enabled`),
///   2. the request planned into a valid, bounded, SINGLE-action [`ActuationPlan`]
///      (NOT a [`PlanError`]),
///   3. the master switch is ON, the human CONFIRMED THIS ONE actuation (the
///      parked per-action replay), `!is_locked_down()`, and the voice-id owner
///      gate passed.
/// This seam does not re-check the gate-routing layers — those are the gate
/// routing's job; it does its OWN device check (Accessibility TCC) and is the
/// final, narrowly-scoped, single-action actuator.
#[allow(dead_code)] // device-gated seam: wired behind the gate, never run in tests
pub async fn do_actuate(plan: &ActuationPlan) -> Result<ActuateResult, ActuateError> {
    // DEVICE GATE: the Accessibility TCC consent. Without it macOS drops every
    // synthetic event — refuse HONESTLY rather than pretend the click landed.
    if !accessibility_permission_granted() {
        return Err(ActuateError::AccessibilityNotGranted);
    }

    // ONE actuation — exactly the plan's single action, then return. No loop, no
    // batch, no second action. This is where the REAL CGEvent post happens
    // on-device; the hermetic tests never reach it.
    match plan.action() {
        Action::Click { x, y } => {
            // Post a left mouse-down + mouse-up CGEvent at (x, y) via
            // CGEventCreateMouseEvent + CGEventPost(kCGHIDEventTap, …). Real on-device
            // post; device-gated, never invoked in a test.
            post_click(*x, *y)?;
        }
        Action::Type { text } => {
            // Post ONE synthetic keyboard CGEvent carrying the whole unicode run via
            // CGEventKeyboardSetUnicodeString for this ONE type op. Real; device-gated.
            post_type(text)?;
        }
        Action::Key { combo } => {
            // Post a key-down + key-up CGEvent for the ONE parsed combo (with modifier
            // flags). Real; device-gated. An unmappable combo returns an HONEST error
            // (never a fabricated/wrong key).
            post_key(combo)?;
        }
    }

    Ok(ActuateResult {
        verb: plan.action().verb(),
        target_desc: plan.target_desc().to_string(),
    })
}

// ---------------------------------------------------------------------------
// REAL CGEvent BACKEND (macOS) — the post_* seams below now post REAL synthetic
// events on-device via dynamically-resolved CoreGraphics symbols. We follow the
// EXACTLY same dynamic-resolution precedent as accessibility_permission_granted:
// dlopen the CoreGraphics framework + dlsym the C entry points — NO new link-time
// crate dependency, the build stays hermetic. The events only land when macOS has
// the runtime TCC Accessibility grant + a live display, so this is DEVICE-gated and
// NEVER reached under cargo test. All FFI uses the correct C signatures, and every
// CF object this code creates is CFRelease'd (no leak, no double-free, no UB).
// ---------------------------------------------------------------------------

/// Dynamically-resolved CoreGraphics entry points for synthetic event posting. All
/// CG types are opaque to us — we carry them as raw pointers (`*mut c_void`), which
/// is exactly how the C ABI passes `CGEventRef` / `CGEventSourceRef`. Resolving the
/// symbols at runtime (dlopen + dlsym) keeps the daemon framework-free at link time,
/// matching the [`accessibility_permission_granted`] precedent. Returns `None` (so
/// the seam fails honestly) if the framework or any symbol cannot be resolved.
#[cfg(target_os = "macos")]
#[allow(dead_code)] // fields read only by the device-gated post_* seams (never run in a test)
struct CoreGraphics {
    // CGEventCreateMouseEvent(source, mouseType, CGPoint{f64,f64}, mouseButton) -> CGEventRef
    create_mouse_event: extern "C" fn(*mut c_void, u32, CGPoint, u32) -> *mut c_void,
    // CGEventCreateKeyboardEvent(source, CGKeyCode(u16), keyDown(bool)) -> CGEventRef
    create_keyboard_event: extern "C" fn(*mut c_void, u16, bool) -> *mut c_void,
    // CGEventKeyboardSetUnicodeString(event, length(UniCharCount=usize), *const UniChar(u16))
    keyboard_set_unicode_string: extern "C" fn(*mut c_void, usize, *const u16),
    // CGEventSetFlags(event, CGEventFlags(u64))
    set_flags: extern "C" fn(*mut c_void, u64),
    // CGEventPost(tapLocation(u32), event)
    post: extern "C" fn(u32, *mut c_void),
    // CFRelease(CFTypeRef) — release every CGEventRef we create.
    cf_release: extern "C" fn(*mut c_void),
}

/// CoreGraphics `CGPoint` — two C `double`s, `#[repr(C)]` so the FFI layout is the
/// real struct passed by value to `CGEventCreateMouseEvent`.
#[cfg(target_os = "macos")]
#[repr(C)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
use std::os::raw::{c_char, c_int, c_void};

// CGEventType mouse constants (CGEventTypes.h).
#[cfg(target_os = "macos")]
const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
#[cfg(target_os = "macos")]
const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
// CGMouseButton.kCGMouseButtonLeft.
#[cfg(target_os = "macos")]
const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;
// CGEventTapLocation.kCGHIDEventTap — post into the HID stream (system-wide).
#[cfg(target_os = "macos")]
const K_CG_HID_EVENT_TAP: u32 = 0;

// CGEventFlags modifier masks (CGEventTypes.h).
#[cfg(target_os = "macos")]
const K_CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
#[cfg(target_os = "macos")]
const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
#[cfg(target_os = "macos")]
const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000; // option/alt
#[cfg(target_os = "macos")]
const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;

#[cfg(target_os = "macos")]
#[allow(dead_code)] // device-gated: resolve() is reached only from the post_* seams
impl CoreGraphics {
    /// Resolve the CoreGraphics symbols at runtime. `None` if the framework or any
    /// symbol is missing (a stripped / non-device host) — the seam then fails
    /// honestly, never fabricating a post.
    fn resolve() -> Option<CoreGraphics> {
        extern "C" {
            fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
            fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        }
        const RTLD_NOW: c_int = 0x2;
        // SAFETY: dlopen/dlsym are the standard runtime-link primitives. We resolve
        // each symbol and transmute it to its exact C signature (verified against
        // CGEvent.h / CGEventTypes.h); a null handle/symbol => None (resolve fails,
        // the seam refuses honestly). No CG object is created here, so nothing to
        // release. The handle is intentionally NOT dlclose'd: CoreGraphics is a
        // system framework that stays mapped for the process lifetime, and closing it
        // could invalidate symbols other callers hold — leaking the handle is the
        // sound, conventional choice (matches accessibility_permission_granted).
        unsafe {
            let fw =
                b"/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics\0";
            let handle = dlopen(fw.as_ptr() as *const c_char, RTLD_NOW);
            if handle.is_null() {
                return None;
            }
            // Resolve one symbol by name; null => bail (honest failure).
            let sym = |name: &[u8]| -> Option<*mut c_void> {
                let p = dlsym(handle, name.as_ptr() as *const c_char);
                if p.is_null() {
                    None
                } else {
                    Some(p)
                }
            };
            Some(CoreGraphics {
                create_mouse_event: std::mem::transmute::<
                    *mut c_void,
                    extern "C" fn(*mut c_void, u32, CGPoint, u32) -> *mut c_void,
                >(sym(b"CGEventCreateMouseEvent\0")?),
                create_keyboard_event: std::mem::transmute::<
                    *mut c_void,
                    extern "C" fn(*mut c_void, u16, bool) -> *mut c_void,
                >(sym(b"CGEventCreateKeyboardEvent\0")?),
                keyboard_set_unicode_string: std::mem::transmute::<
                    *mut c_void,
                    extern "C" fn(*mut c_void, usize, *const u16),
                >(sym(b"CGEventKeyboardSetUnicodeString\0")?),
                set_flags: std::mem::transmute::<*mut c_void, extern "C" fn(*mut c_void, u64)>(
                    sym(b"CGEventSetFlags\0")?,
                ),
                post: std::mem::transmute::<*mut c_void, extern "C" fn(u32, *mut c_void)>(
                    sym(b"CGEventPost\0")?,
                ),
                cf_release: std::mem::transmute::<*mut c_void, extern "C" fn(*mut c_void)>(
                    // CFRelease lives in CoreFoundation; CoreGraphics re-exports the
                    // CF runtime, but resolve it from CoreFoundation directly to be
                    // certain (its symbol is always present there).
                    {
                        let cf = b"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation\0";
                        let cf_handle = dlopen(cf.as_ptr() as *const c_char, RTLD_NOW);
                        if cf_handle.is_null() {
                            return None;
                        }
                        let p = dlsym(cf_handle, b"CFRelease\0".as_ptr() as *const c_char);
                        if p.is_null() {
                            return None;
                        }
                        p
                    },
                ),
            })
        }
    }
}

/// DEVICE-GATED (macOS): post a single synthetic left-click at `(x, y)` via real
/// CGEvents. Builds `CGEventCreateMouseEvent(NULL, …Down)` + `…Up` and
/// `CGEventPost(kCGHIDEventTap, …)` for each, releasing both events. ONE click
/// (down then up), then returns — no loop, no batch. Reached only from
/// [`do_actuate`] after every gate + the TCC consent passed; NEVER run in a test.
#[cfg(target_os = "macos")]
#[allow(dead_code)]
fn post_click(x: i32, y: i32) -> Result<(), ActuateError> {
    let cg = CoreGraphics::resolve()
        .ok_or_else(|| ActuateError::PostFailed("CoreGraphics symbols unavailable".into()))?;
    let point = CGPoint { x: x as f64, y: y as f64 };
    // FFI CONTRACT: the resolved entry points are safe `extern "C" fn` values
    // (their unsafety was discharged in resolve()'s transmute). create_mouse_event
    // takes a NULL event source (valid — "default source"). Each returned CGEventRef
    // is owned by us (the Create rule) and is CFRelease'd exactly once below; a null
    // return means creation failed — refuse honestly. We post down then up (ONE click).
    let down = (cg.create_mouse_event)(
        std::ptr::null_mut(),
        K_CG_EVENT_LEFT_MOUSE_DOWN,
        CGPoint { x: point.x, y: point.y },
        K_CG_MOUSE_BUTTON_LEFT,
    );
    if down.is_null() {
        return Err(ActuateError::PostFailed("could not create mouse-down event".into()));
    }
    (cg.post)(K_CG_HID_EVENT_TAP, down);
    (cg.cf_release)(down);

    let up = (cg.create_mouse_event)(
        std::ptr::null_mut(),
        K_CG_EVENT_LEFT_MOUSE_UP,
        point,
        K_CG_MOUSE_BUTTON_LEFT,
    );
    if up.is_null() {
        return Err(ActuateError::PostFailed("could not create mouse-up event".into()));
    }
    (cg.post)(K_CG_HID_EVENT_TAP, up);
    (cg.cf_release)(up);
    Ok(())
}

/// DEVICE-GATED (macOS): type ONE run of `text` as a single synthetic keyboard
/// event carrying the whole unicode string (the robust path: a synthetic key event
/// with `CGEventKeyboardSetUnicodeString` types arbitrary text without per-key
/// keycode mapping). Posts ONE key-down (carrying the string) then its key-up,
/// releasing both. ONE type op, then returns. NEVER run in a test.
#[cfg(target_os = "macos")]
#[allow(dead_code)]
fn post_type(text: &str) -> Result<(), ActuateError> {
    // Empty text is refused by the planner, but stay defensive: nothing to type.
    if text.is_empty() {
        return Err(ActuateError::PostFailed("empty text".into()));
    }
    let cg = CoreGraphics::resolve()
        .ok_or_else(|| ActuateError::PostFailed("CoreGraphics symbols unavailable".into()))?;
    // CGEventKeyboardSetUnicodeString wants UTF-16 (UniChar = u16) units.
    let utf16: Vec<u16> = text.encode_utf16().collect();
    // FFI CONTRACT: create_keyboard_event(NULL source, keycode, keyDown) with keycode
    // 0 is the conventional carrier for a unicode string (the keycode is overridden by
    // the attached string). keyboard_set_unicode_string takes the UTF-16 length + a
    // pointer to that many u16 units (the Vec stays alive for the whole call). Each
    // event is owned by us and CFRelease'd exactly once. The string is attached to
    // both down and up so the run is delivered once on key-down; up carries it too for
    // parity (standard practice) but the OS delivers the characters on down.
    let down = (cg.create_keyboard_event)(std::ptr::null_mut(), 0, true);
    if down.is_null() {
        return Err(ActuateError::PostFailed("could not create keyboard-down event".into()));
    }
    (cg.keyboard_set_unicode_string)(down, utf16.len(), utf16.as_ptr());
    (cg.post)(K_CG_HID_EVENT_TAP, down);
    (cg.cf_release)(down);

    let up = (cg.create_keyboard_event)(std::ptr::null_mut(), 0, false);
    if up.is_null() {
        return Err(ActuateError::PostFailed("could not create keyboard-up event".into()));
    }
    (cg.keyboard_set_unicode_string)(up, utf16.len(), utf16.as_ptr());
    (cg.post)(K_CG_HID_EVENT_TAP, up);
    (cg.cf_release)(up);
    Ok(())
}

/// Map ONE combo string (e.g. `"cmd+s"`, `"return"`, `"escape"`, `"shift+tab"`) to
/// a `(keycode, modifier_flags)` pair using the ANSI virtual keycodes
/// (`Carbon/HIToolbox/Events.h`). HONESTY OVER COMPLETENESS: if the base key cannot
/// be mapped to a correct keycode, this returns `None` and the caller posts NO event
/// (an honest [`ActuateError::PostFailed`]) — it NEVER guesses a wrong/fabricated
/// key. Modifiers (`cmd`/`command`/`ctrl`/`control`/`opt`/`option`/`alt`/`shift`)
/// in any order are folded into the flag mask; the LAST non-modifier token is the
/// base key. Case-insensitive.
#[cfg(target_os = "macos")]
#[allow(dead_code)] // device-gated: reached only from post_key (never run in a test)
fn map_combo(combo: &str) -> Option<(u16, u64)> {
    let mut flags: u64 = 0;
    let mut base: Option<u16> = None;
    for raw in combo.split('+') {
        let tok = raw.trim().to_ascii_lowercase();
        if tok.is_empty() {
            // A stray "+" with nothing around it is malformed — refuse honestly.
            return None;
        }
        match tok.as_str() {
            "cmd" | "command" | "super" | "win" | "meta" => {
                flags |= K_CG_EVENT_FLAG_MASK_COMMAND;
            }
            "ctrl" | "control" => flags |= K_CG_EVENT_FLAG_MASK_CONTROL,
            "opt" | "option" | "alt" => flags |= K_CG_EVENT_FLAG_MASK_ALTERNATE,
            "shift" => flags |= K_CG_EVENT_FLAG_MASK_SHIFT,
            other => {
                // The base key. Only one base key is allowed; a second non-modifier
                // token is a malformed combo (refuse honestly, do not guess).
                if base.is_some() {
                    return None;
                }
                base = Some(keycode_for(other)?);
            }
        }
    }
    base.map(|kc| (kc, flags))
}

/// ANSI virtual keycode for a single base-key token (lowercase). Returns `None` for
/// any token we cannot map to the CORRECT keycode — the caller then refuses honestly
/// rather than post a wrong key. Covers letters, digits, and the common named keys;
/// an unrecognized token is an HONEST miss, never a fabricated keycode.
#[cfg(target_os = "macos")]
#[allow(dead_code)] // device-gated: reached only from map_combo (never run in a test)
fn keycode_for(token: &str) -> Option<u16> {
    // Letters (kVK_ANSI_A …). Layout-position codes, not ASCII.
    let letter = |c: char| -> Option<u16> {
        Some(match c {
            'a' => 0x00, 's' => 0x01, 'd' => 0x02, 'f' => 0x03, 'h' => 0x04,
            'g' => 0x05, 'z' => 0x06, 'x' => 0x07, 'c' => 0x08, 'v' => 0x09,
            'b' => 0x0B, 'q' => 0x0C, 'w' => 0x0D, 'e' => 0x0E, 'r' => 0x0F,
            'y' => 0x10, 't' => 0x11, 'o' => 0x1F, 'u' => 0x20, 'i' => 0x22,
            'p' => 0x23, 'l' => 0x25, 'j' => 0x26, 'k' => 0x28, 'n' => 0x2D,
            'm' => 0x2E,
            _ => return None,
        })
    };
    if token.chars().count() == 1 {
        let c = token.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return letter(c);
        }
        // Digit row (kVK_ANSI_0 … 9).
        return Some(match c {
            '1' => 0x12, '2' => 0x13, '3' => 0x14, '4' => 0x15, '5' => 0x17,
            '6' => 0x16, '7' => 0x1A, '8' => 0x1C, '9' => 0x19, '0' => 0x1D,
            '-' => 0x1B, '=' => 0x18, '[' => 0x21, ']' => 0x1E, '\\' => 0x2A,
            ';' => 0x29, '\'' => 0x27, ',' => 0x2B, '.' => 0x2F, '/' => 0x2C,
            '`' => 0x32, ' ' => 0x31,
            _ => return None,
        });
    }
    // Named keys (kVK_*). Only well-known, correctly-mapped names; anything else is
    // an HONEST miss (None) — never guess.
    Some(match token {
        "return" | "enter" => 0x24,
        "tab" => 0x30,
        "space" | "spacebar" => 0x31,
        "delete" | "backspace" => 0x33,
        "escape" | "esc" => 0x35,
        "forwarddelete" | "fwddelete" => 0x75,
        "home" => 0x73,
        "end" => 0x77,
        "pageup" => 0x74,
        "pagedown" => 0x79,
        "left" | "leftarrow" => 0x7B,
        "right" | "rightarrow" => 0x7C,
        "down" | "downarrow" => 0x7D,
        "up" | "uparrow" => 0x7E,
        "f1" => 0x7A, "f2" => 0x78, "f3" => 0x63, "f4" => 0x76,
        "f5" => 0x60, "f6" => 0x61, "f7" => 0x62, "f8" => 0x64,
        "f9" => 0x65, "f10" => 0x6D, "f11" => 0x67, "f12" => 0x6F,
        _ => return None,
    })
}

/// DEVICE-GATED (macOS): post ONE key combo (key-down + key-up CGEvent with the
/// parsed modifier flags). Maps the combo to a `(keycode, flags)` pair via
/// [`map_combo`]; if it CANNOT be mapped to a correct key, posts NOTHING and returns
/// an HONEST [`ActuateError::PostFailed`] — never a wrong/fabricated key. Posts the
/// down then the up (ONE combo press), releasing both. NEVER run in a test.
#[cfg(target_os = "macos")]
#[allow(dead_code)]
fn post_key(combo: &str) -> Result<(), ActuateError> {
    // HONESTY OVER COMPLETENESS: an unmappable combo is refused honestly, not guessed.
    let (keycode, flags) = map_combo(combo).ok_or_else(|| {
        ActuateError::PostFailed(format!(
            "could not map the key combo \"{combo}\" to a known keycode (refusing rather than \
             pressing a wrong key)"
        ))
    })?;
    let cg = CoreGraphics::resolve()
        .ok_or_else(|| ActuateError::PostFailed("CoreGraphics symbols unavailable".into()))?;
    // FFI CONTRACT: create_keyboard_event(NULL source, keycode, keyDown) with the
    // verified signature; set_flags applies the modifier mask to the event before
    // posting. Each event is owned by us and CFRelease'd exactly once. Down then up =
    // ONE combo press; no loop, no batch.
    let down = (cg.create_keyboard_event)(std::ptr::null_mut(), keycode, true);
    if down.is_null() {
        return Err(ActuateError::PostFailed("could not create key-down event".into()));
    }
    (cg.set_flags)(down, flags);
    (cg.post)(K_CG_HID_EVENT_TAP, down);
    (cg.cf_release)(down);

    let up = (cg.create_keyboard_event)(std::ptr::null_mut(), keycode, false);
    if up.is_null() {
        return Err(ActuateError::PostFailed("could not create key-up event".into()));
    }
    (cg.set_flags)(up, flags);
    (cg.post)(K_CG_HID_EVENT_TAP, up);
    (cg.cf_release)(up);
    Ok(())
}

// ---------------------------------------------------------------------------
// NON-macOS HONEST STUBS — there is no CGEvent surface off macOS, so the seam can
// never post a real event. It refuses HONESTLY (BackendUnavailable), never claiming
// an actuation that cannot happen. (do_actuate also guards on
// accessibility_permission_granted(), which is already `false` off macOS, so these
// are belt-and-suspenders honesty for the post seam itself.)
// ---------------------------------------------------------------------------

/// NON-macOS: no CGEvent backend exists — refuse honestly.
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
fn post_click(_x: i32, _y: i32) -> Result<(), ActuateError> {
    Err(ActuateError::BackendUnavailable)
}

/// NON-macOS: no CGEvent backend exists — refuse honestly.
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
fn post_type(_text: &str) -> Result<(), ActuateError> {
    Err(ActuateError::BackendUnavailable)
}

/// NON-macOS: no CGEvent backend exists — refuse honestly.
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
fn post_key(_combo: &str) -> Result<(), ActuateError> {
    Err(ActuateError::BackendUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    // =====================================================================
    // (2) CONFIG GATE — ui_automation_permitted: enabled-flag semantics
    // (ships ON; this pins the explicit-disable path)
    // =====================================================================

    #[test]
    fn ui_automation_permitted_requires_the_master_switch() {
        assert!(!ui_automation_permitted(false), "disabled => not permitted");
        assert!(
            ui_automation_permitted(true),
            "on => permitted (still gated above by confirm/master/voice-id, and below by TCC)"
        );
    }

    // =====================================================================
    // (1) PLANNER — single action, bounds-validated, refuses degenerate, can't batch
    // =====================================================================

    fn bounds() -> ScreenBounds {
        ScreenBounds { width: 1920, height: 1080 }
    }

    #[test]
    fn plans_a_single_click_in_bounds() {
        let req = ActuationRequest {
            action: Action::Click { x: 960, y: 540 },
            target_desc: "the Send button".into(),
        };
        let plan = plan_actuation(&req, bounds()).expect("a valid in-bounds click plans");
        assert_eq!(*plan.action(), Action::Click { x: 960, y: 540 }, "the ONE planned action");
        assert_eq!(plan.target_desc(), "the Send button");
        assert!(plan.preview().contains("click at (960, 540)"), "faithful preview: {}", plan.preview());
        assert!(plan.preview().contains("Send button"));
    }

    #[test]
    fn plans_a_single_type() {
        let req = ActuationRequest {
            action: Action::Type { text: "hello world".into() },
            target_desc: "the search field".into(),
        };
        let plan = plan_actuation(&req, bounds()).expect("a non-empty type plans");
        assert_eq!(*plan.action(), Action::Type { text: "hello world".into() });
        // The whole string is ONE type op — the preview reports the character
        // count, NOT a per-key batch.
        assert!(plan.preview().contains("type 11 character(s)"), "preview: {}", plan.preview());
    }

    #[test]
    fn plans_a_single_key_combo() {
        let req = ActuationRequest {
            action: Action::Key { combo: "cmd+s".into() },
            target_desc: "the document window".into(),
        };
        let plan = plan_actuation(&req, bounds()).expect("a non-empty key combo plans");
        assert_eq!(*plan.action(), Action::Key { combo: "cmd+s".into() });
        assert!(plan.preview().contains("cmd+s"), "preview: {}", plan.preview());
    }

    #[test]
    fn refuses_a_degenerate_empty_instruction() {
        // Empty / whitespace-only target description: nothing to act on.
        for target in ["", "   ", "\t\n"] {
            let req = ActuationRequest {
                action: Action::Click { x: 10, y: 10 },
                target_desc: target.into(),
            };
            assert_eq!(
                plan_actuation(&req, bounds()),
                Err(PlanError::Empty),
                "an empty target {target:?} must be refused"
            );
        }
        // Empty type text / empty key combo: a degenerate no-op actuation.
        let empty_type = ActuationRequest {
            action: Action::Type { text: String::new() },
            target_desc: "a field".into(),
        };
        assert_eq!(plan_actuation(&empty_type, bounds()), Err(PlanError::DegenerateAction));
        let empty_key = ActuationRequest {
            action: Action::Key { combo: "   ".into() },
            target_desc: "a window".into(),
        };
        assert_eq!(plan_actuation(&empty_key, bounds()), Err(PlanError::DegenerateAction));
    }

    #[test]
    fn refuses_an_off_screen_click() {
        // Beyond the right/bottom edge, negative, and exactly-at-the-edge (strict
        // upper bound) all fall outside the real display and are refused — an
        // actuation must land on a real pixel.
        for (x, y) in [(1920, 540), (960, 1080), (-1, 10), (10, -5), (5000, 5000)] {
            let req = ActuationRequest {
                action: Action::Click { x, y },
                target_desc: "somewhere off-screen".into(),
            };
            let verdict = plan_actuation(&req, bounds());
            assert!(
                matches!(verdict, Err(PlanError::OffScreen { .. })),
                "({x}, {y}) is off-screen and must be refused, got {verdict:?}"
            );
        }
        // The last valid pixel (width-1, height-1) IS in bounds.
        let edge = ActuationRequest {
            action: Action::Click { x: 1919, y: 1079 },
            target_desc: "the corner".into(),
        };
        assert!(plan_actuation(&edge, bounds()).is_ok(), "the last real pixel is in-bounds");
    }

    #[test]
    fn a_zero_sized_display_accepts_no_click() {
        // No display (0x0): every coordinate is off-screen — nothing can be planned.
        let none = ScreenBounds { width: 0, height: 0 };
        let req = ActuationRequest {
            action: Action::Click { x: 0, y: 0 },
            target_desc: "anything".into(),
        };
        assert!(matches!(plan_actuation(&req, none), Err(PlanError::OffScreen { .. })));
    }

    /// STRUCTURAL: a plan can NEVER carry more than one actuation. This is enforced
    /// by the type itself — `Action` has no sequence/batch variant and
    /// `ActuationPlan`/`ActuationRequest` hold a SINGLE `Action`, not a `Vec`. We
    /// assert the contract that EACH plan_actuation call yields exactly one action,
    /// so two actuations require two SEPARATE plans (and therefore two SEPARATE
    /// confirms in the gate). There is no API surface that batches.
    #[test]
    fn one_plan_is_exactly_one_action_never_a_batch() {
        let first = plan_actuation(
            &ActuationRequest {
                action: Action::Click { x: 100, y: 100 },
                target_desc: "button A".into(),
            },
            bounds(),
        )
        .expect("plans");
        let second = plan_actuation(
            &ActuationRequest {
                action: Action::Click { x: 200, y: 200 },
                target_desc: "button B".into(),
            },
            bounds(),
        )
        .expect("plans");
        // Two DISTINCT single-action plans — not one plan holding two actions.
        assert_ne!(first.action(), second.action(), "each plan is its own single action");
        // The action surface is a single Action (compile-time guarantee). Confirm
        // the planned action matches exactly one variant and nothing more.
        assert!(matches!(first.action(), Action::Click { .. }));
        assert!(matches!(second.action(), Action::Click { .. }));
    }

    // =====================================================================
    // (4) ACTUATION SEAM — built, NEVER invoked here. We assert ONLY the pure,
    // device-free pieces: the honest error reasons + the result shape. We do NOT
    // call do_actuate / accessibility_permission_granted / post_* (device-gated).
    // =====================================================================

    #[test]
    fn actuate_error_reasons_are_honest() {
        assert!(
            ActuateError::AccessibilityNotGranted
                .reason()
                .to_lowercase()
                .contains("accessibility permission not granted"),
            "the absent-consent reason must be the honest TCC message"
        );
        assert!(ActuateError::NoDisplay.reason().contains("no usable display"));
        assert!(ActuateError::PostFailed("x".into()).reason().contains("x"));
        // The unavailable-backend variant must HONESTLY say nothing was performed —
        // this is the variant the NON-macOS post_click/post_type/post_key return (on
        // macOS the post is real and any failure is PostFailed), so do_actuate can
        // never report a fabricated success for an action it did not post.
        assert!(
            ActuateError::BackendUnavailable.reason().contains("not implemented")
                && ActuateError::BackendUnavailable.reason().contains("no action was performed"),
            "the unavailable-backend reason must honestly state nothing was actuated"
        );
    }

    #[test]
    fn plan_error_reasons_are_honest() {
        assert!(PlanError::Empty.reason().contains("no target"));
        assert!(
            PlanError::OffScreen { x: 5000, y: 5000, bounds: bounds() }
                .reason()
                .contains("off-screen")
        );
        assert!(PlanError::DegenerateAction.reason().contains("empty"));
    }

    // NOTE: there is intentionally NO test that calls do_actuate / post_click /
    // post_type / post_key / accessibility_permission_granted. The actuation is
    // DEVICE-gated (the vision-capture / apply-heal / shell-exec precedent): the
    // planner and the gate routing are proven hermetically; the actuation only
    // ever happens on-device behind the full gate + the Accessibility TCC consent.
    // Posting a real CGEvent / AX action in a test is the one hard prohibition for
    // this feature.
}
