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
