#!/bin/bash
# DARWIN boot wrapper: MLX inference server.
# Invoked by the com.darwin.inference LaunchAgent. Resolves the project root
# from its own location so the plist only needs to point at this script.
set -euo pipefail

DARWIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$DARWIN_ROOT"

# Gitignored secrets (e.g. export ANTHROPIC_API_KEY=...).
if [ -f "$DARWIN_ROOT/state/env.sh" ]; then
    # shellcheck disable=SC1091
    source "$DARWIN_ROOT/state/env.sh"
fi

export DARWIN_ROOT

# Bound state/logs: launchd appends to the StandardOut/ErrorPath forever with no
# rotation of its own (~5.8 MB/day measured), so rotate at START — the only point
# where no writer holds the fd. Keeps one previous generation; never fails boot.
if [ -f "$DARWIN_ROOT/boot/rotate_logs.sh" ]; then
    # shellcheck disable=SC1091
    source "$DARWIN_ROOT/boot/rotate_logs.sh"
    rotate_darwin_log "$DARWIN_ROOT/state/logs/launchd-inference.log"
    rotate_darwin_log "$DARWIN_ROOT/state/logs/inference.log"
fi


# Guardrail: with KeepAlive=true, a missing venv would otherwise be a silent
# ~10s crash-loop spamming state/logs/launchd-inference.log. Fail loudly.
PYTHON="$DARWIN_ROOT/.venv/bin/python"
if [ ! -x "$PYTHON" ]; then
    echo "error: $PYTHON missing — create the venv per the README Quick start" >&2
    exit 78  # EX_CONFIG
fi

exec "$PYTHON" "$DARWIN_ROOT/inference/server.py"
