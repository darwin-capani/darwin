//! Silicon Canvas binary entry — the daemon-launched micro-app process.
//!
//! Runtime contract (`daemon/src/apps.rs`, runtime = "binary"): darwind execs
//! this binary directly under `sandbox-exec`, handing it the socket + token via
//! the environment (NEVER argv — argv is world-readable via `ps`):
//!   - `DARWIN_APP_SOCKET` — abs path of the per-app Unix socket
//!   - `DARWIN_APP_TOKEN`  — the capability token to stamp on every line
//!   - `DARWIN_APP_NAME`   — "silicon-canvas"
//!
//! Like global-scan's `main()`, it REFUSES to run standalone (no socket/token):
//! this app only runs under the daemon, so an accidental direct invocation exits
//! with code 2 rather than binding anything. When the env is present it connects
//! to the daemon's socket and hands control to [`silicon_canvas::ipc::run`], the
//! JSONL op/telemetry loop.
//!
//! HARD SAFETY (never violated here): this binary binds NO listener (it CONNECTS
//! to the daemon's socket), opens NO window, touches NO GPU, and plays NO audio.
//!
//! THE GPU RENDERER IS NOT DRIVEN FROM THIS BINARY AT ALL — say so plainly. This
//! comment used to read "[it] is only ever entered from inside the ipc loop on an
//! explicit, device-present path", which described a wiring that does not exist:
//! nothing in the crate constructs or calls `render::GpuRenderer` (grep for it —
//! it appears in render.rs and its own tests and nowhere else), `ipc.rs` mentions
//! the renderer only in prose, and install.sh builds this crate with a bare
//! `cargo build --release` with no `--features gpu`, so the SHIPPED binary does
//! not even contain render.rs. Consequently `canvas.render_ms` — one of the three
//! topics manifest.toml declares, with its own P50/P95/DRAWS/CULLED strip in the
//! HUD's SiliconCanvasPanel — has no code path that can produce a payload, with
//! or without the feature and with or without a GPU, and those cells can only
//! ever show "—". The safety claim above is true; the "entered from inside the
//! ipc loop" claim was not, and reading it as a description of live wiring is the
//! mistake it invited.

use std::process::ExitCode;

use silicon_canvas::error::CanvasError;
use silicon_canvas::ipc::{self, AppEnv};

fn main() -> ExitCode {
    // Read the launch env exactly as apps.rs / global-scan establish it. Absent
    // the socket or token, we are not running under darwind — refuse with exit
    // code 2 (matching global-scan's "must be set" contract), binding nothing.
    let env = match AppEnv::from_env() {
        Ok(env) => env,
        Err(CanvasError::Unauthorized) => {
            eprintln!(
                "silicon-canvas: DARWIN_APP_SOCKET and DARWIN_APP_TOKEN must be set \
                 (this app runs under darwind, not standalone)"
            );
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("silicon-canvas: cannot read launch environment: {e}");
            return ExitCode::from(2);
        }
    };

    // Connect to the daemon's per-app socket and run the JSONL op/telemetry loop.
    // ipc::run connects (never binds), reads host commands + ops, and writes
    // token-stamped telemetry back. It returns Ok on a clean `stop` / socket
    // close. No GPU/window/audio is touched on this path.
    match ipc::run(env) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("silicon-canvas: {e}");
            ExitCode::FAILURE
        }
    }
}
