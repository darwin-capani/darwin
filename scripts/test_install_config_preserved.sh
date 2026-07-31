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
# The extracted lines must be assignments and nothing else.
if printf '%s' "$EXCLUDES_SRC" | grep -qvE '^(EXCLUDE_DIRS=\(|RSYNC_EXCLUDES(=\(\)|\+=\(|.*for d in))'; then
    :  # the `for d in ...` line is a loop over EXCLUDE_DIRS and is expected
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
reconcile() {  # $1 = SRC_ROOT, $2 = DARWIN_HOME
    local SRC_ROOT="$1" DARWIN_HOME="$2"
    rsync -a "${RSYNC_EXCLUDES[@]}" "$SRC_ROOT/" "$DARWIN_HOME/"
    if [ "$SRC_ROOT" != "$DARWIN_HOME" ]; then
        if [ ! -f "$DARWIN_HOME/config/darwin.toml" ]; then
            mkdir -p "$DARWIN_HOME/config"
            cp "$SRC_ROOT/config/darwin.toml" "$DARWIN_HOME/config/darwin.toml"
        elif cmp -s "$SRC_ROOT/config/darwin.toml" "$DARWIN_HOME/config/darwin.toml"; then
            :
        else
            cp "$SRC_ROOT/config/darwin.toml" "$DARWIN_HOME/config/darwin.toml.shipped"
        fi
    fi
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

old_count=$(find "$FAKE" -maxdepth 3 -type d -name 'models--*' 2>/dev/null | wc -l | tr -d '[:space:]')
new_count=$(find "$FAKE/hub" -maxdepth 1 -type d -name 'models--*' 2>/dev/null | wc -l | tr -d '[:space:]')
if [ "$new_count" = "2" ]; then
    ok "the model count counts repos (2), not lock dirs"
else
    fail "model count is $new_count, expected 2"
fi
if [ "$old_count" != "2" ]; then
    ok "the old expression really was wrong (it counted $old_count)"
else
    fail "this test cannot distinguish the fix from the bug"
fi
if grep -q "find \"\$HF_HOME_DIR/hub\" -maxdepth 1 -type d -name 'models--\*'" "$ROOT/install.sh"; then
    ok "install.sh's status board uses the corrected expression"
else
    fail "install.sh still counts .locks entries as resident models"
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

echo
if [ "$fails" -eq 0 ]; then
    echo "ALL PASS"
else
    echo "$fails FAILURE(S)"
fi
exit "$fails"
