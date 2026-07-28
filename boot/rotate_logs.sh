#!/bin/bash
# DARWIN log rotation — bounds state/logs so a 24/7 install cannot fill the disk.
#
# WHY: the three LaunchAgents point StandardOutPath/StandardErrorPath at
# state/logs/launchd-<agent>.log, and launchd APPENDS to those paths forever with
# NO rotation of its own. Measured on a real install: ~5.8 MB/day across the four
# files (~2 GB/year), unbounded. The audit log already enforces retention
# (audit.rs); the process logs did not — this closes that inconsistency.
#
# HOW: each boot wrapper sources this and calls `rotate_darwin_log <path>` BEFORE
# exec'ing its binary. If the file is at/over MAX_LOG_BYTES we keep ONE previous
# generation (`<name>.1`, overwritten) and truncate the live file. Rotating at
# START (not while running) is deliberate: launchd holds the fd open for the
# lifetime of the process, so truncating a live file would leave a sparse hole
# and confuse tail -f. At start there is no writer yet, so the swap is clean.
#
# BOUND: at most 2 generations per log => worst case 2 * MAX_LOG_BYTES per agent.
# With the default 16 MiB that is <= 128 MiB total across the 4 log paths, versus
# unbounded before.
#
# HONEST LIMIT: an agent that runs for months without a restart still grows its
# CURRENT log unbounded between restarts — this bounds growth ACROSS restarts
# (the common case: every login, redeploy, crash-restart, or reboot). A
# size-triggered rotation of a live fd would need copytruncate semantics the
# daemon does not implement; macOS `newsyslog` is the OS-level option if a
# running-process cap is ever needed.

# Max bytes a single log may reach before it is rotated at next start (16 MiB).
: "${DARWIN_MAX_LOG_BYTES:=16777216}"

# rotate_darwin_log <logfile>
# Rotates <logfile> to <logfile>.1 when it is at/over the cap. Never fails the
# caller: rotation is hygiene, not a boot precondition, so every error path is
# swallowed and the agent still starts.
rotate_darwin_log() {
    local log="$1"
    [ -n "$log" ] || return 0
    [ -f "$log" ] || return 0

    # Portable size read (macOS stat -f%z; GNU stat -c%s as a fallback).
    local size
    size=$(stat -f%z "$log" 2>/dev/null || stat -c%s "$log" 2>/dev/null || echo 0)
    [ "$size" -ge "$DARWIN_MAX_LOG_BYTES" ] 2>/dev/null || return 0

    # Keep exactly ONE previous generation, then truncate the live path in place
    # (preserving inode/permissions so the plist's path stays valid).
    mv -f "$log" "$log.1" 2>/dev/null || true
    : > "$log" 2>/dev/null || true
    return 0
}
