//! `darwind --app-new <name>` — the micro-app SCAFFOLDER.
//!
//! Generates a new sandboxed micro-app that is CORRECT BY CONSTRUCTION: it
//! imports the shared `apps/_sdk` harness (the socket loop lives in one place),
//! ships a manifest that PASSES the plugin-SDK validator (name==dir, a
//! well-formed exposed tool backed by `[permissions]`, `apps/_sdk` granted for
//! the harness import), and a test file that runs green. It writes ONLY under a
//! fresh `apps/<name>/` and REFUSES to touch an existing directory — it invents
//! no capability (the generated manifest grants only its own scratch dir + the
//! harness read) and deploys nothing at runtime.
//!
//! This is the INSTANT, OFFLINE, DETERMINISTIC complement to the LLM-driven
//! `--forge-goal` path: forge writes a PROPOSAL for review; the scaffolder writes
//! a ready-to-edit skeleton directly (a new dir only — no existing file is ever
//! overwritten, so there is nothing to review-gate). Template generation is PURE
//! and unit-tested; the one side effect (creating the dir + files) is a single
//! function that validates the generated manifest BEFORE writing anything.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// One generated file: a project-relative path and its contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    pub rel_path: String,
    pub contents: String,
}

/// The full scaffold for an app: the three files (main.py, manifest.toml,
/// test_<name>.py) a standard tool-exposing micro-app needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scaffold {
    pub app: String,
    pub files: Vec<GeneratedFile>,
}

/// Validate a proposed app name: a non-empty, lowercase, `[a-z0-9-]` slug that
/// is a legal directory name AND a legal `[app].name` (which must equal the dir).
/// Rejects anything that could escape the apps/ dir or fail manifest validation.
pub fn validate_app_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("app name is empty");
    }
    if name.len() > 48 {
        bail!("app name too long (max 48 chars)");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("app name {name:?} must be lowercase letters, digits, and hyphens only");
    }
    if name.starts_with('-') || name.ends_with('-') {
        bail!("app name {name:?} must not start or end with a hyphen");
    }
    if name.contains("--") {
        // A double hyphen would become an empty dotted segment in the tool name.
        bail!("app name {name:?} must not contain consecutive hyphens");
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        bail!("app name {name:?} must start with a lowercase letter");
    }
    Ok(())
}

/// The dotted tool id a scaffolded app exposes: `<name-with-hyphens-as-dots>.run`
/// — a well-formed dotted-lowercase id (hyphens are not allowed in a tool-name
/// segment, so they become dots, and every segment is non-empty by the name
/// rules). E.g. `url-shortener` -> `url.shortener.run`.
pub fn default_tool_name(app: &str) -> String {
    format!("{}.run", app.replace('-', "."))
}

/// Generate the scaffold for `app` (assumes `validate_app_name` passed). PURE:
/// no I/O. The generated app echoes its input back under `result`/`items` via the
/// shared harness, so it is a working, testable skeleton the author then fills in.
pub fn generate(app: &str) -> Scaffold {
    let tool = default_tool_name(app);
    let test_mod = format!("test_{}", app.replace('-', "_"));

    let main_py = format!(
        r#"#!/usr/bin/env python3
"""{app} — a scaffolded DARWIN micro-app. Replace compute() with real logic.

Runs under the daemon-generated default-deny seatbelt profile and connects to its
own per-app JSONL socket via the shared apps/_sdk harness (which owns the socket
loop, token stamping, framing, and the agent-tool request-id echo)."""
import os
import sys

# Shared host-link plumbing from apps/_sdk (fs_read-granted). __file__-relative so
# it resolves both when darwind launches the app (cwd = project root) and when the
# tests run from the app dir. Bytecode writes are off (apps/_sdk is read-only).
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "_sdk"))
from harness import (  # noqa: E402 — must follow the sys.path insert above
    MAX_FRAME_BYTES,
    TOKEN,
    drain_lines,
    reply_result,
    run,
    send,
)


def compute(payload):
    """PURE, offline, no I/O, never raises. Replace with the app's real logic.

    Reads its inputs from `payload` (a dict, the op object) and returns a plain
    JSON-serializable dict, or {{"error": "..."}} on bad input. This skeleton
    echoes the payload's `text` field back so the app works end-to-end today."""
    try:
        if not isinstance(payload, dict):
            return {{"error": "payload must be a mapping"}}
        text = payload.get("text", "")
        if not isinstance(text, str):
            return {{"error": "text must be a string"}}
        return {{"echo": text, "length": len(text)}}
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {{"error": "unexpected: %s" % e}}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {{"type": "status", "data": {{"tool": "{tool}", "ready": True}}}})
    elif op == "refresh":
        send(conn, {{"type": "items", "data": {{"status": "ok"}}}})
    elif op == "{tool}":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
"#,
        app = app,
        tool = tool,
    );

    let manifest = format!(
        r#"# {app} — scaffolded micro-app manifest. See docs/PLUGIN_SDK.md + docs/SANDBOX.md.
# [app].name MUST equal this directory name. Permissions are DEFAULT-DENY: this
# grants only the app's own scratch dir (fs_write) and the shared apps/_sdk
# harness (fs_read) — add net_hosts / fs_read / gpu / etc. ONLY as the app needs
# them; every grant widens the seatbelt sandbox.
[app]
name        = "{app}"
version     = "0.1.0"
description = "A scaffolded micro-app. Replace with a real one-line description."
entry       = "apps/{app}/main.py"
runtime     = "python"

[permissions]
audio     = false
gpu       = false
net_hosts = []
fs_read   = ["apps/{app}", "apps/_sdk"]
fs_write  = ["state/apps/{app}"]

[ui]
surface         = "panel"
telemetry_topics = ["{app}.status"]

# The one tool this app exposes to the agent loop. consequential = false means it
# is PURE local compute (auto-invocable); a side-effecting tool must set true and
# rides the confirmation gate. `description` + params render into the agent def.
[[tools.exposes]]
name          = "{tool}"
scopes        = []
consequential = false
description   = "Scaffolded tool — replace with what this app computes and when to call it."

[[tools.exposes.params]]
name        = "text"
kind        = "string"
required    = false
description  = "Scaffolded input field — replace with the app's real parameters."
"#,
        app = app,
        tool = tool,
    );

    let test_py = format!(
        r#"#!/usr/bin/env python3
"""Tests for {app}. Run: `python3 {test_mod}.py`."""
import json

import main


class FakeConn:
    """Captures sendall payloads so handle() can be driven without a socket."""

    def __init__(self):
        self.lines = []

    def sendall(self, raw):
        self.lines.append(json.loads(raw.decode("utf-8").strip()))


def check(name, cond):
    if not cond:
        print("FAIL:", name)
        raise SystemExit(1)
    print("ok:", name)


# -- compute (replace these as you implement real logic) ---------------------


def test_compute_happy_path():
    r = main.compute({{"text": "hello"}})
    check("echoes text", r.get("echo") == "hello")
    check("reports length", r.get("length") == 5)


def test_compute_rejects_bad_input():
    check("non-dict -> error", "error" in main.compute("nope"))
    check("non-string text -> error", "error" in main.compute({{"text": 7}}))


# -- SHARED framing tests (identical across every micro-app) ------------------


def test_max_frame_bytes_is_8_mib():
    check("MAX_FRAME_BYTES == 8 MiB", main.MAX_FRAME_BYTES == 8 * 1024 * 1024)


def test_oversized_frame_is_dropped():
    lines, buf, overflowed = main.drain_lines(b"x" * (main.MAX_FRAME_BYTES + 1))
    check("overflowed", overflowed is True)
    check("dropped", buf == b"")
    check("no lines", lines == [])


def test_complete_lines_drain_partial_preserved():
    lines, buf, overflowed = main.drain_lines(b'{{"a":1}}\n{{"b":2}}\n{{"c":3')
    check("complete lines", lines == [b'{{"a":1}}', b'{{"b":2}}'])
    check("partial preserved", buf == b'{{"c":3')
    check("no overflow", overflowed is False)


# -- agent-tool request/response contract -------------------------------------


def test_tool_op_with_id_answers_a_correlated_result():
    conn = FakeConn()
    main.handle(conn, {{"type": "{tool}", "id": "req-1", "text": "hi"}})
    r = conn.lines[0]
    check("id -> result line", r["type"] == "result")
    check("id echoed", r["id"] == "req-1")
    check("token stamped", r["token"] == main.TOKEN)


def test_tool_op_without_id_is_legacy_items():
    conn = FakeConn()
    main.handle(conn, {{"type": "{tool}", "text": "hi"}})
    check("no id -> items", conn.lines[0]["type"] == "items")


if __name__ == "__main__":
    for t in [
        test_compute_happy_path,
        test_compute_rejects_bad_input,
        test_max_frame_bytes_is_8_mib,
        test_oversized_frame_is_dropped,
        test_complete_lines_drain_partial_preserved,
        test_tool_op_with_id_answers_a_correlated_result,
        test_tool_op_without_id_is_legacy_items,
    ]:
        t()
    print("ALL PASSED")
"#,
        app = app,
        test_mod = test_mod,
        tool = tool,
    );

    Scaffold {
        app: app.to_string(),
        files: vec![
            GeneratedFile { rel_path: format!("apps/{app}/main.py"), contents: main_py },
            GeneratedFile { rel_path: format!("apps/{app}/manifest.toml"), contents: manifest },
            GeneratedFile { rel_path: format!("apps/{app}/{test_mod}.py"), contents: test_py },
        ],
    }
}

/// Create the scaffolded app under `project_root`. Validates the name AND the
/// generated manifest (via the plugin-SDK validator) BEFORE writing anything,
/// and REFUSES if `apps/<name>/` already exists — so it can never clobber an
/// existing app or write a manifest the launcher would reject. Returns the paths
/// written. The ONLY side effect; the generation it wraps is pure.
pub fn scaffold_app(project_root: &Path, name: &str) -> Result<Vec<PathBuf>> {
    validate_app_name(name)?;
    let app_dir = project_root.join("apps").join(name);
    if app_dir.exists() {
        bail!(
            "apps/{name}/ already exists — the scaffolder never overwrites an existing app; \
             pick a new name or remove it first"
        );
    }
    let scaffold = generate(name);

    // Validate the generated manifest through the REAL plugin-SDK validator (the
    // same contract the launcher enforces) before touching disk — a scaffold
    // that wouldn't launch is a bug in the template, caught here, never written.
    let manifest_src = &scaffold
        .files
        .iter()
        .find(|f| f.rel_path.ends_with("manifest.toml"))
        .expect("generate() always emits a manifest")
        .contents;
    crate::plugin_sdk::validate_manifest(manifest_src, name)
        .map_err(|e| anyhow::anyhow!("generated manifest is invalid (template bug): {e}"))?;

    std::fs::create_dir_all(&app_dir)?;
    let mut written = Vec::new();
    for f in &scaffold.files {
        let path = project_root.join(&f.rel_path);
        std::fs::write(&path, &f.contents)?;
        written.push(path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_app_name_accepts_slugs_and_rejects_junk() {
        for ok in ["hello", "url-shortener", "app2", "a", "time-zone-2"] {
            validate_app_name(ok).unwrap_or_else(|e| panic!("{ok:?} should be valid: {e}"));
        }
        for bad in ["", "-x", "x-", "Hello", "a/b", "a b", "a--b", "2app", ".hidden", "app_x"] {
            assert!(validate_app_name(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn default_tool_name_dots_the_hyphens() {
        assert_eq!(default_tool_name("hello"), "hello.run");
        assert_eq!(default_tool_name("url-shortener"), "url.shortener.run");
    }

    #[test]
    fn generated_manifest_validates_through_the_real_plugin_sdk() {
        for app in ["hello", "url-shortener"] {
            let s = generate(app);
            let manifest = &s
                .files
                .iter()
                .find(|f| f.rel_path.ends_with("manifest.toml"))
                .unwrap()
                .contents;
            let m = crate::plugin_sdk::validate_manifest(manifest, app)
                .unwrap_or_else(|e| panic!("scaffold for {app:?} must validate: {e}"));
            assert_eq!(m.name(), app);
            assert_eq!(m.tools.exposes.len(), 1);
            assert_eq!(m.tools.exposes[0].name, default_tool_name(app));
            assert!(!m.tools.exposes[0].consequential);
            // The harness read grant is present (else the import crashes at launch).
            assert!(m.permissions.fs_read.iter().any(|p| p == "apps/_sdk"));
        }
    }

    #[test]
    fn generated_main_py_imports_the_harness_and_serves_the_tool() {
        let s = generate("hello");
        let main = &s.files.iter().find(|f| f.rel_path.ends_with("main.py")).unwrap().contents;
        assert!(main.contains("from harness import"));
        assert!(main.contains("sys.dont_write_bytecode = True"));
        assert!(main.contains("os.path.dirname(os.path.abspath(__file__))"));
        assert!(main.contains("reply_result(conn, msg"));
        assert!(main.contains("== \"hello.run\""), "serves the exposed tool");
        assert!(main.contains("sys.exit(run(handle))"));
    }

    #[test]
    fn generated_test_carries_the_framing_and_contract_tests() {
        let s = generate("hello");
        let t = &s
            .files
            .iter()
            .find(|f| f.rel_path.ends_with("test_hello.py"))
            .unwrap()
            .contents;
        assert!(t.contains("MAX_FRAME_BYTES"));
        assert!(t.contains("test_tool_op_with_id_answers_a_correlated_result"));
        assert!(t.contains("class FakeConn"));
        assert!(t.contains("ALL PASSED"));
    }

    #[test]
    fn scaffold_app_writes_the_three_files_and_refuses_to_clobber() {
        let root = std::env::temp_dir().join(format!("darwin-scaffold-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("apps")).unwrap();

        let written = scaffold_app(&root, "hello").expect("scaffold writes");
        assert_eq!(written.len(), 3);
        assert!(root.join("apps/hello/main.py").is_file());
        assert!(root.join("apps/hello/manifest.toml").is_file());
        assert!(root.join("apps/hello/test_hello.py").is_file());

        // Second scaffold of the same name REFUSES (never clobbers).
        assert!(scaffold_app(&root, "hello").is_err(), "must refuse an existing app dir");

        // A bad name never touches disk.
        assert!(scaffold_app(&root, "Bad Name").is_err());
        assert!(!root.join("apps/Bad Name").exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
