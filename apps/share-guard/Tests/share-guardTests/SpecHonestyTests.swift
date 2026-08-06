// SpecHonestyTests.swift — SPEC.md is part of the honesty contract, so a stale
// claim in it is a defect like any other. This reads the doc off disk (located
// from #filePath, no test-bundle resources needed) and cross-checks the one
// paragraph that described a shipped feature as unbuilt.

import XCTest
@testable import share_guard

final class SpecHonestyTests: XCTestCase {

    /// apps/share-guard, derived from this file's own path.
    private var packageRoot: URL {
        URL(fileURLWithPath: #filePath)      // .../Tests/share-guardTests/SpecHonestyTests.swift
            .deletingLastPathComponent()      // .../Tests/share-guardTests
            .deletingLastPathComponent()      // .../Tests
            .deletingLastPathComponent()      // .../apps/share-guard
    }

    /// The repo root, four levels above this file (apps/share-guard/Tests/<target>).
    private var repoRoot: URL {
        packageRoot.deletingLastPathComponent().deletingLastPathComponent()
    }

    private func read(_ url: URL, _ label: String) throws -> String {
        XCTAssertTrue(FileManager.default.fileExists(atPath: url.path),
                      "\(label) not found at \(url.path) — this test must not pass vacuously")
        return try String(contentsOf: url, encoding: .utf8)
    }

    // REGRESSION: SPEC.md labelled the Artifact Registry bridge "(daemon-side,
    // deferred)" long after commit cb47c82 shipped it. A reader auditing the honesty
    // contract concluded the daemon->app bridge was unbuilt and that Share Guard was
    // unreachable in production — while `share_guard_scrub` was in fact a declared
    // agent tool in the live roster, which is exactly what makes this app's detector
    // behaviour user-reachable rather than latent.
    func testArtifactRegistryBridgeIsNotDescribedAsDeferred() throws {
        let spec = try read(packageRoot.appendingPathComponent("SPEC.md"), "SPEC.md")
        guard let line = spec.split(separator: "\n", omittingEmptySubsequences: false)
            .first(where: { $0.contains("**Artifact Registry integration") }) else {
            return XCTFail("SPEC.md no longer has an Artifact Registry integration paragraph")
        }
        XCTAssertFalse(line.lowercased().contains("deferred"),
                       "the bridge ships; SPEC.md still calls it deferred: \(line)")
        XCTAssertTrue(line.contains("SHIPPED"), "say so explicitly: \(line)")
        // The claim is checked against the daemon, not just asserted as prose.
        XCTAssertTrue(spec.contains("preview_payload"),
                      "the text path forwards the clamped preview, not the full body")
    }

    // The symbols SPEC.md now points at have to exist, or the correction is just a
    // different lie. (A comment stating a rule is not the rule.)
    func testTheShippedBridgeSymbolsSpecMdNamesActuallyExist() throws {
        let artifactRS = try read(
            repoRoot.appendingPathComponent("daemon/src/artifact.rs"), "daemon/src/artifact.rs")
        XCTAssertTrue(artifactRS.contains("pub fn scrub_forward("))
        XCTAssertTrue(artifactRS.contains("pub fn resolve_for_scrub("))
        XCTAssertTrue(artifactRS.contains("MAX_PREVIEW_LEN"))

        let anthropicRS = try read(
            repoRoot.appendingPathComponent("daemon/src/anthropic.rs"), "daemon/src/anthropic.rs")
        XCTAssertTrue(anthropicRS.contains("async fn share_guard_scrub_tool("))
        XCTAssertTrue(anthropicRS.contains("\"name\": \"share_guard_scrub\""))

        let agentsRS = try read(
            repoRoot.appendingPathComponent("daemon/src/agents.rs"), "daemon/src/agents.rs")
        XCTAssertTrue(agentsRS.contains("\"share_guard_scrub\""),
                      "the tool is in an agent's live roster")
    }
}
