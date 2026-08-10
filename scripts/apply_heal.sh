#!/bin/bash
# Apply a validated self-heal proposal to the live daemon source tree.
#
# Usage:
#   scripts/apply_heal.sh <ts>          interactive (asks read -r confirmation)
#   scripts/apply_heal.sh <ts> --yes    non-interactive (for the HUD Accept button)
#
#   <ts> is the unix-timestamp directory under state/heal/proposals/ that the
#   heal pipeline announced (heal.proposal telemetry / the first-contact
#   brief / report.md).
#
# The proposal was already validated in a staging copy (patch applied,
# cargo check + cargo test green) when it was DRAFTED. Applying for real is a
# privileged mutation of the daemon, so this script RE-VALIDATES from scratch
# before it ever touches daemon/src:
#   - verify state/heal/proposals/<ts>/ exists,
#   - stage a FRESH copy of the WHOLE daemon crate (everything but target/ and
#     dotfiles) plus every repo-root sibling the sources reach into with an
#     out-of-crate include_str!, as a miniature repo root under
#     state/heal/apply-staging-<ts>/ — the crate lands at .../<ts>/daemon/,
#   - apply patch.diff with /usr/bin/patch -p1 --batch (dry-run, then real),
#   - in the staged CRATE dir: cargo check, cargo clippy --all-targets -D warnings,
#     cargo test, and then the MUTATION PROBE — the patch is split into its fix
#     side and its test side (by `darwind --split-heal-diff`, the daemon's own
#     code, so the two gates cannot drift), the fix is reverse-applied, and the
#     suite must then FAIL. A test that passes without its fix proves nothing
#     about the fix, and the apply is refused,
#   - print the RESPONSIVENESS verdict (`darwind --heal-responsiveness`, the
#     daemon's own function again): whether the patch actually addresses the
#     diagnosis that triggered the heal. Every gate above proves the patch is
#     SOUND; none of them proves it is an ANSWER. This one is ADVISORY and never
#     refuses — a correct fix often lives one layer up from the line that
#     screamed,
#   - enforce the REVIEW-CONFIDENCE FLOOR (`darwind --heal-confidence`, the
#     daemon's own function a third time). The daemon refuses to PROPOSE a patch
#     the adversarial reviewer scored below the review-confidence floor; this refuses
#     to INSTALL one, so an older or hand-edited proposal cannot be applied under
#     a weaker bar than the one that would have blocked it. This one DOES refuse
#     — it is the reviewer's explicit verdict on this patch, not a heuristic
#     about where a fix ought to live,
#   - and ONLY on green apply the same patch to the real daemon/, rebuild the
#     release binary, and clear the meta.heal_pending marker.
# Any gate failure exits non-zero and leaves daemon/src untouched.
#
# --yes skips ONLY the read -r prompt: the GUI's two-step confirm replaces the
# human keystroke. Every gate above still runs. There is no flag that weakens
# the re-validation — that gate is non-negotiable.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROPOSALS="$ROOT/state/heal/proposals"
DAEMON="$ROOT/daemon"
HEAL_ROOT="$ROOT/state/heal"

# Structured progress for the HUD. Stages: revalidating | applying | rebuilding.
# Terminal line is always exactly one RESULT: ok | RESULT: failed <reason>.
stage() { echo "STAGE: $1"; }
result_ok() { echo "RESULT: ok"; }
# Emit the terminal failure line and exit non-zero. daemon/src is NOT modified
# by any path that calls this before the "applying" stage.
fail() {
  echo "RESULT: failed $1" >&2
  exit 1
}

# --selftest runs the hermetic confinement regression (no daemon / no network /
# no live tree touched) and exits. It guards the de-indentation defense below so
# a future edit cannot silently reopen the out-of-tree-write hole.
if [ "${1:-}" = "--selftest" ]; then
  exec "$(dirname "${BASH_SOURCE[0]}")/test_apply_heal_confinement.sh"
fi

SANDBOX_EXEC="/usr/bin/sandbox-exec"
PATCH_BIN="/usr/bin/patch"
BSD_BASE_PROFILE="/System/Library/Sandbox/Profiles/bsd.sb"

# ---------------------------------------------------------- confined patch
# THE LOAD-BEARING DEFENSE. Run /usr/bin/patch under sandbox-exec with a
# DEFAULT-DENY SBPL profile that allows file-write* ONLY under a single
# canonicalized confinement dir (the patch cwd). The kernel seatbelt then
# physically DENIES any write patch attempts outside that dir — so a tampered
# `..`/Index:/de-indented header CANNOT write out-of-tree, no matter how
# leniently /usr/bin/patch parses the header (this is why the fix is "by
# construction", not by re-deriving patch's header parser, which the two prior
# incomplete fixes tried).
#
# Mechanism is the SAME sandbox-exec/SBPL the daemon's micro-app runtime uses
# (daemon/src/apps.rs): `(version 1)` + `(deny default)` + import Apple's bsd.sb
# base (the syscalls/dyld reads every process needs to boot) + scoped allows.
# We allow file-read* broadly (patch reads the staging files + the patch on
# stdin) and process-fork/exec (patch may fork), but file-write* is confined.
#
# patch writes a TEMP/WORKING file (mkstemp) before renaming it onto the target;
# by default that lives under $TMPDIR (/var/folders/...), which is OUTSIDE the
# confinement dir and would be denied. We redirect patch's TMPDIR into a
# .heal-sandbox-tmp/ dir INSIDE the confinement dir, so the ONLY writable
# location is the confinement subtree — the tempfile is allowed, every
# out-of-tree path (including a `..`-escaped victim) is denied.
#
# Usage: confined_patch <confine_dir> [patch args...]   (patch reads stdin)
# Honors the same -p1 --batch [--dry-run] semantics the callers pass.
confined_patch() {
  local confine_raw="$1"; shift
  # Canonicalize: absolute, symlinks resolved, no trailing slash. A trailing
  # slash or a symlinked parent must not let the subpath filter mismatch what
  # the kernel canonicalizes the write target to. `cd && pwd -P` resolves the
  # whole chain; the dir already exists (we created it just above the caller).
  local confine
  if ! confine="$(cd "$confine_raw" 2>/dev/null && pwd -P)"; then
    fail "confinement dir '$confine_raw' does not resolve — refusing to run patch unsandboxed"
  fi

  # patch's tempfile dir, inside the confinement subtree (so it is writable
  # under the profile without opening any out-of-tree path).
  local tmpd="$confine/.heal-sandbox-tmp"
  mkdir -p "$tmpd"

  # Build the deny-default-write profile. Default-deny everything, import the
  # BSD base so patch can even boot, allow reads + process basics, and allow
  # WRITE only under the canonicalized confinement dir.
  local profile
  profile="$(mktemp -t heal-confine-sbpl)"
  {
    echo "(version 1)"
    echo ";; Generated by apply_heal.sh to confine /usr/bin/patch writes to the"
    echo ";; staging/live tree only. DEFAULT-DENY; the only file-write* grant is"
    echo ";; the canonicalized patch cwd. A '..'/Index/de-indented header that"
    echo ";; resolves outside this subtree is DENIED by the kernel, not the"
    echo ";; pre-scan. Mirrors the micro-app SBPL in daemon/src/apps.rs."
    echo "(deny default)"
    if [ -f "$BSD_BASE_PROFILE" ]; then
      echo "(import \"$BSD_BASE_PROFILE\")"
    fi
    echo "(allow process-fork)"
    echo "(allow process-exec*)"
    # patch reads the staging files + the patch body on stdin; reads are not the
    # threat (the out-of-tree WRITE is), so file-read* is broad.
    echo "(allow file-read*)"
    # The single load-bearing grant: writes confined to the patch cwd subtree
    # (which contains the tempfile dir above). Everything else stays denied.
    echo "(allow file-write* (subpath \"$confine\"))"
  } > "$profile"

  local rc=0
  ( cd "$confine" && TMPDIR="$tmpd" "$SANDBOX_EXEC" -f "$profile" "$PATCH_BIN" "$@" ) || rc=$?
  rm -f "$profile"
  return "$rc"
}

# ------------------------------------------------------------- crate staging
# Mirror every file the crate's sources name with an `include_str!` /
# `include_bytes!` whose relative path climbs OUT of the crate into the staging
# ROOT at the same repo-relative path, so those macros resolve under staging
# exactly as they do under the real repo root.
#
# Existence-gated and best-effort by design: a literal that names no real file is
# skipped (a mention of the macro inside a COMMENT is not a compilation input,
# and a genuinely missing include is the compiler's to report, honestly), and so
# is anything that resolves outside the repo root — staging never reaches out of
# the tree it was asked to stage.
# Mirror the repo inputs the crate's TESTS read at RUNTIME.
#
# WHAT WENT WRONG: staging mirrored only the paths named by `include_str!` — a
# COMPILE-time scan. `cargo check` therefore passed and `cargo test` failed 29
# tests in staging that pass in the real tree, so this script always ended
#   RESULT: failed  cargo test failed in staging — live daemon/src NOT modified
# and exit 1. No self-heal patch could be applied by ANY path: interactive,
# --yes, or the HUD Accept button. The operator was told the patch failed the
# test gate when the harness could not run the suite at all.
#
# Staged and run, the failures named themselves:
#   cannot read <staging>/daemon/../config/agents.toml: No such file
#   app registered              (empty registry: no apps/*/manifest.toml)
#   apps/cronwise: tool-exposing app has no main.py
#
# The set is data-only and ~12 MB. This mirrors the same inputs as the daemon's
# own gate (daemon/src/heal.rs RUNTIME_TEST_INPUTS) — the two MUST agree, or a
# patch the daemon proved will fail here for a reason that is not the patch.
mirror_runtime_test_inputs() {
  local daemon_dir="$1" staging="$2" repo_root d
  repo_root="$(cd "$daemon_dir/.." && pwd -P)"
  # `docs` IS NOT OPTIONAL, and leaving it out is not a cosmetic gap: the suite
  # reads <crate>/../docs/SANDBOX.md with `.expect("docs/SANDBOX.md is present")`
  # (apps.rs, the_sandbox_doc_worked_example_names_an_app_whose_manifest_validates).
  # Absent, that test PANICS, the whole `cargo test` gate below fails, and this
  # script ends `RESULT: failed  cargo test failed in staging` — i.e. NO self-heal
  # patch is installable by ANY path (interactive, --yes, or the HUD Accept
  # button), and the operator is told the PATCH failed the test gate when the
  # harness could not run the suite at all. That is the exact defect this
  # function was written to close, recurring on the apply side after the daemon
  # side was fixed alone. THIS LIST AND heal.rs RUNTIME_TEST_INPUTS ARE PINNED
  # IN LOCKSTEP by heal::tests::the_apply_script_mirrors_the_same_runtime_inputs
  # _as_the_daemon_gate — add a directory to one and that test fails until it is
  # added here too.
  for d in config scripts docs; do
    [ -d "$repo_root/$d" ] || continue
    mkdir -p "$staging/$d"
    cp -R "$repo_root/$d/." "$staging/$d/"
  done
  # Apps: manifests, sibling .toml data, and each app's ENTRY file. The registry
  # needs the manifest; the manifest suite asserts a tool-exposing app HAS its
  # entry ("tool-exposing app has no main.py"). Nothing else from an app.
  if [ -d "$repo_root/apps" ]; then
    local f rel
    while IFS= read -r f; do
      rel="${f#"$repo_root"/}"
      mkdir -p "$staging/$(dirname "$rel")"
      cp "$f" "$staging/$rel"
    done < <(find "$repo_root/apps" -maxdepth 2 -type f \
               \( -name '*.toml' -o -name 'main.py' -o -name 'main.rs' -o -name 'main.swift' \) 2>/dev/null)
  fi
}

mirror_out_of_crate_includes() {
  local daemon_dir="$1" staging="$2"
  local crate_abs repo_root src_file lit lit_dir lit_base abs_dir abs rel
  crate_abs="$(cd "$daemon_dir" && pwd -P)"
  repo_root="$(cd "$daemon_dir/.." && pwd -P)"
  while IFS= read -r src_file; do
    while IFS= read -r lit; do
      # In-crate includes came along with the crate copy; only `../` climbs out.
      case "$lit" in ../*) ;; *) continue ;; esac
      lit_dir="$(dirname "$lit")"
      lit_base="$(basename "$lit")"
      # include_str! resolves relative to the SOURCE FILE, so resolve from the
      # real file's dir (the staged copy cannot resolve it — that is the bug).
      abs_dir="$(cd "$(dirname "$src_file")" && cd "$lit_dir" 2>/dev/null && pwd -P)" || continue
      abs="$abs_dir/$lit_base"
      [ -f "$abs" ] || continue
      case "$abs" in
        "$crate_abs"/*) continue ;;
        "$repo_root"/*) rel="${abs#"$repo_root"/}" ;;
        *) continue ;;
      esac
      mkdir -p "$staging/$(dirname "$rel")"
      cp "$abs" "$staging/$rel"
    done < <(grep -oE 'include_(str|bytes)!\("[^"]*"' "$src_file" 2>/dev/null | sed -E 's/^include_(str|bytes)!\("//; s/"$//')
  done < <(find "$daemon_dir/src" -type f -name '*.rs')
}

# Stage the daemon crate for re-validation. `$2` (the staging root) becomes a
# miniature REPO ROOT and the crate is copied to `<staging>/<crate-dir-name>/`;
# the CRATE ROOT is echoed on stdout — that, NOT the staging root, is the dir
# `patch -p1` and cargo must run in.
#
# WHAT WENT WRONG BEFORE: staging copied exactly three things — src/, Cargo.toml
# and Cargo.lock — straight into the staging root, and the real darwin-core TEST
# target needs more inputs than that:
#   * daemon/build.rs + daemon/csrc/thermal_shim.m, which produce the static lib
#     power.rs links with #[link(name = "darwin_thermal_shim", ...)];
#   * three test-only `include_str!("../../…")` that reach OUTSIDE the crate
#     (inference/server.py, config/darwin.toml, apps/vision/manifest.toml).
# `cargo check` neither links nor reads those `#[cfg(test)]` macro inputs, so the
# FIRST gate passed and the SECOND could not even COMPILE:
#     error: couldn't read `src/../../config/darwin.toml`: No such file or directory
# So EVERY apply of EVERY proposal — interactive, --yes, or the HUD Accept button
# — ended in `RESULT: failed cargo test failed in staging` and exit 1, no matter
# how good the patch was. The apply path was dead, and the message told the
# operator the AI had drafted a failing patch when in fact the harness could not
# build at all. scripts/test_apply_heal_confinement.sh only exercised the patch
# header pre-scan and the sandbox, never the build gates, which is why it shipped.
#
# The fix: copy the WHOLE crate directory (so the next file the crate grows is
# staged automatically) and MIRROR the repo-root siblings the sources actually
# name, discovered by SCANNING them — so a new `include_str!("../../…")` cannot
# silently break the gate again. Same shape as the daemon's own stage_sources()
# in daemon/src/heal.rs; the two staging paths must stay in lockstep.
stage_crate() {
  local daemon_dir="$1" staging="$2"
  local crate_name crate_dir entry name
  crate_name="$(basename "$daemon_dir")"
  crate_dir="$staging/$crate_name"
  mkdir -p "$crate_dir"
  # Everything except target/ (gigabytes of build output) and dotfiles (.git,
  # .DS_Store, .gitignore — never build inputs); the unquoted glob skips dotfiles.
  for entry in "$daemon_dir"/*; do
    [ -e "$entry" ] || continue
    name="${entry##*/}"
    case "$name" in target) continue ;; esac
    cp -R "$entry" "$crate_dir/$name"
  done
  mirror_out_of_crate_includes "$daemon_dir" "$staging"
  mirror_runtime_test_inputs "$daemon_dir" "$staging"
  printf '%s\n' "$crate_dir"
}

TS="${1:-}"
MODE_YES=0
# Parse the optional --yes flag (position-independent among args 2+).
for arg in "${@:2}"; do
  case "$arg" in
    --yes) MODE_YES=1 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

if [ -z "$TS" ]; then
  echo "usage: $0 <ts> [--yes]" >&2
  if [ -d "$PROPOSALS" ] && [ -n "$(ls -A "$PROPOSALS" 2>/dev/null)" ]; then
    echo "pending proposals:" >&2
    ls -1 "$PROPOSALS" >&2
  else
    echo "(no pending proposals under state/heal/proposals/)" >&2
  fi
  exit 1
fi

# Validate <ts> is a plausible numeric stamp BEFORE it is ever used as a path
# component — digits only, no slashes, no dots, no "..". This makes path
# traversal impossible (the GUI passes ts straight through, so this guard is
# load-bearing).
case "$TS" in
  '' | *[!0-9]*)
    echo "invalid timestamp '$TS' (must be digits only)" >&2
    exit 2
    ;;
esac

DIR="$PROPOSALS/$TS"
PATCH_FILE="$DIR/patch.diff"
if [ ! -f "$PATCH_FILE" ]; then
  # In --yes mode this still needs to be a structured RESULT line for the HUD.
  if [ "$MODE_YES" -eq 1 ]; then
    fail "no proposal at state/heal/proposals/$TS (missing patch.diff)"
  fi
  echo "no proposal at $DIR (missing patch.diff)" >&2
  exit 1
fi

# ----------------------------------------------------------- interactive gate
# Interactive mode is UNCHANGED from before: show report + diff, ask read -r,
# then fall through to the shared apply path. --yes skips ONLY this block.
if [ "$MODE_YES" -eq 0 ]; then
  if [ -f "$DIR/report.md" ]; then
    echo "=== report ($DIR/report.md) ==="
    cat "$DIR/report.md"
    echo
  fi

  echo "=== proposed diff ==="
  cat "$PATCH_FILE"
  echo "====================="

  printf 'Apply this patch to %s and rebuild the release binary? [y/N] ' "$DAEMON"
  read -r answer
  case "$answer" in
    y | Y | yes | YES) ;;
    *)
      echo "aborted; the proposal is left in place."
      exit 1
      ;;
  esac
fi

# ----------------------------------------------------------- RE-VALIDATION gate
# Stage a fresh copy of the daemon crate and re-run patch + cargo check +
# cargo test there. NOTHING touches daemon/src until this is green. This mirrors
# the daemon's draft-time staging (daemon/src/heal.rs::stage_sources) so a patch
# that no longer applies, no longer compiles, or fails a test is refused here,
# regardless of what was true when it was drafted.
stage "revalidating"

STAGING="$HEAL_ROOT/apply-staging-$TS"
rm -rf "$STAGING"
mkdir -p "$STAGING"

if [ ! -d "$DAEMON/src" ]; then
  fail "daemon sources not found at $DAEMON/src"
fi
# $STAGING is a miniature REPO ROOT; the crate lands one level down, at
# $STAGING/daemon. Both `patch -p1` (whose headers are `a/src/...`) and cargo run
# against $CRATE — running them in $STAGING would leave the crate's out-of-crate
# `include_str!("../../…")` targets unresolvable and the test gate uncompilable.
CRATE="$(stage_crate "$DAEMON" "$STAGING")"

# Path-confinement: /usr/bin/patch is run with `-p1` and cwd = the target dir,
# and macOS patch honors `..` in `---`/`+++` hunk headers — so a header like
# `+++ b/src/../../../../tmp/x` would write OUTSIDE daemon/. Reject any diff whose
# `---`/`+++` target, after the `-p1` strip (drop the first path component), is
# empty, absolute, or contains a `..` component, BEFORE patch ever runs. The
# /dev/null new-file/deleted-file sentinel is exempt. This mirrors the daemon's
# clean_diff() confinement so the human apply path is confined too.
#
# CRITICAL: the header scan MUST see the same headers /usr/bin/patch will. macOS
# `patch` DE-INDENTS a uniformly-indented diff ("Patch is indented N spaces.")
# before reading the `---`/`+++`/`@@` lines, so a column-0-anchored `^(---|+++) `
# grep would miss a header that begins with leading whitespace — the confinement
# loop would never run and an indented `../`-bearing header would write
# out-of-tree. Defense in depth, in order:
#  (a) refuse ANY uniformly-/partially-indented diff up front — legitimate heal
#      diffs emitted by the pipeline are never indented, so an indented patch.diff
#      is itself a tamper signal, and
#  (b) scan headers with a leading-whitespace-tolerant pattern and STRIP that
#      whitespace before extracting the path, so the gate sees exactly what patch
#      will after de-indentation. `Index:` lines are scanned too (a future patch
#      build may select a filename from `Index:`), with the same `..`/abs rule.
if grep -qE '^[[:space:]]+(---|\+\+\+|@@|diff |Index:)' "$PATCH_FILE"; then
  fail "patch.diff is indented (a non-pipeline/tampered diff) — refusing"
fi
# (c) Reject the leading-NON-whitespace-prefix-before-a-header class (the X---
#     residual). macOS patch ALSO de-indents a single leading non-whitespace
#     char, so `X--- a/src/../../../../daemon/src/victim.rs` reaches patch as a
#     `--- ` header — but a column-0 `^(---|+++) ` grep never sees it. Match any
#     line that ends in a `--- `/`+++ `/`Index: ` header but does NOT start at
#     column 0 with it, i.e. has 1+ leading chars before the header token. This
#     is a FAST-FAIL tamper signal only; the sandbox above is the real defense.
#     False-positive guard: a unified-diff CONTENT line legitimately starts with
#     a single `+`/`-`/` ` then arbitrary text — those never contain a ` --- ` /
#     ` +++ ` / `Index: ` *space-delimited header token at the de-indent
#     boundary*, so we anchor on "1+ leading non-whitespace chars, then a real
#     header token". A header proper is `--- `/`+++ ` (three dashes/pluses +
#     space); content lines are a SINGLE `+`/`-` then arbitrary text, which can
#     never form `<prefix>--- `/`<prefix>+++ ` (e.g. `+++added text` is `+` +
#     `++added` — no ` --- `/` +++ ` token follows the prefix). The 1+ quantifier
#     fast-fails BOTH the single-char `X--- ` residual macOS patch de-indents AND
#     a multi-char `ZZ--- ` (which patch ignores, but is still an obvious tamper).
if grep -qE '^[^[:space:]]+(---|\+\+\+) ' "$PATCH_FILE"; then
  fail "patch.diff has a '---'/'+++' header behind a leading prefix char (de-indent tamper, e.g. 'X--- ') — refusing"
fi
if grep -qE '^[^[:space:]]+Index: ' "$PATCH_FILE"; then
  fail "patch.diff has an 'Index:' header behind a leading prefix char (de-indent tamper) — refusing"
fi
# (d) Reject the MIXED whitespace-then-single-non-ws-char prefix class (e.g.
#     ` X--- `, `\tX--- `, ` X --- `). macOS patch (2.0-12u11-Apple) de-indents
#     ANY leading whitespace AND at most ONE non-whitespace char (in any
#     position within the run), so ` X--- a/src/../../../../daemon/src/victim.rs`
#     reaches patch as a real `--- ` header and writes out-of-tree — yet it
#     evades BOTH (a) (the line does NOT start with `---` right after the
#     whitespace, so `^[[:space:]]+(---` misses it) AND (c) (the line starts
#     with whitespace, so `^[^[:space:]]+` misses it). Empirically mapped: 1
#     non-ws char anywhere in the leading whitespace de-indents; 2+ non-ws chars
#     do NOT (patch leaves the line as content). Anchor on the de-indent
#     boundary: leading whitespace, then exactly one non-ws char, then optional
#     whitespace, then a real `--- `/`+++ `/`Index: ` header token. A unified-
#     diff CONTENT line legitimately starting with ` `/`+`/`-` then text can
#     never form `<ws><one-char><ws>--- `/`+++ `/`Index: ` (a real header needs
#     three dashes/pluses + space; a context line with `--- ` mid-text has 2+
#     non-ws chars before it, exceeding the single-char de-indent boundary and
#     thus is not an escape either) — so this stays false-positive-free. The
#     sandbox already DENIES the write for this class; this is the fast-fail
#     tamper signal that was missing for the mixed ordering.
if grep -qE '^[[:space:]]+[^[:space:]][[:space:]]*(---|\+\+\+|Index:) ' "$PATCH_FILE"; then
  fail "patch.diff has a '---'/'+++'/'Index:' header behind a mixed whitespace+char prefix (de-indent tamper, e.g. ' X--- ') — refusing"
fi
while IFS= read -r hdr; do
  # hdr is the path token after `--- ` / `+++ `, before any trailing tab/timestamp.
  path="${hdr%%$'\t'*}"
  # Trim a trailing whitespace-delimited timestamp field if present (no tab).
  path="${path%% *}"
  [ "$path" = "/dev/null" ] && continue
  # Mirror -p1: strip up to and including the first '/'.
  case "$path" in
    */*) stripped="${path#*/}" ;;
    *)   stripped="" ;;
  esac
  case "$stripped" in
    '' | /* )            fail "patch header '$path' is not confined (empty or absolute after -p1)" ;;
    '..' | '../'* | *'/../'* | *'/..' ) fail "patch header '$path' escapes via '..' — refusing" ;;
  esac
done < <(grep -E '^[[:space:]]*(---|\+\+\+|Index:) ' "$PATCH_FILE" | sed -E 's/^[[:space:]]*(---|\+\+\+|Index:) //')

# Apply to the STAGING copy: dry-run first (so a bad hunk is caught before any
# file is written), then for real. A failed hunk -> refuse.
if ! confined_patch "$CRATE" -p1 --batch --dry-run <"$PATCH_FILE" >/dev/null 2>&1; then
  fail "patch does not apply cleanly to a fresh staging copy (hunk reject)"
fi
if ! confined_patch "$CRATE" -p1 --batch <"$PATCH_FILE"; then
  fail "patch application to staging failed"
fi

# cargo check + cargo test in the staged CRATE. These are the SAME gates the
# daemon ran at draft time and they are never weakened. Either failing -> refuse
# to touch the live tree.
if ! (cd "$CRATE" && cargo check); then
  fail "cargo check failed in staging — live daemon/src NOT modified"
fi

# THE APPLY GATE MUST MATCH THE DAEMON'S. The daemon's staged validation runs
# check -> clippy -> test (daemon/src/heal.rs); if this script skipped clippy, a
# patch the daemon REJECTED for a lint could still be applied by hand here, and a
# patch it ACCEPTED would be re-proven against a weaker bar. Both directions are
# wrong. `-D warnings` is this project's real merge standard.
if ! (cd "$CRATE" && cargo clippy --all-targets -- -D warnings); then
  fail "cargo clippy failed in staging (-D warnings) — live daemon/src NOT modified"
fi
# Three tests cannot run inside a stage and are skipped BY NAME, matching
# daemon/src/heal.rs UNRUNNABLE_IN_STAGE. The two apply_forge tests shell out to
# a script that cd's into a full repo layout; the heal pipeline test runs this
# very staging routine NESTED inside a stage. They pass in the real tree and fail
# here for reasons unrelated to the patch. `--skip` is a libtest flag, so it goes
# after `--` — `cargo test --skip X` answers "unexpected argument" and would fail
# the gate for the wrong reason.
if ! (cd "$CRATE" && cargo test -- \
        --skip forge::tests::apply_forge_accepts_legit_multiline_manifest \
        --skip forge::tests::apply_forge_refuses_multiline_overbroad_manifests \
        --skip heal::tests::full_pipeline_via_mock_brain_rejects_when_no_candidate_validates); then
  fail "cargo test failed in staging — live daemon/src NOT modified"
fi

# ------------------------------------------- RESPONSIVENESS PROBE (advisory)
# Every gate in this script — patch, check, clippy and test above, and the
# mutation probe below — proves the patch is SOUND. Not one of them proves it is
# an ANSWER: none of them ever looks at the DIAGNOSIS that triggered the heal, so
# a patch that fixes something else entirely (a real bug, with a real test that
# really fails when its fix is reversed) clears all five and is handed to the
# operator labelled "VALIDATED".
#
# This re-derives the verdict from `darwind --heal-responsiveness` — the
# daemon's OWN function, the same one-implementation-two-callers shape as
# --split-heal-diff — and PRINTS it, so the person about to mutate a privileged
# daemon (or the HUD's Accept button, which reads this stdout) sees whether the
# patch has anything to do with the burst.
#
# IT NEVER REFUSES. A correct fix routinely lives one layer up from the line
# that screamed; hard-rejecting on that heuristic would throw away exactly those
# patches, which is a worse failure than the hole it closes.
#
# ORDER MATTERS: THIS MUST RUN *BEFORE* THE MUTATION PROBE, NOT AFTER IT. That
# probe reverse-applies the patch's FIX into $CRATE and never puts it back, and
# a crate with its fix lifted out very often no longer COMPILES — which the
# probe ACCEPTS as PROVEN, because all it asks is whether `cargo test` fails.
# `cargo run --bin darwind` cannot build such a tree: both self-proof calls come
# back empty, this block decides the probe "does not discriminate", and every
# patch of that shape gets RESPONSIVENESS: UNKNOWN — the shell half of the gate
# silently inert, and blaming the wrong thing. Run it here, against the patched,
# green crate the gates above just built. Pinned by
# heal.rs::the_responsiveness_probe_runs_before_the_mutation_reverse_apply.
RESP_WORD="UNKNOWN"
RESP_DETAIL="no diagnosis.json in the proposal — nothing to check the patch against"
if [ -f "$DIR/diagnosis.json" ]; then
  # An unrecognized flag makes darwind fall through to ORDINARY DAEMON STARTUP,
  # which would HANG this script on a booted daemon. Never invoke it unguarded
  # (the same hazard --split-heal-diff and apply_forge.sh document).
  #
  # THE GUARD MATCHES THE ARGV COMPARISON, NOT THE PROSE — and every flag guard
  # below it does the same. COUNTED on the real main.rs, not guessed: besides its
  # argv dispatch each flag is named --heal-responsiveness 3 more times,
  # --heal-confidence 2, --split-heal-diff 4. Most of those are `//` COMMENTS (the
  # entrypoint block above each handler); exactly one apiece is the handler's own
  # eprintln! usage string, which is a CODE line — the comment class below is NOT
  # what holds that one back, and does not need to be: it carries no argv
  # comparison at all. So
  # the plain `grep -q -- '--heal-responsiveness' "$CRATE/src/main.rs"` this used
  # to be still succeeded on a staged source whose dispatch literal had drifted
  # or been renamed: the script cleared its own fail-closed guard on a COMMENT
  # and then invoked a flag the staged daemon does not implement — booting a
  # daemon instead of getting an answer, with the gate SKIPPED rather than
  # enforced. PROVED BY EXECUTION on all three guards: with the argv literal
  # renamed by one letter and the prose untouched, the old grep ACCEPTED and
  # this one REFUSES.
  #
  # Anchoring: the line's first non-blank character must not be `/`, so a `//`
  # comment quoting the argv form does not clear it either (proved: without that
  # class, a comment carrying `a == "--heal-responsiveness"` re-opens the hole).
  # MEASURED across line shapes: the real dispatch ACCEPTS, and so does a
  # rustfmt-wrapped `.position(|a| a == "--flag")` continuation, so ordinary
  # reformatting does not break the apply. TWO SHAPE CLASSES diverge from
  # heal.rs's `!starts_with("//") && contains(...)` code-line rule, and both
  # REFUSE here where that rule would ACCEPT: a `/*` block comment, and the
  # comparison standing alone at the start of its line — at column 0 AND AT ANY
  # INDENT. The indented one is the likely shape, because it is what rustfmt
  # emits from a block-bodied closure (`.position(|a| {` / `a == "--flag"` /
  # `})`), and it is pinned by execution rather than by this sentence, in
  # scripts/test_apply_heal_confinement.sh Part D.
  #
  # WHAT A DIVERGENCE COSTS IS NOT THE SAME AT ALL THREE GUARDS — re-derived from
  # the control flow here, not restated. The --heal-confidence and
  # --split-heal-diff guards below both refuse the run outright, so for those two
  # a divergence does mean nothing installs. THIS GUARD IS NOT ONE OF THEM: the
  # responsiveness probe is ADVISORY and never refuses an apply (see IT NEVER
  # REFUSES in the block header above; pinned by
  # heal.rs::the_apply_script_never_refuses_on_responsiveness, which scans this
  # block). A refusal here only sets RESP_DETAIL, leaves RESP_WORD at UNKNOWN,
  # prints "older source" about a daemon that is not older, and the run CONTINUES
  # through the confidence floor, the mutation probe and the live apply. That is
  # still fail-closed against the hazard this guard exists for — it never invokes
  # an unimplemented flag, so it can never boot a daemon and hang — but it is NOT
  # "nothing installs": the operator loses the advisory verdict and is told the
  # wrong reason for losing it.
  # daemon/src/heal.rs pins this exact text three ways — the per-flag parity
  # tests and every_staged_flag_guard_matches_the_argv_comparison_not_the_prose,
  # which ENUMERATES the guards, so a fourth one written in the old prose form
  # fails the daemon suite instead of shipping. Script and tests move together.
  if ! grep -qE '^[[:space:]]*[^/[:space:]].*a == "--heal-responsiveness"' "$CRATE/src/main.rs"; then
    RESP_DETAIL="the staged daemon does not implement --heal-responsiveness (older source)"
  else
    # A probe that always answers the same word is not a probe. Prove it
    # discriminates on two synthetic pairs before any verdict is believed.
    RP="$STAGING/responsiveness-selfproof"
    rm -rf "$RP"; mkdir -p "$RP"
    printf '%s' '{"signatures":["router dispatch exploded"],"files":["src/router.rs"],"line_numbers":[],"subsystem":"router","log_context":"","burst_lines":[],"source_excerpts":[]}' > "$RP/d.json"
    printf -- '--- a/src/router.rs\n+++ b/src/router.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n'     > "$RP/hit.diff"
    printf -- '--- a/src/colorlab.rs\n+++ b/src/colorlab.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n' > "$RP/miss.diff"
    RP_HIT=$( (cd "$CRATE" && cargo run --quiet --bin darwind -- \
                 --heal-responsiveness "$RP/d.json" "$RP/hit.diff"  2>/dev/null) | head -1 || true)
    RP_MISS=$( (cd "$CRATE" && cargo run --quiet --bin darwind -- \
                 --heal-responsiveness "$RP/d.json" "$RP/miss.diff" 2>/dev/null) | head -1 || true)
    if [ "$RP_HIT" != "DIRECT" ] || [ "$RP_MISS" != "UNRELATED" ]; then
      RESP_DETAIL="the responsiveness probe does not discriminate (hit=>'$RP_HIT', miss=>'$RP_MISS') — no verdict is trustworthy"
    else
      RESP_OUT=$( (cd "$CRATE" && cargo run --quiet --bin darwind -- \
                     --heal-responsiveness "$DIR/diagnosis.json" "$PATCH_FILE" 2>/dev/null) || true)
      RESP_WORD=$(printf '%s\n' "$RESP_OUT" | head -1 || true)
      RESP_DETAIL=$(printf '%s\n' "$RESP_OUT" | tail -n +2 || true)
      case "$RESP_WORD" in
        DIRECT | SUBSYSTEM | SIGNATURE | UNRELATED | INDETERMINATE) ;;
        # Not a verdict this script knows. Say so; never pass it through as one.
        *)
          RESP_WORD="UNKNOWN"
          RESP_DETAIL="the responsiveness probe returned an unrecognized verdict"
          ;;
      esac
    fi
  fi
fi
echo "RESPONSIVENESS: $RESP_WORD"
# A bare `[ -n "$X" ] && printf ...` as the last command of a block returns 1
# when X is empty, and `set -e` would kill the script on an EMPTY DETAIL LINE.
if [ -n "$RESP_DETAIL" ]; then
  printf '%s\n' "$RESP_DETAIL"
fi

# ------------------------------------- REVIEW-CONFIDENCE FLOOR (REFUSES)
# The daemon will not PROPOSE a patch the adversarial reviewer scored below
# the review-confidence floor ([self_heal].confidence_floor; the NUMBER lives in
# Rust and is deliberately not repeated here) — the
# four staged gates are all mechanical and blind to
# whether the patch is a good IDEA, and the reviewer is the only stage that
# judges that. This refuses to INSTALL one for the same reason, so a proposal
# written by an OLDER daemon (or edited by hand) cannot be applied under a
# weaker bar than the one that would have stopped it being written.
# BOTH GATES OR NEITHER.
#
# UNLIKE THE RESPONSIVENESS PROBE ABOVE, THIS ONE REFUSES. That probe is a
# heuristic about WHERE a fix lives and hard-rejecting on it would throw away
# correct patches; this is the reviewer's own explicit verdict on the patch in
# front of it, and "nobody vouched for this" is not something to install with
# one click.
#
# The threshold and the parser are NOT reimplemented here. `darwind
# --heal-confidence` is the daemon's own function — the same
# one-implementation-two-callers shape as --split-heal-diff and
# --heal-responsiveness — because a bash copy of a threshold is the
# gates-drift-apart defect by construction.
#
# ORDER: like the responsiveness probe, this must run while $CRATE is still the
# patched, GREEN tree the gates above built. The mutation probe below
# reverse-applies the fix and leaves a crate that routinely no longer compiles,
# and `cargo run --bin darwind` cannot be built from that one.
# THE ARGV COMPARISON, NOT THE PROSE — see the long note on the responsiveness
# guard above. COUNTED, not copied: main.rs names `--heal-confidence` TWO more
# times besides its argv dispatch — once in the entrypoint comment block over the
# handler, once in that handler's eprintln! usage string. (Four is
# --split-heal-diff's number; three is --heal-responsiveness's.) Two is already
# enough: a bare `grep -q -- '--heal-confidence'` clears itself on them and the
# script then invokes a flag a drifted staged daemon does not implement, and an
# unknown flag falls through to ORDINARY DAEMON STARTUP rather than erroring.
# WHAT THAT COSTS, re-derived from the lines just below rather than restated: a
# booted daemon answers none of the three self-proof probes, so this gate would
# REFUSE on "does not discriminate" — or, more likely, the command substitution
# never returns and the apply HANGS on it. So the gate is not "skipped"; what the
# guard buys is an immediate refusal that names the real reason instead of a hang
# or a misleading one.
if ! grep -qE '^[[:space:]]*[^/[:space:]].*a == "--heal-confidence"' "$CRATE/src/main.rs"; then
  fail "the staged daemon does not implement --heal-confidence — live daemon/src NOT modified"
fi
# A gate that always answers the same word is not a gate. Prove it discriminates
# across all three verdicts before believing anything it says about the real
# proposal (the same self-proof --split-heal-diff and apply_forge.sh do).
CP="$STAGING/confidence-selfproof"
rm -rf "$CP"; mkdir -p "$CP"
printf -- '- review confidence: 0.95\n' > "$CP/high.md"
printf -- '- review confidence: 0.01\n' > "$CP/low.md"
printf -- '- no score in this report\n' > "$CP/none.md"
CF_HIGH=$( (cd "$CRATE" && cargo run --quiet --bin darwind -- \
              --heal-confidence "$CP/high.md" 2>/dev/null) | head -1 || true)
CF_LOW=$(  (cd "$CRATE" && cargo run --quiet --bin darwind -- \
              --heal-confidence "$CP/low.md"  2>/dev/null) | head -1 || true)
CF_NONE=$( (cd "$CRATE" && cargo run --quiet --bin darwind -- \
              --heal-confidence "$CP/none.md" 2>/dev/null) | head -1 || true)
if [ "$CF_HIGH" != "ABOVE_FLOOR" ] || [ "$CF_LOW" != "BELOW_FLOOR" ] || [ "$CF_NONE" != "NO_SCORE" ]; then
  fail "the confidence gate does not discriminate (high=>'$CF_HIGH', low=>'$CF_LOW', none=>'$CF_NONE') — live daemon/src NOT modified"
fi
if [ ! -f "$DIR/report.md" ]; then
  fail "the proposal has no report.md, so no adversarial review stands behind it — live daemon/src NOT modified"
fi
CONF_OUT=$( (cd "$CRATE" && cargo run --quiet --bin darwind -- \
               --heal-confidence "$DIR/report.md" 2>/dev/null) || true)
CONF_WORD=$(printf '%s\n' "$CONF_OUT" | head -1 || true)
CONF_DETAIL=$(printf '%s\n' "$CONF_OUT" | tail -n +2 || true)
echo "REVIEW CONFIDENCE: $CONF_WORD"
if [ -n "$CONF_DETAIL" ]; then
  printf '%s\n' "$CONF_DETAIL"
fi
case "$CONF_WORD" in
  ABOVE_FLOOR) ;;
  BELOW_FLOOR) fail "the adversarial reviewer did not back this patch — live daemon/src NOT modified" ;;
  NO_SCORE)    fail "this proposal carries no review confidence — live daemon/src NOT modified" ;;
  # Not a verdict this script knows. Never pass it through as one.
  *)           fail "the confidence gate returned an unrecognized verdict '$CONF_WORD' — live daemon/src NOT modified" ;;
esac

# STAGE 4: MUTATION PROOF. check+clippy+test prove the patch compiles, lints and
# passes; none of them prove its new test would CATCH THE BUG COMING BACK. So the
# patch is split into its FIX side and its TEST side, the fix is reverse-applied,
# and the suite must now FAIL. If it still passes, the test does not demonstrate
# the defect and this refuses to touch the live tree.
#
# The split is NOT reimplemented here. `darwind --split-heal-diff` is the very
# function daemon/src/heal.rs uses, so the two gates cannot drift apart — a bash
# copy of a hunk classifier would be that drift by construction. It runs from the
# STAGED crate, which is already built by the gates above.
SPLIT_DIR="$STAGING/mutation-split"
rm -rf "$SPLIT_DIR"

# The staged daemon MUST implement the probe. An older or mismatched source would
# not know `--split-heal-diff` and would fall through to ORDINARY DAEMON STARTUP
# inside this script — booting a daemon instead of answering, and skipping the
# gate entirely. apply_forge.sh documents this exact hazard for its own gate flag.
# Checked against the staged source, so it cannot hang waiting on a daemon.
#
# THE ARGV COMPARISON, NOT THE PROSE — see the long note on the responsiveness
# guard above. The stage header two paragraphs up and main.rs's own entrypoint
# comment both spell `--split-heal-diff`, so the bare `grep -q -- '--split-heal-diff'`
# this replaces cleared itself on a comment while the dispatch literal had drifted.
if ! grep -qE '^[[:space:]]*[^/[:space:]].*a == "--split-heal-diff"' "$CRATE/src/main.rs"; then
  fail "the staged daemon does not implement the mutation probe — live daemon/src NOT modified"
fi

# ... and PROVE the binary actually discriminates before any verdict from it is
# trusted, again mirroring apply_forge.sh. A gate that always answers the same
# word is not a gate. One synthetic patch that IS separable and one that is NOT:
# both answers must come back right, or this fails closed.
PROBE="$STAGING/mutation-selfproof"
rm -rf "$PROBE"; mkdir -p "$PROBE/src"
printf 'fn a() {}\nfn b() {}\nfn c() {}\n#[cfg(test)]\nmod t {}\n' > "$PROBE/src/lib.rs"
printf -- '--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,1 @@\n-fn a() {}\n+fn a() { }\n@@ -5,1 +5,2 @@\n mod t {}\n+// t\n' > "$PROBE/sep.diff"
printf -- '--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,5 +1,6 @@\n-fn a() {}\n+fn a() { }\n fn b() {}\n fn c() {}\n #[cfg(test)]\n mod t {}\n+// t\n' > "$PROBE/mixed.diff"
SP_SEP=$(cd "$CRATE" && cargo run --quiet --bin darwind -- \
      --split-heal-diff "$PROBE/sep.diff" "$PROBE" "$PROBE/out" 2>/dev/null || true)
SP_MIX=$(cd "$CRATE" && cargo run --quiet --bin darwind -- \
      --split-heal-diff "$PROBE/mixed.diff" "$PROBE" "$PROBE/out2" 2>/dev/null || true)
if [ "$SP_SEP" != "PROVABLE" ] || [ "$SP_MIX" != "UNSPLITTABLE" ]; then
  fail "the mutation probe does not discriminate (separable=>'$SP_SEP', mixed=>'$SP_MIX') — live daemon/src NOT modified"
fi

if ! SPLIT_VERDICT=$(cd "$CRATE" && cargo run --quiet --bin darwind -- \
      --split-heal-diff "$PATCH_FILE" "$CRATE" "$SPLIT_DIR" 2>/dev/null); then
  fail "could not split the patch for the mutation probe — live daemon/src NOT modified"
fi

case "$SPLIT_VERDICT" in
  PROVABLE)
    # Take the fix away, keep the test. The suite MUST fail now.
    if ! confined_patch "$CRATE" -p1 --batch -R <"$SPLIT_DIR/fix.diff" >/dev/null 2>&1; then
      echo "MUTATION: INCONCLUSIVE — the fix could not be lifted back out of the patch"
    elif (cd "$CRATE" && cargo test -- \
            --skip forge::tests::apply_forge_accepts_legit_multiline_manifest \
            --skip forge::tests::apply_forge_refuses_multiline_overbroad_manifests \
            --skip heal::tests::full_pipeline_via_mock_brain_rejects_when_no_candidate_validates \
            >/dev/null 2>&1); then
      fail "the patch's own test PASSES without the patch's fix — it does not demonstrate the defect; live daemon/src NOT modified"
    else
      echo "MUTATION: PROVEN — the patch's test fails once its fix is taken away"
    fi
    ;;
  NO_TESTS)   echo "MUTATION: UNPROVEN — the patch adds no test" ;;
  TESTS_ONLY) echo "MUTATION: N/A — the patch is tests only" ;;
  UNSPLITTABLE) echo "MUTATION: INCONCLUSIVE — the fix and its test share a hunk" ;;
  # Anything else is not a verdict this script knows. Do not pass it through.
  *)          fail "the mutation probe returned an unrecognized verdict '$SPLIT_VERDICT' — live daemon/src NOT modified" ;;
esac

# ----------------------------------------------------------------- apply (live)
# Green. Apply the SAME patch to the real daemon/ tree. Dry-run first here too.
stage "applying"

if ! confined_patch "$DAEMON" -p1 --batch --dry-run <"$PATCH_FILE" >/dev/null 2>&1; then
  fail "patch no longer applies to the live daemon/ tree (hunk reject) — live daemon/src NOT modified"
fi
if ! confined_patch "$DAEMON" -p1 --batch <"$PATCH_FILE"; then
  fail "patch application to daemon/ failed"
fi

# ----------------------------------------------------------------- rebuild
stage "rebuilding"
if ! (cd "$DAEMON" && cargo build --release); then
  fail "release rebuild failed (patch is applied to daemon/src; fix or revert by hand)"
fi

# Clear the pending marker so DARWIN stops announcing the proposal.
if command -v sqlite3 >/dev/null 2>&1; then
  sqlite3 "$ROOT/state/darwin.db" "DELETE FROM facts WHERE key = 'meta.heal_pending';" || true
else
  echo "sqlite3 not found; clear the marker manually:" >&2
  echo "  sqlite3 $ROOT/state/darwin.db \"DELETE FROM facts WHERE key = 'meta.heal_pending';\"" >&2
fi

# ----------------------------------------------------------------- restart
# Restart darwind if its launchd service is loaded so the healed binary runs.
# kickstart -k restarts a running service; if the service is not loaded the
# command fails and we fall back to telling the user to restart manually.
RESTARTED=0
if command -v launchctl >/dev/null 2>&1; then
  if launchctl kickstart -k "gui/$(id -u)/com.darwin.daemon" >/dev/null 2>&1; then
    RESTARTED=1
  fi
fi

if [ "$RESTARTED" -eq 1 ]; then
  echo "daemon restarted (launchctl kickstart com.darwin.daemon)."
else
  echo "restart darwind manually to run the patched build (launchd service not loaded)."
fi

result_ok
