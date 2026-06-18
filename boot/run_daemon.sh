#!/bin/bash
# JARVIS boot wrapper: jarvisd daemon.
# Invoked by the com.jarvis.daemon LaunchAgent. Resolves the project root
# from its own location so the plist only needs to point at this script.
set -euo pipefail

JARVIS_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$JARVIS_ROOT"

# Gitignored secrets (e.g. export ANTHROPIC_API_KEY=... for cloud fallback).
if [ -f "$JARVIS_ROOT/state/env.sh" ]; then
    # shellcheck disable=SC1091
    source "$JARVIS_ROOT/state/env.sh"
fi

export JARVIS_ROOT

# Guardrail: with KeepAlive=true, a missing binary would otherwise be a silent
# ~10s crash-loop spamming state/logs/launchd-daemon.log. Fail loudly.
JARVISD="$JARVIS_ROOT/daemon/target/release/jarvisd"
if [ ! -x "$JARVISD" ]; then
    echo "error: $JARVISD missing — run scripts/install_boot.sh --install (builds it) or cargo build --release" >&2
    exit 78  # EX_CONFIG
fi

exec "$JARVISD"
