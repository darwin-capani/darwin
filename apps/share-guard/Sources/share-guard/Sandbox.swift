// Sandbox.swift — WRITE CONFINEMENT for the redacted copy (defense in depth over
// the seatbelt profile). Mirrors the vision app's VideoPathResolver, but for the
// OUTPUT side: Share Guard writes the scrubbed copy ONLY under its OWN sandbox
// dir, NEVER the user's original file and never anywhere else.
//
// The manifest grants fs_write on `state/tmp/share-guard` (the app's own scratch
// root) ONLY. This resolver is the in-process belt-and-suspenders that keeps the
// output path inside that root even before the kernel profile would deny it:
//   1. resolveRedactedPath — FS-free lexical check: reject absolute paths, any
//      `..` component, and an empty name; place the file under <root>/redacted.
//      Unit-testable with a fake root, no disk touch.
//   2. writeRedacted — FS-aware: create the dir, write, then canonicalize the
//      written file's parent AND the root and require the real target to live
//      under the real root (closes the symlink-escape hole), else refuse.
//
// SAFETY: there is deliberately NO API here that takes a caller-supplied
// destination outside the root, and NONE that touches the user's original. The
// only write path is a name RELATIVE to the sandbox's redacted dir.

import Foundation
#if canImport(Darwin)
import Darwin
#endif

/// Why a redacted-copy write was refused. Surfaced as a clean share.error, never
/// a crash or a silent write outside the sandbox.
public enum SandboxError: Error, Equatable, CustomStringConvertible {
    case emptyName
    case escapesSandbox(String)   // absolute / `..` / resolves outside the root
    case writeFailed(String)

    public var description: String {
        switch self {
        case .emptyName: return "redacted-copy name is empty"
        case .escapesSandbox(let n): return "redacted-copy path \(n) escapes the sandbox dir"
        case .writeFailed(let p): return "could not write the redacted copy to \(p)"
        }
    }

    /// The share.error `code` for this rejection.
    public var code: String {
        switch self {
        case .emptyName: return "bad_name"
        case .escapesSandbox: return "path_denied"
        case .writeFailed: return "write_failed"
        }
    }
}

/// The app's OWN sandbox directory (`state/tmp/share-guard`), with the input +
/// redacted subdirs. All write confinement is relative to `root`.
public struct SandboxRoot: Sendable {
    /// Absolute path to the sandbox root (the manifest's fs_write grant).
    public let root: String

    /// The sandbox root relative to the project root the daemon runs the child
    /// under (cwd). Matches the manifest: `state/tmp/share-guard`.
    public static let relativeRoot = "state/tmp/share-guard"

    /// Build from the project root (the production path): root = <projectRoot>/
    /// state/tmp/share-guard.
    public init(projectRoot: String) {
        self.root = (projectRoot as NSString).appendingPathComponent(SandboxRoot.relativeRoot)
    }

    /// Build from an explicit absolute root (tests pass a temp dir so the
    /// confinement is exercised without touching the repo).
    public init(absoluteRoot: String) {
        self.root = absoluteRoot
    }

    /// Where redacted copies are written (`<root>/redacted`).
    public var redactedDir: String { (root as NSString).appendingPathComponent("redacted") }

    /// Where the host stages a to-be-scrubbed image payload (`<root>/input`) —
    /// the manifest's fs_read grant. (Read confinement for the image runner.)
    public var inputDir: String { (root as NSString).appendingPathComponent("input") }

    // -- LEXICAL confinement (FS-free, unit-testable) -------------------------

    /// Resolve a redacted-copy `name` to an absolute path under `redactedDir`,
    /// WITHOUT touching disk. Rejects an empty name, an absolute path, and any
    /// `..` component (a traversal attempt) BEFORE any resolution. A bare filename
    /// or a safe relative sub-path is placed under the redacted dir.
    public func resolveRedactedPath(_ name: String) throws -> String {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { throw SandboxError.emptyName }
        // An absolute path escapes the sandbox outright.
        if trimmed.hasPrefix("/") { throw SandboxError.escapesSandbox(name) }
        // Any parent component anywhere is a traversal attempt.
        let components = trimmed.split(separator: "/", omittingEmptySubsequences: true)
        guard !components.isEmpty else { throw SandboxError.escapesSandbox(name) }
        for comp in components where comp == ".." {
            throw SandboxError.escapesSandbox(name)
        }
        let confined = (redactedDir as NSString).appendingPathComponent(trimmed)
        if confined.hasSuffix("/") { throw SandboxError.escapesSandbox(name) }
        return confined
    }

    // -- FS-aware write (creates the dir, confines the real target) -----------

    /// Write the redacted `contents` to a copy named `name` under the sandbox's
    /// redacted dir. Confinement is TWO passes: the lexical `resolveRedactedPath`
    /// above, then — after ensuring the parent dir exists — a real-path check that
    /// the written file's parent lives under the real sandbox root (closing the
    /// symlink-escape hole). Returns the absolute path written, or throws. NEVER
    /// writes outside the sandbox, and NEVER touches the user's original.
    @discardableResult
    public func writeRedacted(name: String, contents: String) throws -> String {
        let target = try resolveRedactedPath(name)
        let parent = (target as NSString).deletingLastPathComponent
        let fm = FileManager.default
        do {
            try fm.createDirectory(atPath: parent, withIntermediateDirectories: true)
        } catch {
            throw SandboxError.writeFailed(target)
        }
        // FS-aware confinement: the real parent must live under the real root.
        // (Both canonicalized so a symlinked temp root — macOS /tmp -> /private/tmp
        // — does not falsely reject.)
        guard let realParent = Self.realPath(parent), let realRoot = Self.realPath(root) else {
            throw SandboxError.escapesSandbox(name)
        }
        let rootWithSep = realRoot.hasSuffix("/") ? realRoot : realRoot + "/"
        guard realParent == realRoot || realParent.hasPrefix(rootWithSep) else {
            throw SandboxError.escapesSandbox(name)
        }
        do {
            try contents.write(toFile: target, atomically: true, encoding: .utf8)
        } catch {
            throw SandboxError.writeFailed(target)
        }
        return target
    }

    // -- READ confinement for a staged image payload --------------------------

    /// Resolve a staged-image `name` to an absolute path under `inputDir` WITHOUT
    /// touching disk — same lexical rules as `resolveRedactedPath` (reject empty /
    /// absolute / `..`). The host stages the to-be-scrubbed image here; this keeps
    /// the OCR runner's read confined to the granted input dir even before the
    /// seatbelt profile would deny an escape.
    public func resolveInputPath(_ name: String) throws -> String {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { throw SandboxError.emptyName }
        if trimmed.hasPrefix("/") { throw SandboxError.escapesSandbox(name) }
        let components = trimmed.split(separator: "/", omittingEmptySubsequences: true)
        guard !components.isEmpty else { throw SandboxError.escapesSandbox(name) }
        for comp in components where comp == ".." {
            throw SandboxError.escapesSandbox(name)
        }
        let confined = (inputDir as NSString).appendingPathComponent(trimmed)
        if confined.hasSuffix("/") { throw SandboxError.escapesSandbox(name) }
        return confined
    }

    /// FS-AWARE read confinement: lexically confine `name` under `inputDir`, then
    /// require the REAL target (canonicalized) to live under the REAL input dir
    /// (closes the symlink-escape hole) and to exist. Returns the canonical path
    /// safe to hand the OCR runner, or throws.
    public func confinedInputPath(_ name: String) throws -> String {
        let confined = try resolveInputPath(name)
        let fm = FileManager.default
        var isDir: ObjCBool = false
        guard fm.fileExists(atPath: confined, isDirectory: &isDir), !isDir.boolValue else {
            throw SandboxError.escapesSandbox(name)
        }
        guard let realTarget = Self.realPath(confined), let realRoot = Self.realPath(inputDir) else {
            throw SandboxError.escapesSandbox(name)
        }
        let rootWithSep = realRoot.hasSuffix("/") ? realRoot : realRoot + "/"
        guard realTarget == realRoot || realTarget.hasPrefix(rootWithSep) else {
            throw SandboxError.escapesSandbox(name)
        }
        return realTarget
    }

    /// Canonicalize a path with realpath(3) — follows every symlink. nil if the
    /// path does not resolve (e.g. the dir was not created).
    static func realPath(_ path: String) -> String? {
        #if canImport(Darwin)
        return path.withCString { cpath -> String? in
            guard let resolved = realpath(cpath, nil) else { return nil }
            defer { free(resolved) }
            return String(cString: resolved)
        }
        #else
        return path
        #endif
    }
}
