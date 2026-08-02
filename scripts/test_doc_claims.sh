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
PY="${DARWIN_PY:-$HOME/Library/Application Support/DARWIN/.venv/bin/python}"
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
    if [ -n "$median" ] && grep -q "~${median} ms median" "$ROOT/config/darwin.toml"; then
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
if [ -n "$fields" ]; then
    if grep -q "($fields today)" "$ROOT/docs/ARCHITECTURE.md"; then
        ok "ARCHITECTURE's config-section count matches Config ($fields fields)"
    else
        fail "ARCHITECTURE's config-section count is stale (Config has $fields fields)"
    fi
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

echo
if [ "$fails" -eq 0 ]; then echo "ALL PASS"; else echo "$fails FAILURE(S)"; fi
exit "$fails"
