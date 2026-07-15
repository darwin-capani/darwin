// Op.swift — the HOST -> APP command vocabulary (mirrors apps/vision/.../Op.swift;
// FROZEN wire contract). The daemon forwards two kinds of host->app JSONL lines:
//   1. control verbs:  {"type":"start"|"refresh"|"stop"}
//   2. op lines:       {"type":"op","op":"<name>", ...op-specific fields... }
// The daemon does NOT interpret the op body — the op contract lives HERE.
//
// Decoding is TOTAL: an unknown/malformed line decodes to `.unknown(raw:)` so the
// app drops it + emits share.error rather than crashing.
//
// ARTIFACT REGISTRY INTEGRATION (daemon/src/artifact.rs): Share Guard scrubs a
// PAYLOAD, not the daemon's private registry. "Scrub artifact <id> before I share
// it" is resolved DAEMON-SIDE: the daemon reads the ArtifactRef by id and forwards
// its text as `scrub.text` (or stages its image under the app's input dir and
// forwards `scrub.image`), passing the artifact id along in `artifact_id` purely
// for HUD correlation. The app never reaches into the registry — keeping it
// sandbox-honest — and tags its share.redactions readout with the id it was given.

import Foundation

/// A command the app received from the host.
public enum Op: Sendable, Equatable {
    // --- control verbs (host lifecycle) ---
    case start
    case refresh
    case stop

    // --- share-guard ops (the app's own contract) ---

    /// scrub.text {text, artifact_id?}: redact PII in a supplied TEXT payload
    /// (e.g. an artifact's markdown/preview the daemon resolved from the registry)
    /// and write the redacted copy under the app's OWN sandbox dir. `text` is
    /// REQUIRED; `artifact_id` is OPTIONAL correlation for the HUD.
    case scrubText(text: String, artifactId: String?)

    /// scrub.image {path, artifact_id?}: run the on-device OCR over a supplied
    /// IMAGE payload the host staged under the app's input dir, redact PII in the
    /// recognized text, and write the redacted text copy under the sandbox dir.
    /// `path` is REQUIRED; `artifact_id` is OPTIONAL correlation. DEVICE-GATED at
    /// the OCR step (not exercised in tests).
    case scrubImage(path: String, artifactId: String?)

    /// status: emit a share.status snapshot.
    case status

    /// A line we could not classify — kept so the app can drop + report it.
    case unknown(raw: String)

    /// The canonical op / control-verb name as it appears on the wire. `unknown`
    /// has none.
    public var wireName: String? {
        switch self {
        case .start:      return "start"
        case .refresh:    return "refresh"
        case .stop:       return "stop"
        case .scrubText:  return "scrub.text"
        case .scrubImage: return "scrub.image"
        case .status:     return "status"
        case .unknown:    return nil
        }
    }
}

extension Op {
    /// Decode one already-parsed JSON object into an Op. Total: anything that does
    /// not match a known shape becomes `.unknown(raw:)`. `raw` is the original line
    /// text, carried into `.unknown` for diagnostics.
    public static func decode(json: [String: Any], raw: String) -> Op {
        let type = (json["type"] as? String) ?? ""
        switch type {
        case "start":   return .start
        case "refresh": return .refresh
        case "stop":    return .stop
        case "op":
            let name = (json["op"] as? String) ?? ""
            return decodeOp(name: name, json: json, raw: raw)
        default:
            return .unknown(raw: raw)
        }
    }

    /// Decode one raw JSONL line into an Op (parses JSON then dispatches).
    public static func decode(line: String) -> Op {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              let data = trimmed.data(using: .utf8),
              let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        else {
            return .unknown(raw: line)
        }
        return decode(json: obj, raw: line)
    }

    private static func decodeOp(name: String, json: [String: Any], raw: String) -> Op {
        switch name {
        case "scrub.text":
            // `text` is REQUIRED (nothing to scrub without it). An empty string is
            // allowed (a valid, no-PII payload); a MISSING text is malformed.
            guard let text = json["text"] as? String else { return .unknown(raw: raw) }
            return .scrubText(text: text, artifactId: optionalId(json))
        case "scrub.image":
            // `path` is REQUIRED — the host names the staged image under the app's
            // input dir. A scrub.image without a path is malformed.
            guard let path = json["path"] as? String, !path.isEmpty else { return .unknown(raw: raw) }
            return .scrubImage(path: path, artifactId: optionalId(json))
        case "status":
            return .status
        default:
            return .unknown(raw: raw)
        }
    }

    /// Optional `artifact_id` correlation field (a non-empty string, else nil).
    private static func optionalId(_ json: [String: Any]) -> String? {
        guard let id = json["artifact_id"] as? String, !id.isEmpty else { return nil }
        return id
    }
}
