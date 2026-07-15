// SharedTypes.swift — the CANONICAL shared vocabulary for the Share Guard
// micro-app (mirrors apps/vision/Sources/vision/SharedTypes.swift).
//
// FROZEN: everything that crosses a module boundary — a detected PII span, the
// scrub result, the mask style, the launch env, an op, a telemetry event —
// lives here so the pure detection/redaction seam, the sandbox confinement, and
// the (device-gated) OCR runner stay disjoint and wire-compatible.
//
// HONESTY invariants baked into the types:
//   - Share Guard reads GLYPH TEXT only — a recognized string, never a face /
//     person id. There is deliberately no identity field anywhere here.
//   - Redaction is BEST-EFFORT text detection, NOT a guarantee. The types carry
//     COUNTS + a bounded, secret-free PREVIEW for telemetry — never the raw PII,
//     and never the full redacted body on the wire (that lives in the sandbox
//     copy the user shares themselves).
//   - DARWIN cannot SEND. Share Guard produces a scrubbed COPY inside its own
//     sandbox dir; the user shares it. No egress lives on this path.

import Foundation

// ===========================================================================
// PIIKind — the closed vocabulary of PII this best-effort detector recognizes
// ===========================================================================

/// A category of personally-identifying information the detector can find. A
/// closed, honest vocabulary — the detector never claims a kind it did not
/// actually match, and everything outside this set is left untouched (a
/// best-effort text scan, not an exhaustive PII guarantee).
public enum PIIKind: String, Sendable, Codable, CaseIterable {
    /// An email address (RFC-ish `local@domain.tld`).
    case email
    /// A phone number: a 10–12 digit run (NANP with an optional country code),
    /// typically with `+`, spaces, dashes, dots, or parens.
    case phone
    /// A long account / payment-card number: a 13–19 digit run that PASSES a
    /// Luhn check. The Luhn gate is what separates a real card/account number
    /// from an arbitrary long number (which is deliberately NOT masked).
    case card

    /// The human label used in a redaction marker and the secret-free preview.
    public var marker: String {
        switch self {
        case .email: return "EMAIL"
        case .phone: return "PHONE"
        case .card:  return "CARD"
        }
    }
}

// ===========================================================================
// PIISpan — one detected PII occurrence in a source string
// ===========================================================================

/// One detected PII occurrence: its kind, the character RANGE it occupies in
/// the source string, and the exact matched substring. The range is over the
/// SOURCE string's `String.Index` space so redaction composition can splice
/// without any UTF-16/Character mismatch. `matched` is retained so tests can
/// assert exactly what was found; it is NEVER put on telemetry (that would leak
/// the PII) — only counts + a preview cross the wire.
public struct PIISpan: Sendable, Equatable {
    public let kind: PIIKind
    public let range: Range<String.Index>
    public let matched: String

    public init(kind: PIIKind, range: Range<String.Index>, matched: String) {
        self.kind = kind
        self.range = range
        self.matched = matched
    }
}

// ===========================================================================
// MaskStyle — how a detected span is rewritten in the redacted copy
// ===========================================================================

/// How a detected PII span is rewritten in the redacted copy. Default is a
/// LABELED marker (`[EMAIL REDACTED]`) — it removes the PII entirely while
/// telling the reader what kind of thing was scrubbed, which is honest and does
/// not leak the length of the original. A `.fixed` block is offered for callers
/// that prefer an opaque bar.
public enum MaskStyle: Sendable, Equatable {
    /// `[EMAIL REDACTED]` / `[PHONE REDACTED]` / `[CARD REDACTED]`.
    case labeled
    /// A fixed opaque block, kind-agnostic: `[REDACTED]`.
    case fixed

    /// The replacement text for a span of the given kind.
    public func replacement(for kind: PIIKind) -> String {
        switch self {
        case .labeled: return "[\(kind.marker) REDACTED]"
        case .fixed:   return "[REDACTED]"
        }
    }
}

// ===========================================================================
// ScrubResult — the composed outcome of a scrub pass (PURE, testable)
// ===========================================================================

/// The result of scrubbing a text payload: the marked-up redacted copy, the
/// spans that were masked, and a per-kind count breakdown. Produced entirely by
/// the PURE seam (`PIIDetector` + `Redaction`), so the whole thing is unit-
/// testable with literal strings — no OCR, no capture, no socket.
public struct ScrubResult: Sendable, Equatable {
    /// The redacted copy: the original text with every detected span replaced by
    /// its mask marker. This is the "marked-up redaction preview".
    public let redactedText: String
    /// The spans that were detected + masked (in source order).
    public let spans: [PIISpan]
    /// Per-kind count of masked spans (a kind with zero is omitted).
    public let counts: [PIIKind: Int]
    /// The character length of the ORIGINAL text (a size signal for telemetry;
    /// never the text itself).
    public let originalLength: Int

    public init(redactedText: String, spans: [PIISpan], counts: [PIIKind: Int], originalLength: Int) {
        self.redactedText = redactedText
        self.spans = spans
        self.counts = counts
        self.originalLength = originalLength
    }

    /// Total number of masked spans across all kinds.
    public var total: Int { counts.values.reduce(0, +) }

    /// Whether ANY PII was found + masked.
    public var foundPII: Bool { total > 0 }

    /// The count for one kind (0 if none).
    public func count(_ kind: PIIKind) -> Int { counts[kind] ?? 0 }

    /// A short, SECRET-FREE preview line for the HUD / telemetry / spoken reply.
    /// It names WHAT was redacted (counts by kind) but never any actual PII and
    /// never the redacted body. HONEST framing: redaction is best-effort text
    /// detection, not a guarantee.
    public func preview() -> String {
        guard foundPII else {
            return "No PII detected — best-effort scan found nothing to redact (review before sharing)."
        }
        // Stable kind order so the preview is deterministic.
        let parts: [String] = PIIKind.allCases.compactMap { kind in
            let n = count(kind)
            guard n > 0 else { return nil }
            let noun = kind.marker.lowercased()
            return "\(n) \(noun)\(n == 1 ? "" : "s")"
        }
        return "Redacted \(total) item\(total == 1 ? "" : "s") (best-effort): " + parts.joined(separator: ", ") + "."
    }
}
