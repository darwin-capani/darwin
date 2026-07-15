// SandboxTests.swift — WRITE-CONFINEMENT tests: the redacted copy is written ONLY
// under the app's own sandbox dir, never anywhere else. Drives a real temp dir as
// the sandbox root (so nothing in the repo is touched) and asserts both the
// lexical rejection of escaping names and the FS-aware confinement of a real write.

import XCTest
@testable import share_guard

final class SandboxTests: XCTestCase {

    private var tmpRoot: String!
    private var sandbox: SandboxRoot!

    override func setUpWithError() throws {
        let base = NSTemporaryDirectory()
        tmpRoot = (base as NSString).appendingPathComponent("share-guard-test-\(UUID().uuidString)")
        try FileManager.default.createDirectory(atPath: tmpRoot, withIntermediateDirectories: true)
        sandbox = SandboxRoot(absoluteRoot: tmpRoot)
    }

    override func tearDownWithError() throws {
        if let tmpRoot { try? FileManager.default.removeItem(atPath: tmpRoot) }
    }

    // --- lexical confinement (FS-free) ---------------------------------------

    func testResolvesPlainNameUnderRedactedDir() throws {
        let p = try sandbox.resolveRedactedPath("scrub.txt")
        XCTAssertTrue(p.hasPrefix(sandbox.redactedDir + "/"), "resolved under the redacted dir")
        XCTAssertTrue(p.hasSuffix("/scrub.txt"))
    }

    func testRejectsAbsolutePath() {
        XCTAssertThrowsError(try sandbox.resolveRedactedPath("/etc/passwd")) { err in
            XCTAssertEqual(err as? SandboxError, .escapesSandbox("/etc/passwd"))
        }
    }

    func testRejectsParentTraversal() {
        XCTAssertThrowsError(try sandbox.resolveRedactedPath("../evil.txt"))
        XCTAssertThrowsError(try sandbox.resolveRedactedPath("a/../../evil.txt"))
        XCTAssertThrowsError(try sandbox.resolveRedactedPath("nested/../../out.txt"))
    }

    func testRejectsEmptyName() {
        XCTAssertThrowsError(try sandbox.resolveRedactedPath("")) { err in
            XCTAssertEqual(err as? SandboxError, .emptyName)
        }
        XCTAssertThrowsError(try sandbox.resolveRedactedPath("   "))
    }

    // --- FS-aware write confinement ------------------------------------------

    func testWriteLandsUnderSandboxRoot() throws {
        let written = try sandbox.writeRedacted(name: "out.txt", contents: "redacted body")
        // The written file exists and its REAL path is under the REAL sandbox root.
        XCTAssertTrue(FileManager.default.fileExists(atPath: written))
        let realRoot = try XCTUnwrap(SandboxRoot.realPath(tmpRoot))
        let realWritten = try XCTUnwrap(SandboxRoot.realPath(written))
        XCTAssertTrue(realWritten.hasPrefix(realRoot + "/"),
                      "the redacted copy is confined under the sandbox root")
        let readBack = try String(contentsOfFile: written, encoding: .utf8)
        XCTAssertEqual(readBack, "redacted body")
    }

    func testWriteRefusesEscapingNameAndWritesNothing() {
        XCTAssertThrowsError(try sandbox.writeRedacted(name: "../escape.txt", contents: "x"))
        // Nothing was written outside the sandbox: the parent of the redacted dir
        // must not have gained an escape.txt.
        let parentOfRoot = (tmpRoot as NSString).deletingLastPathComponent
        XCTAssertFalse(FileManager.default.fileExists(
            atPath: (parentOfRoot as NSString).appendingPathComponent("escape.txt")))
    }

    func testWriteCreatesRedactedSubdir() throws {
        _ = try sandbox.writeRedacted(name: "made.txt", contents: "y")
        var isDir: ObjCBool = false
        XCTAssertTrue(FileManager.default.fileExists(atPath: sandbox.redactedDir, isDirectory: &isDir))
        XCTAssertTrue(isDir.boolValue)
    }

    // --- input read confinement ----------------------------------------------

    func testInputPathConfinement() {
        XCTAssertThrowsError(try sandbox.resolveInputPath("/etc/hosts"))
        XCTAssertThrowsError(try sandbox.resolveInputPath("../secret.png"))
        XCTAssertNoThrow(try sandbox.resolveInputPath("staged.png"))
    }

    func testConfinedInputRejectsMissingFile() {
        // A lexically-valid name that does not exist is refused (no phantom read).
        XCTAssertThrowsError(try sandbox.confinedInputPath("does-not-exist.png"))
    }

    // --- output naming -------------------------------------------------------

    func testOutputNameFromArtifactIdIsSanitized() {
        let name = Pipeline.outputName(artifactId: "art/../id 42")
        XCTAssertFalse(name.contains("/"), "path separators sanitized out")
        XCTAssertFalse(name.contains(".."))
        XCTAssertTrue(name.hasPrefix("redacted-artifact-"))
        XCTAssertTrue(name.hasSuffix(".txt"))
        // And the sanitized name is lexically confinable.
        XCTAssertNoThrow(try sandbox.resolveRedactedPath(name))
    }

    func testOutputNameWithoutArtifactId() {
        let name = Pipeline.outputName(artifactId: nil)
        XCTAssertTrue(name.hasPrefix("redacted-scrub-"))
        XCTAssertTrue(name.hasSuffix(".txt"))
    }
}
