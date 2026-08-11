#!/bin/bash
# uninstall.sh — completely remove D.A.R.W.I.N. from this machine.
#
# TWO-STEP TYPED CONFIRMATION (deliberate, for a destructive action):
#   1. "Delete DARWIN completely? (yes/no)"            -> no cancels.
#   2. "Are you ABSOLUTELY sure? (yes/no)"             -> no cancels; only yes deletes.
# Any unrecognized / empty / EOF input is treated as NO — it never deletes on doubt.
#
# It removes ONLY DARWIN's own footprint. Every target is a SPECIFIC, guarded path
# (never a broad or globbed rm):
#   - the install home  ~/Library/Application Support/DARWIN  (code, .venv, models, all state)
#   - the 3 LaunchAgents (com.darwin.hud / com.darwin.daemon / com.darwin.inference) — unloaded + removed
#   - the installed HUD app  /Applications/DARWIN.app  and  ~/Applications/DARWIN.app —
#     each removed ONLY after its Info.plist verifies bundle id com.darwin.hud
#   - the DARWIN Keychain items (ONLY the service "com.darwin.daemon") — your stored keys/tokens
#   - the HUD's per-bundle WebKit state, which macOS keeps OUTSIDE both the app bundle
#     and the install home — each is exactly the com.darwin.hud directory under
#     ~/Library/WebKit, ~/Library/Caches, ~/Library/HTTPStorages and
#     ~/Library/Saved Application State (plus com.darwin.hud.savedState there)
# DARWIN's logs are NOT a separate footprint item: they live in the install home at
# state/logs/ (see boot/*.plist StandardOutPath), so removing the home removes them.
# NOT removed, and worth knowing before you answer yes: (1) macOS TCC grants (Microphone /
#   Screen Recording / Accessibility) — nothing in this product calls tccutil; (2) model
#   weights that live OUTSIDE the install home. install.sh only DEFAULTS HF_HOME to
#   $DARWIN_HOME/models: if state/env.sh already carries an `export HF_HOME=` install.sh
#   honors it (explicitly, for "the user pointing at a bigger disk"), and scripts/doctor.sh
#   treats that split as a supported state — so those gigabytes are not under the install
#   home and this script leaves them, deliberately, because such a cache may be shared with
#   other tools. Check yours with:
#     grep HF_HOME "$HOME/Library/Application Support/DARWIN/state/env.sh"
# It removes the INSTALLED OS. A source clone you may have elsewhere is left untouched.
#
# Usage:
#   ~/Library/Application\ Support/DARWIN/uninstall.sh   # interactive, two-step confirm
#   ./uninstall.sh --dry-run                             # show what WOULD be removed; delete nothing
#   ./uninstall.sh --selftest                            # hermetic teardown selftest; deletes nothing
#   ./uninstall.sh --help
#
# $HOME must be the invoking user's real home (the guard below verifies it against
# the directory service, not against $HOME itself). Set DARWIN_ALLOW_FOREIGN_HOME=1
# only if you deliberately installed under a different $HOME.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- UI: prefer a sibling scripts/ui.sh, else a bundled ui.sh; else a no-op shim
# so uninstall always works even from a bare copy. --------------------------------
for _ui in "$SCRIPT_DIR/scripts/ui.sh" "$SCRIPT_DIR/ui.sh"; do
    if [ -f "$_ui" ]; then
        # shellcheck disable=SC1090
        . "$_ui"
        break
    fi
done
if ! command -v ui_init >/dev/null 2>&1; then
    ui_init() { :; }
    darwin_banner() { printf '\n  D.A.R.W.I.N. — uninstall\n\n'; }
    ui_hr() { printf -- '  ------------------------------------------------------------\n'; }
    ui_ok()   { printf '  [ok]  %s\n' "$1"; }
    ui_warn() { printf '  [!]   %s\n' "$1"; }
    ui_err()  { printf '  [x]   %s\n' "$1" >&2; }
    ui_info() { printf '  -     %s\n' "$1"; }
    ui_note() { printf '        %s\n' "$1"; }
    ui_online() { :; }
fi
ui_init

# --- the DARWIN footprint (specific, hard-coded paths) ---------------------------
DARWIN_HOME="$HOME/Library/Application Support/DARWIN"
AGENT_DIR="$HOME/Library/LaunchAgents"
KEYCHAIN_SERVICE="com.darwin.daemon"
# Teardown order: unload the visible HUD first, then the daemon, then inference.
# (This list drives the dry-run preview + the fallback loop used only when
# scripts/install_boot.sh is absent; the normal path delegates to that script's
# --uninstall, whose own LABELS is the single source of truth.)
LABELS=("com.darwin.hud" "com.darwin.daemon" "com.darwin.inference")
GUI_DOMAIN="gui/$(id -u)"
# The HUD app install.sh places via place_hud_app (/Applications first, then
# ~/Applications). Each is a SPECIFIC path, and is removed ONLY if its bundle
# identifier verifies as the DARWIN HUD (hud/src-tauri/tauri.conf.json).
HUD_BUNDLE_ID="com.darwin.hud"
APP_PATHS=("/Applications/DARWIN.app" "$HOME/Applications/DARWIN.app")

DRY_RUN=0
case "${1:-}" in
    --dry-run|--check) DRY_RUN=1 ;;
    # This script's own hermetic teardown selftest, reachable FROM the script it
    # tests — the same shape as `scripts/apply_heal.sh --selftest` and
    # `scripts/apply_code_diff.sh --selftest`. A test that exists but is named by no
    # runnable command is the defect this repo has now hit four times (the 158
    # src-tauri tests, the three feature-gated suites, and these installer
    # harnesses). It runs BEFORE guard_home and deletes nothing outside its own
    # mktemp sandbox.
    # `exec bash <file>` rather than exec'ing the file: it runs the same harness
    # whether or not the copy in this tree carries its exec bit.
    --selftest) exec bash "$SCRIPT_DIR/scripts/test_uninstall_footprint.sh" ;;
    # --help IS the header block above. DERIVE its end (the first non-comment
    # line) instead of hard-coding a range: this was `sed -n '2,22p'`, so every
    # line the footprint list grows would silently fall off the only in-band
    # documentation of what this destructive script deletes.
    -h|--help) awk 'NR > 1 { if (substr($0, 1, 1) != "#") exit; print }' "${BASH_SOURCE[0]}"; exit 0 ;;
    "") : ;;
    *) printf 'uninstall.sh: unknown argument %q (use --dry-run, --selftest or --help)\n' "$1" >&2; exit 2 ;;
esac

# --- SAFETY GUARD: refuse to act unless $HOME really is this user's home. ---------
# WHAT WENT WRONG: this guard advertised that it "makes a broad/accidental delete
# impossible even if $HOME were malformed", and it could not fire at all. It built
# `base="$HOME/Library/Application Support/DARWIN"` and compared it to DARWIN_HOME,
# which line 50 builds from the SAME expression — so the `!=` branch was
# unreachable; and DARWIN_HOME always ends in "/DARWIN", so it could never equal
# any protected path in the case list either. Both exit paths were dead code. Under
# HOME='' , HOME=/, HOME=/tmp or HOME=/System the guard PASSED, and the run then
# deleted the one target that is not $HOME-derived (/Applications/DARWIN.app),
# left the real install home, the three LaunchAgents and the Keychain secrets in
# place, and still printed "D.A.R.W.I.N. has been completely removed from this
# machine" while the daemon, inference server and autostart agents kept running.
#
# Comparing $HOME-derived strings to each other can never detect a wrong $HOME. So
# resolve the real home INDEPENDENTLY of the environment and compare against that.
# The structural check on DARWIN_HOME is kept as an invariant for a future edit
# that stops deriving it from $HOME — it is not, and never was, the $HOME check.
real_home() {
    local u h=""
    u="$(id -un 2>/dev/null || true)"
    [ -n "$u" ] || return 1
    h="$(dscl . -read "/Users/$u" NFSHomeDirectory 2>/dev/null | sed -n 's/^NFSHomeDirectory: //p')"
    if [ -z "$h" ]; then
        h="$(eval echo "~$u" 2>/dev/null || true)"
        case "$h" in "~"*) h="" ;; esac   # unexpanded => no such user record
    fi
    [ -n "$h" ] || return 1
    printf '%s\n' "$h"
}

guard_home() {
    # 1. $HOME must be a usable ABSOLUTE path. Empty / "/" / relative are never
    #    legitimate and have no override: an empty $HOME also makes remove_home's
    #    `cd "$HOME"` a silent no-op (bash `cd ""` returns 0 without moving).
    case "$HOME" in
        "" | "/") ui_err "Refusing to run: \$HOME is empty or \"/\"." ; exit 1 ;;
        /*) : ;;
        *) ui_err "Refusing to run: \$HOME (\"$HOME\") is not an absolute path." ; exit 1 ;;
    esac
    # 2. THE ACTUAL GUARD: $HOME must be this user's real home, per the directory
    #    service — not per $HOME.
    local rh
    if rh="$(real_home)"; then
        if [ "${HOME%/}" != "${rh%/}" ]; then
            # KEEP THIS COMPARE EXACT. "This hatch cannot be tripped by accident"
            # is true only because it takes the literal string "1": a
            # DARWIN_ALLOW_FOREIGN_HOME that a wrapper, a CI runner or an operator
            # set to `true`, `yes`, `01` or anything else must NOT disarm the guard
            # standing in front of remove_home's rm -rf. Loosening this to != "0"
            # once passed every gate in the repo, so it is now probed on both
            # sides: scripts/test_doc_claims.sh drives guard_home with "1" (opens)
            # and with true / 01 / " 1" / xyz (all still refused).
            if [ "${DARWIN_ALLOW_FOREIGN_HOME:-0}" = "1" ]; then
                ui_warn "\$HOME ($HOME) is not $(id -un)'s home ($rh) — proceeding because DARWIN_ALLOW_FOREIGN_HOME=1."
                # STATE THE BREADTH OF THE HATCH, because it is not symmetric: only the
                # $HOME-DERIVED targets move with $HOME. Four do not, and they hit the
                # REAL login session no matter what $HOME says — so a hatch used with a
                # $HOME that is NOT where DARWIN is installed produces exactly the
                # half-uninstall-and-report-success this guard exists to prevent (see
                # WHAT WENT WRONG above), and one of the four is irreversible. Saying so
                # costs four lines; the user still has the two-step typed confirm after
                # this. NOTE: interpolate nothing here — scripts/test_doc_claims.sh
                # drives this function sliced out of the file, under `set -u`, with the
                # ui_* helpers stubbed and no globals defined.
                ui_note "Only \$HOME-derived targets follow \$HOME: the install home, ~/Library/LaunchAgents,"
                ui_note "~/Applications/DARWIN.app and the HUD's ~/Library WebKit/cache/saved-state dirs."
                ui_note "These do NOT, and hit your REAL login session regardless of \$HOME: launchctl"
                ui_note "bootout of the three agents, pkill of a running darwind / server.py / HUD, the"
                ui_note "com.darwin.daemon Keychain deletion (IRREVERSIBLE), and /Applications/DARWIN.app."
            else
                ui_err "Refusing to run: \$HOME (\"$HOME\") is not $(id -un)'s home directory (\"$rh\")."
                ui_note "Most of what this script removes is derived from \$HOME, but four things are"
                ui_note "not — launchctl bootout, the pkill of a running darwind/server.py/HUD, the"
                ui_note "com.darwin.daemon Keychain deletion and /Applications/DARWIN.app — so running"
                ui_note "like this would half-uninstall and report success."
                ui_note "Re-run without overriding \$HOME (e.g. not under sudo/su with always_set_home),"
                ui_note "or set DARWIN_ALLOW_FOREIGN_HOME=1 if you really installed under this \$HOME."
                exit 1
            fi
        fi
    else
        ui_warn "Could not resolve $(id -un 2>/dev/null || echo 'this user')'s home from the directory service; trusting \$HOME."
    fi
    # 3. Structural invariant on the install path itself.
    case "$DARWIN_HOME" in
        "" | "/" | "$HOME" | "$HOME/" | "$HOME/Library" | "$HOME/Library/" \
            | "$HOME/Library/Application Support" | "$HOME/Library/Application Support/")
            ui_err "Refusing to run: the install path resolves to a protected directory."
            exit 1 ;;
    esac
    if [ "$DARWIN_HOME" != "$HOME/Library/Application Support/DARWIN" ]; then
        ui_err "Refusing to run: install path is not the expected ~/Library/Application Support/DARWIN."
        exit 1
    fi
}
guard_home

# --- read yes/no, FAIL-SAFE to NO ------------------------------------------------
# 0 = an explicit yes; 1 = no / empty / EOF / repeated garbage. Empty (just Enter)
# is NO. Re-prompts up to 3 times on unrecognized input, then defaults to NO.
ask_yes_no() {
    local prompt="$1" ans="" tries=0
    while [ "$tries" -lt 3 ]; do
        printf '%s%s%s ' "${UI_BOLD:-}${UI_YELLOW:-}" "$prompt" "${UI_RESET:-}"
        if ! IFS= read -r ans; then
            printf '\n'
            return 1   # EOF -> NO
        fi
        case "$(printf '%s' "$ans" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]')" in
            yes|y)  return 0 ;;
            no|n|"") return 1 ;;
            *) ui_warn "Please type 'yes' or 'no'."; tries=$((tries + 1)) ;;
        esac
    done
    return 1   # too many invalid answers -> NO (fail-safe)
}

# --- the "confirmation window": exactly what will be removed ----------------------
present_targets() {
    ui_hr
    ui_warn "This will COMPLETELY and PERMANENTLY remove D.A.R.W.I.N. from this Mac."
    ui_hr
    ui_info "The following will be deleted:"
    if [ -d "$DARWIN_HOME" ]; then
        local size; size="$(du -sh "$DARWIN_HOME" 2>/dev/null | cut -f1 || true)"
        ui_note "$DARWIN_HOME"
        ui_note "    └ the OS, code, .venv, all state, and the models in its own models/ cache${size:+  (~$size)}"
    else
        ui_note "$DARWIN_HOME  (not installed — nothing to remove there)"
    fi
    ui_note "LaunchAgents: ${LABELS[*]}  (autostart unloaded + removed)"
    local app
    for app in "${APP_PATHS[@]}"; do
        [ -d "$app" ] && ui_note "HUD app: $app  (removed only if it verifies as bundle $HUD_BUNDLE_ID)"
    done
    ui_note "Keychain items under service \"$KEYCHAIN_SERVICE\"  (your stored API keys / tokens)"
    ui_note "HUD support data: ~/Library/{WebKit,Caches,HTTPStorages,Saved Application State}/$HUD_BUNDLE_ID"
    ui_note "Logs live in the install home at state/logs/ and go with it (no separate log dir)."
    ui_hr
    ui_info "This removes the INSTALLED OS (~/Library/...). A source clone elsewhere is untouched."
    ui_hr
}

# --- destructive steps (each is a no-op that only PRINTS in --dry-run) ------------
stop_and_remove_agents() {
    if [ "$DRY_RUN" -eq 1 ]; then
        ui_note "[dry run] would unload + remove LaunchAgents: ${LABELS[*]}"
        return 0
    fi
    # Prefer the project's own boot uninstaller (single source of truth) if present.
    local boot="$SCRIPT_DIR/scripts/install_boot.sh"
    if [ -x "$boot" ]; then
        "$boot" --uninstall || ui_warn "boot uninstaller reported an issue (continuing)."
    else
        for label in "${LABELS[@]}"; do
            launchctl bootout "$GUI_DOMAIN/$label" 2>/dev/null || true
            rm -f "$AGENT_DIR/$label.plist"
        done
    fi
    # Best-effort: reap any still-running processes (never fatal).
    #
    # THE HUD NEEDS ITS OWN REAP HERE. The daemon and the inference server were
    # reaped unconditionally; the HUD was not — its only `pkill` lived inside
    # remove_hud_app's loop body, which runs ONLY for an /Applications (or
    # ~/Applications) DARWIN.app that EXISTS and VERIFIES. But boot/run_hud.sh
    # prefers the bundle built UNDER THE INSTALL HOME
    # ($DARWIN_ROOT/hud/src-tauri/target/release/bundle/.../DARWIN.app), and
    # install.sh's place_hud_app is explicitly "never fatal" — it warns and leaves
    # INSTALLED_APP_PATH at the build path when neither Applications dir is
    # writable. In that (or any hand-moved) install, remove_hud_app finds nothing,
    # so nothing reaped the HUD, and remove_home then deleted the running HUD's own
    # executable and Resources out from under it — a live agent pointed at deleted
    # files, while the script printed "completely removed from this machine".
    # launchctl bootout cannot be relied on to have finished either: install_boot.sh
    # --uninstall deliberately does NOT wait_for_bootout (only --install polls), and
    # that unfinished-teardown race is the whole reason the two reaps below exist.
    # Scoped to the install home's own bundle path, exactly like the two above.
    pkill -f "DARWIN/daemon/target/release/darwind" 2>/dev/null || true
    pkill -f "DARWIN/inference/server.py" 2>/dev/null || true
    pkill -f "DARWIN/hud/src-tauri/target/release/bundle/" 2>/dev/null || true
    ui_ok "Autostart unloaded and LaunchAgents removed."
}

# Is $1 the DARWIN HUD app bundle? TRUE only when it is a directory whose
# Info.plist carries the DARWIN bundle identifier — we never rm -rf a path that
# does not verify, even at the expected location (e.g. some unrelated folder a
# user parked there under the name DARWIN.app).
is_darwin_hud_app() {
    local app="$1"
    [ -d "$app" ] && [ -f "$app/Contents/Info.plist" ] || return 1
    local bid=""
    bid="$(defaults read "$app/Contents/Info" CFBundleIdentifier 2>/dev/null || true)"
    [ "$bid" = "$HUD_BUNDLE_ID" ]
}

remove_hud_app() {
    local app found=0
    for app in "${APP_PATHS[@]}"; do
        [ -e "$app" ] || continue
        found=1
        if ! is_darwin_hud_app "$app"; then
            ui_warn "left in place: $app (did not verify as the DARWIN HUD bundle $HUD_BUNDLE_ID — refusing to delete an unverified path)"
            continue
        fi
        if [ "$DRY_RUN" -eq 1 ]; then
            ui_note "[dry run] would: rm -rf \"$app\"  (verified bundle $HUD_BUNDLE_ID)"
            continue
        fi
        # Best-effort: quit a running HUD first (never fatal) — scoped to an
        # executable INSIDE a DARWIN.app bundle, like the daemon reaps above.
        pkill -f "/DARWIN.app/Contents/MacOS/" 2>/dev/null || true
        rm -rf "$app"
        ui_ok "Removed the HUD app: $app"
    done
    if [ "$found" -eq 0 ]; then
        if [ "$DRY_RUN" -eq 1 ]; then
            ui_note "[dry run] no DARWIN.app present in /Applications or ~/Applications"
        else
            ui_info "No installed DARWIN.app was present."
        fi
    fi
}

remove_home() {
    if [ "$DRY_RUN" -eq 1 ]; then
        ui_note "[dry run] would: rm -rf \"$DARWIN_HOME\""
        return 0
    fi
    guard_home   # re-assert the guard immediately before removing the install home
    cd "$HOME"   # never rm -rf the directory we are standing in ($HOME is
                 # guaranteed absolute and non-empty by guard_home; bash `cd ""`
                 # would silently return 0 without moving)
    if [ -d "$DARWIN_HOME" ]; then
        rm -rf "$DARWIN_HOME"
        ui_ok "Removed the install home."
    else
        ui_info "Install home was not present."
    fi
}

remove_keychain_items() {
    if [ "$DRY_RUN" -eq 1 ]; then
        ui_note "[dry run] would delete every Keychain item under service \"$KEYCHAIN_SERVICE\""
        return 0
    fi
    # Delete one matching generic-password per call; loop until none remain. Scoped
    # STRICTLY to the DARWIN service, so no other Keychain item is ever touched.
    local removed=0
    while security delete-generic-password -s "$KEYCHAIN_SERVICE" >/dev/null 2>&1; do
        removed=$((removed + 1))
        [ "$removed" -ge 64 ] && break   # safety stop; DARWIN never stores this many
    done
    if [ "$removed" -gt 0 ]; then
        ui_ok "Removed $removed DARWIN Keychain item(s) (service $KEYCHAIN_SERVICE)."
    else
        ui_info "No DARWIN Keychain items were present."
    fi
}

# The HUD is a WebKit app, so macOS keeps per-bundle state OUTSIDE the app bundle and
# outside the install home. Neither was removed, so a script that announced "D.A.R.W.I.N.
# has been completely removed from this machine" left the HUD's browsing state, local
# storage and caches behind — and a later reinstall silently inherited them.
#
# Every path is derived from HUD_BUNDLE_ID and each is checked to be exactly the
# bundle-named directory under a known system location, so this can never widen into
# ~/Library/Caches itself.
remove_hud_support_dirs() {
    local base d
    for base in "$HOME/Library/WebKit" "$HOME/Library/Caches" \
                "$HOME/Library/Saved Application State" "$HOME/Library/HTTPStorages"; do
        d="$base/$HUD_BUNDLE_ID"
        [ -d "$d" ] || continue
        case "$d" in
            "$HOME/Library/"*"/$HUD_BUNDLE_ID") ;;   # shape guard
            *) ui_note "refusing to remove an unexpected path: $d"; continue ;;
        esac
        if [ "$DRY_RUN" -eq 1 ]; then
            ui_note "[dry run] would: rm -rf \"$d\""
        else
            rm -rf "$d"
            ui_ok "Removed HUD support data ($d)."
        fi
    done
    # Saved Application State uses a .savedState suffix. It gets the SAME shape
    # guard as the loop above — the comment on this function claims "each is
    # checked to be exactly the bundle-named directory", and this path used to
    # skip the check, so the claim did not hold for one of the five targets.
    d="$HOME/Library/Saved Application State/$HUD_BUNDLE_ID.savedState"
    case "$d" in
        "$HOME/Library/"*"/$HUD_BUNDLE_ID.savedState") ;;
        *) ui_note "refusing to remove an unexpected path: $d"; d="" ;;
    esac
    if [ -n "$d" ] && [ -d "$d" ]; then
        if [ "$DRY_RUN" -eq 1 ]; then
            ui_note "[dry run] would: rm -rf \"$d\""
        else
            rm -rf "$d"
            ui_ok "Removed HUD saved state."
        fi
    fi
}

# There is deliberately NO remove_logs(). It targeted ~/Library/Logs/DARWIN, a
# directory NOTHING in this product ever creates: a repo-wide grep for
# "Library/Logs" found only uninstall.sh itself, and no source constructs the path
# piecewise either. DARWIN's real logs go to $DARWIN_HOME/state/logs/ —
# boot/com.darwin.daemon.plist's StandardOutPath is
# __DARWIN_ROOT__/state/logs/launchd-daemon.log and boot/run_daemon.sh rotates
# state/logs/*.log (same for inference and the HUD) — which is inside the install
# home remove_home already deletes. The removal was a guaranteed no-op, while the
# two-step confirmation window and --help told the user their DARWIN logs live at a
# path that does not exist and never names where they actually are.

# --- main ------------------------------------------------------------------------
clear 2>/dev/null || true
darwin_banner
present_targets

if [ "$DRY_RUN" -eq 1 ]; then
    ui_warn "DRY RUN — nothing will be deleted no matter what you answer."
fi

# STEP 1
if ! ask_yes_no "Delete DARWIN completely? (yes/no)"; then
    ui_ok "Cancelled. Nothing was deleted."
    exit 0
fi

# STEP 2
if ! ask_yes_no "This is PERMANENT and cannot be undone. Are you ABSOLUTELY sure? (yes/no)"; then
    ui_ok "Cancelled. Nothing was deleted."
    exit 0
fi

# Both confirmations were an explicit yes — proceed.
ui_hr
ui_info "Removing D.A.R.W.I.N. ..."
stop_and_remove_agents
remove_hud_app
remove_home
remove_keychain_items
remove_hud_support_dirs
ui_hr
if [ "$DRY_RUN" -eq 1 ]; then
    ui_ok "Dry run complete — the above is exactly what a real run would remove. Nothing was deleted."
else
    ui_ok "D.A.R.W.I.N. has been completely removed from this machine."
    ui_note "Thank you."
fi
exit 0
