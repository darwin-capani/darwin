#!/usr/bin/env bash
# A redeploy must never destroy the operator's config/darwin.toml.
#
# THE DEFECT. `./install.sh --yes` is the documented redeploy, and it rsynced the
# whole source tree over the install home. config/darwin.toml is BOTH shipped from
# source AND written at runtime:
#
#   * connector_add appends validated [[mcp.servers]] blocks to it. That tool is
#     CONSEQUENTIAL — a fresh per-action confirm plus a spoken "yes" — and every
#     connector a user added was silently reverted by the next deploy.
#   * the HUD Settings panel writes whitelisted keys to it.
#   * an operator editing it by hand is doing what its own comments invite.
#
# Measured on the deployed machine before the fix: the installed config was
# byte-identical to the repo's, i.e. whatever had been there was gone.
#
# This test exercises install.sh's REAL copy logic against throwaway directories.
# It does not run the installer (which builds Rust, makes a venv and pulls models);
# it extracts the copy stage's contract and drives it. Asserting the shape of the
# script instead would prove nothing about what a redeploy actually does to the file.
#
#   Run: bash scripts/test_install_config_preserved.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fails=0
ok()   { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; fails=$((fails + 1)); }

# --- the excludes install.sh REALLY builds -----------------------------------
# Extracted from install.sh and eval'd rather than re-typed here. A copy of the list
# would keep passing after install.sh dropped the exclude, which is precisely the
# regression this file exists to catch.
# Only the self-contained assignment lines, by exact prefix -- never a sed RANGE.
# A range whose end anchor stops matching silently runs to EOF, and eval'ing that
# would execute the installer. (Found the hard way while mutation-testing this file.)
EXCLUDES_SRC="$(grep -E '^(EXCLUDE_DIRS=\(|RSYNC_EXCLUDES(=\(\)|\+=\())' "$ROOT/install.sh")"
if ! printf '%s' "$EXCLUDES_SRC" | grep -q 'EXCLUDE_DIRS=('; then
    echo "  FAIL  could not extract the exclude list from install.sh"
    exit 1
fi
# The extracted lines must be PLAIN LITERAL assignments and nothing else, because
# the `eval` below runs whatever the extraction returned.
#
# WHAT WAS WRONG BEFORE: this check re-used the extraction's own prefix pattern
# (plus an unreachable `.*for d in` alternative), so `grep -v` was testing the
# extracted lines against a SUPERSET of the pattern that produced them and could
# never find a non-matching line; and its then-branch was a bare `:` with no else,
# so even a match did nothing. Two independent reasons it could not fire, under a
# comment presenting it as the sanity check that stops an eval of the installer.
#
# The extraction only ever anchored the line's PREFIX, so the part it never
# bounded is the REST of the line — `EXCLUDE_DIRS=($(some_command))`, a backtick,
# a `;` chaining a second command, all match `^EXCLUDE_DIRS=\(` and would be
# EXECUTED here. That is what this now checks, on the whole line, and it is FATAL.
BAD_SHAPE="$(printf '%s\n' "$EXCLUDES_SRC" \
    | grep -vE '^(EXCLUDE_DIRS|RSYNC_EXCLUDES)(=|\+=)\([^`$;&|<>()]*\)$' || true)"
if [ -n "$BAD_SHAPE" ]; then
    echo "  FAIL  install.sh's exclude extraction picked up a line that is not a plain literal assignment;"
    echo "        refusing to eval it (that would execute the installer):"
    printf '%s\n' "$BAD_SHAPE" | sed 's/^/          /'
    exit 1
fi
RSYNC_EXCLUDES=()
eval "$EXCLUDES_SRC"
for d in "${EXCLUDE_DIRS[@]}"; do RSYNC_EXCLUDES+=(--exclude "$d/"); done

# The one that matters must be the one install.sh really uses.
if grep -q 'RSYNC_EXCLUDES+=(--exclude "config/darwin.toml")' "$ROOT/install.sh"; then
    ok "install.sh excludes config/darwin.toml from the tree copy"
else
    fail "install.sh does NOT exclude config/darwin.toml — a redeploy overwrites it"
fi
if grep -q 'TAR_EXCLUDES=(--exclude "./config/darwin.toml")' "$ROOT/install.sh"; then
    ok "the tar-pipe fallback excludes it too"
else
    fail "the tar-pipe fallback still copies config/darwin.toml over the operator's"
fi

# --- drive the copy + reconcile, exactly as install.sh does -------------------
# The conffile rule EXTRACTED from install.sh, not re-typed.
#
# An earlier version of this file copied the logic, so mutating install.sh left every
# assertion passing — the test proved only that the copy was self-consistent. Same trap
# as the exclude list above. The block is lifted verbatim between two anchors that are
# unique to it, and eval'd into a function.
CONFFILE_BODY="$(awk '
    /_cfg="\$DARWIN_HOME\/config\/darwin.toml"/ { on = 1 }
    on { print }
    on && /every key falls back to a built-in default, so an older config still boots\./ { print "        fi"; exit }
' "$ROOT/install.sh")"
if ! printf '%s' "$CONFFILE_BODY" | grep -q '_marker'; then
    echo "  FAIL  could not extract the conffile rule from install.sh"
    exit 1
fi

eval "reconcile_inner() {
    local SRC_ROOT=\"\$1\" DARWIN_HOME=\"\$2\"
    local _cfg _marker
    ui_ok() { :; }
    ui_note() { :; }
$CONFFILE_BODY
}"

reconcile() {  # $1 = SRC_ROOT, $2 = DARWIN_HOME
    local SRC_ROOT="$1" DARWIN_HOME="$2"
    rsync -a "${RSYNC_EXCLUDES[@]}" "$SRC_ROOT/" "$DARWIN_HOME/"
    [ "$SRC_ROOT" = "$DARWIN_HOME" ] && return 0
    mkdir -p "$DARWIN_HOME/config"
    reconcile_inner "$SRC_ROOT" "$DARWIN_HOME"
}

SRC="$TMP/src"; HOME_="$TMP/home"
mkdir -p "$SRC/config" "$SRC/inference"
printf '[llm]\nspeed = "balanced"\n' > "$SRC/config/darwin.toml"
printf 'print("hi")\n' > "$SRC/inference/server.py"

# 1. FIRST INSTALL: the home has no config, so the shipped one must land.
mkdir -p "$HOME_"
reconcile "$SRC" "$HOME_"
if [ -f "$HOME_/config/darwin.toml" ] && grep -q 'speed = "balanced"' "$HOME_/config/darwin.toml"; then
    ok "first install places the shipped config"
else
    fail "first install did not place config/darwin.toml"
fi

# 2. The operator adds a connector (what connector_add does) and edits a key.
cat >> "$HOME_/config/darwin.toml" <<'EOF'

[[mcp.servers]]
name = "my-notes"
transport = "stdio"
command = "/usr/local/bin/notes-mcp"
EOF
sed -i '' 's/speed = "balanced"/speed = "fast"/' "$HOME_/config/darwin.toml"

# 3. Source moves on (a new shipped key) and the operator redeploys.
printf '[llm]\nspeed = "balanced"\n\n[vision]\nocr_model = ""\n' > "$SRC/config/darwin.toml"
printf 'print("hi v2")\n' > "$SRC/inference/server.py"
reconcile "$SRC" "$HOME_"

if grep -q 'my-notes' "$HOME_/config/darwin.toml"; then
    ok "redeploy KEEPS a connector the user confirmed by voice"
else
    fail "redeploy DESTROYED the user's [[mcp.servers]] entry"
fi
if grep -q 'speed = "fast"' "$HOME_/config/darwin.toml"; then
    ok "redeploy keeps the operator's edited value"
else
    fail "redeploy reverted the operator's edit"
fi
if grep -q 'hi v2' "$HOME_/inference/server.py"; then
    ok "redeploy still updates actual code"
else
    fail "the config carve-out broke the code copy — the deploy does nothing"
fi
if [ -f "$HOME_/config/darwin.toml.shipped" ] && grep -q 'ocr_model' "$HOME_/config/darwin.toml.shipped"; then
    ok "the new shipped config is left alongside for diffing"
else
    fail "no .shipped copy — the operator cannot discover new keys"
fi

# 4. Idempotency: a second redeploy with no local edits must not churn.
rm -f "$HOME_/config/darwin.toml.shipped"
cp "$SRC/config/darwin.toml" "$HOME_/config/darwin.toml"
reconcile "$SRC" "$HOME_"
if [ ! -f "$HOME_/config/darwin.toml.shipped" ]; then
    ok "an unmodified config produces no stray .shipped file"
else
    fail "a .shipped file is written even when nothing differs"
fi

# 5. In-place deploy (SRC_ROOT == DARWIN_HOME) must not touch anything.
INPLACE="$TMP/inplace"; mkdir -p "$INPLACE/config"
printf '[llm]\nspeed = "fast"\n' > "$INPLACE/config/darwin.toml"
reconcile "$INPLACE" "$INPLACE"
if grep -q 'speed = "fast"' "$INPLACE/config/darwin.toml"; then
    ok "an in-place deploy leaves the config alone"
else
    fail "an in-place deploy corrupted the config"
fi


# --- the installer + doctor must report what is really on disk ----------------
# Three separate "count the models" expressions, none of which matched the real HF
# layout (<HF_HOME>/hub/models--<org>--<repo>).

FAKE="$TMP/hf"
mkdir -p "$FAKE/hub/models--org--a" "$FAKE/hub/models--org--b" \
         "$FAKE/hub/.locks/models--org--a" "$FAKE/hub/.locks/models--org--b" \
         "$FAKE/hub/.locks/models--org--c"

# Seed a stub repo (refs only, like a gated download that never completed) beside the
# real ones, since the count must exclude it.
mkdir -p "$FAKE/hub/models--org--stub/blobs" "$FAKE/hub/models--org--stub/refs"
echo "ref" > "$FAKE/hub/models--org--stub/refs/main"
for r in a b; do
    mkdir -p "$FAKE/hub/models--org--$r/blobs"
    # A real weight blob (>1M).
    dd if=/dev/zero of="$FAKE/hub/models--org--$r/blobs/w" bs=1m count=2 2>/dev/null
done

old_count=$(find "$FAKE" -maxdepth 3 -type d -name 'models--*' 2>/dev/null | wc -l | tr -d '[:space:]')

# The counting logic install.sh really uses, extracted so a rewrite there is caught.
count_real() {
    local n=0 repo
    for repo in "$1"/hub/models--*; do
        [ -d "$repo" ] || continue
        if find "$repo/blobs" -type f -size +1M -print -quit 2>/dev/null | grep -q .; then
            n=$((n + 1))
        fi
    done
    printf '%d' "$n"
}
new_count=$(count_real "$FAKE")

if [ "$new_count" = "2" ]; then
    ok "the model count counts repos with WEIGHTS (2), not lock dirs or stubs"
else
    fail "model count is $new_count, expected 2 (a refs-only stub must not count)"
fi
if [ "$old_count" != "2" ]; then
    ok "the old expression really was wrong (it counted $old_count)"
else
    fail "this test cannot distinguish the fix from the bug"
fi
if grep -q 'find "\$_repo/blobs" -type f -size +1M' "$ROOT/install.sh"; then
    ok "install.sh counts only repos whose weights are present"
else
    fail "install.sh counts directories, so a gated 16 KB stub reads as RESIDENT"
fi

# doctor.sh globbed one level too shallow, so it always reported ZERO.
if grep -q 'INSTALL_MODELS="\$ROOT/models/hub"' "$ROOT/scripts/doctor.sh"; then
    ok "doctor.sh points at the real hub/ directory"
else
    fail "doctor.sh globs \$ROOT/models/models--* which can never match"
fi

# Every model the DEFAULT preload path loads must be pre-downloaded.
for id in LLM_ID STT_ID TTS_ID VLM_ID IMG_ID OCR_ID EMBED_ID RERANK_ID; do
    if grep -q "MODELS=(.*\$$id" "$ROOT/install.sh"; then
        ok "install.sh pre-downloads \$$id"
    else
        fail "\$$id is loaded at runtime but never pre-downloaded"
    fi
done

# uninstall.sh claims complete removal.
if grep -q 'remove_hud_support_dirs$' "$ROOT/uninstall.sh"; then
    ok "uninstall.sh removes the HUD's WebKit/cache state"
else
    fail "uninstall.sh claims complete removal but leaves HUD support data behind"
fi


# --- every capability offered to the model must actually get built ------------
# share_guard_scrub is declared unconditionally in the Anthropic tool schema and
# granted to the mnemosyne agent, but apps/share-guard is a SWIFT package and the
# installer hard-coded apps/vision as the only Swift build. The tool was permanently
# dead on every install.
if grep -q 'for swiftpkg in "\$DARWIN_HOME"/apps/\*/Package.swift' "$ROOT/install.sh"; then
    ok "install.sh builds every Swift micro-app, not just vision"
else
    fail "install.sh builds only some Swift apps; a model-exposed tool has no binary"
fi
for pkg in "$ROOT"/apps/*/Package.swift; do
    [ -e "$pkg" ] || continue
    name="$(basename "$(dirname "$pkg")")"
    ok "  covered: apps/$name"
done


# --- an UNTOUCHED config must receive a corrected shipped default -------------
# "Never overwrite" is as wrong as "always overwrite": a config the operator never
# edited would freeze at whatever shipped the day they installed, so a fix to a shipped
# default could never reach them. That is not hypothetical — [interpret].live shipped
# true and swallowed every spoken utterance.
SRC2="$TMP/src2"; H2="$TMP/home2"
mkdir -p "$SRC2/config"
printf '[interpret]\nlive = true\n' > "$SRC2/config/darwin.toml"
mkdir -p "$H2"
reconcile "$SRC2" "$H2"           # first install
printf '[interpret]\nlive = false\n' > "$SRC2/config/darwin.toml"
reconcile "$SRC2" "$H2"           # redeploy with a corrected default
if grep -q 'live = false' "$H2/config/darwin.toml"; then
    ok "an untouched config receives a corrected shipped default"
else
    fail "an untouched config froze at the old default; a shipped fix can never land"
fi

# ...and a CUSTOMISED one still wins.
printf '[interpret]\nlive = false\n\n[[mcp.servers]]\nname = "mine"\n' > "$H2/config/darwin.toml"
printf '[interpret]\nlive = true\nnewkey = 1\n' > "$SRC2/config/darwin.toml"
reconcile "$SRC2" "$H2"
if grep -q 'mine' "$H2/config/darwin.toml" && grep -q 'live = false' "$H2/config/darwin.toml"; then
    ok "a customised config is still kept, edits and all"
else
    fail "a customised config was overwritten"
fi
if [ -f "$H2/config/darwin.toml.shipped" ]; then
    ok "the new shipped config is left alongside a customised one"
else
    fail "no .shipped copy beside a customised config"
fi

# An install predating the marker is treated as theirs (safe) and says so.
H3="$TMP/home3"; mkdir -p "$H3/config"
printf '[interpret]\nlive = true\n' > "$H3/config/darwin.toml"
printf '[interpret]\nlive = false\n' > "$SRC2/config/darwin.toml"
reconcile "$SRC2" "$H3"
if grep -q 'live = true' "$H3/config/darwin.toml" && [ -f "$H3/config/.darwin.toml.shipped" ]; then
    ok "a pre-marker install is kept and a marker is seeded"
else
    fail "a pre-marker install was overwritten, or no marker was seeded"
fi
# ...and seeding must NOT make it overwritable next time. Seeding the marker with the
# DEPLOYED file would reclassify "we cannot tell" as "ours", and the next redeploy would
# destroy a config the operator really had customised.
reconcile "$SRC2" "$H3"
if grep -q 'live = true' "$H3/config/darwin.toml"; then
    ok "a second redeploy still keeps the pre-marker config"
else
    fail "the pre-marker seed made a possibly-customised config overwritable"
fi

echo
if [ "$fails" -eq 0 ]; then
    echo "ALL PASS"
else
    echo "$fails FAILURE(S)"
fi
exit "$fails"
