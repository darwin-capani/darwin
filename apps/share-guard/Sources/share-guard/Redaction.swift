// Redaction.swift — the PURE redaction composer + the top-level scrub entry.
// Mirrors the vision app's pure structuring seam: a deterministic value
// transform over a text string + its detected spans, with NO I/O.
//
// Responsibility: given a source text and the PII spans PIIDetector found,
// compose the marked-up REDACTED copy (each span replaced by its mask marker),
// tally per-kind counts, and return a ScrubResult. The whole path
// (`ShareGuard.scrub`) is pure + unit-testable with literal strings — no OCR, no
// capture, no socket, no filesystem.
//
// HONESTY: the redacted copy removes the PII substrings entirely (labeled
// markers, not the original glyphs). It is BEST-EFFORT — it only masks what the
// detector matched; unusual formats survive, so the copy must be reviewed before
// sharing (surfaced in ScrubResult.preview()).

import Foundation

/// The pure redaction composer.
public enum Redaction {

    /// Compose a `ScrubResult` from `text` and its detected `spans`. Spans are
    /// assumed non-overlapping (as `PIIDetector.detect` returns); they are sorted
    /// defensively and any span overlapping an already-emitted one is skipped so
    /// composition can never splice a torn range. Each surviving span's substring
    /// is replaced by `mask.replacement(for:)`; the text between spans is copied
    /// verbatim.
    public static func compose(text: String, spans: [PIISpan], mask: MaskStyle = .labeled) -> ScrubResult {
        let ordered = spans.sorted { $0.range.lowerBound < $1.range.lowerBound }

        var redacted = ""
        redacted.reserveCapacity(text.count)
        var cursor = text.startIndex
        var applied: [PIISpan] = []
        var counts: [PIIKind: Int] = [:]

        for span in ordered {
            // Skip a span that starts before the cursor (an overlap with a span
            // already applied) — never splice a torn range.
            guard span.range.lowerBound >= cursor, span.range.upperBound <= text.endIndex else { continue }
            // Copy the untouched text before this span.
            redacted += text[cursor..<span.range.lowerBound]
            // Replace the span with its mask marker.
            redacted += mask.replacement(for: span.kind)
            cursor = span.range.upperBound
            applied.append(span)
            counts[span.kind, default: 0] += 1
        }
        // Copy the tail after the last span.
        redacted += text[cursor..<text.endIndex]

        return ScrubResult(
            redactedText: redacted,
            spans: applied,
            counts: counts,
            originalLength: text.count)
    }
}

// ===========================================================================
// ShareGuard — the one call the OCR/text path funnels through
// ===========================================================================

/// The top-level pure scrub: detect PII spans, then compose the redacted copy.
/// This is the single seam BOTH the device-gated OCR runner and a plain-text
/// payload go through, so the entire redaction decision is unit-testable without
/// any device path.
public enum ShareGuard {
    /// Detect + redact PII in `text`, returning the marked-up copy + counts.
    /// Pure — no I/O. `mask` defaults to labeled markers.
    public static func scrub(text: String, mask: MaskStyle = .labeled) -> ScrubResult {
        let spans = PIIDetector.detect(in: text)
        return Redaction.compose(text: text, spans: spans, mask: mask)
    }
}
