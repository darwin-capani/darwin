#!/usr/bin/env bash
# Documentation claims that are CHECKABLE must stay true.
#
# A previous sweep found four artifacts in one benchmark directory contradicting each
# other. This one found seven more across the top-level docs and the shipped config:
# a reranker latency 5.3x overstated (the pre-optimization number, left in place), an
# embed speedup mixing a per-predict cost against an end-to-end baseline, a doctor that
# checked two of three LaunchAgents, a README that counted two, an architecture doc
# claiming ~38 config sections against 81, a bringup doc naming a model the script
# never downloads, and a sandbox spec describing a declaration channel that was
# replaced.
#
# Every assertion here derives the truth from the code or an artifact rather than
# restating a number, so it cannot go stale the way the claims did.
#
#   Run: bash scripts/test_doc_claims.sh
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# THE INTERPRETER. This is a REPO test, so prefer the repo's own venv and fall
# back to the deployed install; $DARWIN_PY overrides both.
#
# WHAT WENT WRONG: this defaulted straight to the DEPLOYED install's python. On
# any machine without one — a fresh clone, CI, anything before the first
# ./install.sh — both python-backed assertions swallowed the error with
# `2>/dev/null` and then branched on the empty result. The ARCHITECTURE
# config-section guard simply VANISHED (no ok, no fail — the exact drift class it
# was written to catch, silently unguarded), and the reranker guard emitted an
# uninterpretable "artifact says ~ ms" failure pointing at a doc mismatch that did
# not exist. Both now fail LOUDLY and say the interpreter is the problem.
PY="${DARWIN_PY:-}"
if [ -z "$PY" ]; then
    for _cand in "$ROOT/.venv/bin/python" "$HOME/Library/Application Support/DARWIN/.venv/bin/python"; do
        if [ -x "$_cand" ]; then PY="$_cand"; break; fi
    done
    [ -n "$PY" ] || PY="$ROOT/.venv/bin/python"   # name the repo-local one in the error
fi
fails=0
ok()   { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; fails=$((fails + 1)); }

# --- the reranker latency the config advertises ------------------------------
artifact="$ROOT/inference/benchmarks/coreml_rerank_eval/results.json"
if [ -f "$artifact" ]; then
    median="$("$PY" -c "
import json,sys
d=json.load(open('$artifact'))
print(round(d['B_reranked']['20']['rerank_latency']['median_ms']))" 2>/dev/null)"
    if [ -z "$median" ]; then
        fail "could not read the reranker median out of results.json with '$PY' — this claim went UNCHECKED (set DARWIN_PY to a python3)"
    elif grep -q "~${median} ms median" "$ROOT/config/darwin.toml"; then
        ok "the reranker latency in config matches the artifact (~${median} ms at K=20)"
    else
        fail "config's reranker latency does not match results.json (artifact says ~${median} ms)"
    fi
    if grep -q "221 ms" "$ROOT/config/darwin.toml"; then
        fail "config still advertises the PRE-optimization reranker latency (221 ms)"
    else
        ok "the superseded 221 ms figure is gone"
    fi
fi

# --- every LaunchAgent install_boot.sh installs must be checked by doctor.sh --
labels="$(grep -oE 'com\.darwin\.[a-z]+' "$ROOT/scripts/install_boot.sh" | sort -u)"
missing=""
for l in $labels; do
    grep -q "$l" "$ROOT/scripts/doctor.sh" || missing="$missing $l"
done
if [ -z "$missing" ]; then
    ok "doctor.sh checks every agent install_boot.sh installs ($(echo $labels | wc -w | tr -d ' '))"
else
    fail "doctor.sh does not check:$missing — a dead agent yields a green board"
fi

# --- the README's LaunchAgent count ------------------------------------------
n_labels="$(echo "$labels" | wc -w | tr -d ' ')"
if grep -qE "the (two|three|four) LaunchAgents" "$ROOT/README.md"; then
    word="$(grep -oE 'the (two|three|four) LaunchAgents' "$ROOT/README.md" | head -1 | awk '{print $2}')"
    case "$n_labels:$word" in
        2:two|3:three|4:four) ok "README's LaunchAgent count matches ($word = $n_labels)" ;;
        *) fail "README says '$word LaunchAgents'; install_boot.sh installs $n_labels" ;;
    esac
fi

# --- the architecture doc's config-section count -----------------------------
fields="$("$PY" -c "
import re
s=open('$ROOT/daemon/src/config.rs').read()
i=s.index('pub struct Config {'); j=s.index(chr(10)+'}', i)
print(len(re.findall(r'^\s+pub \w+:', s[i:j], re.M)))" 2>/dev/null)"
if [ -z "$fields" ]; then
    fail "could not count Config's fields with '$PY' — the ARCHITECTURE config-section guard did NOT run (set DARWIN_PY to a python3)"
elif grep -q "($fields today)" "$ROOT/docs/ARCHITECTURE.md"; then
    ok "ARCHITECTURE's config-section count matches Config ($fields fields)"
else
    fail "ARCHITECTURE's config-section count is stale (Config has $fields fields)"
fi

# --- BRINGUP must not name models deploy_models.py does not fetch -------------
if grep -q "Kokoro TTS)" "$ROOT/docs/BRINGUP.md"; then
    fail "BRINGUP says deploy_models.py downloads Kokoro TTS; it downloads [models] only"
else
    ok "BRINGUP does not overclaim what deploy_models.py fetches"
fi

# --- SANDBOX must describe the channel that is actually live -----------------
if grep -q "DARWIN_VISION_SCREEN" "$ROOT/docs/SANDBOX.md"; then
    fail "SANDBOX describes an env-var declaration channel that is neither set nor read"
else
    ok "SANDBOX describes the manifest as the declaration channel"
fi
if grep -qE '^\s*screen\s*=\s*true' "$ROOT/apps/vision/manifest.toml"; then
    ok "...and the vision manifest really does declare it"
else
    fail "SANDBOX says the manifest declares screen, but it does not"
fi

# --- SANDBOX's plugin-SDK posture must match the shipped gate ----------------
# SANDBOX.md is the document an operator reads to know what is ARMED on a default
# install. It said the register-on-launch handshake "ships OFF" while
# config/darwin.toml, the Default impl and PLUGIN_SDK.md all ship it ON — an
# audit of the shipped attack surface from that file got the posture backwards.
sdk_enabled="$(awk '/^\[plugin_sdk\]/{f=1;next} /^\[/{f=0} f && /^enabled[[:space:]]*=/{print;exit}' \
    "$ROOT/config/darwin.toml" | grep -oE 'true|false' | head -1)"
sdk_doc="$(grep -oE 'register_plugin[^)]*ships \*\*(ON|OFF)\*\*' "$ROOT/docs/SANDBOX.md" | grep -oE 'ON|OFF' | tail -1)"
if [ -z "$sdk_enabled" ] || [ -z "$sdk_doc" ]; then
    fail "could not derive the plugin_sdk posture (config=[$sdk_enabled] SANDBOX=[$sdk_doc]) — the claim went UNCHECKED"
else
    sdk_want=OFF; [ "$sdk_enabled" = "true" ] && sdk_want=ON
    if [ "$sdk_doc" = "$sdk_want" ]; then
        ok "SANDBOX's register_plugin posture matches the shipped gate (ships $sdk_want)"
    else
        fail "SANDBOX says register_plugin ships $sdk_doc; [plugin_sdk].enabled = $sdk_enabled means it ships $sdk_want"
    fi
fi

# --- HUD.md must not promise a key re-read the daemon cannot do --------------
# resolve_api_key() caches into a `OnceLock`, which has no reset path, and the
# HUD's verify_and_store never restarts the daemon. HUD.md §5.1 told the user the
# key is re-read without a restart; ARCHITECTURE.md, BRINGUP.md and hud/README.md
# all say the opposite. Following HUD.md leaves cloud routing silently degraded to
# the local 4B with an amber CLOUD KEY light and no error.
if grep -q 'static API_KEY: OnceLock' "$ROOT/daemon/src/anthropic.rs"; then
    if grep -q 're-reads the key without a restart' "$ROOT/docs/HUD.md"; then
        fail "HUD.md says the daemon re-reads the Anthropic key without a restart; anthropic.rs caches it in a OnceLock (once per process)"
    else
        ok "HUD.md does not promise a keyless-restart re-read that the OnceLock cannot do"
    fi
else
    ok "anthropic.rs no longer caches the key in a OnceLock — re-check HUD.md's restart wording by hand"
fi

# --- ARCHITECTURE must describe the VAD that actually ships ------------------
# ARCHITECTURE.md wins over every other doc by its own rule (line 3), and its
# component diagram still called the VAD "the RMS gate" — now only the degraded
# fallback — while omitting the `[audio].vad` key entirely. Latency work driven
# off that diagram tunes rms_threshold, which does not affect segmentation while
# the Silero weights are present.
vad_default="$(grep -m1 -E '^vad[[:space:]]*=' "$ROOT/config/darwin.toml" | sed -E 's/^vad[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/')"
if [ -z "$vad_default" ]; then
    fail "could not read [audio].vad out of config/darwin.toml — the ARCHITECTURE VAD claim went UNCHECKED"
elif grep -qi "$vad_default" "$ROOT/docs/ARCHITECTURE.md"; then
    ok "ARCHITECTURE names the shipped VAD backend ($vad_default)"
else
    fail "ARCHITECTURE never mentions the shipped VAD backend '$vad_default' (its diagram still describes the RMS gate)"
fi

# --- config must not advertise a control that has no command arm -------------
# config/darwin.toml stated as fact that "A per-turn voice command can override
# this for a single turn". boundary::set_turn_trim has no production caller — it
# and TurnTrimGuard are both #[allow(dead_code)] and boundary.rs says so itself
# ("the run_pipeline install is the integration step"). The shipped,
# operator-facing file claimed a live privacy control the user never gets.
trim_callers="$(grep -rn 'boundary::set_turn_trim(' "$ROOT/daemon/src" 2>/dev/null || true)"
if [ -n "$trim_callers" ]; then
    ok "the per-turn trim override has a production caller — config may advertise it"
elif grep -q 'A per-turn voice command can override this for a single turn' "$ROOT/config/darwin.toml"; then
    fail "config/darwin.toml states the per-turn voice trim override as fact, but nothing calls boundary::set_turn_trim"
else
    ok "config/darwin.toml does not claim a per-turn trim override that has no command arm"
fi

# --- each script's in-band --help must print its WHOLE header block ----------
# `-h|--help` in all three scripts prints the header comment as the help text via
# a HARD-CODED line range, so every line added to the header silently truncated
# what --help showed. install.sh's ended mid-clause on "MCP/webhooks need a",
# dropping the promise that no secret, key, state DB, venv, model or build
# artifact is ever written into the source repo; install_boot.sh's stopped before
# --install and --uninstall, the only two flags that do anything. Derived: the
# LAST line of the header block (lines 2.. up to the first non-comment) must
# appear in the help output.
for _s in "$ROOT/install.sh" "$ROOT/uninstall.sh" "$ROOT/scripts/install_boot.sh"; do
    _n="${_s##*/}"
    _tail="$(awk 'NR > 1 { if (substr($0, 1, 1) != "#") exit; print }' "$_s" | tail -1 | sed 's/^# \{0,1\}//')"
    if [ -z "$_tail" ]; then
        fail "$_n: could not read its header block — the --help completeness claim went UNCHECKED"
        continue
    fi
    if bash "$_s" --help 2>/dev/null | grep -qF "$_tail"; then
        ok "$_n --help prints its whole header block (through \"$(printf '%.42s' "$_tail")…\")"
    else
        fail "$_n --help TRUNCATES its header block — the last line (\"$(printf '%.42s' "$_tail")…\") never reaches the user"
    fi
done

# --- install_boot.sh's own prose must match its LABELS -----------------------
# This file is the source of truth the agent-count check above reads. Its own
# usage line and preflight comment still said "both agents" after com.darwin.hud
# made it three, and nothing checked the one file the gate trusts.
n_boot="$(grep -m1 -E '^LABELS=\(' "$ROOT/scripts/install_boot.sh" | grep -oE 'com\.darwin\.[a-z]+' | wc -l | tr -d ' ')"
if [ "${n_boot:-0}" -eq 0 ]; then
    fail "could not read LABELS out of install_boot.sh — its prose count went UNCHECKED"
elif [ "$n_boot" -ne 2 ] && grep -q 'both agents' "$ROOT/scripts/install_boot.sh"; then
    fail "install_boot.sh installs $n_boot agents but its own prose still says 'both agents'"
else
    ok "install_boot.sh's prose agent count matches LABELS ($n_boot)"
fi

# --- install.sh must not advertise a model it never fetches ------------------
# The #37 speculative-decoding DRAFT checkpoint was deliberately dropped from the
# download set, but --no-models and --check still named it, so a user reading
# either was told a draft checkpoint is part of the model set. It is not fetched
# in any mode.
if grep -qE '^MODELS=\(' "$ROOT/install.sh" && ! grep -E '^MODELS=\(' "$ROOT/install.sh" | grep -qi 'draft'; then
    draft_lines="$(grep -nE '^[[:space:]]*(ui_warn|ui_note|ui_info|ui_ok|ui_err|plan|echo) ' "$ROOT/install.sh" | grep -i 'draft' || true)"
    if [ -n "$draft_lines" ]; then
        fail "install.sh's MODELS set has no draft checkpoint but its user-facing text still advertises one: $(printf '%s' "$draft_lines" | head -1)"
    else
        ok "install.sh does not advertise a DRAFT model it never fetches"
    fi
fi

# --- install.sh's status board must never call ZERO models RESIDENT ----------
# The final board is the last thing an operator reads after a multi-GB install.
# The download loop is deliberately best-effort (warn + continue), so an offline /
# proxied / rate-limited machine reaches the board with an empty cache — and the
# else-branch emitted the bare success tag "RESIDENT" on a count of 0. Driven, not
# grepped: the tag block is sliced out of install.sh and EVALUATED.
board_blk="$(awk '/if \[ "\$_mcount" -gt 0 \]; then/{c=1} c{print} c && /fi[[:space:]]*$/{exit}' "$ROOT/install.sh")"
if [ -z "$board_blk" ]; then
    fail "could not slice install.sh's ON-DEVICE MODELS tag block — the zero-model claim went UNCHECKED"
else
    board_tag() {
        _mcount="$1" HF_HOME_DIR="$TMP_EMPTY_HF" bash -c '
            set -u
            '"$board_blk"'
            printf "%s\n" "${MODELS_TAG:-}"'
    }
    TMP_EMPTY_HF="$(mktemp -d)"
    tag0="$(board_tag 0)"
    tag7="$(board_tag 7)"
    rm -rf "$TMP_EMPTY_HF"
    case "$tag0" in
        "")         fail "install.sh's board produced no tag for zero resident models" ;;
        *RESIDENT*) case "$tag0" in
                        RESIDENT) fail "install.sh's board reports a bare 'RESIDENT' with ZERO models resident" ;;
                        *)        ok "install.sh's board reports '$tag0' when no model is resident" ;;
                    esac ;;
        *)          ok "install.sh's board reports '$tag0' when no model is resident" ;;
    esac
    if [ "$tag7" = "7 RESIDENT" ]; then
        ok "...and a real count when models ARE present ($tag7)"
    else
        fail "install.sh's board reports '$tag7' for 7 resident models"
    fi
fi

# --- uninstall.sh's --help must name every directory it deletes --------------
# --help IS the header block, and it is the documented way to see what this
# destructive script removes. The four ~/Library/{WebKit,Caches,HTTPStorages,
# Saved Application State}/com.darwin.hud trees were added later and never
# propagated, so --help under-reported real delete targets.
un_help="$(bash "$ROOT/uninstall.sh" --help 2>/dev/null)"
hud_bases="$(awk '/^remove_hud_support_dirs\(\) \{/{c=1} c{print} c && /^\}$/{exit}' "$ROOT/uninstall.sh" \
    | grep -oE '\$HOME/Library/[A-Za-z][A-Za-z ]*' | sed 's|\$HOME/Library/||' | sed 's/ *$//' | sort -u)"
if [ -z "$hud_bases" ] || [ -z "$un_help" ]; then
    fail "could not derive uninstall.sh's HUD-support delete targets — the --help footprint claim went UNCHECKED"
else
    un_missing=""
    while IFS= read -r _b; do
        [ -n "$_b" ] || continue
        printf '%s' "$un_help" | grep -qF "$_b" || un_missing="$un_missing [$_b]"
    done <<EOF
$hud_bases
EOF
    if [ -z "$un_missing" ]; then
        ok "uninstall.sh --help names every HUD-support directory it deletes"
    else
        fail "uninstall.sh --help omits delete target(s):$un_missing"
    fi
fi

# --- bringup.sh's degraded socket probe must actually be announced -----------
# unix_connectable's comment claimed that with no python available it falls back
# to an existence test "and SAY so via the caller". No caller said anything: the
# word "existence" appeared nowhere else in the file. On a no-python tree a STALE
# socket inode therefore made bringup print "already reachable" and mark the stage
# PASS with nothing connected. DRIVEN, not grepped: the real functions are sliced
# out and run against a stale socket with no python reachable.
bp_fn() { awk -v want="$1() {" 'index($0, want) == 1 {c=1} c{print} c && /^\}$/{exit}' "$ROOT/scripts/bringup.sh"; }
bp_src="$(bp_fn probe_python)"$'\n'"$(bp_fn unix_connectable)"$'\n'"$(bp_fn probe_note)"
case "$bp_src" in
    *"probe_python() {"*"unix_connectable() {"*"probe_note() {"*)
        bp_tmp="$(mktemp -d)"
        bp_empty="$bp_tmp/nobin"; mkdir -p "$bp_empty"
        # A real socket inode with nothing listening — exactly the stale-socket case.
        "$PY" -c "import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])" \
            "$bp_tmp/stale.sock" >/dev/null 2>&1
        if [ -S "$bp_tmp/stale.sock" ]; then
            # NOTE the absolute /bin/bash: a bare `bash` would be looked up in the
            # stripped PATH we are setting for this very command, and not found.
            bp_out="$(PATH="$bp_empty" VENV_PY=/nonexistent SOCK="$bp_tmp/stale.sock" /bin/bash -c '
                set -uo pipefail
                '"$bp_src"'
                if unix_connectable "$SOCK"; then printf "REACHABLE"; else printf "NO"; fi
                printf "|%s" "$(probe_note)"' 2>/dev/null)"
            bp_verdict="${bp_out%%|*}"
            bp_note="${bp_out#*|}"
            if [ "$bp_verdict" = "REACHABLE" ] && [ -n "$bp_note" ]; then
                ok "bringup.sh's existence-only fallback announces itself —$bp_note"
            elif [ "$bp_verdict" = "REACHABLE" ]; then
                fail "bringup.sh calls a STALE socket reachable and says nothing — a PASS the run never verified"
            elif [ "$bp_verdict" = "NO" ]; then
                ok "bringup.sh's probe rejects a stale socket outright (no degraded PASS is possible)"
            else
                fail "could not drive bringup.sh's socket probe (got '$bp_out') — the claim went UNCHECKED"
            fi
        else
            fail "could not create a stale socket with '$PY' — bringup.sh's degraded-probe claim went UNCHECKED"
        fi
        rm -rf "$bp_tmp"
        # ...and every PASS derived from that probe must carry the note.
        bp_sites="$(grep -cE 'say_ok "(inference server|daemon command channel)' "$ROOT/scripts/bringup.sh" | tr -d ' ')"
        bp_noted="$(grep -E 'say_ok "(inference server|daemon command channel)' "$ROOT/scripts/bringup.sh" | grep -c 'probe_note' | tr -d ' ')"
        if [ "$bp_sites" -gt 0 ] && [ "$bp_sites" = "$bp_noted" ]; then
            ok "all $bp_sites socket-derived say_ok lines in bringup.sh carry probe_note"
        else
            fail "$((bp_sites - bp_noted)) of $bp_sites socket-derived say_ok lines in bringup.sh report a PASS without probe_note"
        fi
        ;;
    *)
        fail "could not slice probe_python/unix_connectable/probe_note out of bringup.sh — the degraded-probe claim went UNCHECKED"
        ;;
esac

# --- uninstall.sh's SAFETY GUARD must actually refuse a malformed $HOME ------
# The guard's own comment is a checkable claim — "makes a broad/accidental delete
# impossible even if $HOME were malformed" — and for a long time it was false. It
# compared DARWIN_HOME to a string built from the SAME $HOME expression, so the
# `!=` branch was unreachable and the protected-path case list could never match.
# Under HOME='' , HOME=/, HOME=/tmp or HOME=/System the guard PASSED, the run then
# deleted the one non-$HOME-derived target (/Applications/DARWIN.app), left the
# real install home / LaunchAgents / Keychain items in place, and still reported
# "completely removed". So DRIVE the guard rather than reading it.
guard_src="$(awk '/^real_home\(\) \{/{c=1} c{print} c && /^\}$/{n++} c && n==2{exit}' "$ROOT/uninstall.sh")"
guard_probe() {   # $1 = the $HOME to run the guard under
    HOME="$1" GUARD_SRC="$guard_src" bash -c '
        set -uo pipefail
        ui_err() { :; }; ui_note() { :; }; ui_warn() { :; }
        DARWIN_HOME="$HOME/Library/Application Support/DARWIN"
        eval "$GUARD_SRC"
        guard_home
        echo PASSED' 2>/dev/null || echo REFUSED
}
case "$guard_src" in
    *"real_home() {"*"guard_home() {"*)
        guard_bad=""
        for _h in "" "/" "/tmp" "/System" "relative/path"; do
            [ "$(guard_probe "$_h")" = "REFUSED" ] || guard_bad="$guard_bad [HOME='$_h']"
        done
        if [ -n "$guard_bad" ]; then
            fail "uninstall.sh's guard_home ACCEPTS a malformed \$HOME:$guard_bad — it would half-uninstall and report success"
        else
            ok "uninstall.sh's guard_home refuses an empty / root / foreign / relative \$HOME"
        fi
        if [ "$(guard_probe "$HOME")" = "PASSED" ]; then
            ok "...and still accepts this user's real \$HOME (the guard is not a blanket refusal)"
        else
            fail "uninstall.sh's guard_home REFUSES this user's real \$HOME ($HOME) — a legitimate uninstall cannot run"
        fi
        ;;
    *)
        fail "could not slice real_home + guard_home out of uninstall.sh — the guard claim went UNCHECKED"
        ;;
esac

# --- uninstall.sh must not advertise a log dir nothing writes ----------------
# LOG_DIR was ~/Library/Logs/DARWIN. Nothing in the product ever created it — the
# real logs are $DARWIN_HOME/state/logs (boot/*.plist StandardOutPath,
# boot/run_daemon.sh), inside the install home remove_home already deletes. The
# confirmation window and --help both told the user their logs live somewhere
# that does not exist.
log_writers="$(grep -rIl 'Library/Logs' "$ROOT/daemon/src" "$ROOT/boot" "$ROOT/inference" \
    "$ROOT/hud/src" "$ROOT/hud/src-tauri/src" 2>/dev/null || true)"
log_shown="$(printf '%s' "$un_help" | grep 'Library/Logs' || true)"
log_told="$(grep -E '^[[:space:]]*(ui_note|ui_info|ui_warn|ui_ok) ' "$ROOT/uninstall.sh" | grep 'Library/Logs' || true)"
if [ -n "$log_writers" ]; then
    ok "something in the product writes ~/Library/Logs — uninstall.sh may name it ($log_writers)"
elif [ -n "$log_shown" ] || [ -n "$log_told" ]; then
    fail "uninstall.sh tells the user about ~/Library/Logs/DARWIN but nothing in the product writes there (real logs: \$DARWIN_HOME/state/logs)"
else
    ok "uninstall.sh does not advertise a log directory nothing writes to"
fi

echo
if [ "$fails" -eq 0 ]; then echo "ALL PASS"; else echo "$fails FAILURE(S)"; fi
exit "$fails"
