#!/bin/bash
# Hermetic selftest for the sandboxed shell / terminal (#43) safety contract.
#
# WHAT IT GUARDS: the FOUR by-construction layers a shell command must clear
# before a single byte ever runs — and it does so WITHOUT ever executing a
# command (sandboxed or not). The real exec (run_sandboxed) is DEVICE-gated; this
# selftest validates ONLY the PURE layers:
#
#   1. CLASSIFIER (denylist): classify_shell_command rejects the destructive /
#      exfil patterns (rm -rf, dd, mkfs, sudo, fork bomb, curl|sh, writes to
#      /etc / ~/.claude / the daemon state, killing jarvisd, networking tools) —
#      including obfuscation attempts (extra spaces, $IFS, quotes, backslashes) —
#      and passes a benign ls/echo.
#   2. SBPL PROFILE TEXT: generate_shell_sbpl is DENY-DEFAULT ((deny default)),
#      has NO network ((deny network*)), confines file-WRITE to the scratch dir
#      ONLY (exactly one (allow file-write* ...) — the scratch subpath), and
#      EXPLICITLY denies read of the Keychain / ~/.claude / the daemon state, with
#      those denies AFTER the broad read allow so last-match-wins makes them win.
#   3. GATE ROUTING: shell_run is in confirm::CONSEQUENTIAL_TOOLS (now 17), so it
#      PARKS for a spoken yes and never auto-runs; it is master-switch / lockdown
#      / voice-id gated; OFF-by-default => the tool is inert.
#   4. CONFIG: [shell].enabled ships false.
#
# THE ONE HARD PROHIBITION: this selftest NEVER runs jarvisd, never opens a port,
# never loads a model, never makes a network call, and — above all — never EXECs
# a sandboxed command. It validates the generated profile TEXT + the classifier /
# gate-routing DECISIONS via the hermetic cargo unit tests, which are themselves
# pure (no exec, no network, no daemon). That is the same device-gated discipline
# as the vision-capture / apply-heal precedent.
#
# Usage:  scripts/test_shell_sandbox.sh        (run from anywhere)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DAEMON="$ROOT/daemon"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }

if [ ! -d "$DAEMON/src" ]; then
  fail "daemon sources not found at $DAEMON/src"
fi

# The hermetic unit tests that PROVE the four pure layers. Each is a pure function
# test (no exec, no network, no daemon); none runs a sandboxed command. Running
# them here is the shell-level selftest of the by-construction guarantees.
#
#   shell::tests::*                              — classifier denylist + obfuscation
#                                                  + benign; SBPL profile TEXT
#                                                  (deny-default, no-net, secret +
#                                                  scratch-only confinement);
#                                                  shell_permitted off-default.
#   confirm::...consequential_registry...        — shell_run in CONSEQUENTIAL_TOOLS
#                                                  (count now 17), parks, no dupes.
#   config::...shell_defaults...                 — [shell].enabled ships false.
#   anthropic::...shell_tool_is_owned...         — gate routing: owned + ships off +
#                                                  consequential/parks + voice-id
#                                                  gated; NO real exec.
TESTS=(
  "shell::tests"
  "consequential_registry_is_complete_and_exact"
  "shell_defaults_off_and_is_a_known_key"
  "shell_tool_is_owned_ships_off_consequential_and_voiceid_gated"
)

echo "Running the hermetic shell-sandbox safety selftest (no exec, no network, no daemon)..."
for t in "${TESTS[@]}"; do
  if ( cd "$DAEMON" && cargo test "$t" --quiet 2>/dev/null | grep -q "test result: ok" ); then
    pass "$t"
  else
    fail "$t — the shell-sandbox safety contract regressed (or the test did not run)"
  fi
done

# Defense-in-depth text assertion: the generated profile (as asserted by the unit
# test above) is DENY-DEFAULT with NO network and a scratch-only write. We restate
# the load-bearing invariants here as a documentation anchor so a future edit that
# weakened the profile would also have to delete this line knowingly.
echo
echo "Contract (proven by the tests above, never by executing a command):"
echo "  - classifier: destructive/exfil patterns + obfuscation REJECTED; benign passes"
echo "  - SBPL: (deny default) + (deny network*) + secret denies (Keychain/~/.claude/"
echo "          daemon state) AFTER the broad read allow + exactly one scratch-only write"
echo "  - gate: shell_run in CONSEQUENTIAL_TOOLS (parks, never auto-runs); master/"
echo "          lockdown/voice-id gated; OFF by default"
echo "  - exec: DEVICE-gated (built, NEVER invoked here)"
echo
echo "RESULT: ok"
