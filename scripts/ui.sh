# shellcheck shell=bash
# ui.sh — J.A.R.V.I.S. futuristic terminal UI library (PURE BASH, no deps).
#
# Sourced by install.sh (and any other script that wants the Iron-Man boot
# aesthetic). Provides a 24-bit truecolor palette with graceful fallback to
# 256/16-color and a full no-color mode, an arc-reactor banner, staged
# boot-sequence headers, ok/warn/err/info lines with glyphs, a smooth progress
# bar, a spinner, a horizontal rule, and an "ONLINE" flourish.
#
# Design rules:
#   - NO external binaries beyond coreutils + tput. No python, no awk for output.
#   - Robust under `set -euo pipefail`: every function returns 0 and never
#     trips errexit (no bare commands that can fail the caller's pipeline).
#   - Degrades cleanly on a dumb/non-tty terminal and honours NO_COLOR.
#   - All drawing goes to stdout; the caller decides where that points.
#
# Usage:
#   source scripts/ui.sh
#   ui_init                 # detect color depth + width (call once, optional)
#   jarvis_banner
#   ui_stage 1 7 "PREFLIGHT"
#   ui_ok   "Rust toolchain present"
#   ui_warn "Xcode CLT not found"
#   ui_err  "Intel Mac unsupported"
#   ui_info "Resolved JARVIS_ROOT: ..."
#   ui_progress 42 "downloading model"
#   ui_hr
#   ui_online
#
# This file is meant to be *sourced*; it defines functions and sets a few
# UI_* globals. It does not enable errexit itself (that is the installer's job).

# --- guard against double-sourcing ------------------------------------------
if [ -n "${__JARVIS_UI_SH_LOADED:-}" ]; then
    return 0 2>/dev/null || true
fi
__JARVIS_UI_SH_LOADED=1

# --- capability detection ----------------------------------------------------
# UI_COLOR_MODE is one of: truecolor | 256 | 16 | none
UI_COLOR_MODE="none"
UI_WIDTH=80
UI_UTF8=0

# Detect once. Safe to call repeatedly. Honours an explicit override via
# JARVIS_UI_COLOR (truecolor|256|16|none) for testing/rendering.
ui_init() {
    # Width: prefer COLUMNS, then tput, then a sane default. Never fail.
    local cols=""
    if [ -n "${COLUMNS:-}" ]; then
        cols="$COLUMNS"
    elif command -v tput >/dev/null 2>&1; then
        cols="$(tput cols 2>/dev/null || true)"
    fi
    case "$cols" in
        ''|*[!0-9]*) UI_WIDTH=80 ;;
        *)           UI_WIDTH="$cols" ;;
    esac
    # Clamp width to a comfortable band so banners do not wrap or sprawl.
    [ "$UI_WIDTH" -lt 40 ] && UI_WIDTH=40
    [ "$UI_WIDTH" -gt 120 ] && UI_WIDTH=120

    # UTF-8 glyph support (for ✓ ⚠ ✗ ▸ and box drawing).
    case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in
        *[Uu][Tt][Ff]-8*|*[Uu][Tt][Ff]8*) UI_UTF8=1 ;;
        *) UI_UTF8=0 ;;
    esac

    # Explicit override wins (lets the render harness force a mode).
    if [ -n "${JARVIS_UI_COLOR:-}" ]; then
        UI_COLOR_MODE="$JARVIS_UI_COLOR"
        _ui_define_palette
        return 0
    fi

    # NO_COLOR (https://no-color.org/) or a non-tty stdout => no color.
    if [ -n "${NO_COLOR:-}" ] || [ ! -t 1 ]; then
        UI_COLOR_MODE="none"
        _ui_define_palette
        return 0
    fi

    # Truecolor: COLORTERM advertises it explicitly.
    case "${COLORTERM:-}" in
        *truecolor*|*24bit*) UI_COLOR_MODE="truecolor" ; _ui_define_palette ; return 0 ;;
    esac

    # Otherwise fall back on the terminfo color count.
    local ncolors=0
    if command -v tput >/dev/null 2>&1; then
        ncolors="$(tput colors 2>/dev/null || echo 0)"
        case "$ncolors" in ''|*[!0-9]*) ncolors=0 ;; esac
    fi
    if [ "$ncolors" -ge 256 ]; then
        UI_COLOR_MODE="256"
    elif [ "$ncolors" -ge 8 ]; then
        UI_COLOR_MODE="16"
    else
        UI_COLOR_MODE="none"
    fi
    _ui_define_palette
    return 0
}

# Emit an SGR escape for a 24-bit foreground color, degrading by UI_COLOR_MODE.
# Usage: ui_fg R G B   (each 0-255). Prints nothing in none-mode.
ui_fg() {
    case "$UI_COLOR_MODE" in
        truecolor) printf '\033[38;2;%d;%d;%dm' "$1" "$2" "$3" ;;
        256)       printf '\033[38;5;%dm' "$(_ui_rgb_to_256 "$1" "$2" "$3")" ;;
        16)        printf '\033[%dm' "$(_ui_rgb_to_16 "$1" "$2" "$3")" ;;
        *)         : ;;
    esac
    return 0
}

# Convert an RGB triple to the nearest xterm-256 cube/grey index.
_ui_rgb_to_256() {
    local r="$1" g="$2" b="$3"
    # Greyscale shortcut when r≈g≈b.
    local mx=$r mn=$r
    [ "$g" -gt "$mx" ] && mx=$g; [ "$b" -gt "$mx" ] && mx=$b
    [ "$g" -lt "$mn" ] && mn=$g; [ "$b" -lt "$mn" ] && mn=$b
    if [ $((mx - mn)) -le 12 ]; then
        local grey=$(( (r + g + b) / 3 ))
        if [ "$grey" -lt 8 ]; then printf '16'; return 0; fi
        if [ "$grey" -gt 248 ]; then printf '231'; return 0; fi
        printf '%d' $(( ((grey - 8) * 24 / 247) + 232 ))
        return 0
    fi
    # 6x6x6 color cube.
    local ri gi bi
    ri=$(( r * 5 / 255 )); gi=$(( g * 5 / 255 )); bi=$(( b * 5 / 255 ))
    printf '%d' $(( 16 + 36 * ri + 6 * gi + bi ))
    return 0
}

# Convert an RGB triple to a basic 16-color SGR code (30-37 / 90-97).
_ui_rgb_to_16() {
    local r="$1" g="$2" b="$3"
    local bright=0
    [ $(( (r + g + b) / 3 )) -ge 128 ] && bright=1
    local rb=0 gb=0 bb=0
    [ "$r" -ge 96 ] && rb=1
    [ "$g" -ge 96 ] && gb=1
    [ "$b" -ge 96 ] && bb=1
    local base=$(( rb + gb * 2 + bb * 4 ))   # 0..7 ANSI color order
    if [ "$bright" -eq 1 ]; then
        printf '%d' $(( 90 + base ))
    else
        printf '%d' $(( 30 + base ))
    fi
    return 0
}

# Palette + glyphs. Sets UI_RESET/UI_BOLD/UI_DIM and named color escapes.
# This is a sourced library: the full palette (UI_BLUE) and glyph set
# (UI_BAR_HALF) are defined for completeness so any consumer can use them,
# even though install.sh does not reference every member.
# shellcheck disable=SC2034
_ui_define_palette() {
    if [ "$UI_COLOR_MODE" = "none" ]; then
        UI_RESET="" ; UI_BOLD="" ; UI_DIM=""
        UI_CYAN="" ; UI_BRIGHT="" ; UI_GREEN="" ; UI_YELLOW="" ; UI_RED="" ; UI_BLUE="" ; UI_GREY=""
    else
        UI_RESET=$'\033[0m'
        UI_BOLD=$'\033[1m'
        UI_DIM=$'\033[2m'
        UI_CYAN="$(ui_fg 0 200 255)"     # arc-reactor cyan
        UI_BRIGHT="$(ui_fg 235 250 255)" # near-white highlight
        UI_GREEN="$(ui_fg 80 230 160)"
        UI_YELLOW="$(ui_fg 245 200 70)"
        UI_RED="$(ui_fg 255 90 90)"
        UI_BLUE="$(ui_fg 90 150 255)"
        UI_GREY="$(ui_fg 120 140 160)"
    fi

    # Glyphs degrade to ASCII when UTF-8 is unavailable.
    if [ "$UI_UTF8" -eq 1 ]; then
        UI_G_OK="✓" ; UI_G_WARN="⚠" ; UI_G_ERR="✗" ; UI_G_INFO="▸"
        UI_G_DOT="•" ; UI_HR_CH="─"
        UI_SPIN='⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏'
        UI_BAR_FULL="█" ; UI_BAR_HALF="▌" ; UI_BAR_EMPTY="·"
    else
        UI_G_OK="+" ; UI_G_WARN="!" ; UI_G_ERR="x" ; UI_G_INFO=">"
        UI_G_DOT="*" ; UI_HR_CH="-"
        UI_SPIN='| / - \'
        UI_BAR_FULL="#" ; UI_BAR_HALF="=" ; UI_BAR_EMPTY="."
    fi
    return 0
}

# Ensure a palette exists even if the caller forgot ui_init.
_ui_ensure() {
    [ -n "${UI_RESET+x}" ] || ui_init
    return 0
}

# --- primitives --------------------------------------------------------------

# A horizontal rule spanning UI_WIDTH (cyan, dim).
ui_hr() {
    _ui_ensure
    local n="$UI_WIDTH" line="" i=0
    while [ "$i" -lt "$n" ]; do line="${line}${UI_HR_CH}"; i=$((i + 1)); done
    printf '%s%s%s\n' "${UI_DIM}${UI_CYAN}" "$line" "$UI_RESET"
    return 0
}

# Stage header:  ── [ 1/7 ] PREFLIGHT ──────────────────────────────
ui_stage() {
    _ui_ensure
    local n="$1" total="$2" label="$3"
    local tag="[ ${n}/${total} ]"
    local left="${UI_HR_CH}${UI_HR_CH} "
    # Pad with rule chars to UI_WIDTH (count visible chars only).
    local visible=$(( ${#left} + ${#tag} + 1 + ${#label} + 1 ))
    local pad="" i="$visible"
    while [ "$i" -lt "$UI_WIDTH" ]; do pad="${pad}${UI_HR_CH}"; i=$((i + 1)); done
    printf '\n%s%s%s%s%s %s%s%s %s%s%s\n' \
        "${UI_DIM}${UI_CYAN}" "$left" "$UI_RESET" \
        "${UI_BOLD}${UI_CYAN}" "$tag" \
        "${UI_BOLD}${UI_BRIGHT}" "$label" "$UI_RESET" \
        "${UI_DIM}${UI_CYAN}" "$pad" "$UI_RESET"
    return 0
}

# Status lines. Each takes a message; all return 0.
ui_ok()   { _ui_ensure; printf '  %s%s%s %s\n' "${UI_BOLD}${UI_GREEN}"  "$UI_G_OK"   "$UI_RESET" "$1"; return 0; }
ui_warn() { _ui_ensure; printf '  %s%s%s %s%s%s\n' "${UI_BOLD}${UI_YELLOW}" "$UI_G_WARN" "$UI_RESET" "$UI_YELLOW" "$1" "$UI_RESET"; return 0; }
ui_err()  { _ui_ensure; printf '  %s%s%s %s%s%s\n' "${UI_BOLD}${UI_RED}"    "$UI_G_ERR"  "$UI_RESET" "$UI_RED" "$1" "$UI_RESET"; return 0; }
ui_info() { _ui_ensure; printf '  %s%s%s %s%s%s\n' "${UI_CYAN}" "$UI_G_INFO" "$UI_RESET" "$UI_GREY" "$1" "$UI_RESET"; return 0; }

# A plain dim sub-note, indented under a status line.
ui_note() { _ui_ensure; printf '    %s%s %s%s\n' "$UI_DIM" "$UI_G_DOT" "$1" "$UI_RESET"; return 0; }

# A smooth progress bar:  [██████████····················]  42%  label
# Args: pct (0-100), label (optional). Truecolor draws a cyan->white gradient.
ui_progress() {
    _ui_ensure
    local pct="$1" label="${2:-}"
    case "$pct" in ''|*[!0-9]*) pct=0 ;; esac
    [ "$pct" -gt 100 ] && pct=100
    [ "$pct" -lt 0 ] && pct=0

    local barwidth=30
    local filled=$(( pct * barwidth / 100 ))
    local empty=$(( barwidth - filled ))

    local bar="" i=0
    if [ "$UI_COLOR_MODE" = "truecolor" ]; then
        # Gradient cyan(0,200,255) -> white(235,250,255) across the filled span.
        while [ "$i" -lt "$filled" ]; do
            local t=$(( barwidth > 1 ? i * 255 / (barwidth - 1) : 255 ))
            local r=$(( 0   + (235 - 0)   * t / 255 ))
            local g=$(( 200 + (250 - 200) * t / 255 ))
            local b=255
            bar="${bar}$(ui_fg "$r" "$g" "$b")${UI_BAR_FULL}"
            i=$((i + 1))
        done
        bar="${bar}${UI_RESET}"
    else
        local fillcol="${UI_CYAN}"
        i=0
        while [ "$i" -lt "$filled" ]; do bar="${bar}${UI_BAR_FULL}"; i=$((i + 1)); done
        bar="${fillcol}${bar}${UI_RESET}"
    fi
    local empties="" j=0
    while [ "$j" -lt "$empty" ]; do empties="${empties}${UI_BAR_EMPTY}"; j=$((j + 1)); done

    # \r so repeated calls overwrite the same line on a tty; trailing newline
    # at 100% (final state stays on screen) and on any non-tty (so each step
    # lands on its own line in a captured log rather than one mangled line).
    local eol="\r"
    [ "$pct" -ge 100 ] && eol="\n"
    [ ! -t 1 ] && eol="\n"
    printf '  %s[%s%s%s]%s %s%3d%%%s %s%s%s%b' \
        "$UI_DIM" "$UI_RESET" "$bar" "${UI_DIM}${UI_GREY}${empties}" "$UI_RESET" \
        "${UI_BOLD}${UI_BRIGHT}" "$pct" "$UI_RESET" \
        "$UI_GREY" "$label" "$UI_RESET" "$eol"
    return 0
}

# Spinner for a long opaque step. Runs the given command in the background and
# animates until it exits, then reports ok/err. Falls back to a plain ok/err
# (no animation) on a non-tty so logs stay clean.
#   ui_spin "compiling daemon" -- some_command --with args
ui_spin() {
    _ui_ensure
    local label="$1"; shift
    [ "${1:-}" = "--" ] && shift
    if [ "$#" -eq 0 ]; then ui_warn "ui_spin: no command given"; return 0; fi

    if [ ! -t 1 ] || [ "$UI_COLOR_MODE" = "none" ]; then
        printf '  %s%s%s %s ...\n' "$UI_CYAN" "$UI_G_INFO" "$UI_RESET" "$label"
        if "$@"; then ui_ok "$label"; return 0; else ui_err "$label (failed)"; return 1; fi
    fi

    "$@" &
    local pid=$!
    local -a frames
    # shellcheck disable=SC2206
    frames=($UI_SPIN)
    local k=0
    while kill -0 "$pid" 2>/dev/null; do
        local f="${frames[$(( k % ${#frames[@]} ))]}"
        printf '\r  %s%s%s %s' "${UI_BOLD}${UI_CYAN}" "$f" "$UI_RESET" "$label"
        k=$((k + 1))
        sleep 0.08
    done
    if wait "$pid"; then
        printf '\r\033[K'; ui_ok "$label"; return 0
    else
        printf '\r\033[K'; ui_err "$label (failed)"; return 1
    fi
}

# --- arc-reactor banner ------------------------------------------------------
# A striking ASCII "arc-reactor" + J.A.R.V.I.S. wordmark with a cyan->white
# vertical truecolor gradient. Centered to UI_WIDTH.
jarvis_banner() {
    _ui_ensure
    # Each art row below is exactly 28 visible chars wide so the centering math
    # (which counts ${#line}) lines up with what the terminal actually renders.
    # Single backslashes only — these are single-quoted, so a backslash is one
    # literal char (a doubled "\\" would over-count the width and skew centering).
    local -a art
    art=(
'       .-===========-.       '
'     /    .-------.    \     '
'    |    /  .---.  \    |    '
'    |   |  (  @  )  |   |    '
'    |    \  `---`  /    |    '
'     \    `-------`    /     '
'       `-===========-`       '
'                            '
'  J . A . R . V . I . S .   '
'                            '
'Just A Rather Very Intelligent'
'            System           '
    )
    local rows="${#art[@]}"
    printf '\n'
    local idx=0
    for line in "${art[@]}"; do
        # Vertical gradient: top cyan -> bottom near-white.
        local t=$(( rows > 1 ? idx * 255 / (rows - 1) : 0 ))
        local r=$(( 0   + (235 - 0)   * t / 255 ))
        local g=$(( 200 + (250 - 200) * t / 255 ))
        local b=255
        # Center the line.
        local len=${#line}
        local lpad=$(( (UI_WIDTH - len) / 2 ))
        [ "$lpad" -lt 0 ] && lpad=0
        local sp="" s=0
        while [ "$s" -lt "$lpad" ]; do sp="${sp} "; s=$((s + 1)); done
        if [ "$UI_COLOR_MODE" = "none" ]; then
            printf '%s%s\n' "$sp" "$line"
        else
            local emph=""
            [ "$idx" -eq 8 ] && emph="$UI_BOLD"   # the wordmark row pops
            printf '%s%s%s%s%s\n' "$sp" "$emph" "$(ui_fg "$r" "$g" "$b")" "$line" "$UI_RESET"
        fi
        idx=$((idx + 1))
    done
    printf '\n'
    # Subtitle rule.
    if [ "$UI_COLOR_MODE" = "none" ]; then
        printf '%s\n' "  one-command install // built fresh // honesty-first"
    else
        printf '  %s%s%s\n' "${UI_DIM}${UI_CYAN}" "one-command install // built fresh // honesty-first" "$UI_RESET"
    fi
    return 0
}

# --- ONLINE flourish ---------------------------------------------------------
ui_online() {
    _ui_ensure
    ui_hr
    local msg="J . A . R . V . I . S .   I S   O N L I N E"
    local len=${#msg}
    local lpad=$(( (UI_WIDTH - len) / 2 ))
    [ "$lpad" -lt 0 ] && lpad=0
    local sp="" s=0
    while [ "$s" -lt "$lpad" ]; do sp="${sp} "; s=$((s + 1)); done
    if [ "$UI_COLOR_MODE" = "none" ]; then
        printf '%s%s\n' "$sp" "$msg"
    else
        printf '%s%s%s%s\n' "$sp" "${UI_BOLD}${UI_BRIGHT}" "$msg" "$UI_RESET"
    fi
    ui_hr
    return 0
}
