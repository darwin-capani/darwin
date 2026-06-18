#!/bin/bash
# install.sh — the ONE command that installs ALL of J.A.R.V.I.S., built FRESH,
# into a per-user home with no sudo.
#
#   curl -fsSL https://raw.githubusercontent.com/<YOUR-GH-USER>/jarvis/main/install.sh | bash
#
# (./install.sh also works from a local clone — it copies *this* clone into the
#  install home, builds every artifact fresh there, and loads the LaunchAgents.)
#
# Install home (per-user, no sudo, relocatable — the daemon + boot wrappers are
# JARVIS_ROOT-relative, so the tree runs from wherever it lands):
#
#   ~/Library/Application Support/JARVIS
#
# What it does (staged):
#   1. PREFLIGHT  — macOS + arm64, Xcode CLT, Rust, Python 3.11, Node/npm
#   2. PLACE      — copy the project tree into the install home (excluding
#                   built/fetched dirs: target .venv node_modules .build state models)
#   3. PYTHON ENV — python3.11 -m venv, upgrade pip, install the FULL dep set
#   4. MODELS     — pre-download every model the OS uses into HF_HOME
#   5. BUILD      — cargo build --release (daemon + apps), swift build, HUD/Tauri .app
#   6. AUTOSTART  — render + load the 2 LaunchAgents via scripts/install_boot.sh --install
#   7. FINISH     — "JARVIS IS ONLINE" + honest next-steps (TCC grants, keys, wake word)
#
# Flags:
#   --check / --dry-run   print the full plan and run only READ-ONLY detection
#   --yes / -y            assume "yes" to consent prompts (rustup install, etc.)
#   --no-models           skip the model pre-download stage (build everything else)
#   --help / -h           this help
#
# Idempotent + resumable: re-running skips work already done (existing venv,
# already-downloaded models, up-to-date release binaries) and is safe to ctrl-C.
#
# HONESTY: nothing consequential is enabled. The daemon's actuation stays
# OFF-by-default behind its master switch + per-turn/per-action confirm +
# voice-id + lockdown + policy + allowlist. The optional ElevenLabs cloud voice
# tier and the cloud LLM fallback stay OFF until YOU add a key. No secret, key,
# state DB, venv, model, or build artifact is ever written into the source repo.

set -euo pipefail

# ----------------------------------------------------------------------------
# Locate the source tree (this script's dir) and source the UI library.
# ----------------------------------------------------------------------------
SRC_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=scripts/ui.sh
if [ -f "$SRC_ROOT/scripts/ui.sh" ]; then
    # shellcheck disable=SC1091
    source "$SRC_ROOT/scripts/ui.sh"
else
    echo "error: scripts/ui.sh not found next to install.sh (run from a JARVIS clone)" >&2
    exit 1
fi
ui_init

# ----------------------------------------------------------------------------
# Configuration.
# ----------------------------------------------------------------------------
JARVIS_HOME="$HOME/Library/Application Support/JARVIS"
PY311_CANDIDATES=(
    "/opt/homebrew/bin/python3.11"
    "/usr/local/bin/python3.11"
    "python3.11"
)
CARGO="$HOME/.cargo/bin/cargo"
TOTAL_STAGES=7

# Model ids — kept in sync with inference/server.py DEFAULT_* constants. The
# installer reads server.py at runtime (below) and these are only the fallback
# if that parse fails, so an out-of-date list here never silently ships.
FALLBACK_LLM="mlx-community/Qwen3-4B-Instruct-2507-4bit"
FALLBACK_STT="mlx-community/whisper-small-mlx"
FALLBACK_TTS="mlx-community/Kokoro-82M-bf16"
FALLBACK_VLM="mlx-community/Qwen2-VL-2B-Instruct-4bit"

# Dirs that are BUILT or FETCHED fresh in the install home and must NOT be copied
# from the source tree (so we never ship a stale daemon binary, a wrong-path
# venv, someone else's state DB, or gigabytes of model weights).
EXCLUDE_DIRS=(target .venv node_modules .build .git state models dist gen)

# ----------------------------------------------------------------------------
# Flag parsing.
# ----------------------------------------------------------------------------
MODE="install"
ASSUME_YES=0
DO_MODELS=1

while [ "$#" -gt 0 ]; do
    case "$1" in
        --check|--dry-run) MODE="check" ;;
        -y|--yes)          ASSUME_YES=1 ;;
        --no-models)       DO_MODELS=0 ;;
        -h|--help)
            # Print the header comment block (the doc lines above `set -euo
            # pipefail`) as the help text, stripping the leading "# ".
            sed -n '2,38p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            ui_err "unknown argument: $1 (try --help)"
            exit 2
            ;;
    esac
    shift
done

# ----------------------------------------------------------------------------
# Small helpers.
# ----------------------------------------------------------------------------

# Ask for consent unless --yes; in --check mode never prompt (assume no).
confirm() {
    local prompt="$1"
    [ "$ASSUME_YES" -eq 1 ] && return 0
    [ "$MODE" = "check" ] && return 1
    [ ! -t 0 ] && return 1   # piped stdin (curl | bash) without -y => decline
    local reply=""
    printf '  %s%s %s [y/N] %s' "${UI_BOLD}${UI_CYAN}" "$UI_G_INFO" "$prompt" "$UI_RESET"
    read -r reply || true
    case "$reply" in [Yy]*) return 0 ;; *) return 1 ;; esac
}

# Find a python3.11 interpreter; echo its path, or empty if none.
find_py311() {
    local p
    for p in "${PY311_CANDIDATES[@]}"; do
        if command -v "$p" >/dev/null 2>&1; then
            # Confirm it really is 3.11 (a bare "python3.11" on PATH might lie).
            if "$p" -c 'import sys; raise SystemExit(0 if sys.version_info[:2]==(3,11) else 1)' >/dev/null 2>&1; then
                command -v "$p"
                return 0
            fi
        fi
    done
    return 0
}

# Read a DEFAULT_* model id out of server.py; falls back to the constant given.
read_model_id() {
    local key="$1" fallback="$2" line=""
    line="$(grep -m1 -E "^${key}[[:space:]]*=" "$SRC_ROOT/inference/server.py" 2>/dev/null || true)"
    # Extract the quoted value.
    line="${line#*\"}"; line="${line%%\"*}"
    if [ -n "$line" ]; then printf '%s' "$line"; else printf '%s' "$fallback"; fi
}

# Plan-only printer for --check: describe a command instead of running it.
plan() { ui_note "would run: $*"; }

# ----------------------------------------------------------------------------
# Banner.
# ----------------------------------------------------------------------------
jarvis_banner
if [ "$MODE" = "check" ]; then
    ui_info "DRY RUN (--check): printing the plan + read-only detection only."
    ui_info "No venv, no downloads, no build, no ~/Library writes, no launchctl."
fi
ui_info "Source tree:  $SRC_ROOT"
ui_info "Install home: $JARVIS_HOME"

# ============================================================================
# STAGE 1 — PREFLIGHT
# ============================================================================
ui_stage 1 "$TOTAL_STAGES" "PREFLIGHT"

PREFLIGHT_FATAL=0

# --- OS + arch ---
OS_NAME="$(uname -s)"
ARCH="$(uname -m)"
if [ "$OS_NAME" != "Darwin" ]; then
    ui_err "JARVIS requires macOS (found: $OS_NAME)."
    PREFLIGHT_FATAL=1
elif [ "$ARCH" != "arm64" ]; then
    ui_err "JARVIS requires Apple Silicon (arm64). Found: $ARCH — Intel Macs are unsupported (MLX is Metal/Apple-GPU only)."
    PREFLIGHT_FATAL=1
else
    OS_VER="$(sw_vers -productVersion 2>/dev/null || echo '?')"
    ui_ok "macOS $OS_VER on $ARCH (Apple Silicon)"
fi

# --- Xcode Command Line Tools (needed for swift + native compiles) ---
if xcode-select -p >/dev/null 2>&1; then
    ui_ok "Xcode Command Line Tools: $(xcode-select -p)"
else
    ui_warn "Xcode Command Line Tools not found."
    ui_note "install with:  xcode-select --install   (then re-run this installer)"
    PREFLIGHT_FATAL=1
fi

# --- Rust toolchain ---
if [ -x "$CARGO" ] || command -v cargo >/dev/null 2>&1; then
    [ -x "$CARGO" ] || CARGO="$(command -v cargo)"
    ui_ok "Rust toolchain: $("$CARGO" --version 2>/dev/null || echo cargo)"
else
    ui_warn "Rust toolchain (cargo) not found."
    if [ "$MODE" = "check" ]; then
        plan "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"
    elif confirm "install Rust via rustup (no sudo, into ~/.rustup + ~/.cargo)?"; then
        ui_spin "installing rustup" -- bash -c \
            "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path"
        CARGO="$HOME/.cargo/bin/cargo"
        if [ -x "$CARGO" ]; then ui_ok "Rust installed: $("$CARGO" --version)"; else ui_err "rustup install did not produce $CARGO"; PREFLIGHT_FATAL=1; fi
    else
        ui_err "Rust is required to build the daemon + app crates."
        PREFLIGHT_FATAL=1
    fi
fi

# --- Python 3.11 ---
PY311="$(find_py311)"
if [ -n "$PY311" ]; then
    ui_ok "Python 3.11: $PY311 ($("$PY311" --version 2>&1))"
else
    ui_warn "Python 3.11 not found (MLX has no wheels for 3.12+/3.14 — 3.11 is required)."
    ui_note "install with:  brew install python@3.11   (then re-run)"
    PREFLIGHT_FATAL=1
fi

# --- Node + npm (HUD / Tauri) ---
if command -v node >/dev/null 2>&1 && command -v npm >/dev/null 2>&1; then
    ui_ok "Node $(node --version) / npm $(npm --version)"
else
    ui_warn "Node + npm not found (needed for the HUD / Tauri build)."
    ui_note "install with:  brew install node   (then re-run)"
    PREFLIGHT_FATAL=1
fi

if [ "$PREFLIGHT_FATAL" -ne 0 ]; then
    if [ "$MODE" = "check" ]; then
        ui_warn "Preflight reported gaps (above). Resolve them, then run a real install."
    else
        ui_err "Preflight failed — resolve the items above and re-run."
        exit 1
    fi
fi

# ============================================================================
# STAGE 2 — PLACE (copy the tree into the install home)
# ============================================================================
ui_stage 2 "$TOTAL_STAGES" "PLACE"

# Build the rsync/cp exclude args once.
RSYNC_EXCLUDES=()
for d in "${EXCLUDE_DIRS[@]}"; do RSYNC_EXCLUDES+=(--exclude "$d/"); done
RSYNC_EXCLUDES+=(--exclude ".DS_Store" --exclude "*.pyc" --exclude "state/env.sh")

ui_info "Excluding from the copy (built/fetched fresh in the home): ${EXCLUDE_DIRS[*]}"

if [ "$MODE" = "check" ]; then
    plan "mkdir -p \"$JARVIS_HOME\""
    plan "rsync -a ${RSYNC_EXCLUDES[*]} \"$SRC_ROOT/\" \"$JARVIS_HOME/\""
    ui_note "(source == install home would be a no-op copy; the build/model/launch"
    ui_note " stages would still run against the home)"
else
    mkdir -p "$JARVIS_HOME"
    if [ "$SRC_ROOT" = "$JARVIS_HOME" ]; then
        ui_ok "Source IS the install home — already in place, nothing to copy."
    elif command -v rsync >/dev/null 2>&1; then
        ui_spin "copying project tree -> install home" -- \
            rsync -a "${RSYNC_EXCLUDES[@]}" "$SRC_ROOT/" "$JARVIS_HOME/"
        ui_ok "Project tree placed at $JARVIS_HOME"
    else
        # Fallback: tar-pipe with excludes (rsync is standard on macOS, but be safe).
        TAR_EXCLUDES=()
        for d in "${EXCLUDE_DIRS[@]}"; do TAR_EXCLUDES+=(--exclude "./$d"); done
        TAR_EXCLUDES+=(--exclude "./.DS_Store")
        ui_spin "copying project tree (tar) -> install home" -- bash -c \
            "tar -C '$SRC_ROOT' ${TAR_EXCLUDES[*]} -cf - . | tar -C '$JARVIS_HOME' -xf -"
        ui_ok "Project tree placed at $JARVIS_HOME"
    fi
fi

export JARVIS_ROOT="$JARVIS_HOME"
ui_info "export JARVIS_ROOT=\"$JARVIS_ROOT\""

# From here on, paths are relative to the install home (so we build IN PLACE
# where the LaunchAgents expect the artifacts). In --check the home may not
# exist yet, so the build/model commands are only described.
HOME_PY_REQ="$JARVIS_HOME/inference/requirements.txt"
VENV="$JARVIS_HOME/.venv"
VENV_PY="$VENV/bin/python"
VENV_PIP="$VENV/bin/pip"
HF_HOME_DIR="$JARVIS_HOME/models"

# ============================================================================
# STAGE 3 — PYTHON ENV (venv + the FULL dependency set)
# ============================================================================
ui_stage 3 "$TOTAL_STAGES" "PYTHON ENV"

# The "every bit" extras the base requirements.txt leaves OPT-IN or transitive,
# pinned to real, current versions. These are added on top of requirements.txt
# so a full install pulls every model backend the OS can use.
EVERYBIT_EXTRAS=(
    "mlx-vlm>=0.1"        # on-device VLM (op=describe_image)
    "mflux>=0.4"          # on-device text->image (op=generate_image)
    "soundfile>=0.12"     # WAV/PCM IO for the voice pipeline
    "huggingface_hub>=0.24"  # model pre-download (huggingface-cli)
    "elevenlabs>=1.0"     # OPTIONAL cloud voice tier SDK (stays OFF until a key is set)
)

if [ "$MODE" = "check" ]; then
    plan "$PY311 -m venv \"$VENV\""
    plan "$VENV_PIP install --upgrade pip wheel"
    plan "$VENV_PIP install -r \"$HOME_PY_REQ\""
    plan "$VENV_PIP install ${EVERYBIT_EXTRAS[*]}"
    plan "$VENV_PY -m spacy download en_core_web_sm   # misaki G2P fallback"
    ui_note "base deps come from inference/requirements.txt (mlx, mlx-lm, mlx-whisper,"
    ui_note " mlx-audio, misaki, numpy, coremltools, spacy, ...); extras add the full set."
else
    if [ -z "${PY311:-}" ]; then ui_err "no python3.11 — cannot create the venv"; exit 1; fi
    if [ -x "$VENV_PY" ]; then
        ui_ok ".venv already present ($("$VENV_PY" --version 2>&1)) — reusing."
    else
        ui_spin "creating .venv (python3.11)" -- "$PY311" -m venv "$VENV"
        ui_ok ".venv created"
    fi
    ui_spin "upgrading pip + wheel" -- "$VENV_PIP" install --quiet --upgrade pip wheel
    if [ -f "$HOME_PY_REQ" ]; then
        ui_spin "pip install -r inference/requirements.txt (base deps)" -- \
            "$VENV_PIP" install --quiet -r "$HOME_PY_REQ"
    else
        ui_warn "inference/requirements.txt missing in the home — installing the pinned base set."
        ui_spin "pip install base deps (fallback pins)" -- "$VENV_PIP" install --quiet \
            "mlx>=0.20" "mlx-lm>=0.31.1" "mlx-whisper>=0.4" "mlx-audio>=0.4.4" \
            "misaki>=0.9.4" espeakng-loader num2words phonemizer-fork "spacy>=3.8" \
            "numpy>=1.26" "coremltools>=8.0"
    fi
    ui_spin "pip install the 'every bit' extras (VLM, image, soundfile, hf_hub, elevenlabs)" -- \
        "$VENV_PIP" install --quiet "${EVERYBIT_EXTRAS[@]}"
    ui_spin "spacy: download en_core_web_sm (G2P fallback)" -- \
        "$VENV_PY" -m spacy download en_core_web_sm
    ui_ok "Python environment ready — full dependency set installed."
fi

# ============================================================================
# STAGE 4 — MODELS (pre-download every model the OS uses)
# ============================================================================
ui_stage 4 "$TOTAL_STAGES" "MODELS"

LLM_ID="$(read_model_id DEFAULT_LLM "$FALLBACK_LLM")"
STT_ID="$(read_model_id DEFAULT_STT "$FALLBACK_STT")"
TTS_ID="$(read_model_id DEFAULT_TTS "$FALLBACK_TTS")"
VLM_ID="$(read_model_id DEFAULT_VLM "$FALLBACK_VLM")"
# The image model id ("schnell") resolves to a FLUX.1-schnell repo inside mflux;
# we let mflux's own resolver fetch it on first generate, but also name the repo
# here so the pre-download covers it.
IMG_ID="black-forest-labs/FLUX.1-schnell"

MODELS=("$LLM_ID" "$STT_ID" "$TTS_ID" "$VLM_ID" "$IMG_ID")

ui_info "HF_HOME -> $HF_HOME_DIR (models cached in the install home, never the repo)"
ui_info "LLM   : $LLM_ID"
ui_info "STT   : $STT_ID"
ui_info "TTS   : $TTS_ID"
ui_info "VLM   : $VLM_ID"
ui_info "IMAGE : $IMG_ID"

if [ "$DO_MODELS" -eq 0 ]; then
    ui_warn "--no-models: skipping model pre-download (features that need a model"
    ui_note "will fetch it on first use, gated on enough RAM)."
elif [ "$MODE" = "check" ]; then
    plan "mkdir -p \"$HF_HOME_DIR\""
    for m in "${MODELS[@]}"; do
        plan "HF_HOME=\"$HF_HOME_DIR\" \"$VENV/bin/huggingface-cli\" download \"$m\""
    done
    ui_note "multi-GB total; each download shows progress and is cache-first (resumable)."
else
    mkdir -p "$HF_HOME_DIR"
    export HF_HOME="$HF_HOME_DIR"
    HFCLI="$VENV/bin/huggingface-cli"
    if [ ! -x "$HFCLI" ]; then ui_err "huggingface-cli missing in the venv (Stage 3 should have installed it)"; exit 1; fi
    n=0; total="${#MODELS[@]}"
    for m in "${MODELS[@]}"; do
        n=$((n + 1))
        ui_progress $(( (n - 1) * 100 / total )) "model $n/$total: $m"
        # huggingface-cli download is cache-first + resumable, so re-runs are cheap.
        ui_spin "download $m" -- "$HFCLI" download "$m"
    done
    ui_progress 100 "all models cached"
    ui_ok "Every model the OS uses is pre-downloaded into $HF_HOME_DIR"
fi

# ============================================================================
# STAGE 5 — BUILD FRESH (daemon + app crates + swift vision + HUD/Tauri .app)
# ============================================================================
ui_stage 5 "$TOTAL_STAGES" "BUILD FRESH"

# Cargo manifests to build --release, in dependency-friendly order.
CARGO_MANIFESTS=(
    "daemon/Cargo.toml"                 # jarvisd (the daemon)
    "apps/silicon-canvas/Cargo.toml"    # silicon-canvas
    "apps/mark-forge/Cargo.toml"        # mark-forge
    "apps/nexus/core/Cargo.toml"        # nexus_core
)

if [ "$MODE" = "check" ]; then
    for mf in "${CARGO_MANIFESTS[@]}"; do
        plan "$CARGO build --release --manifest-path \"$JARVIS_HOME/$mf\""
    done
    plan "swift build -c release --package-path \"$JARVIS_HOME/apps/vision\""
    plan "(cd \"$JARVIS_HOME/hud\" && npm ci && npm run tauri build)   # -> JARVIS.app"
    ui_note "every artifact is built FRESH in the install home (never shipped prebuilt)."
else
    for mf in "${CARGO_MANIFESTS[@]}"; do
        crate="$(dirname "$mf")"
        if [ -f "$JARVIS_HOME/$mf" ]; then
            ui_spin "cargo build --release ($crate)" -- \
                "$CARGO" build --release --manifest-path "$JARVIS_HOME/$mf"
        else
            ui_warn "skip $mf (not present in the home)"
        fi
    done
    # Verify the daemon binary landed (the LaunchAgent crash-loops without it).
    if [ -x "$JARVIS_HOME/daemon/target/release/jarvisd" ]; then
        ui_ok "daemon binary: daemon/target/release/jarvisd (fresh)"
    else
        ui_err "jarvisd not produced by the daemon build"; exit 1
    fi
    # Swift vision app.
    if [ -f "$JARVIS_HOME/apps/vision/Package.swift" ]; then
        ui_spin "swift build -c release (apps/vision)" -- \
            swift build -c release --package-path "$JARVIS_HOME/apps/vision"
    fi
    # HUD / Tauri release .app.
    if [ -f "$JARVIS_HOME/hud/package.json" ]; then
        ui_spin "npm ci (HUD deps)" -- bash -c "cd '$JARVIS_HOME/hud' && npm ci"
        ui_spin "npm run tauri build (-> JARVIS.app)" -- bash -c "cd '$JARVIS_HOME/hud' && npm run tauri build"
        APP_BUNDLE="$(find "$JARVIS_HOME/hud/src-tauri/target/release/bundle" -maxdepth 2 -name 'JARVIS.app' 2>/dev/null | head -1 || true)"
        if [ -n "$APP_BUNDLE" ]; then ui_ok "HUD bundled: $APP_BUNDLE"; else ui_warn "tauri build finished but JARVIS.app not located"; fi
    fi
    ui_ok "All release artifacts built fresh in the install home."
fi

# ============================================================================
# STAGE 6 — AUTOSTART (delegate to the existing scripts/install_boot.sh)
# ============================================================================
ui_stage 6 "$TOTAL_STAGES" "AUTOSTART"

BOOT_SCRIPT="$JARVIS_HOME/scripts/install_boot.sh"
[ -f "$BOOT_SCRIPT" ] || BOOT_SCRIPT="$SRC_ROOT/scripts/install_boot.sh"

if [ "$MODE" = "check" ]; then
    plan "\"$BOOT_SCRIPT\" --install   # renders + bootstraps com.jarvis.inference + com.jarvis.daemon"
    ui_note "AUTOSTART is delegated to the existing scripts/install_boot.sh (its"
    ui_note "launchctl logic is reused verbatim, not reimplemented)."
    if [ -x "$BOOT_SCRIPT" ]; then
        ui_info "Boot installer dry-run (read-only) preview:"
        "$BOOT_SCRIPT" 2>&1 | sed 's/^/      /' || true
    fi
else
    ui_info "Loading the 2 LaunchAgents via scripts/install_boot.sh --install ..."
    "$BOOT_SCRIPT" --install
    ui_ok "LaunchAgents installed + loaded (RunAtLoad starts them)."
fi

# ============================================================================
# STAGE 7 — FINISH
# ============================================================================
ui_stage 7 "$TOTAL_STAGES" "FINISH"

if [ "$MODE" = "check" ]; then
    ui_ok "Plan complete. This was a dry run — nothing was installed."
    ui_info "Run without --check to perform the install (add -y to auto-accept consent prompts)."
    exit 0
fi

ui_online
cat <<EOF

  ${UI_BOLD}${UI_CYAN}Next steps — honest, and nothing consequential is on yet:${UI_RESET}

  ${UI_G_INFO} ${UI_BRIGHT}TCC permissions${UI_RESET}: macOS will prompt for Accessibility, Microphone,
    and Screen Recording the first time JARVIS needs each. Grant them in
    System Settings > Privacy & Security when asked. They cannot be pre-granted.

  ${UI_G_INFO} ${UI_BRIGHT}API keys (optional, OFF until set)${UI_RESET}: the cloud LLM fallback and the
    ElevenLabs cloud voice tier stay disabled with no key. To enable, put
      export ANTHROPIC_API_KEY=...
      export ELEVENLABS_API_KEY=...
    in  ${UI_GREY}$JARVIS_HOME/state/env.sh${UI_RESET}  and  chmod 600  it (state/ is gitignored),
    or store them in the macOS Keychain. Local inference works fully offline
    with no key at all.

  ${UI_G_INFO} ${UI_BRIGHT}Voice wake word${UI_RESET}: say "JARVIS" to wake it once the daemon is up.
    Every consequential action still requires the master switch ON +
    per-action confirm + voice-id + policy/allowlist — none of which this
    installer turns on. Self-healing ships OFF.

  ${UI_G_INFO} ${UI_BRIGHT}Boot-to-JARVIS${UI_RESET}: for a deployment Mac, enable auto-login so the
    gui-domain agents start at power-on (see scripts/install_boot.sh checklist).
    Do not install these agents on a dev machine.

  ${UI_G_INFO} ${UI_BRIGHT}To remove JARVIS completely${UI_RESET}: run
    "$JARVIS_HOME/uninstall.sh"  (two typed confirmations; --dry-run to preview).

EOF
exit 0
