// ShareGuardEvent.swift — the APP -> HOST telemetry contract (mirrors
// apps/vision/Sources/vision/VisionEvent.swift). The items/status/log surface is
// FROZEN — it is the wire shape the daemon verifies + relays.
//
// WIRE FRAMING (daemon/src/apps.rs::relay_line / classify_inbound_line): every
// app->host line is one JSON object:
//     {"token": <hex>, "type": "items"|"status"|"log", "data": <obj>}
//   - token is verified on EVERY line (HMAC-SHA256); a bad/missing token drops
//     the line + emits app.auth_failed.
//   - type "items"/"status" -> relayed as telemetry "app.data" with payload
//     {"name":"share-guard","topic":<topic>,"payload":<data>}; the relay TOPIC is
//     data["topic"] if the app DECLARED it in manifest.toml telemetry_topics, else
//     the first declared topic. So every event's `data` MUST carry a share.* topic.
//   - type "log" -> app.log {"name","line"}.
//
// SECRET-FREE BY CONSTRUCTION (the honesty contract): `data` carries COUNTS, a
// bounded PREVIEW, and the sandbox-relative OUTPUT path — NEVER the raw PII, and
// NEVER the full redacted body (that lives in the sandbox copy the user shares
// themselves). Nothing here sends; DARWIN cannot share on the user's behalf.

import Foundation

/// The share.* telemetry topics (must match manifest.toml exactly).
public enum ShareTopic {
    public static let redactions = "share.redactions"  // default topic — the scrub result
    public static let status     = "share.status"      // lifecycle
    public static let error      = "share.error"       // recoverable errors

    /// All declared topics, in manifest order (first = default).
    public static let all = [redactions, status, error]
}

/// The daemon `type` field for an app->host line.
public enum RelayType: String, Sendable {
    case items    // -> app.data
    case status   // -> app.data
    case log      // -> app.log
}

/// A typed Share Guard telemetry event. `encodeData()` builds the `data` object
/// (always including its `topic`); `line(token:)` builds the token-stamped JSONL
/// line the app writes to its socket.
public enum ShareGuardEvent: Sendable {

    /// share.redactions — the result of a scrub. `type:"items"`. SECRET-FREE:
    /// per-kind counts, the total, the honest best-effort preview, the sandbox-
    /// relative output path of the redacted copy, the optional artifact id, and
    /// the original length — NEVER the PII or the redacted body.
    case redactions(counts: [PIIKind: Int], total: Int, foundPII: Bool,
                    preview: String, output: String?, artifactId: String?, originalLength: Int)

    /// share.status — lifecycle snapshot. `type:"status"`.
    case status(state: State, message: String?)

    /// share.error — a recoverable error. `type:"status"`.
    case error(code: String, message: String)

    /// The lifecycle states reported on share.status.
    public enum State: String, Sendable, Codable {
        case idle
        case scrubbing
        case stopped
    }

    /// The daemon `type` for this event.
    public var relayType: RelayType {
        switch self {
        case .redactions:       return .items
        case .status, .error:   return .status
        }
    }

    /// The share.* topic this event targets.
    public var topic: String {
        switch self {
        case .redactions: return ShareTopic.redactions
        case .status:     return ShareTopic.status
        case .error:      return ShareTopic.error
        }
    }

    /// Build the `data` object for this event. ALWAYS includes `topic` so the
    /// daemon relays it onto the right share.* channel.
    public func encodeData() -> [String: Any] {
        var d: [String: Any] = ["topic": topic]
        switch self {
        case let .redactions(counts, total, foundPII, preview, output, artifactId, originalLength):
            d["total"] = total
            d["found_pii"] = foundPII
            // Per-kind counts as a stable {kind: n} map (kinds with 0 omitted).
            var byKind: [String: Int] = [:]
            for kind in PIIKind.allCases where (counts[kind] ?? 0) > 0 {
                byKind[kind.rawValue] = counts[kind]
            }
            d["by_kind"] = byKind
            d["preview"] = preview
            d["original_length"] = originalLength
            if let output { d["output"] = output }
            if let artifactId { d["artifact_id"] = artifactId }

        case let .status(state, message):
            d["state"] = state.rawValue
            if let message { d["message"] = message }

        case let .error(code, message):
            d["code"] = code
            d["message"] = message
        }
        return d
    }

    /// The full token-stamped JSONL line (no trailing newline; the ipc writer
    /// appends one). Returns nil only if JSON serialization fails.
    public func line(token: String) -> String? {
        let envelope: [String: Any] = [
            "token": token,
            "type": relayType.rawValue,
            "data": encodeData(),
        ]
        guard JSONSerialization.isValidJSONObject(envelope),
              let data = try? JSONSerialization.data(withJSONObject: envelope, options: [.sortedKeys]),
              let s = String(data: data, encoding: .utf8)
        else { return nil }
        return s
    }
}

// ===========================================================================
// EventSink + token-stamping writer (mirrors vision's OutboundSink seam)
// ===========================================================================

/// A newline-framed line writer. The socket path backs this with the real fd;
/// tests back it with an in-memory collector to prove token-stamping + framing
/// without a socket.
public protocol LineWriter: Sendable {
    /// Write one already-serialized line (no newline); the writer appends '\n'.
    func writeLine(_ line: String) async
}

/// Consumes typed events. Backed by `OutboundSink` (stamps the token) in
/// production; tests can supply a collector.
public protocol EventSink: Sendable {
    func emit(_ event: ShareGuardEvent) async
}

/// EventSink that serializes + token-stamps each event and writes it as a JSONL
/// line. FROZEN behavior (stamp the AppEnv token, frame one line per event) — the
/// wire contract the daemon verifies. Transport is injected (LineWriter).
public struct OutboundSink: EventSink {
    private let token: String
    private let writer: LineWriter

    public init(token: String, writer: LineWriter) {
        self.token = token
        self.writer = writer
    }

    public func emit(_ event: ShareGuardEvent) async {
        guard let line = event.line(token: token) else { return }
        await writer.writeLine(line)
    }
}

/// Stub line writer — drops lines. Used where no socket is present.
public struct StubLineWriter: LineWriter {
    public init() {}
    public func writeLine(_ line: String) async {}
}
