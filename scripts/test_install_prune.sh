#!/bin/bash
# Hermetic regression for install.sh's shipped-manifest prune.
#
# WHY THIS EXISTS: rsync runs without --delete, so before the manifest a file
# deleted from the repo lived in the install home forever — MEASURED, apps
# deleted in #237 were still deployed three weeks later, logging "skipping
# invalid micro-app manifest" every boot. The obvious fix (--delete) is WRONG:
# the install home also holds forge-generated apps that are absent from source,
# and --delete would destroy the owner's generated work.
#
# So the prune is manifest-driven, and the property that makes it safe is the
# THIRD assertion below, not the first. No daemon, no network, no live tree.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
INSTALL="$HERE/../install.sh"
W=$(mktemp -d); trap 'rm -rf "$W"' EXIT
SRC_ROOT="$W/src"; DARWIN_HOME="$W/home"
mkdir -p "$SRC_ROOT/apps/kept" "$SRC_ROOT/apps/gone" "$DARWIN_HOME/state"
touch "$SRC_ROOT/apps/kept/manifest.toml" "$SRC_ROOT/apps/gone/manifest.toml"
ui_note(){ :; }; ui_ok(){ :; }
eval "$(sed -n '/^prune_unshipped()/,/^}$/p' "$INSTALL")"
eval "$(sed -n '/^write_shipped_manifest()/,/^}$/p' "$INSTALL")"
SHIPPED_MANIFEST="$DARWIN_HOME/state/.shipped-manifest"
pass=0; fail=0
ck(){ if [ "$1" = 0 ]; then echo "ok   $2"; pass=$((pass+1)); else echo "FAIL $2"; fail=$((fail+1)); fi; }

write_shipped_manifest
mkdir -p "$DARWIN_HOME/apps/kept" "$DARWIN_HOME/apps/gone"
mkdir -p "$DARWIN_HOME/apps/forge-made"; touch "$DARWIN_HOME/apps/forge-made/main.py"
rm -rf "$SRC_ROOT/apps/gone"
prune_unshipped
[ ! -e "$DARWIN_HOME/apps/gone" ]; ck $? "a path this build no longer ships is pruned"
[ -e "$DARWIN_HOME/apps/kept" ]; ck $? "a still-shipped path survives"
[ -e "$DARWIN_HOME/apps/forge-made/main.py" ]; ck $? "a RUNTIME-CREATED path never in a manifest is UNTOUCHED"

rm -f "$SHIPPED_MANIFEST"; rm -rf "$SRC_ROOT/apps/kept"
prune_unshipped
[ -e "$DARWIN_HOME/apps/kept" ]; ck $? "no manifest prunes NOTHING (first install / pre-manifest upgrade)"

printf '%s\n' "  ---" "  install prune: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
