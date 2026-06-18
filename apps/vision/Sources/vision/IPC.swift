// IPC.swift — IPC module (filled by the ipc agent).
//
// Responsibility: own the per-app Unix socket connection.
//   - connect() to AppEnv.socketPath (the daemon owns + binds it; the app
//     dials in — see daemon/src/apps.rs).
//   - READ host->app JSONL lines, decode each via Op.decode(line:), hand them
//     to the Pipeline. The daemon sends control verbs ({"type":"start"|...})
//     and verbatim op lines.
//   - WRITE app->host JSONL lines: implement EventSink by stamping the token
//     from AppEnv on every VisionEvent via `event.line(token:)` and writing it
//     newline-framed. The host VERIFIES the token on every line; an unstamped
//     line is dropped + app.auth_failed.
//
// This file provides the PUBLIC SEAM (AppConnection protocol + the OutboundSink
// EventSink that frames+stamps lines) plus a stub connection so the package
// builds. The ipc agent implements the real socket I/O (the framing/stamping in
// OutboundSink is already correct + unit-testable via the LineWriter seam).
//
// HARD SAFETY (mirrors silicon-canvas/src/ipc.rs + main.rs): this CONNECTS to
// the daemon's socket — it binds NO listener, opens NO window, touches NO GPU,
// plays NO audio, and never auto-starts the camera/screen (capture only ever
// begins from an explicit watch.start op, dispatched into the Pipeline). The
// token from AppEnv is stamped on EVERY outbound line; it is NEVER logged.

import Foundation
#if canImport(Darwin)
import Darwin
#endif

// ===========================================================================
// FROZEN public seam — do not change the behavior below this banner. The
// daemon's relay_line (apps.rs) verifies the token + framing of every line
// OutboundSink writes, so its contract is load-bearing.
// ===========================================================================

/// A low-level newline-framed line writer. The ipc agent backs this with the
/// real socket file descriptor; tests back it with an in-memory collector to
/// prove token-stamping + framing without a socket.
public protocol LineWriter: Sendable {
    /// Write one already-serialized line (no newline); the writer appends '\n'.
    func writeLine(_ line: String) async
}

/// EventSink that serializes + token-stamps each VisionEvent and writes it as a
/// JSONL line. This is the load-bearing app->host adapter and is FROZEN — its
/// behavior (stamp the AppEnv token, frame one line per event) is the wire
/// contract the daemon verifies. The transport is injected (LineWriter).
public struct OutboundSink: EventSink {
    private let token: String
    private let writer: LineWriter

    public init(token: String, writer: LineWriter) {
        self.token = token
        self.writer = writer
    }

    public func emit(_ event: VisionEvent) async {
        guard let line = event.line(token: token) else { return }
        await writer.writeLine(line)
    }
}

/// The app's connection to its per-app socket. Implemented by the ipc agent.
public protocol AppConnection: Sendable {
    /// Connect to the socket and run the read loop, dispatching each decoded Op
    /// to `onOp`, until the connection closes or the task is cancelled.
    func run(onOp: @escaping @Sendable (Op) async -> Void) async throws
}

/// Stub line writer — drops lines. Replaced by the ipc agent's socket writer.
public struct StubLineWriter: LineWriter {
    public init() {}
    public func writeLine(_ line: String) async {}
}

/// Stub connection — connects to nothing, returns immediately. Replaced by the
/// ipc agent with real Unix-socket I/O. Present so main compiles + links and so
/// the headless tests can wire the pipeline without a socket.
public struct StubAppConnection: AppConnection {
    public let env: AppEnv
    public init(env: AppEnv) { self.env = env }
    public func run(onOp: @escaping @Sendable (Op) async -> Void) async throws {
        // Stub: no socket. The real implementation is SocketAppConnection below.
    }
}

// ===========================================================================
// analyze.file / watch.start(file:) path confinement (defense in depth over
// the seatbelt profile — mirrors silicon-canvas resolve_project_path +
// confine_real_path). The manifest grants fs_read on apps/vision/videos/input
// ONLY; a path is permitted iff it normalizes to something under that dir and
// its REAL on-disk target stays inside it (closing the symlink-escape hole).
// ===========================================================================

/// Why a requested capture/analyze path was refused. Surfaced as a clean
/// vision.error (code "path_denied" / "decode_failed"), never a crash.
public enum VisionPathError: Error, Equatable, CustomStringConvertible {
    case notPermitted(String)     // `..` / absolute / outside the granted dir
    case unsupportedType(String)  // not a recognized video container extension
    case notFound(String)         // permitted, but no such file on disk
    case tooLarge(String)         // over the in-app size cap

    public var description: String {
        switch self {
        case .notPermitted(let p): return "path \(p) is outside the granted videos/input directory"
        case .unsupportedType(let p): return "path \(p) is not a supported video file type"
        case .notFound(let p): return "no such file: \(p)"
        case .tooLarge(let p): return "file \(p) exceeds the size cap"
        }
    }

    /// The vision.error `code` for this rejection.
    public var code: String {
        switch self {
        case .notPermitted: return "path_denied"
        case .unsupportedType: return "unsupported_file"
        case .notFound: return "not_found"
        case .tooLarge: return "file_too_large"
        }
    }
}

/// Resolves + confines a video path for analyze.file / watch.start(file:).
///
/// The single granted directory is `<projectRoot>/apps/vision/videos/input`
/// (manifest fs_read). Confinement is TWO passes, exactly as silicon-canvas:
///   1. `resolveLexical` — FS-free string check: reject `..`, absolute paths,
///      and a normalized result not under the granted dir; gate the extension.
///      Unit-testable with a fake root, no disk touch.
///   2. `confineReal` — FS-aware: canonicalize the resolved path AND the
///      granted root and require the real target to live under the real root,
///      closing the symlink-inside-input/ hole the lexical pass cannot.
public struct VideoPathResolver: Sendable {
    /// Absolute project root the daemon runs the child under (current_dir).
    public let projectRoot: String

    public init(projectRoot: String) {
        self.projectRoot = projectRoot
    }

    /// The one directory video paths are confined to: <root>/apps/vision/videos/input.
    public var grantedDir: String {
        (projectRoot as NSString)
            .appendingPathComponent("apps/vision/videos/input")
    }

    /// Supported video container extensions (AVFoundation-readable). Lower-cased.
    public static let supportedExtensions: Set<String> =
        ["mov", "mp4", "m4v", "qt"]

    /// In-app cap on a single video file (defense in depth; the supervisor is the
    /// outer boundary). 2 GiB is comfortably above any short test clip.
    public static let maxFileBytes: Int64 = 2 * 1024 * 1024 * 1024

    /// LEXICAL pass — FS-free. Returns the confined absolute path string, or a
    /// VisionPathError. Accepts both a bare filename ("clip.mov", defaulted under
    /// the granted dir) and an "videos/input/clip.mov"-rooted path; rejects any
    /// `..`/absolute/escaping path BEFORE any disk touch.
    public func resolveLexical(_ raw: String) throws -> String {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            throw VisionPathError.notPermitted(raw)
        }
        // Absolute paths escape the granted dir outright — reject.
        if trimmed.hasPrefix("/") {
            throw VisionPathError.notPermitted(raw)
        }
        // Any parent (`..`) component anywhere is a traversal attempt — reject
        // before resolving so we never canonicalize an escape. Split on "/" and
        // inspect each component (covers "a/../b", "../x", "x/..").
        let components = trimmed.split(separator: "/", omittingEmptySubsequences: true)
        for comp in components {
            if comp == ".." {
                throw VisionPathError.notPermitted(raw)
            }
        }
        // Confine: a path already rooted at the granted-dir tail keeps its tail;
        // a bare relative path is placed directly under the granted dir.
        let granted = grantedDir
        let relative: String
        // Normalize a leading "videos/input/" or "apps/vision/videos/input/"
        // prefix to just the file tail so both spellings land in the same place.
        if let tail = stripGrantedPrefix(trimmed) {
            relative = tail
        } else {
            relative = trimmed
        }
        if relative.isEmpty || relative.hasSuffix("/") {
            throw VisionPathError.notPermitted(raw)
        }
        let confined = (granted as NSString).appendingPathComponent(relative)
        // Extension gate.
        let ext = (confined as NSString).pathExtension.lowercased()
        guard VideoPathResolver.supportedExtensions.contains(ext) else {
            throw VisionPathError.unsupportedType(raw)
        }
        return confined
    }

    /// Strip a leading granted-dir prefix ("videos/input/…" or
    /// "apps/vision/videos/input/…") and return the remaining tail, or nil when
    /// the path carries no such prefix (a bare filename / sub-path).
    private func stripGrantedPrefix(_ p: String) -> String? {
        let prefixes = ["apps/vision/videos/input/", "videos/input/"]
        for prefix in prefixes where p.hasPrefix(prefix) {
            return String(p.dropFirst(prefix.count))
        }
        return nil
    }

    /// FS-AWARE confinement: canonicalize the resolved target AND the granted
    /// root, then require the real target to live under the real root. Closes
    /// the symlink-escape hole. Requires the file to exist. Also enforces the
    /// size cap. Returns the canonical absolute path on success.
    public func confineReal(_ confined: String) throws -> String {
        let fm = FileManager.default
        var isDir: ObjCBool = false
        guard fm.fileExists(atPath: confined, isDirectory: &isDir), !isDir.boolValue else {
            throw VisionPathError.notFound(confined)
        }
        // Canonicalize both sides into the same fully-resolved namespace so a
        // symlinked temp root (macOS /tmp -> /private/tmp) does not falsely
        // reject. realpath() follows every symlink in the path.
        guard let realTarget = Self.realPath(confined) else {
            throw VisionPathError.notPermitted(confined)
        }
        guard let realRoot = Self.realPath(grantedDir) else {
            // The granted dir does not exist -> nothing is permitted under it.
            throw VisionPathError.notPermitted(confined)
        }
        // The real target must live under the real root.
        let rootWithSep = realRoot.hasSuffix("/") ? realRoot : realRoot + "/"
        guard realTarget == realRoot || realTarget.hasPrefix(rootWithSep) else {
            // Report the requested (pre-resolution) path so the error never
            // leaks where a symlink pointed.
            throw VisionPathError.notPermitted(confined)
        }
        // Size cap.
        if let attrs = try? fm.attributesOfItem(atPath: realTarget),
           let size = attrs[.size] as? NSNumber,
           size.int64Value > VideoPathResolver.maxFileBytes {
            throw VisionPathError.tooLarge(confined)
        }
        return realTarget
    }

    /// Full resolution: lexical confine + real-path confine + size cap. Returns
    /// the canonical absolute path that is safe to open, or throws.
    public func resolve(_ raw: String) throws -> String {
        let confined = try resolveLexical(raw)
        return try confineReal(confined)
    }

    /// Canonicalize a path with realpath(3) — follows every symlink. nil on
    /// failure (e.g. the path does not resolve). FS-free callers must not use
    /// this; it touches disk.
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

// ===========================================================================
// Real Unix-socket transport (replaces the stubs behind the FROZEN seams).
// ===========================================================================

#if canImport(Darwin)

/// Errors raised while establishing or using the per-app Unix socket.
public enum SocketError: Error, CustomStringConvertible {
    case create(Int32)
    case pathTooLong(String)
    case connect(String, Int32)
    case dup(Int32)

    public var description: String {
        switch self {
        case .create(let e): return "socket() failed: \(String(cString: strerror(e)))"
        case .pathTooLong(let p): return "socket path too long for sockaddr_un: \(p)"
        case .connect(let p, let e): return "connect(\(p)) failed: \(String(cString: strerror(e)))"
        case .dup(let e): return "dup() failed: \(String(cString: strerror(e)))"
        }
    }
}

/// Connect a Unix-domain stream socket to `path` and return the connected fd.
/// Throws so main can exit cleanly (the app only runs under the daemon).
func connectUnixSocket(path: String) throws -> Int32 {
    let sock = socket(AF_UNIX, SOCK_STREAM, 0)
    guard sock >= 0 else { throw SocketError.create(errno) }
    var addr = sockaddr_un()
    addr.sun_family = sa_family_t(AF_UNIX)
    let maxLen = MemoryLayout.size(ofValue: addr.sun_path)
    let pathBytes = Array(path.utf8)
    guard pathBytes.count < maxLen else {
        close(sock)
        throw SocketError.pathTooLong(path)
    }
    withUnsafeMutableBytes(of: &addr.sun_path) { raw in
        raw.copyBytes(from: pathBytes)
        raw[pathBytes.count] = 0 // NUL-terminate
    }
    let rc: Int32 = withUnsafePointer(to: &addr) { ptr in
        ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
            connect(sock, sa, socklen_t(MemoryLayout<sockaddr_un>.size))
        }
    }
    guard rc == 0 else {
        let err = errno
        close(sock)
        throw SocketError.connect(path, err)
    }
    return sock
}

/// The READ half of a connected socket: blocking, newline-framed line reads.
/// Isolated to its own actor so a blocking `read()` (waiting for the next host
/// op) NEVER serializes behind app->host writes — the writer owns an
/// independent dup of the fd. read(2)/write(2) on the same socket from separate
/// threads are safe at the OS level. Closing is idempotent.
public actor SocketReader {
    private var fd: Int32
    private var closed = false
    private var buffer: [UInt8] = []

    /// Own an already-connected fd (the connection's read dup, or a socketpair
    /// half in tests).
    public init(fd: Int32) { self.fd = fd }

    /// Read one newline-terminated line, or nil at EOF / fatal error. Buffers
    /// across reads so a partial line completes on the next call. The trailing
    /// newline is trimmed. A line over `maxLineBytes` without a newline is
    /// truncated defensively (a peer cannot grow our memory without bound).
    public func readLine(maxLineBytes: Int = 1 << 20) -> String? {
        if let line = takeBufferedLine() { return line }
        var chunk = [UInt8](repeating: 0, count: 4096)
        while !closed {
            let n = chunk.withUnsafeMutableBytes { raw -> Int in
                read(fd, raw.baseAddress, raw.count)
            }
            if n > 0 {
                buffer.append(contentsOf: chunk[0..<n])
                if buffer.count > maxLineBytes {
                    if let nl = buffer.firstIndex(of: 0x0A) {
                        return takeLine(upTo: nl)
                    }
                    let truncated = String(decoding: buffer.prefix(maxLineBytes), as: UTF8.self)
                    buffer.removeAll(keepingCapacity: false)
                    return truncated
                }
                if let line = takeBufferedLine() { return line }
            } else if n == 0 {
                // EOF: flush a trailing partial line, then signal close.
                if !buffer.isEmpty {
                    let last = String(decoding: buffer, as: UTF8.self)
                    buffer.removeAll(keepingCapacity: false)
                    return last
                }
                return nil
            } else {
                if errno == EINTR { continue }
                return nil
            }
        }
        return nil
    }

    public func close() {
        guard !closed else { return }
        closed = true
        Darwin.close(fd)
        fd = -1
    }

    private func takeBufferedLine() -> String? {
        guard let nl = buffer.firstIndex(of: 0x0A) else { return nil }
        return takeLine(upTo: nl)
    }

    private func takeLine(upTo nl: Int) -> String {
        let lineBytes = Array(buffer[0..<nl])
        buffer.removeSubrange(0...nl)
        return String(decoding: lineBytes, as: UTF8.self)
    }
}

/// The WRITE half of a connected socket: newline-framed line writes, serialized
/// by the actor. Owns its own dup of the fd so writes proceed even while the
/// reader is blocked in a read(). Implements the FROZEN `LineWriter` seam.
public actor SocketWriter: LineWriter {
    private var fd: Int32
    private var closed = false

    public init(fd: Int32) { self.fd = fd }

    /// Write the full line + a trailing newline. Best-effort: on a write error
    /// the peer is gone and the reader will observe EOF; we never crash.
    public func writeLine(_ line: String) {
        guard !closed else { return }
        var bytes = Array(line.utf8)
        bytes.append(0x0A) // '\n'
        var offset = 0
        bytes.withUnsafeBytes { raw in
            guard let base = raw.baseAddress else { return }
            while offset < bytes.count {
                let n = write(fd, base + offset, bytes.count - offset)
                if n > 0 {
                    offset += n
                } else if n < 0 && errno == EINTR {
                    continue
                } else {
                    return // peer gone / fatal write error
                }
            }
        }
    }

    public func close() {
        guard !closed else { return }
        closed = true
        Darwin.close(fd)
        fd = -1
    }
}

/// A connected per-app socket, split into an independent reader + writer over
/// the SAME underlying connection (the writer holds a `dup()` of the fd so a
/// blocking read never blocks a write). Built by connecting to AppEnv.socketPath
/// (production) or wrapping a socketpair half (tests).
public struct SocketConnection: Sendable {
    public let reader: SocketReader
    public let writer: SocketWriter

    /// Connect to the daemon's per-app socket and split into reader/writer.
    public init(connectingTo path: String) throws {
        let fd = try connectUnixSocket(path: path)
        let writeFd = dup(fd)
        guard writeFd >= 0 else {
            let e = errno
            close(fd)
            throw SocketError.dup(e)
        }
        self.reader = SocketReader(fd: fd)
        self.writer = SocketWriter(fd: writeFd)
    }

    /// Connect using the launch env's socket path.
    public init(env: AppEnv) throws {
        try self.init(connectingTo: env.socketPath)
    }

    /// Test seam: wrap an already-connected fd (e.g. a socketpair half), duping
    /// it for the independent write side.
    public init(fd: Int32) throws {
        let writeFd = dup(fd)
        guard writeFd >= 0 else { throw SocketError.dup(errno) }
        self.reader = SocketReader(fd: fd)
        self.writer = SocketWriter(fd: writeFd)
    }
}

/// LineWriter backed by a connected socket — the real app->host writer. Just
/// forwards to the SocketWriter actor; token-stamping is FROZEN in OutboundSink.
public struct SocketLineWriter: LineWriter {
    private let writer: SocketWriter
    public init(writer: SocketWriter) { self.writer = writer }
    public func writeLine(_ line: String) async {
        await writer.writeLine(line)
    }
}

/// The real per-app socket connection: connect to AppEnv.socketPath, read every
/// host->app JSONL line, decode via Op.decode(line:), and dispatch to `onOp`.
///
/// The daemon sends an initial {"type":"start"} on accept (apps.rs send_command)
/// then control verbs + verbatim op lines; Op.decode handles ALL of them and is
/// total (a malformed/unknown line decodes to .unknown, which the Pipeline turns
/// into a clean vision.error rather than crashing). A {"type":"stop"} decodes to
/// .stop; we dispatch it then return so main exits cleanly. EOF / read error
/// also returns cleanly.
///
/// HARD SAFETY: connects, never binds; no camera/screen is opened here (capture
/// begins only inside the Pipeline on an explicit watch.start). The token is
/// never read or logged on this path — it rides outbound lines only.
public struct SocketAppConnection: AppConnection {
    private let reader: SocketReader

    /// Connect to the daemon's per-app socket at AppEnv.socketPath (read side).
    /// For the matching writer, build a SocketConnection and use its `.writer`.
    public init(reader: SocketReader) {
        self.reader = reader
    }

    public func run(onOp: @escaping @Sendable (Op) async -> Void) async throws {
        while true {
            guard let line = await reader.readLine() else {
                break // EOF / fatal read error — connection closed.
            }
            let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
            if trimmed.isEmpty { continue }
            let op = Op.decode(line: trimmed)
            await onOp(op)
            // A clean stop ends the loop AFTER the Pipeline has handled it.
            if case .stop = op { break }
        }
        await reader.close()
    }
}

/// Create a connected pair of SocketConnections over socketpair(2) for headless
/// tests (no daemon, no bound socket). Returns (a, b): a line written to a.writer
/// is readable from b.reader and vice versa.
public func makeSocketConnectionPair() throws -> (SocketConnection, SocketConnection) {
    var fds: [Int32] = [0, 0]
    let rc = fds.withUnsafeMutableBufferPointer { buf -> Int32 in
        socketpair(AF_UNIX, SOCK_STREAM, 0, buf.baseAddress)
    }
    guard rc == 0 else { throw SocketError.create(errno) }
    return (try SocketConnection(fd: fds[0]), try SocketConnection(fd: fds[1]))
}

#endif // canImport(Darwin)
