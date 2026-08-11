#!/usr/bin/env bash
# Hermetic selftest for uninstall.sh — WHAT IS LEFT BEHIND after it runs.
#
# WHY IT EXISTS. uninstall.sh is 393 lines of `rm -rf`, `launchctl bootout`,
# `pkill` and `security delete-generic-password`, and it had no test of its own.
# scripts/test_doc_claims.sh drives its `guard_home` (empty / "/" / foreign /
# relative $HOME) and pins its --help footprint text; nothing exercised the
# TEARDOWN ITSELF. The defect that matters most in an uninstaller is a PARTIAL
# one: a still-running agent pointed at files that no longer exist, under a final
# line that says "completely removed from this machine".
#
# THE ONE HARD PROHIBITION: this selftest never runs the real `launchctl`,
# `security`, `pkill` or `defaults`, never touches ~/Library/Application
# Support/DARWIN, ~/Library/LaunchAgents, /Applications, the real Keychain or the
# real launchd domain, never starts or stops a daemon, and never opens a socket.
#
# HOW THAT IS PROVED, not asserted:
#   1. STATIC: uninstall.sh must invoke launchctl / security / pkill / defaults /
#      rm as BARE WORDS. An absolute path (/bin/rm, /usr/bin/security, …) cannot
#      be shadowed by a shell function, so finding one is FATAL here — the test
#      refuses to run rather than run unconfined.
#   2. DYNAMIC: the harness defines each of those five as a bash FUNCTION, which
#      takes precedence over $PATH unconditionally (no PATH ordering to get
#      wrong), and re-checks `type -t` for all five INSIDE the very shell that
#      will run the sliced code, aborting with status 99 if any is not shadowed.
#   3. The `rm` shadow REFUSES any target outside the throwaway sandbox and
#      records an ESCAPE line, so a future hard-coded path fails this test
#      loudly instead of deleting something.
# Every path the teardown is pointed at ($HOME, DARWIN_HOME, AGENT_DIR,
# APP_PATHS) is inside one mktemp -d sandbox, and GUI_DOMAIN is a nonexistent
# uid domain, so even a hypothetical escape has nothing real to name.
#
# WHAT IT PINS (the concrete answers to "what is left behind?"):
#   A. all three agents are BOOTED OUT, not merely plist-deleted;
#   B. the rendered plists are removed;
#   C. when scripts/install_boot.sh is present the bootout is DELEGATED to it
#      (its LABELS stays the single source of truth) — and the process reaps
#      still happen on that path too;
#   D. the daemon, the inference server AND the HUD are all reaped, before the
#      install home is deleted;
#   E. the install home goes whole (state/, the SQLite DBs, config) and a
#      sibling directory beside it does not;
#   F. remove_home RE-ASSERTS guard_home immediately before the rm -rf;
#   G. Keychain deletion is scoped to service com.darwin.daemon and loops until
#      the service is empty;
#   H. a DECOY DARWIN.app (right path, wrong bundle id) is LEFT IN PLACE, and a
#      verified one IS removed — both sides of that boundary;
#   I. the HUD's out-of-home WebKit/cache/HTTPStorages/saved-state dirs go, and
#      a sibling bundle's directory beside them does not;
#   J. teardown ORDER: stop_and_remove_agents runs before remove_home;
#   K. every reap pattern is still a substring of what its boot wrapper actually
#      execs — the reaps and boot/run_*.sh do not compose for free;
#   L. the three LABELS lists (installer, uninstaller fallback, this harness) are
#      one SET, so neither the fallback teardown nor section 4 can drift;
#   M. the sliced bodies reach for no command this harness does not shadow —
#      they are EXECUTED, and killall/sudo are not confinable by $HOME.
#
#   Run: bash scripts/test_uninstall_footprint.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNINSTALL="$ROOT/uninstall.sh"
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/darwin-uninstall-selftest.XXXXXX")"
trap 'command rm -rf "$SANDBOX"' EXIT INT TERM

fails=0
ok()   { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; fails=$((fails + 1)); }
die()  { printf '  FATAL %s\n' "$1" >&2; exit 1; }

[ -f "$UNINSTALL" ] || die "no uninstall.sh at $UNINSTALL"

# --- 1. STATIC CONFINEMENT PROOF -------------------------------------------------
# A shell function shadows a BARE command word. It does NOT shadow an absolute
# path. So before anything runs, prove uninstall.sh contains no absolute form of
# any command this harness intends to shadow.
ABS_HITS="$(grep -nE '(^|[^[:alnum:]_/])/(usr/)?(s?bin)/(launchctl|security|pkill|defaults|rm)([^[:alnum:]_]|$)' \
    "$UNINSTALL" || true)"
if [ -n "$ABS_HITS" ]; then
    die "uninstall.sh invokes a dangerous command by ABSOLUTE PATH, which a shell-function shadow cannot intercept — refusing to run unconfined:
$ABS_HITS"
fi
ok "confinement (static): uninstall.sh calls launchctl/security/pkill/defaults/rm as bare words only"

# --- 2. slice the teardown functions out of the real script ----------------------
# Read them from uninstall.sh rather than restating them: a copy would keep
# passing after the real script changed, which is the whole point of the file.
slice() {   # $1 = function name -> its body, `name() {` .. the first column-0 `}`
    awk -v F="$1" 'index($0, F"() {") == 1 { c = 1 } c { print } c && /^\}$/ { exit }' "$UNINSTALL"
}
FNS=(stop_and_remove_agents is_darwin_hud_app remove_hud_app remove_home
     remove_keychain_items remove_hud_support_dirs)
: > "$SANDBOX/slices.sh"
for fn in "${FNS[@]}"; do
    body="$(slice "$fn")"
    case "$body" in
        "$fn"'() {'*'}') printf '%s\n' "$body" >> "$SANDBOX/slices.sh" ;;
        *) die "could not slice $fn() out of uninstall.sh — every assertion below it would be VACUOUS" ;;
    esac
    # A slice that bound to a one-line stub would pass the shape check above and
    # then prove nothing about a real body.
    [ "$(printf '%s\n' "$body" | wc -l | tr -d ' ')" -ge 5 ] \
        || die "the $fn() slice is only $(printf '%s\n' "$body" | wc -l | tr -d ' ') lines — too short to be its real body"
done
ok "sliced all ${#FNS[@]} teardown functions out of the real uninstall.sh"

# The slices EXECUTE. Only launchctl / pkill / security / defaults / rm are shadowed,
# and the static check above only proves those five are not called by ABSOLUTE PATH —
# it says nothing about a SIXTH command. A future edit that put `killall`, `sudo`,
# `osascript`, `tccutil`, `ditto`, `chflags`, `diskutil`, `srm`, `rmdir` or `unlink`
# into one of these six functions would run it FOR REAL from inside a file whose whole
# claim is that it runs none of that: `killall` and `sudo` are process- and
# machine-global, so pointing $HOME at a sandbox would not contain them.
# A deny-list, not an allow-list: the sliced bodies are ordinary shell, not a parseable
# command list. Scoped to the six functions this file actually executes (the rest of
# uninstall.sh is not policed) and to non-comment lines, so prose cannot trip it.
# MEASURED clean today: the only external commands in the six bodies are launchctl,
# pkill, security, defaults and rm — all five shadowed.
UNSHADOWED="$(sed -e '/^[[:space:]]*#/d' "$SANDBOX/slices.sh" \
    | grep -nE '(^|[^[:alnum:]_./-])(killall|sudo|osascript|tccutil|shutdown|reboot|diskutil|chflags|srm|ditto|rmdir|unlink)([[:space:]]|$)' || true)"
FIND_DELETE="$(sed -e '/^[[:space:]]*#/d' "$SANDBOX/slices.sh" \
    | grep -nE 'find .*(-delete|-exec[[:space:]]+rm)' || true)"
if [ -n "$UNSHADOWED$FIND_DELETE" ]; then
    die "a sliced teardown function calls a command this harness does NOT shadow, so it would run FOR REAL (and killall/sudo are not confinable by \$HOME). Shadow it in the harness FIRST, then add it:
$UNSHADOWED$FIND_DELETE"
fi
ok "confinement (static): the sliced bodies reach for no process- or disk-global command outside the five shadows"


# --- 3. the harness the slices run inside ----------------------------------------
cat > "$SANDBOX/harness.sh" <<'HARNESS'
set -uo pipefail
LOG="$SANDBOX/calls.log"
: > "$LOG"

# --- recorders. Functions, not PATH stubs: a function wins over $PATH always. ----
launchctl() { printf 'launchctl %s\n' "$*" >> "$LOG"; return 0; }
pkill()     { printf 'pkill %s\n' "$*" >> "$LOG"; return 0; }
defaults()  {   # only form used: defaults read <app>/Contents/Info CFBundleIdentifier
    local p="${2:-}.plist"
    [ -f "$p" ] || return 1
    sed -n 's|.*<string>\(.*\)</string>.*|\1|p' "$p" | head -1
}
# `security delete-generic-password -s <svc>` succeeds KC_ITEMS times, then fails
# (which is how the real one signals "no more items under this service").
KC_CALLS=0
security() {
    printf 'security %s\n' "$*" >> "$LOG"
    KC_CALLS=$((KC_CALLS + 1))
    [ "$KC_CALLS" -le "${KC_ITEMS:-0}" ]
}
# The tripwire: refuse, loudly, any target outside the sandbox.
rm() {
    local a
    for a in "$@"; do
        case "$a" in
            -*) continue ;;
            "$SANDBOX"/*) continue ;;
            *) printf 'ESCAPE %s\n' "$a" >> "$LOG"; return 1 ;;
        esac
    done
    printf 'rm %s\n' "$*" >> "$LOG"
    command rm "$@"
}
# remove_home re-asserts this immediately before the rm -rf. test_doc_claims.sh
# already DRIVES the real guard against malformed $HOMEs; here we pin only that
# the re-assertion is still on the path to the delete.
guard_home() { printf 'guard_home\n' >> "$LOG"; }

ui_ok() { :; }; ui_warn() { :; }; ui_info() { :; }; ui_note() { :; }; ui_err() { :; }
ui_hr() { :; }

# --- DYNAMIC CONFINEMENT PROOF, inside the shell that runs the code -------------
for c in launchctl pkill security defaults rm; do
    [ "$(type -t "$c")" = function ] \
        || { echo "CONFINEMENT FAILURE: $c is not shadowed" >&2; exit 99; }
done

# --- the teardown globals, all pointed inside the sandbox ------------------------
DRY_RUN=0
DARWIN_HOME="$SANDBOX/home/Library/Application Support/DARWIN"
AGENT_DIR="$SANDBOX/home/Library/LaunchAgents"
SCRIPT_DIR="$SANDBOX/home/Library/Application Support/DARWIN"
KEYCHAIN_SERVICE="com.darwin.daemon"
HUD_BUNDLE_ID="com.darwin.hud"
LABELS=("com.darwin.hud" "com.darwin.daemon" "com.darwin.inference")
GUI_DOMAIN="gui/4294967000"          # a uid domain that cannot exist
APP_PATHS=("$SANDBOX/home/Applications/DARWIN.app")

. "$SANDBOX/slices.sh"
"$1"
HARNESS

# Run one sliced function. $1 = function name; extra env comes from the caller.
drive() {
    local fn="$1"
    ( export SANDBOX; HOME="$SANDBOX/home" bash "$SANDBOX/harness.sh" "$fn" >/dev/null 2>&1 )
    local rc=$?
    [ "$rc" -ne 99 ] || die "the harness reported CONFINEMENT FAILURE (a command was not shadowed)"
    return "$rc"
}
logged() { grep -Fq "$1" "$SANDBOX/calls.log"; }

# --- 4. A/B/D: bootout + plist removal + all three process reaps (fallback path) --
command rm -rf "$SANDBOX/home"
mkdir -p "$SANDBOX/home/Library/LaunchAgents"
for l in com.darwin.hud com.darwin.daemon com.darwin.inference; do
    printf 'rendered\n' > "$SANDBOX/home/Library/LaunchAgents/$l.plist"
done
drive stop_and_remove_agents
miss=""
for l in com.darwin.hud com.darwin.daemon com.darwin.inference; do
    logged "launchctl bootout gui/4294967000/$l" || miss="$miss $l"
    [ ! -e "$SANDBOX/home/Library/LaunchAgents/$l.plist" ] || miss="$miss $l(plist-left)"
done
if [ -z "$miss" ]; then
    ok "all three agents are BOOTED OUT and their plists removed (not just deleted-in-place)"
else
    fail "stop_and_remove_agents left agents registered or plists on disk:$miss"
fi
# The reaps. The HUD one is the point of this file: run_hud.sh prefers the bundle
# built INSIDE the install home, and remove_home is about to delete it.
logged 'pkill -f DARWIN/daemon/target/release/darwind' \
    && ok "the daemon is reaped before the install home is deleted" \
    || fail "nothing reaps darwind"
logged 'pkill -f DARWIN/inference/server.py' \
    && ok "the inference server is reaped before the install home is deleted" \
    || fail "nothing reaps the inference server"
if logged 'pkill -f DARWIN/hud/src-tauri/target/release/bundle/'; then
    ok "the HUD is reaped UNCONDITIONALLY here too, so remove_home cannot delete a running HUD's own executable"
else
    fail "stop_and_remove_agents does not reap the in-home HUD bundle. boot/run_hud.sh execs \
\$DARWIN_ROOT/hud/src-tauri/target/release/bundle/.../DARWIN.app/Contents/MacOS/DARWIN in \
preference to any /Applications copy, and install.sh's place_hud_app is 'never fatal' — with no \
verified DARWIN.app in /Applications or ~/Applications, remove_hud_app's pkill never runs and \
remove_home deletes the RUNNING HUD's executable out from under it while the script prints \
'completely removed from this machine'"
fi

# --- 4b. LAYERS DO NOT COMPOSE FOR FREE: each reap pattern must still MATCH the
# path its own boot wrapper execs. Everything above pins that the three `pkill`
# calls are PRESENT and spelled a particular way; nothing pinned that the spelling
# still describes reality. MEASURED: changing boot/run_hud.sh's bundle root from
# "$DARWIN_ROOT/hud/src-tauri/target/release/bundle" to "$DARWIN_ROOT/hud/dist-app"
# left THIS file, scripts/test_install_boot.sh and scripts/test_doc_claims.sh all at
# exit 0 while the HUD reap could no longer match any process — i.e. the defect this
# file was written to catch would have come straight back, silently.
#
# The wrappers resolve every target from $DARWIN_ROOT and the install home's last
# component is literally DARWIN, so render $DARWIN_ROOT -> DARWIN in the wrapper and
# require each pattern to be a literal substring of it. Two narrowings, both needed:
#   * COMMENTS and `echo`/`printf` lines are stripped first. run_hud.sh names its
#     bundle path a SECOND time inside an error message, and matching that made the
#     probe above pass while the `find` that actually locates the app had moved —
#     the check matched prose instead of the code path. (Two separate `-e` deletes,
#     not one `\(echo\|printf\)` alternation: BSD sed's BRE has no `\|`, so the
#     alternation silently matched nothing and the probe above passed anyway.)
#   * a trailing "/" on a pattern only asserts "something below this directory" (the
#     HUD's bundle dir is a find ROOT with the .app beneath it), so it is stripped.
# Over-stripping cannot pass vacuously: an empty wrapper text matches nothing and
# every pattern is then reported unmatched.
reap_src="$(slice stop_and_remove_agents)"
wrapper_txt=""
for w in run_daemon.sh run_inference.sh run_hud.sh; do
    [ -f "$ROOT/boot/$w" ] || die "missing boot/$w — the reap-pattern check would be VACUOUS"
    wrapper_txt="$wrapper_txt
$(sed -e '/^[[:space:]]*#/d' -e '/^[[:space:]]*echo[[:space:]]/d' \
      -e '/^[[:space:]]*printf[[:space:]]/d' \
      -e 's|\${DARWIN_ROOT}|DARWIN|g' -e 's|\$DARWIN_ROOT|DARWIN|g' "$ROOT/boot/$w")"
done
[ -n "${wrapper_txt//[[:space:]]/}" ] || die "the boot wrappers stripped to nothing — the reap-pattern check would be VACUOUS"
reap_pats="$(printf '%s\n' "$reap_src" | sed -n 's/^[[:space:]]*pkill -f "\([^"]*\)".*/\1/p')"
n_reap="$(printf '%s\n' "$reap_pats" | grep -c . || true)"
if [ "${n_reap:-0}" -ne 3 ]; then
    fail "expected 3 pkill patterns in stop_and_remove_agents (daemon, inference, HUD), extracted ${n_reap:-0} — the reap-pattern/boot-wrapper agreement went UNCHECKED"
else
    reap_bad=""
    while IFS= read -r pat; do
        [ -n "$pat" ] || continue
        printf '%s' "$wrapper_txt" | grep -Fq "${pat%/}" || reap_bad="$reap_bad $pat"
    done <<REAPPATS
$reap_pats
REAPPATS
    if [ -z "$reap_bad" ]; then
        ok "all 3 reap patterns are still substrings of what boot/run_{daemon,inference,hud}.sh actually exec (comments and error text excluded)"
    else
        fail "reap pattern(s) match NO path any boot wrapper execs, so the pkill silently reaps nothing:$reap_bad — a wrapper moved its target and only this check notices"
    fi
fi

# --- 5. C: with scripts/install_boot.sh present, the bootout is DELEGATED ---------
command rm -rf "$SANDBOX/home"
mkdir -p "$SANDBOX/home/Library/LaunchAgents" \
         "$SANDBOX/home/Library/Application Support/DARWIN/scripts"
BOOT_STUB="$SANDBOX/home/Library/Application Support/DARWIN/scripts/install_boot.sh"
printf '#!/bin/bash\nprintf "boot-stub %%s\\n" "$*" >> "%s/boot_stub.log"\n' "$SANDBOX" > "$BOOT_STUB"
chmod 755 "$BOOT_STUB"
: > "$SANDBOX/boot_stub.log"
for l in com.darwin.hud com.darwin.daemon com.darwin.inference; do
    printf 'rendered\n' > "$SANDBOX/home/Library/LaunchAgents/$l.plist"
done
drive stop_and_remove_agents
if grep -Fq 'boot-stub --uninstall' "$SANDBOX/boot_stub.log"; then
    ok "with scripts/install_boot.sh present the teardown DELEGATES to its --uninstall (one LABELS list, not two)"
else
    fail "stop_and_remove_agents did not delegate to scripts/install_boot.sh --uninstall; its own LABELS fallback would drift from the installer's"
fi
logged 'launchctl bootout' \
    && fail "the delegating path ALSO ran launchctl itself — the bootout happens twice" \
    || ok "the delegating path does not duplicate the bootout"
logged 'pkill -f DARWIN/hud/src-tauri/target/release/bundle/' \
    && ok "the HUD reap happens on the delegating path too (it is outside the if/else)" \
    || fail "the HUD is not reaped when the teardown delegates to install_boot.sh"

# --- 5b. THREE LABELS LISTS, ONE TRUTH. The delegation check above names this risk
# in its own failure text ("its own LABELS fallback would drift from the installer's")
# and then never checks it. There are three copies of the agent list in play:
#   * scripts/install_boot.sh's LABELS — the single source of truth;
#   * uninstall.sh's LABELS — the confirmation preview AND the fallback loop that
#     runs when scripts/install_boot.sh is absent or non-executable. A label added to
#     the installer alone is an agent that fallback leaves booted and whose plist it
#     leaves on disk, under "completely removed from this machine";
#   * this harness's own LABELS (in the generated harness.sh) — scaffolding, but if
#     THAT drifts, every bootout/plist assertion in section 4 checks the wrong
#     labels and still reports ok. Section 4 cannot catch either drift, because the
#     sliced function reads the harness's LABELS, not uninstall.sh's.
# The ORDER deliberately differs (uninstall tears the visible HUD down first, the
# installer brings inference up first), so compare SETS, not sequences.
labels_of() {   # $1 = file -> its LABELS entries, one per line, sorted
    grep -m1 -E '^[[:space:]]*LABELS=\(' "$1" | grep -oE 'com\.darwin\.[a-z]+' | sort
}
un_labels="$(labels_of "$UNINSTALL")"
boot_labels="$(labels_of "$ROOT/scripts/install_boot.sh")"
harness_labels="$(labels_of "$SANDBOX/harness.sh")"
flat() { printf '%s' "$1" | tr '\n' ' '; }
if [ -z "$un_labels" ] || [ -z "$boot_labels" ] || [ -z "$harness_labels" ]; then
    fail "could not read a LABELS array out of uninstall.sh / scripts/install_boot.sh / this harness (got '$(flat "$un_labels")' / '$(flat "$boot_labels")' / '$(flat "$harness_labels")') — the drift check went UNCHECKED, not passed"
elif [ "$un_labels" = "$boot_labels" ] && [ "$un_labels" = "$harness_labels" ]; then
    ok "all three LABELS lists (installer, uninstaller fallback, this harness) are the same SET ($(flat "$un_labels"))"
else
    fail "the LABELS lists have DRIFTED — install_boot.sh: $(flat "$boot_labels")| uninstall.sh: $(flat "$un_labels")| this harness: $(flat "$harness_labels"). The uninstaller's copy drives the fallback teardown and the confirmation preview; the harness's copy decides which labels section 4 checks at all"
fi

# --- 6. E/F: the install home goes whole; a sibling beside it does not ------------
command rm -rf "$SANDBOX/home"
H="$SANDBOX/home/Library/Application Support"
mkdir -p "$H/DARWIN/state/ipc" "$H/DARWIN/config" "$H/OtherApp"
printf 'db\n'   > "$H/DARWIN/state/darwin.db"
printf 'db\n'   > "$H/DARWIN/state/audit.db"
printf 'toml\n' > "$H/DARWIN/config/darwin.toml"
printf 'keep\n' > "$H/OtherApp/keep.txt"
drive remove_home
[ ! -e "$H/DARWIN" ] \
    && ok "remove_home deletes the install home whole — state/, both SQLite DBs and config with it" \
    || fail "remove_home left the install home (or part of it) behind"
[ -f "$H/OtherApp/keep.txt" ] \
    && ok "a sibling directory beside the install home is untouched" \
    || fail "remove_home deleted a sibling of the install home"
logged 'guard_home' \
    && ok "remove_home re-asserts guard_home immediately before the rm -rf" \
    || fail "remove_home no longer re-asserts guard_home before deleting"
grep -Fq 'ESCAPE ' "$SANDBOX/calls.log" \
    && fail "remove_home aimed an rm at a path OUTSIDE the sandbox: $(grep -F 'ESCAPE ' "$SANDBOX/calls.log")" \
    || ok "every rm target stayed inside the throwaway sandbox"

# --- 7. G: Keychain deletion is service-scoped and loops until empty --------------
command rm -rf "$SANDBOX/home"; mkdir -p "$SANDBOX/home"
KC_ITEMS=3 drive remove_keychain_items
n="$(grep -c '^security ' "$SANDBOX/calls.log" | tr -d ' ')"
[ "$n" = "4" ] \
    && ok "remove_keychain_items loops until the service is empty (3 items -> 4 calls, the last one the miss)" \
    || fail "expected 4 security calls for 3 items, saw $n — the delete loop does not drain the service"
bad="$(grep '^security ' "$SANDBOX/calls.log" | grep -v 'delete-generic-password -s com.darwin.daemon' || true)"
[ -z "$bad" ] \
    && ok "every Keychain call is delete-generic-password scoped to service com.darwin.daemon" \
    || fail "an unscoped / unexpected Keychain call: $bad"

# --- 8. H: the decoy must survive, the verified bundle must not -------------------
plant_app() {   # $1 = bundle id to write into Info.plist
    command rm -rf "$SANDBOX/home"
    mkdir -p "$SANDBOX/home/Applications/DARWIN.app/Contents"
    cat > "$SANDBOX/home/Applications/DARWIN.app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>$1</string>
</dict></plist>
EOF
    printf 'payload\n' > "$SANDBOX/home/Applications/DARWIN.app/Contents/precious.txt"
}
plant_app "com.acme.notdarwin"
drive remove_hud_app
if [ -f "$SANDBOX/home/Applications/DARWIN.app/Contents/precious.txt" ]; then
    ok "a DECOY at the expected path (right name, bundle id com.acme.notdarwin) is LEFT IN PLACE"
else
    fail "remove_hud_app rm -rf'd a DARWIN.app that does NOT carry bundle id com.darwin.hud — one misplaced folder from deleting the wrong tree"
fi
plant_app "com.darwin.hud"
drive remove_hud_app
[ ! -e "$SANDBOX/home/Applications/DARWIN.app" ] \
    && ok "...and a bundle that DOES verify as com.darwin.hud is removed (the check is not a blanket refusal)" \
    || fail "remove_hud_app left a verified com.darwin.hud bundle behind — the uninstall is incomplete"

# --- 9. I: the HUD's out-of-home support dirs, and only those ---------------------
command rm -rf "$SANDBOX/home"
L="$SANDBOX/home/Library"
for b in WebKit Caches HTTPStorages "Saved Application State"; do
    mkdir -p "$L/$b/com.darwin.hud"; printf 'x\n' > "$L/$b/com.darwin.hud/f"
done
mkdir -p "$L/Saved Application State/com.darwin.hud.savedState"
printf 'x\n' > "$L/Saved Application State/com.darwin.hud.savedState/f"
mkdir -p "$L/Caches/com.acme.other"; printf 'keep\n' > "$L/Caches/com.acme.other/f"
drive remove_hud_support_dirs
left=""
for d in "$L/WebKit/com.darwin.hud" "$L/Caches/com.darwin.hud" "$L/HTTPStorages/com.darwin.hud" \
         "$L/Saved Application State/com.darwin.hud" "$L/Saved Application State/com.darwin.hud.savedState"; do
    [ -e "$d" ] && left="$left $d"
done
[ -z "$left" ] \
    && ok "all five out-of-home HUD support paths go (WebKit, Caches, HTTPStorages, saved state + .savedState)" \
    || fail "a reinstall would inherit HUD state left at:$left"
[ -f "$L/Caches/com.acme.other/f" ] \
    && ok "another bundle's cache directory beside them is untouched" \
    || fail "remove_hud_support_dirs widened past the com.darwin.hud directory into ~/Library/Caches itself"

# --- 10. J: teardown ORDER — processes before files ------------------------------
# Read the order off the main block, bounded at both ends so a match elsewhere in
# the file (a comment, a definition) cannot stand in for the call sequence.
main="$(awk '/^ui_info "Removing D\.A\.R\.W\.I\.N\. \.\.\."$/{c=1} c{print} c && /^ui_hr$/ && NR>1 {n++} c && n==1{exit}' "$UNINSTALL")"
case "$main" in
    *stop_and_remove_agents*remove_home*) ok "main tears the agents down BEFORE deleting the install home" ;;
    *remove_home*stop_and_remove_agents*) fail "main deletes the install home BEFORE stopping the agents — every agent would be left pointed at deleted files" ;;
    *) fail "could not read the teardown call order out of uninstall.sh's main block — the ordering claim went UNCHECKED" ;;
esac

echo
if [ "$fails" -eq 0 ]; then echo "ALL PASS"; else echo "$fails FAILURE(S)"; fi
exit "$fails"
