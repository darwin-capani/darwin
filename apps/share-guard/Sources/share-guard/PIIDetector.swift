// PIIDetector.swift — the PURE, unit-testable PII-span detector (the heart of
// Share Guard's redaction seam). Mirrors the vision app's ScreenStructurer: a
// deterministic value transform with NO I/O, NO OCR, NO capture, NO socket.
//
// Responsibility: given a recognized/plain TEXT STRING (from the device-gated
// OCR runner, or a plain text payload), find the PII spans:
//   - EMAILs         — `local@domain.tld`
//   - PHONE numbers  — a 10–12 digit run (NANP ± country code), typically
//                      formatted with `+`, spaces, dashes, dots, or parens
//   - CARD / account — a 13–19 digit run that PASSES a Luhn check
//
// HONESTY / NON-OVER-MASKING is the load-bearing property:
//   - A digit run of 13–19 digits is masked ONLY if Luhn-valid. An arbitrary
//     long number that fails Luhn is NOT a card and is left ALONE — this is the
//     "a partial/invalid number is not over-masked" contract.
//   - Phone is capped at 10–12 digits, strictly BELOW the card band, so a long
//     non-card number never falls through to a phone mask.
//   - Short numbers (< 10 digits — dates, house numbers, extensions, SSN-length
//     runs) are below the phone floor and are never masked.
//   - Number spans that overlap an email are dropped (an email's digits are not
//     a phone).
//
// DEFENSIVE: the input is glyph text; nothing here attaches an identity. It only
// finds + classifies substrings. Best-effort by construction — it will miss
// unusual formats and is NOT a guarantee (documented on ScrubResult.preview()).

import Foundation

/// The pure PII-span detector. Every method is a deterministic value transform,
/// so the unit tests drive it with literal strings and assert the found spans.
public enum PIIDetector {

    // -- length bands (the honest classification gates) -----------------------

    /// Phone: a 10–12 digit run (NANP, optional country code, optional trailing
    /// extension digits). Strictly BELOW the card band so a long number never
    /// falls through to a phone mask.
    public static let phoneDigitRange = 10...12
    /// Card / long account: a 13–19 digit run — masked ONLY if Luhn-valid.
    public static let cardDigitRange = 13...19

    // -- compiled patterns ----------------------------------------------------
    //
    // Compiled once (static). The email pattern is deliberately conservative;
    // the number pattern captures a CANDIDATE run of digits+separators which is
    // then classified by digit count + Luhn (the regex intentionally does not
    // try to be the whole decision — the pure classifier below is).

    /// `local@domain.tld` — conservative, single-line, case-insensitive local +
    /// domain classes; TLD of ≥2 letters.
    static let emailPattern =
        #"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"#

    /// A candidate numeric token: starts with `+`, `(`, or a digit, then a run
    /// of digits + common phone/card separators (space, dash, dot, parens),
    /// ending in a digit. The `{5,}` middle enforces a minimum length so tiny
    /// numbers (`42`, `1234`) never even become candidates.
    static let numberPattern =
        #"[+(]?\d[\d\s()\-.]{5,}\d"#

    private static let emailRegex = makeRegex(emailPattern)
    private static let numberRegex = makeRegex(numberPattern)

    private static func makeRegex(_ pattern: String) -> NSRegularExpression {
        // These literals are known-valid; a compile failure is a programmer
        // error, so trap loudly at first use rather than fail silently.
        // swiftlint:disable:next force_try
        try! NSRegularExpression(pattern: pattern, options: [.caseInsensitive])
    }

    // -- the entry point ------------------------------------------------------

    /// Find all PII spans in `text`, in SOURCE ORDER (ascending by start index),
    /// non-overlapping. Emails are found first; number candidates are classified
    /// as card (13–19 digits + Luhn) or phone (10–12 digits) and any number span
    /// overlapping an email is dropped.
    public static func detect(in text: String) -> [PIISpan] {
        guard !text.isEmpty else { return [] }

        var spans: [PIISpan] = []

        // 1. EMAILs.
        let emailRanges = matchRanges(emailRegex, in: text)
        for range in emailRanges {
            spans.append(PIISpan(kind: .email, range: range, matched: String(text[range])))
        }

        // 2. NUMBER candidates -> classify as card / phone / (skip).
        for range in matchRanges(numberRegex, in: text) {
            // Drop a number that overlaps an email (its digits are the email's).
            if emailRanges.contains(where: { rangesOverlap($0, range) }) { continue }
            classifyNumberCandidate(String(text[range]), at: range, in: text, into: &spans)
        }

        // Source order (ascending start). Emails and numbers can interleave.
        spans.sort { $0.range.lowerBound < $1.range.lowerBound }
        return spans
    }

    /// The band of ONE numeric token: card (13–19 digits AND Luhn-valid), phone
    /// (10–12), or `nil` (not PII). A long number that is not Luhn-valid is never
    /// masked (no over-mask) and never demoted to a phone.
    static func numberBand(_ token: String, _ digitCount: Int) -> PIIKind? {
        if cardDigitRange.contains(digitCount) {
            return luhnValid(token) ? .card : nil
        }
        if phoneDigitRange.contains(digitCount) {
            return .phone
        }
        return nil
    }

    /// Classify a number CANDIDATE (which — because the run class allows whitespace
    /// to catch a spaced phone like "555 123 4567" — may greedily span TWO
    /// bare-adjacent numbers) into 0+ spans. A real grouped phone/card has small
    /// groups (≤4 digits each: "555 123 4567", "4539 5787 6362 1486"); when a
    /// whitespace-separated group is LARGER, the whitespace joins SEPARATE numbers,
    /// so we split and classify each over ITS OWN range. Without this, two adjacent
    /// numbers merge into one out-of-band run and BOTH silently leak — the
    /// dangerous under-mask direction for a redactor (the review's finding).
    static func classifyNumberCandidate(
        _ token: String,
        at range: Range<String.Index>,
        in text: String,
        into spans: inout [PIISpan]
    ) {
        let digitsOf: (Substring) -> Int = { $0.reduce(0) { $0 + ($1.isNumber ? 1 : 0) } }

        // If the WHOLE candidate is a valid single number — a grouped phone/card like
        // "555 123 4567", "(555) 123-4567", or "4539 5787 6362 1486" — it is ONE
        // number. (Two benign shorts that happen to sum into a band, e.g. "12345
        // 67890", are structurally indistinguishable from a real spaced phone, so
        // masking them is the SAFE over-mask direction for a redactor.)
        let whole = token.reduce(0) { $0 + ($1.isNumber ? 1 : 0) }
        if let kind = numberBand(token, whole) {
            spans.append(PIISpan(kind: kind, range: range, matched: token))
            return
        }

        // Out of every band. If it contains whitespace, the run class glued SEPARATE
        // numbers together (e.g. "5551234567 5559876543" -> a 20-digit run): split
        // and classify each piece over ITS OWN range so both redact, instead of
        // silently leaking both as one out-of-band run (the review's under-mask leak).
        guard token.contains(where: { $0.isWhitespace }) else { return }
        var cursor = range.lowerBound
        for sub in token.split(omittingEmptySubsequences: true, whereSeparator: { $0.isWhitespace }) {
            while cursor < range.upperBound, text[cursor].isWhitespace {
                cursor = text.index(after: cursor)
            }
            let subStart = cursor
            for _ in 0..<sub.count { cursor = text.index(after: cursor) }
            let subToken = String(sub)
            if let kind = numberBand(subToken, digitsOf(sub)) {
                spans.append(PIISpan(kind: kind, range: subStart..<cursor, matched: subToken))
            }
        }
    }

    // -- Luhn -----------------------------------------------------------------

    /// The Luhn (mod-10) checksum over the DIGITS of `token` (non-digits are
    /// ignored, so a spaced/dashed card number validates as written). Requires at
    /// least 2 digits. This is the gate that separates a real card/account number
    /// from an arbitrary long number.
    public static func luhnValid(_ token: String) -> Bool {
        let digits = token.compactMap { $0.wholeNumberValue }
        return luhnValid(digits: digits)
    }

    /// Luhn over an explicit digit array (the testable core).
    public static func luhnValid(digits: [Int]) -> Bool {
        guard digits.count >= 2 else { return false }
        guard digits.allSatisfy({ (0...9).contains($0) }) else { return false }
        var sum = 0
        var double = false
        // Walk right-to-left, doubling every second digit.
        for d in digits.reversed() {
            var v = d
            if double {
                v *= 2
                if v > 9 { v -= 9 }
            }
            sum += v
            double.toggle()
        }
        return sum % 10 == 0
    }

    // -- helpers --------------------------------------------------------------

    /// All match ranges of `regex` in `text`, as `Range<String.Index>` (converted
    /// from the UTF-16 NSRange so downstream splicing stays Character-correct).
    private static func matchRanges(_ regex: NSRegularExpression, in text: String) -> [Range<String.Index>] {
        let full = NSRange(text.startIndex..<text.endIndex, in: text)
        return regex.matches(in: text, options: [], range: full).compactMap { m in
            Range(m.range, in: text)
        }
    }

    /// Whether two source ranges overlap at all.
    private static func rangesOverlap(_ a: Range<String.Index>, _ b: Range<String.Index>) -> Bool {
        a.lowerBound < b.upperBound && b.lowerBound < a.upperBound
    }
}
