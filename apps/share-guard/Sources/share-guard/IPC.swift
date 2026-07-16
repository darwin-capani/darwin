// IPC.swift — the per-app Unix socket transport (mirrors apps/vision/.../IPC.swift,
// trimmed to Share Guard's needs).
//
// Responsibility: own the per-app Unix socket connection.
//   - connect() to AppEnv.socketPath (the daemon owns + binds it; the app dials
//     in — see daemon/src/apps.rs).
//   - READ host->app JSONL lines, decode each via Op.decode(line:), hand them to
//     the Pipeline. The daemon sends control verbs ({"type":"start"|...}) and
//     verbatim op lines.
//   - WRITE app->host JSONL lines via the token-stamping OutboundSink (in
//     ShareGuardEvent.swift). The host VERIFIES the token on every line.
//
// HARD SAFETY (mirrors silicon-canvas/vision): this CONNECTS to the daemon's
// socket — it binds NO listener, opens NO window, touches NO GPU directly, plays
// NO audio, opens NO camera/screen. It only reads a supplied payload + writes a
// redacted copy inside its own sandbox dir. The token rides outbound lines only
// and is NEVER logged.

import Foundation
#if canImport(Darwin)
import Darwin
#endif

/// The app's connection to its per-app socket. Reads host->app lines and
/// dispatches each decoded Op to `onOp` until the connection closes / is stopped.
public protocol AppConnection: Sendable {
    func run(onOp: @escaping @Sendable (Op) async -> Void) async throws
}

/// Stub connection — connects to nothing, returns immediately. Present so the app
/// links where no socket is available (e.g. a non-Darwin build).
public struct StubAppConnection: AppConnection {
    public let env: AppEnv
    public init(env: AppEnv) { self.env = env }
    public func run(onOp: @escaping @Sendable (Op) async -> Void) async throws {}
}

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
/// Isolated to its own actor so a blocking `read()` never serializes behind
/// app->host writes (the writer owns an independent dup of the fd). Closing is
/// idempotent.
public actor SocketReader {
    private var fd: Int32
    private var closed = false
    private var buffer: [UInt8] = []

    public init(fd: Int32) { self.fd = fd }

    /// Read one newline-terminated line, or nil at EOF / fatal error. Buffers
    /// across reads; the trailing newline is trimmed. A line over `maxLineBytes`
    /// without a newline is truncated defensively (a peer cannot grow our memory
    /// without bound).
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

    /// Write the full line + a trailing newline. Best-effort: on a write error the
    /// peer is gone and the reader will observe EOF; we never crash.
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

/// A connected per-app socket, split into an independent reader + writer over the
/// SAME underlying connection (the writer holds a `dup()` of the fd).
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

    /// Test seam: wrap an already-connected fd (e.g. a socketpair half), duping it
    /// for the independent write side.
    public init(fd: Int32) throws {
        let writeFd = dup(fd)
        guard writeFd >= 0 else { throw SocketError.dup(errno) }
        self.reader = SocketReader(fd: fd)
        self.writer = SocketWriter(fd: writeFd)
    }
}

/// LineWriter backed by a connected socket — the real app->host writer. Forwards
/// to the SocketWriter actor; token-stamping is FROZEN in OutboundSink.
public struct SocketLineWriter: LineWriter {
    private let writer: SocketWriter
    public init(writer: SocketWriter) { self.writer = writer }
    public func writeLine(_ line: String) async {
        await writer.writeLine(line)
    }
}

/// The real per-app socket connection: read every host->app JSONL line, decode
/// via Op.decode(line:), and dispatch to `onOp`. A {"type":"stop"} decodes to
/// .stop; we dispatch it then return so main exits cleanly. EOF / read error also
/// returns cleanly. HARD SAFETY: connects, never binds.
public struct SocketAppConnection: AppConnection {
    private let reader: SocketReader

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
            if case .stop = op { break }
        }
        await reader.close()
    }
}

/// Create a connected pair of SocketConnections over socketpair(2) for headless
/// tests (no daemon, no bound socket). A line written to a.writer is readable from
/// b.reader and vice versa.
public func makeSocketConnectionPair() throws -> (SocketConnection, SocketConnection) {
    var fds: [Int32] = [0, 0]
    let rc = fds.withUnsafeMutableBufferPointer { buf -> Int32 in
        socketpair(AF_UNIX, SOCK_STREAM, 0, buf.baseAddress)
    }
    guard rc == 0 else { throw SocketError.create(errno) }
    return (try SocketConnection(fd: fds[0]), try SocketConnection(fd: fds[1]))
}

#endif // canImport(Darwin)
