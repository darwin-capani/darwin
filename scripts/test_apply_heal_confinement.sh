#!/bin/bash
# Hermetic regression test for the apply_heal.sh path-confinement defenses.
#
# WHAT IT GUARDS: a hand-tampered state/heal/proposals/<ts>/patch.diff whose
# `---`/`+++`/`Index:` header carries a `..`/absolute target so that, after
# /usr/bin/patch's lenient header de-indentation + `-p1` strip, the write lands
# OUTSIDE the staging tree (e.g. ROOT/daemon/src/victim.rs) — planting attacker
# Rust into the live daemon source that the next `cargo build --release` would
# compile in (daemon RCE).
#
# THREE successive incomplete pre-scan fixes proved you cannot win by
# re-deriving patch's header parser:
#   1. un-indented `..` headers (caught by a column-0 grep),
#   2. uniformly WHITESPACE-indented `..` headers (patch de-indents leading ws),
#   3. THE RESIDUAL: a single leading NON-whitespace char — `X--- a/src/..` —
#      which macOS patch (2.0-12u11-Apple) ALSO de-indents, evading both the
#      whitespace-indent rejection and the column-0 header scan -> zero headers
#      matched -> the confinement loop never ran -> ACCEPTED -> out-of-tree write.
#
# THE FIX UNDER TEST is BY CONSTRUCTION, not by out-parsing patch: apply_heal.sh
# now runs every /usr/bin/patch invocation under sandbox-exec with a
# DEFAULT-DENY SBPL profile that allows file-write* ONLY under the canonicalized
# patch cwd (the confinement dir). The kernel seatbelt physically DENIES any
# out-of-tree write regardless of how patch parses the header. The strengthened
# header pre-scan stays as a best-effort fast-fail tamper signal.
#
# This test is HERMETIC: it NEVER invokes darwind, the daemon, cargo, launchd,
# sqlite, or any network. It exercises (a) the script's strengthened pre-scan
# (sliced verbatim from the live script) and (b) the real `confined_patch`
# sandbox helper against a fake repo layout in a temp dir — proving each escape
# leaves the sibling victim byte-for-byte UNCHANGED, while the legit confined
# applies still succeed — and (c) the script's real `stage_crate`, against the
# repo's real daemon crate, proving the STAGED tree can compile its TESTS and not
# merely pass `cargo check`. Invoked by the script's own `--selftest` hook so the
# defense cannot silently regress.
#
# Run:  scripts/test_apply_heal_confinement.sh
# Exit: 0 = all cases pass, 1 = a case regressed.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/apply_heal.sh"
SANDBOX_EXEC="/usr/bin/sandbox-exec"
PATCH_BIN="/usr/bin/patch"
BSD_BASE_PROFILE="/System/Library/Sandbox/Profiles/bsd.sb"

PASS=0
FAIL=0
ok()   { echo "ok   - $1"; PASS=$((PASS + 1)); }
bad()  { echo "FAIL - $1"; FAIL=$((FAIL + 1)); }

# ---------------------------------------------------------------------------
# Part A: the strengthened header PRE-SCAN (fast-fail tamper signal).
# ---------------------------------------------------------------------------
# Slice the pre-scan + confinement loop verbatim out of the live script so the
# test can never drift from what the script actually runs. Anchor on the stable
# `# Path-confinement:` banner through the `done < <(grep ...)` that closes the
# loop. Anchoring on the banner — not on any one defensive line — means a
# REGRESSED gate is sliced and exercised too (it then fails the assertion).
run_gate() {
  local patch_file="$1"
  local gate
  gate="$(awk '
    /^# Path-confinement:/ { capture=1 }
    capture { print }
    /^done < <\(grep -E/ { if (capture) exit }
  ' "$SCRIPT")"
  if [ -z "$gate" ]; then
    echo "INTERNAL: could not slice the confinement gate out of apply_heal.sh" >&2
    return 2
  fi
  PATCH_FILE="$patch_file" bash -c '
    set -uo pipefail
    fail() { echo "REJECTED: $1"; exit 1; }
    '"$gate"'
    echo "ACCEPTED"
  '
}

# ---------------------------------------------------------------------------
# Part B: the real `confined_patch` SANDBOX helper (the load-bearing defense).
# ---------------------------------------------------------------------------
# Slice the helper body verbatim out of the live script (from its function
# header to its closing brace) and source it into a tiny shim so the test runs
# the EXACT sandbox profile the script builds — no re-implementation.
load_confined_patch() {
  awk '
    /^confined_patch\(\) \{/ { capture=1 }
    capture { print }
    capture && /^\}$/ { exit }
  ' "$SCRIPT"
}

# Run the sliced confined_patch against a hermetic repo. Returns patch's rc;
# prints nothing on stdout we rely on (we assert on the victim file instead).
# fail() is stubbed to just echo so a confinement-dir resolve error is visible.
sandbox_apply() {
  local confine="$1" patch_file="$2" extra="${3:-}"
  local helper
  helper="$(load_confined_patch)"
  CONFINE="$confine" PATCH_INPUT="$patch_file" EXTRA="$extra" \
  SANDBOX_EXEC="$SANDBOX_EXEC" PATCH_BIN="$PATCH_BIN" BSD_BASE_PROFILE="$BSD_BASE_PROFILE" \
  bash -c '
    set -uo pipefail
    fail() { echo "FAIL_HELPER: $1" >&2; exit 99; }
    '"$helper"'
    if [ "${EXTRA}" = "dry" ]; then
      confined_patch "$CONFINE" -p1 --batch --dry-run < "$PATCH_INPUT"
    else
      confined_patch "$CONFINE" -p1 --batch < "$PATCH_INPUT"
    fi
  '
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Build a faithful fake repo for a given test name and return its parts via
# globals: REPO, STAGING (the patch cwd, == the staged CRATE dir
# ROOT/state/heal/apply-staging-123/daemon), VICTIM (the OUT-OF-TREE sibling
# target ROOT/daemon/src/victim.rs). STAGING/src is the dir -p1 paths resolve
# against; victim.rs is a SIBLING of the staging tree, reachable only via a
# `..`-escape.
#
# The crate sits one level BELOW the staging root because the staging root is a
# miniature repo root (see stage_crate in apply_heal.sh) — that extra level is
# why the escape headers below need FIVE `..`, not four.
make_repo() {
  local name="$1"
  REPO="$WORK/$name"
  STAGING="$REPO/state/heal/apply-staging-123/daemon"
  VICTIM="$REPO/daemon/src/victim.rs"
  rm -rf "$REPO"
  mkdir -p "$STAGING/src" "$REPO/daemon/src"
  printf 'ORIGINAL DAEMON FILE\nkeep\n' > "$VICTIM"
  printf 'placeholder\n' > "$STAGING/src/lib.rs"
}

# A malicious diff body whose `---`/`+++` target escapes the staging tree to the
# sibling victim. $1 = leading-prefix string prepended to EVERY line (the
# de-indent tamper: '' = column-0, 'X' = the residual, $'\t' = tab, 'ZZ' =
# multi-char). The target after -p1 + de-indent resolves to ROOT/daemon/src/...
# Relative to STAGING (the patch cwd = the staged CRATE dir):
#   src/../../../../../daemon/src/victim.rs ==
#   src/..(=CRATE) /..(=apply-staging-123) /..(=heal) /..(=state) /..(=ROOT)
#   /daemon/src/victim.rs.
write_escape_diff() {
  local file="$1" prefix="$2" header_kind="${3:-dashes}"
  local h1 h2
  case "$header_kind" in
    dashes) h1='--- a/src/../../../../../daemon/src/victim.rs'
            h2='+++ a/src/../../../../../daemon/src/victim.rs' ;;
    index)  h1='Index: src/../../../../../daemon/src/victim.rs'
            h2='+++ a/src/../../../../../daemon/src/victim.rs' ;;
  esac
  {
    printf '%s%s\n' "$prefix" "$h1"
    printf '%s%s\n' "$prefix" "$h2"
    printf '%s%s\n' "$prefix" '@@ -1,2 +1,2 @@'
    printf '%s%s\n' "$prefix" '-ORIGINAL DAEMON FILE'
    printf '%s%s\n' "$prefix" '+INJECTED_OUT_OF_TREE'
    printf '%s%s\n' "$prefix" ' keep'
  } > "$file"
}

# Prove the OLD (raw, unsandboxed) behavior WOULD escape for this diff: run a
# raw /usr/bin/patch (no sandbox) in a throwaway clone and confirm the victim
# IS clobbered. This makes each case a true regression proof — the escape is
# real, and the sandbox is what closes it.
prove_old_escapes() {
  local diff="$1"
  local clone="$WORK/oldproof-$RANDOM"
  local cstg="$clone/state/heal/apply-staging-123/daemon"
  local cvic="$clone/daemon/src/victim.rs"
  mkdir -p "$cstg/src" "$clone/daemon/src"
  printf 'ORIGINAL DAEMON FILE\nkeep\n' > "$cvic"
  printf 'placeholder\n' > "$cstg/src/lib.rs"
  ( cd "$cstg" && "$PATCH_BIN" -p1 --batch < "$diff" ) >/dev/null 2>&1
  if grep -q INJECTED_OUT_OF_TREE "$cvic" 2>/dev/null; then
    rm -rf "$clone"; return 0   # old behavior escaped (as expected)
  fi
  rm -rf "$clone"; return 1      # old behavior did NOT escape -> not a real residual
}

# A full malicious case: prove the escape-class behaves as claimed under RAW
# patch, then prove the sandbox helper leaves the out-of-tree victim byte-for-
# byte UNCHANGED. $label, $prefix, $header_kind, $expect_prescan, $escape_class.
#   escape_class=escapable -> raw /usr/bin/patch DOES write out-of-tree (a real
#                             residual the sandbox must close).
#   escape_class=benign    -> raw /usr/bin/patch already FAILS to apply this
#                             tamper (e.g. a 2+ char prefix patch won't de-indent
#                             so no header is found); the sandbox + pre-scan must
#                             still reject/deny it, but we do NOT pretend it was
#                             a raw escape. Honest about which classes are real.
malicious_case() {
  local label="$1" prefix="$2" header_kind="${3:-dashes}" expect_prescan="${4:-reject}" escape_class="${5:-escapable}"
  make_repo "case_${label// /_}"
  local diff="$REPO/patch.diff"
  write_escape_diff "$diff" "$prefix" "$header_kind"

  # 1. Regression proof on RAW patch, per the claimed escape class.
  if [ "$escape_class" = "escapable" ]; then
    if ! prove_old_escapes "$diff"; then
      bad "$label: OLD raw patch did NOT escape — case is not a faithful residual"
      return
    fi
  else
    if prove_old_escapes "$diff"; then
      bad "$label: claimed benign but RAW patch DID escape — reclassify as escapable"
      return
    fi
    ok "$label: RAW patch rejects this tamper (no de-indent -> no header -> no escape)"
  fi

  # 2. The load-bearing sandbox: applying under confined_patch must NOT clobber
  #    the out-of-tree victim. Confinement dir = STAGING (the real patch cwd).
  local before after rc
  before="$(cat "$VICTIM")"
  sandbox_apply "$STAGING" "$diff" >/dev/null 2>&1
  rc=$?
  after="$(cat "$VICTIM")"
  if [ "$before" = "$after" ]; then
    if [ "$escape_class" = "escapable" ]; then
      ok "$label: sandbox DENIED the out-of-tree write (victim unchanged, was escapable raw, rc=$rc)"
    else
      ok "$label: out-of-tree victim still unchanged under the sandbox (rc=$rc)"
    fi
  else
    bad "$label: out-of-tree victim.rs WAS modified under the sandbox -> escape reachable"
  fi

  # 3. Best-effort pre-scan fast-fail (where applicable).
  if [ "$expect_prescan" = "reject" ]; then
    local out; out="$(run_gate "$diff")"
    case "$out" in
      REJECTED:*) ok "$label: pre-scan also fast-fails this tamper [$out]" ;;
      *)          bad "$label: pre-scan did NOT fast-fail [$out] (sandbox still caught it, but the signal should fire)" ;;
    esac
  fi
}

echo "== malicious cases: escape DENIED under the sandbox; victim must stay intact =="

# Case 1: THE RESIDUAL — single leading non-whitespace 'X' prefix. macOS patch
#         de-indents the lone 'X' -> a real out-of-tree escape under raw patch.
malicious_case "X-prefix '..' header (the residual)" "X" "dashes" "reject" "escapable"

# Case 2: leading TAB prefix (whitespace de-indent class) — raw patch de-indents
#         the tab and escapes.
malicious_case "leading-tab '..' header" "$(printf '\t')" "dashes" "reject" "escapable"

# Case 3: multi-char non-whitespace prefix. macOS patch de-indents only a SINGLE
#         leading non-ws char, so 'ZZ--- ' is NOT recognized as a header -> raw
#         patch already fails (no escape). The pre-scan + sandbox still reject it,
#         but we are honest that this class is not a raw residual.
malicious_case "multi-char-prefix '..' header" "ZZ" "dashes" "reject" "benign"

# Case 4: Index: header carrying '..' (single-space indent, whitespace class).
malicious_case "Index: '..' header" " " "index" "reject" "escapable"

# Case 5: column-0 '..' header (the always-caught variant).
malicious_case "column-0 '..' header" "" "dashes" "reject" "escapable"

# Case 6: uniformly whitespace-indented (one space) '..' header — the prior
#         residual that motivated the de-indent defense (raw patch de-indents).
malicious_case "single-space-indented '..' header" " " "dashes" "reject" "escapable"

# Case 7: MIXED whitespace+single-non-ws-char prefix (' X--- ...'). macOS patch
#         de-indents leading whitespace AND one non-ws char, so this is a REAL
#         raw escape — and it slips BOTH the whitespace-indent scan (line starts
#         with ws but not '---' right after) AND the non-ws-first prefix scan
#         (line starts with ws, not a non-ws char). The strengthened pre-scan
#         rule (d) now fast-fails it; the sandbox already denied it. This is the
#         variant the final re-attack surfaced as a pre-scan gap.
malicious_case "mixed ws+char ' X' '..' header" " X" "dashes" "reject" "escapable"

# Case 8: mixed TAB+char prefix ('\tX--- ...') — same de-indent class, tab form.
malicious_case "mixed tab+char '..' header" "$(printf '\t')X" "dashes" "reject" "escapable"

echo
echo "== legit cases: confined applies must still SUCCEED under the sandbox =="

# Legit 1: a confined a/src diff applies and writes inside the staging tree.
make_repo "legit_confined"
GOOD="$REPO/good.diff"
printf '%s\n' \
  '--- a/src/lib.rs' \
  '+++ b/src/lib.rs' \
  '@@ -1,1 +1,1 @@' \
  '-placeholder' \
  '+patched_ok' > "$GOOD"
# pre-scan must accept it
out="$(run_gate "$GOOD")"
[ "$out" = "ACCEPTED" ] || bad "legit confined diff wrongly pre-scan-rejected [$out]"
# dry-run under sandbox succeeds, writes nothing
sandbox_apply "$STAGING" "$GOOD" dry >/dev/null 2>&1
drc=$?
if [ "$drc" -eq 0 ] && [ "$(cat "$STAGING/src/lib.rs")" = "placeholder" ]; then
  ok "legit confined diff: sandbox dry-run succeeds and writes nothing"
else
  bad "legit confined diff: sandbox dry-run failed (rc=$drc) or wrote early"
fi
# real apply under sandbox writes the patched file inside the staging tree
sandbox_apply "$STAGING" "$GOOD" >/dev/null 2>&1
rrc=$?
if [ "$rrc" -eq 0 ] && [ "$(cat "$STAGING/src/lib.rs")" = "patched_ok" ]; then
  ok "legit confined diff: sandbox real apply writes inside the staging tree"
else
  bad "legit confined diff: sandbox real apply failed (rc=$rrc) or did not patch"
fi

# Legit 2: a /dev/null new-file diff is accepted by the pre-scan and applies
#          inside the staging tree under the sandbox.
make_repo "legit_newfile"
NEWF="$REPO/newfile.diff"
printf '%s\n' \
  '--- /dev/null' \
  '+++ b/src/new.rs' \
  '@@ -0,0 +1,1 @@' \
  '+hello' > "$NEWF"
out="$(run_gate "$NEWF")"
[ "$out" = "ACCEPTED" ] || bad "legit /dev/null new-file diff wrongly pre-scan-rejected [$out]"
sandbox_apply "$STAGING" "$NEWF" >/dev/null 2>&1
nrc=$?
if [ "$nrc" -eq 0 ] && [ -f "$STAGING/src/new.rs" ] && [ "$(cat "$STAGING/src/new.rs")" = "hello" ]; then
  ok "legit /dev/null new-file diff: sandbox real apply creates the file in-tree"
else
  bad "legit /dev/null new-file diff: sandbox apply failed (rc=$nrc) or did not create file"
fi

# Legit 3: a unified-diff CONTENT line that legitimately starts with '+'/'-'
#          must NOT be false-rejected by the strengthened leading-prefix scan
#          (the `+something` / `-something` content lines look superficially like
#          a prefixed header but are not `--- `/`+++ ` header tokens).
make_repo "legit_content"
CONT="$REPO/content.diff"
printf '%s\n' \
  '--- a/src/lib.rs' \
  '+++ b/src/lib.rs' \
  '@@ -1,3 +1,3 @@' \
  '-placeholder' \
  '+++added marker line' \
  '---removed marker line' \
  ' tail' > "$CONT"
out="$(run_gate "$CONT")"
case "$out" in
  ACCEPTED) ok "content lines starting with '+'/'-'/'+++'/'---' are NOT false-rejected by the prefix scan" ;;
  *)        bad "legit content lines were false-rejected by the strengthened pre-scan [$out]" ;;
esac

echo
echo "== staging completeness: the STAGED crate must be able to compile its TESTS =="

# ---------------------------------------------------------------------------
# Part C: the staging COPY (the build gates' input).
# ---------------------------------------------------------------------------
# WHAT WENT WRONG: staging copied exactly three things — src/, Cargo.toml and
# Cargo.lock — into a bare dir. `cargo check` neither links nor reads the inputs
# of `#[cfg(test)]` macros, so gate 1 passed and gate 2 could not even COMPILE:
#   error: couldn't read `src/../../config/darwin.toml`: No such file or directory
# Every apply of every proposal died at `cargo test failed in staging` — the
# whole propose->apply path was dead, and the operator was told the patch failed
# the test gate. Parts A and B never touch the staging copy or the build gates,
# so nothing in this suite could catch it.
#
# Still HERMETIC: no cargo, no daemon, no network. We run the script's REAL
# `stage_crate` (sliced verbatim, so the test cannot drift from the script)
# against the repo's real daemon crate into a temp dir, then assert the two
# properties `cargo test` needs and `cargo check` does not:
#   1. every out-of-crate include_str!/include_bytes! target RESOLVES from the
#      STAGED source file's own directory — that is the compiler's rule, and
#   2. the native link inputs (build.rs + csrc/) came along.
# Both are derived from the real crate, so a new out-of-crate include is covered
# the day it lands rather than when someone remembers to update a list here.

# Slice a top-level function verbatim out of the live script, by name.
load_fn() {
  local fn="$1"
  awk -v want="$fn() {" '
    index($0, want) == 1 { capture = 1 }
    capture { print }
    capture && /^\}$/ { exit }
  ' "$SCRIPT"
}

# Enumerate the `include_str!`/`include_bytes!` literals in one .rs file.
include_literals() {
  grep -oE 'include_(str|bytes)!\("[^"]*"' "$1" 2>/dev/null \
    | sed -E 's/^include_(str|bytes)!\("//; s/"$//'
}

REPO_ROOT="$(cd "$HERE/.." && pwd -P)"
REAL_CRATE="$REPO_ROOT/daemon"
STAGE_ROOT="$WORK/stagecheck"

STAGE_FNS="$(load_fn stage_crate)"$'\n'"$(load_fn mirror_out_of_crate_includes)"$'\n'"$(load_fn mirror_runtime_test_inputs)"
case "$STAGE_FNS" in
  *"stage_crate() {"*"mirror_out_of_crate_includes() {"*"mirror_runtime_test_inputs() {"*) ;;
  *) bad "could not slice stage_crate + its mirror helpers out of apply_heal.sh" ;;
esac

# The slice above is BY NAME, so adding a helper to stage_crate without adding it
# here leaves the sliced copy calling an undefined function: `stage_crate` then
# dies, returns '', and the staging-completeness check below fails for a reason
# that has nothing to do with staging. That is exactly what happened when
# mirror_runtime_test_inputs was introduced. So: verify the slice is CLOSED —
# every apply_heal.sh function the slice calls must itself be in the slice.
DEFINED_ALL="$(grep -oE '^[a-z_][a-z0-9_]*\(\) \{' "$SCRIPT" | sed 's/() {//')"
SLICE_MISSING=""
for fn in $DEFINED_ALL; do
  case "$STAGE_FNS" in
    *"$fn() {"*) continue ;;                      # already in the slice
  esac
  # Called by the slice (as a command word) but not defined in it?
  if printf '%s\n' "$STAGE_FNS" | grep -qE "(^|[;&|(]|then |else |do )[[:space:]]*$fn([[:space:]]|$)"; then
    SLICE_MISSING="$SLICE_MISSING $fn"
  fi
done
if [ -n "$SLICE_MISSING" ]; then
  bad "the staging slice calls apply_heal.sh helpers it does not include:$SLICE_MISSING (add load_fn for each)"
else
  ok "the staging function slice is closed — every helper it calls is included"
fi

if [ ! -d "$REAL_CRATE/src" ]; then
  bad "staging completeness: no daemon crate at $REAL_CRATE (cannot verify the staging copy)"
elif [ -z "$STAGE_FNS" ]; then
  : # already reported by the slice guard above
else
  CRATE_OUT="$(DAEMON_DIR="$REAL_CRATE" STAGE="$STAGE_ROOT" bash -c '
    set -euo pipefail
    '"$STAGE_FNS"'
    stage_crate "$DAEMON_DIR" "$STAGE"
  ' 2>/dev/null)" || CRATE_OUT=""

  if [ "$CRATE_OUT" = "$STAGE_ROOT/daemon" ]; then
    ok "stage_crate stages the crate one level down ($STAGE_ROOT/daemon) and returns that CRATE dir"
  else
    bad "stage_crate returned '$CRATE_OUT', expected '$STAGE_ROOT/daemon' (cargo/patch would run in the wrong dir)"
  fi

  if [ -n "$CRATE_OUT" ] && [ -d "$CRATE_OUT/src" ]; then
    # (1) THE COMPILER'S RULE, applied independently of how stage_crate works:
    #     resolve each out-of-crate literal from the STAGED file's own directory.
    inc_checked=0
    inc_missing=""
    while IFS= read -r rs; do
      rel_rs="${rs#"$REAL_CRATE"/}"
      while IFS= read -r lit; do
        case "$lit" in ../*) ;; *) continue ;; esac
        # Only literals naming a file that REALLY exists are compilation inputs:
        # a mention inside a comment is not, and a genuinely absent include is
        # the compiler's to report.
        real_dir="$(cd "$(dirname "$rs")" && cd "$(dirname "$lit")" 2>/dev/null && pwd -P)" || continue
        [ -f "$real_dir/$(basename "$lit")" ] || continue
        inc_checked=$((inc_checked + 1))
        staged_rs="$CRATE_OUT/$rel_rs"
        staged_dir="$(cd "$(dirname "$staged_rs")" && cd "$(dirname "$lit")" 2>/dev/null && pwd -P)" || staged_dir=""
        if [ -z "$staged_dir" ] || [ ! -f "$staged_dir/$(basename "$lit")" ]; then
          inc_missing="$inc_missing $rel_rs -> $lit"
        fi
      done < <(include_literals "$rs")
    done < <(find "$REAL_CRATE/src" -type f -name '*.rs')

    if [ "$inc_checked" -eq 0 ]; then
      # Never report a pass on an empty set — that is how a vacuous assertion
      # looks exactly like a real one.
      bad "staging completeness: found ZERO out-of-crate includes to check — the enumeration broke, not the crate"
    elif [ -n "$inc_missing" ]; then
      bad "staged tree cannot compile its tests: unresolvable out-of-crate include(s):$inc_missing"
    else
      ok "all $inc_checked out-of-crate include_str!/include_bytes! targets resolve from the STAGED sources"
    fi

    # (2) The native link inputs cargo test needs and cargo check does not.
    link_missing=""
    for want in build.rs csrc; do
      if [ -e "$REAL_CRATE/$want" ] && [ ! -e "$CRATE_OUT/$want" ]; then
        link_missing="$link_missing $want"
      fi
    done
    if [ -n "$link_missing" ]; then
      bad "staged tree is missing the crate's native link input(s):$link_missing (cargo check does not link; cargo test does)"
    else
      ok "the crate's native link inputs (build.rs, csrc/) are staged"
    fi

    # (3) target/ is gigabytes of build output and is never a build input.
    if [ -e "$CRATE_OUT/target" ]; then
      bad "staging copied daemon/target/ (gigabytes of build output)"
    else
      ok "staging skipped daemon/target/"
    fi
  fi
  rm -rf "$STAGE_ROOT"
fi

echo
echo "== staged-flag guards: the ARGV COMPARISON, not the prose =="

# ---------------------------------------------------------------------------
# Part D: the staged-flag guards (fail-closed, and they must not clear on a
# COMMENT).
# ---------------------------------------------------------------------------
# WHAT WENT WRONG: in this codebase an unknown flag is NOT an error.
# `std::env::args().position(|a| a == "--flag")` simply does not match and
# darwind falls through to ORDINARY DAEMON STARTUP — so a script that invokes a
# flag a staged daemon does not implement BOOTS A DAEMON instead of getting an
# answer, and its gate is SKIPPED rather than enforced. apply_heal.sh therefore
# confirms each flag against the staged source first.
#
# Those guards used to be `grep -q -- '--heal-confidence' "$CRATE/src/main.rs"`,
# and main.rs discusses every one of these flags in the comment block above its
# handler. So on a staged source whose dispatch literal had drifted or been
# renamed, the grep matched the PROSE, the script cleared its own fail-closed
# guard, and it invoked the flag anyway. (The daemon-side test that pins these
# greps had already been hardened against exactly this; the SCRIPT had not.)
#
# Still HERMETIC: no cargo, no daemon, no network. We run the script's REAL
# guard commands — sliced verbatim, so the test cannot drift from what the
# script runs — against two fake $CRATE trees built from the repo's OWN
# daemon/src/main.rs:
#   LEGIT   the real file. A guard that refuses everything is not a fix, so the
#           accept case is checked against the source the script will really see.
#   DRIFTED the same file with every `a == "--flag"` argv literal renamed and
#           ALL PROSE UNTOUCHED. This is the tree that must be refused.
# Deriving both from the real main.rs (rather than a synthetic stub) means a
# fourth flag guard is covered the day it lands.

FLAG_GUARDS="$(sed -nE 's/^[[:space:]]*if ! (grep .*"\$CRATE\/src\/main\.rs");[[:space:]]*then$/\1/p' "$SCRIPT")"
GUARD_N="$(printf '%s\n' "$FLAG_GUARDS" | grep -c 'grep' || true)"

# A FOR-ALL OVER NOTHING IS NOT A CHECK: if the slice stops binding, every case
# below would "pass" having run no guard at all.
if [ "${GUARD_N:-0}" -lt 3 ]; then
  bad "sliced $GUARD_N staged-flag guards out of apply_heal.sh, expected at least 3 — the slice no longer binds and Part D would test nothing"
else
  ok "sliced $GUARD_N staged-flag guards verbatim out of apply_heal.sh"

  FAKE="$WORK/flagguard"
  rm -rf "$FAKE"
  mkdir -p "$FAKE/legit/src" "$FAKE/drift/src"
  REAL_MAIN="$REPO_ROOT/daemon/src/main.rs"
  if [ ! -f "$REAL_MAIN" ]; then
    bad "no daemon/src/main.rs to build the staged-flag fixtures from"
  else
    cp "$REAL_MAIN" "$FAKE/legit/src/main.rs"
    # Rename every argv literal; touch no prose. The suffix keeps the flag NAME
    # present everywhere it was, which is the whole point of the fixture.
    sed -E 's/(a == ")(--[a-z-]+)(")/\1\2-renamed\3/g' "$REAL_MAIN" > "$FAKE/drift/src/main.rs"
    if cmp -s "$FAKE/legit/src/main.rs" "$FAKE/drift/src/main.rs"; then
      bad "the drifted fixture is byte-identical to main.rs — the rename matched nothing and the refuse case below would be vacuous"
    else
      ok "the drifted fixture renames main.rs's argv literals (prose untouched)"
    fi

    while IFS= read -r guard; do
      [ -n "$guard" ] || continue
      # Read the flag out of the guard's QUOTED FLAG TOKEN, not out of `a == "..."`.
      # Keying on the hardened form would make a guard REGRESSED to the prose form
      # unparseable, and Part D would report a parse failure instead of running it
      # — the regression would be caught, but for the wrong reason and without
      # ever demonstrating that the old form ACCEPTS a drifted tree. This way the
      # regressed guard is executed and fails on what it actually does.
      flag="$(printf '%s\n' "$guard" | sed -E "s/.*[\"'](--[a-z][a-z-]*)[\"'].*/\1/")"
      case "$flag" in --*) ;; *) bad "could not read a flag name out of the guard: $guard"; continue ;; esac

      # PRECONDITION. The drifted tree must still SAY the flag everywhere main.rs
      # said it in prose — otherwise a refusal below proves only that the name
      # vanished, not that the guard reads the dispatch.
      if grep -q -- "$flag" "$FAKE/drift/src/main.rs"; then
        ok "$flag: the drifted fixture still names the flag in prose (the refusal below is not vacuous)"
      else
        bad "$flag: the drifted fixture lost the flag name entirely — this case cannot show prose-matching"
      fi

      if CRATE="$FAKE/legit" bash -c "$guard" 2>/dev/null; then
        ok "$flag: the guard ACCEPTS the real main.rs (a guard that refuses everything is not a fix)"
      else
        bad "$flag: the guard REFUSES the repo's own main.rs — every apply would fail forever"
      fi

      if CRATE="$FAKE/drift" bash -c "$guard" 2>/dev/null; then
        bad "$flag: the guard ACCEPTS a staged source that mentions the flag only in PROSE — it clears itself on a comment and would then invoke a flag the daemon does not implement (unknown flags boot the daemon, they do not error)"
      else
        ok "$flag: the guard REFUSES a staged source that names the flag only in PROSE"
      fi

      # ...AND ONE LEVEL DOWN. Anchoring on `a == "--flag"` without excluding
      # comment lines just moves the hole: a `//` line QUOTING the argv form
      # clears the guard on a source whose dispatch literal has drifted. Same
      # drifted tree, plus a comment that spells the comparison.
      mkdir -p "$FAKE/cmt/src"
      { printf '// the dispatch reads `a == "%s"`; see apply_heal.sh\n' "$flag"
        cat "$FAKE/drift/src/main.rs"; } > "$FAKE/cmt/src/main.rs"
      if CRATE="$FAKE/cmt" bash -c "$guard" 2>/dev/null; then
        bad "$flag: the guard ACCEPTS a drifted source whose only argv-form match is inside a // COMMENT — it must exclude comment lines"
      else
        ok "$flag: the guard REFUSES an argv-form match that lives in a // COMMENT"
      fi

      # ...AND THE DIVERGENCE THE GUARD IS DOCUMENTED TO HAVE, MADE EXECUTABLE.
      # The regex spends one character on [^/[:space:]] before `.*`, so a line
      # whose ONLY `a == "--flag"` starts at the line's first non-blank column
      # never matches — at column 0 and, the realistic case, INDENTED. rustfmt
      # emits exactly that from a block-bodied closure. heal.rs's
      # `!starts_with("//") && contains(...)` rule ACCEPTS such a line, so the
      # two gates disagree there; the disagreement is fail-closed (this refuses)
      # and is written up above the responsiveness guard in apply_heal.sh, where
      # the COST of a refusal differs per guard. Pin it, so widening the regex to
      # reach parity forces whoever does it back through that note.
      mkdir -p "$FAKE/bare/src"
      { printf '    if let Some(pos) = std::env::args().position(|a| {\n'
        printf '        a == "%s"\n' "$flag"
        printf '    }) {\n'
        cat "$FAKE/drift/src/main.rs"; } > "$FAKE/bare/src/main.rs"
      # PRECONDITION: the fixture really carries the bare INDENTED comparison —
      # the exact shape heal.rs's code-line rule accepts. Without this the pin
      # below would also "pass" on a fixture that has no argv form at all.
      if grep -qE '^[[:space:]]+a == "'"$flag"'"$' "$FAKE/bare/src/main.rs"; then
        ok "$flag: the bare-comparison fixture carries an indented, comment-free argv comparison (the shape heal.rs accepts)"
      else
        bad "$flag: the bare-comparison fixture does not carry that line — the divergence pin below would be vacuous"
      fi
      if CRATE="$FAKE/bare" bash -c "$guard" 2>/dev/null; then
        bad "$flag: the guard ACCEPTS a bare indented argv comparison. That is PARITY with heal.rs rather than a hole, but apply_heal.sh documents this shape as REFUSED and spells out what a refusal costs at each guard — move the note and this pin together"
      else
        ok "$flag: the guard REFUSES a bare indented argv comparison (documented fail-closed divergence from heal.rs's code-line rule)"
      fi
    done <<EOF
$FLAG_GUARDS
EOF
  fi
  rm -rf "$FAKE"
fi

echo
echo "apply_heal confinement: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
