// TEST-ONLY. The commands that document the LOCAL gate set are themselves a
// contract, and this module is where that contract is enforced.
//
// WHY IT HAS TO EXIST. `.github/workflows/release.yml` triggers only on a pushed
// `v*` tag — there is no PR check and no CI on a push to main — so the commands a
// contributor is TOLD to run are the entire merge gate. Three test populations were
// found sitting outside it, and two were invisible for the same reason: the
// document that names the gate named the wrong command.
//
// - `hud/src-tauri` has 158 tests covering the config whitelist, control-character
//   rejection, path-absoluteness and duplicate rejection — the surface that decides
//   WHAT MAY BE WRITTEN INTO `config/darwin.toml`. `hud/README.md` documented
//   `cargo check` for "Rust shell", and `cargo check` does not set `--cfg test`: it
//   never compiles `#[cfg(test)] mod tests` at all. The tag build does run them,
//   which is long after the moment they would have caught something.
// - `apps/nexus/core`'s manifest prescribed the `cargo check` form of its feature
//   command — same defect, four `io_proc_mix` tests reachable by no documented
//   command (102 tests without the feature, 106 with).
// - `apps/silicon-canvas`'s manifest already prescribes `cargo test --features gpu`
//   (22 render.rs tests, 167 -> 189). It is pinned here so it cannot slide back.
//
// And this module is inside `cargo test --bin darwind`, which IS run — a guard
// parked in a suite nobody runs is the very defect being guarded against. The
// cross-tree `include_str!` is the pattern `config.rs` and `command.rs` already use
// to hold the daemon/HUD wire contract together; `scripts/apply_heal.sh --selftest`
// independently proves every such target is copied into a heal staging tree (its
// count moves 43 -> 46 with these three).

/// The bounded body of a section, `start..` up to `end` (exclusive), asserted
/// non-empty at both ends. A window that binds to nothing passes every `contains`
/// check that follows it — so the binding is proved before anything is read from it.
fn section<'a>(doc: &'a str, what: &str, start: &str, end: &str) -> &'a str {
    let i = doc
        .find(start)
        .unwrap_or_else(|| panic!("{what}: the {start:?} section is gone; re-point this guard"));
    let rest = &doc[i + start.len()..];
    let j = rest
        .find(end)
        .unwrap_or_else(|| panic!("{what}: the {start:?} section has no {end:?} terminator"));
    let body = &rest[..j];
    assert!(
        !body.trim().is_empty(),
        "{what}: the {start:?} window bound to an EMPTY span — every assertion \
         below it would be vacuous"
    );
    body
}

/// `hud/README.md` must send a contributor to `cargo test` for the Tauri shell.
///
/// The NEGATIVE half is scoped to the fenced command block, not to the section:
/// the prose below the fence explains why `cargo check` was the wrong command, and
/// a guard that forbade the phrase anywhere would forbid the retraction that keeps
/// it from coming back. What must not name `cargo check` is the list of commands
/// someone copies.
#[test]
fn the_hud_readme_documents_running_the_src_tauri_tests_not_just_checking_them() {
    let readme = include_str!("../../hud/README.md");
    let sect = section(readme, "hud/README.md", "## Tests & checks", "\n## ");
    let cmds = section(sect, "hud/README.md Tests & checks", "```sh", "```");
    assert!(
        cmds.contains("src-tauri") && cmds.contains("cargo test"),
        "hud/README.md's Tests & checks command block must name `cargo test` for \
         src-tauri: its 158 tests are the gate on what may be written into \
         config/darwin.toml, and CI runs them only on a v* tag"
    );
    assert!(
        !cmds.contains("cargo check"),
        "hud/README.md's command block presents `cargo check` as the src-tauri \
         check. `cargo check` does not set --cfg test and never compiles \
         `#[cfg(test)] mod tests`, so it proves nothing about those 158 tests"
    );
}

/// `apps/nexus/core` — same defect, second crate. The `[features]` block is the
/// paragraph someone reads when they turn the feature on, so that is where the
/// prescription is pinned; the header prose above it is free to name the old
/// command in order to retract it (silicon-canvas's manifest does exactly that).
#[test]
fn the_nexus_core_manifest_documents_testing_the_coreaudio_feature_not_checking_it() {
    let manifest = include_str!("../../apps/nexus/core/Cargo.toml");
    let feats = section(
        manifest,
        "apps/nexus/core/Cargo.toml",
        "[features]",
        "\n[dev-dependencies]",
    );
    assert!(
        feats.contains("cargo test --features coreaudio"),
        "apps/nexus/core/Cargo.toml's [features] block must prescribe `cargo test \
         --features coreaudio`; it is the only command that compiles AND RUNS the 4 \
         device-free io_proc_mix tests behind that feature (102 tests without it, \
         106 with)"
    );
    assert!(
        !feats.contains("cargo check --features coreaudio"),
        "apps/nexus/core/Cargo.toml's [features] block is back to prescribing \
         `cargo check --features coreaudio` — which never compiles `#[cfg(test)]`, \
         leaving those 4 tests reachable by no documented command"
    );
}

/// `daemon` itself — THE THIRD CRATE WITH THE SAME DEFECT, found by enumerating
/// every `[features]` block in the tree rather than by guessing which crate to look
/// at. There are exactly four features in four manifests: `coreaudio`
/// (apps/nexus/core), `gpu` (apps/silicon-canvas), `endpoint-security` (here), and
/// none at all in apps/mark-forge, apps/silicon-canvas/fuzz or hud/src-tauri. Two
/// were fixed above. The third was still prescribing a BUILD.
///
/// `daemon/src/main.rs` declares `mod es;` under `#[cfg(feature =
/// "endpoint-security")]`, so `es::tests::map_event_covers_each_kind_and_rejects_unknown`
/// — the pure kind→`SecurityEvent` mapping the whole ES path funnels through — does
/// not exist under the documented gate; naming it there reports "0 filtered out"
/// rather than failing. MEASURED: 3519 tests without the feature, 3520 with, and
/// linking `-lEndpointSecurity`/`-lbsm` needs no entitlement (that gate is at
/// runtime), so the command runs off-device.
///
/// Pinned in the `[features]` block, like nexus's: that is the paragraph someone
/// reads when they turn the feature on.
///
/// The nexus/silicon-canvas guards above forbid the string `cargo check --features
/// X` anywhere in their window, which works only because their retraction prose
/// lives ABOVE `[features]`. This manifest's retraction ("a build or a check proves
/// nothing about that test") is INSIDE the block, so a blanket string ban would
/// either fail on correct text or have to be shaped around the exact sentence I
/// wrote — a guard fitted to its own author. Instead the block declares its
/// prescription on ONE line after `VERIFY IT WITH: `, and the command is extracted
/// from there and judged: the surrounding prose may say whatever it needs to,
/// including naming the wrong commands in order to retract them.
#[test]
fn the_daemon_manifest_documents_testing_the_endpoint_security_feature_not_building_it() {
    let manifest = include_str!("../Cargo.toml");
    let feats = section(
        manifest,
        "daemon/Cargo.toml",
        "[features]",
        "\n[build-dependencies]",
    );
    assert!(
        feats.contains("endpoint-security = []"),
        "the window missed the key it documents — every assertion below it would be \
         vacuous:\n{feats}"
    );
    // The prescription is exactly the text between `VERIFY IT WITH: ` and the end of
    // that line — bounded at BOTH ends, so the retraction prose below it is out of
    // scope and cannot stand in for the real command either way.
    let lead = "VERIFY IT WITH: ";
    let at = feats
        .find(lead)
        .unwrap_or_else(|| panic!("daemon/Cargo.toml's [features] block no longer declares a \
             `{lead}` line; the feature is back to having no documented command at all:\n{feats}"));
    let after = &feats[at + lead.len()..];
    let cmd = after[..after.find('\n').unwrap_or(after.len())].trim();
    assert!(
        cmd.len() > 20,
        "the prescribed command extracted as {cmd:?} — too short to be a command, so \
         every check below it would be vacuous"
    );
    assert!(
        cmd.starts_with("cargo test "),
        "daemon/Cargo.toml prescribes `{cmd}`. Only `cargo test` sets --cfg test; a \
         build or a check never compiles `#[cfg(test)] mod tests`, which is the exact \
         defect that left the nexus and silicon-canvas tests reachable by no \
         documented command"
    );
    assert!(
        cmd.contains("--features endpoint-security"),
        "the prescribed command `{cmd}` does not name the feature, so it runs the \
         DEFAULT gate — under which es.rs does not exist and naming its test reports \
         \"0 filtered out\" rather than failing"
    );
    assert!(
        cmd.contains("--manifest-path daemon/Cargo.toml") || cmd.contains("-p darwin-core"),
        "the prescribed command `{cmd}` does not say WHICH crate to run it in; the \
         documented gate set is run from the repo root"
    );
}

/// THE FOURTH UNREACHABLE SHELL HARNESS — the same defect a SIXTH time.
/// `scripts/test_shell_sandbox.sh` covers the sandboxed-shell surface (#43): the
/// classifier denylist plus obfuscation, the deny-default / no-network /
/// scratch-only SBPL profile text, `shell_run`'s `CONSEQUENTIAL_TOOLS` membership,
/// and `[shell].enabled` shipping true. It RUNS and PASSES today — MEASURED: 14
/// tests across four cargo filters, 0.87s warm (best of three, M1 Pro) against an
/// already-built `daemon/target`, 217s cold because the first run pays the debug
/// build — and it was named by no runnable command and by no guard.
///
/// It has no installer to be reached FROM, so it is wired like `test_doc_claims.sh`:
/// a documented command block plus this guard.
///
/// Scoped to the FENCED BLOCK, not the section: the prose around it names the script
/// in order to explain what it covers, and `docs/BRINGUP.md` ALREADY named it before
/// this — as "still outstanding". A guard that looked for the string anywhere in the
/// section would therefore have passed on the exact broken state being fixed.
#[test]
fn the_bringup_doc_documents_running_the_shell_sandbox_selftest() {
    let doc = include_str!("../../docs/BRINGUP.md");
    let sect = section(
        doc,
        "docs/BRINGUP.md",
        "## Surface gate — the shell sandbox (#43)",
        "\n## ",
    );
    let cmds = section(sect, "docs/BRINGUP.md Surface gate", "```sh", "```");
    assert!(
        cmds.contains("scripts/test_shell_sandbox.sh"),
        "docs/BRINGUP.md's Surface gate command block does not name \
         `scripts/test_shell_sandbox.sh`. CI runs on a pushed v* tag only, so the \
         commands a contributor is told to run ARE the merge gate — and this harness, \
         unlike the three installer ones, has no script of its own to be reached \
         from.\nblock was:\n{cmds}"
    );
    // Both measured costs must stay stated, not rediscovered. The cold one is the
    // number that surprises whoever runs this first.
    assert!(
        sect.contains("0.87s") && sect.contains("217s"),
        "docs/BRINGUP.md's Surface gate section no longer states the MEASURED warm \
         (0.87s) and cold (217s) cost of scripts/test_shell_sandbox.sh, so its price \
         is something the next contributor discovers instead of reads"
    );
}

/// `apps/silicon-canvas` got this right first; pin it so it cannot slide back.
#[test]
fn the_silicon_canvas_manifest_keeps_documenting_the_gpu_test_command() {
    let manifest = include_str!("../../apps/silicon-canvas/Cargo.toml");
    let feats = section(
        manifest,
        "apps/silicon-canvas/Cargo.toml",
        "[features]",
        "\n[dev-dependencies]",
    );
    assert!(
        feats.contains("cargo test --features gpu"),
        "apps/silicon-canvas/Cargo.toml's [features] block must prescribe `cargo \
         test --features gpu`: render.rs is #[cfg(feature = \"gpu\")], so under a \
         default `cargo test` its 22 unit tests DO NOT EXIST (167 tests without the \
         feature, 189 with) and asking for one by name reports \"0 filtered out\" \
         rather than failing"
    );
    assert!(
        !feats.contains("cargo check --features gpu"),
        "apps/silicon-canvas/Cargo.toml's [features] block is back to prescribing \
         `cargo check --features gpu`, the command its own header comment records \
         as the one that proved nothing"
    );
}

/// THE INSTALLER / UNINSTALLER LIFECYCLE HARNESSES — the same defect a FIFTH time,
/// in shell rather than in Rust. `scripts/test_install_boot.sh` and
/// `scripts/test_install_config_preserved.sh` both existed, both passed, and a grep
/// over `docs/*.md` plus this module found NEITHER: no documented command ran them
/// and no guard required one to. `uninstall.sh` — 393 lines of `rm -rf`,
/// `launchctl bootout`, `pkill` and `security delete-generic-password` — had no
/// teardown harness at all (`scripts/test_doc_claims.sh` drives its `guard_home`
/// and pins its `--help` text; nothing exercised the teardown), so
/// `scripts/test_uninstall_footprint.sh` was written for it.
///
/// All three are now reached FROM the script each one tests (`./install.sh
/// --selftest`, `./uninstall.sh --selftest`, `scripts/install_boot.sh --selftest`)
/// — the shape `scripts/apply_heal.sh --selftest` and `scripts/apply_code_diff.sh
/// --selftest` already use. This guard pins the DOCUMENTED half: the section of
/// `docs/BRINGUP.md` a contributor reads before touching an installer must name all
/// three commands. MEASURED on an M1 Pro, best of three warm runs of THOSE COMMANDS (not of
/// the bare harnesses): 0.96s + 0.81s + 0.28s + 0.85s = 2.90s -> 2.9s total, and
/// the check counts are 30 / 23 / 6 / 28. `test_doc_claims.sh` is in the list
/// because 14 of its 28 checks have install.sh, uninstall.sh or install_boot.sh as their
/// subject — among them the
/// only one that DRIVES uninstall.sh's guard_home — and it was reachable by no
/// documented command either.
///
/// Scoped to the fenced command block, not the section: the prose below the fence
/// explains what the uninstall harness pins and legitimately mentions the scripts by
/// name, so a guard that only looked for the strings anywhere in the section would
/// pass on prose that named no runnable command at all.
#[test]
fn the_bringup_doc_documents_running_the_installer_lifecycle_selftests() {
    let doc = include_str!("../../docs/BRINGUP.md");
    let sect = section(
        doc,
        "docs/BRINGUP.md",
        "## Lifecycle gates — run these before merging a change to the installers",
        "\n## ",
    );
    let cmds = section(sect, "docs/BRINGUP.md Lifecycle gates", "```sh", "```");
    for cmd in [
        "./install.sh --selftest",
        "./uninstall.sh --selftest",
        "scripts/install_boot.sh --selftest",
        "scripts/test_doc_claims.sh",
    ] {
        assert!(
            cmds.contains(cmd),
            "docs/BRINGUP.md's Lifecycle gates command block does not name `{cmd}`. \
             CI runs on a pushed v* tag only, so the commands a contributor is told to \
             run ARE the merge gate — a lifecycle harness named by no command is a test \
             that exists and runs nowhere, which is how the 158 src-tauri tests and \
             three feature-gated suites were lost.\nblock was:\n{cmds}"
        );
    }
    // The measured cost must stay stated, not rediscovered. The three runtimes are
    // the reason this is cheap enough to be a per-merge gate at all.
    assert!(
        sect.contains("2.9s"),
        "docs/BRINGUP.md's Lifecycle gates section no longer states the MEASURED total \
         runtime of the three harnesses, so their cost is something the next contributor \
         discovers instead of reads"
    );
}

/// THE PRUNE HARNESS MUST BE REACHABLE, for the same reason its three siblings
/// are: CI runs on a pushed v* tag only, so the commands a contributor is told to
/// run ARE the merge gate. This one guards the ONLY destructive thing install.sh
/// does, and the property that makes the prune safe — a runtime-created path that
/// never appeared in a manifest is untouched, so forge-generated apps survive —
/// has no other test anywhere in the tree.
#[test]
fn the_bringup_doc_documents_running_the_install_prune_harness() {
    let doc = include_str!("../../docs/BRINGUP.md");
    assert!(
        doc.contains("scripts/test_install_prune.sh"),
        "docs/BRINGUP.md never tells anyone to run scripts/test_install_prune.sh, so \
         the guard on install.sh's destructive path runs nowhere"
    );
    assert!(
        doc.contains("Measured cost: 0.08s"),
        "the prune harness's measured cost is not stated beside it — the sibling \
         gates all state theirs so the price is known rather than discovered"
    );
}

/// THE ONLY DOCUMENTED CHECK THAT PROVES DARWIN ANSWERS. Every selfcheck leg and
/// every doctor check is a PRECONDITION — a dir, a port, a model file, a socket
/// that accepts a connection — and a daemon can pass all of them while never
/// replying. That gap existed for the whole life of this repo. If the command
/// stops being documented, the gap silently returns.
#[test]
fn the_bringup_doc_documents_the_end_to_end_answer_check() {
    let doc = include_str!("../../docs/BRINGUP.md");
    assert!(
        doc.contains("scripts/doctor.sh --ask"),
        "docs/BRINGUP.md no longer tells anyone how to verify DARWIN actually ANSWERS; \
         every other documented check only proves it is installed"
    );
    assert!(
        doc.contains("PRECONDITION"),
        "the doc must say WHY --ask is different from the checks around it, or the \
         next reader deletes it as redundant"
    );
}
