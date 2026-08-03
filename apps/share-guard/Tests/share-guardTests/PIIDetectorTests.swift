// PIIDetectorTests.swift — PURE-logic tests for the PII-span detector. NO OCR, NO
// capture, NO socket — every test drives literal strings and asserts the found
// spans + the (non-)over-masking contract deterministically.

import XCTest
@testable import share_guard

final class PIIDetectorTests: XCTestCase {

    private func kinds(_ text: String) -> [PIIKind] {
        PIIDetector.detect(in: text).map(\.kind)
    }

    // --- emails --------------------------------------------------------------

    func testFindsEmail() {
        let spans = PIIDetector.detect(in: "reach me at jane.doe@example.com please")
        XCTAssertEqual(spans.map(\.kind), [.email])
        XCTAssertEqual(spans.first?.matched, "jane.doe@example.com")
    }

    func testFindsMultipleEmails() {
        let spans = PIIDetector.detect(in: "a@b.co and c.d+tag@sub.example.org")
        XCTAssertEqual(spans.map(\.kind), [.email, .email])
        XCTAssertEqual(spans[0].matched, "a@b.co")
        XCTAssertEqual(spans[1].matched, "c.d+tag@sub.example.org")
    }

    func testEmailWithDigitsDoesNotAlsoMatchAsPhone() {
        // The digits inside an email's local part must NOT be masked as a phone;
        // the number span overlaps the email and is dropped.
        let spans = PIIDetector.detect(in: "user12345678@example.com")
        XCTAssertEqual(spans.map(\.kind), [.email], "only the email, no phantom phone")
    }

    // --- phones --------------------------------------------------------------

    func testFindsFormattedPhone() {
        let spans = PIIDetector.detect(in: "call +1 (555) 123-4567 today")
        XCTAssertEqual(spans.map(\.kind), [.phone])
        XCTAssertTrue(spans.first?.matched.contains("555") ?? false)
    }

    func testFindsBarePhone() {
        // 10 digits, no separators -> phone.
        let spans = PIIDetector.detect(in: "num 5551234567 end")
        XCTAssertEqual(spans.map(\.kind), [.phone])
        XCTAssertEqual(spans.first?.matched, "5551234567")
    }

    func testFindsDashedPhone() {
        let spans = PIIDetector.detect(in: "555-123-4567")
        XCTAssertEqual(kinds("555-123-4567"), [.phone])
        XCTAssertEqual(spans.first?.matched, "555-123-4567")
    }

    // --- cards / long account numbers (Luhn-gated) ---------------------------

    func testFindsLuhnValidCard() {
        // 4111 1111 1111 1111 is the classic Luhn-valid Visa test number.
        let spans = PIIDetector.detect(in: "card 4111 1111 1111 1111 on file")
        XCTAssertEqual(spans.map(\.kind), [.card])
        XCTAssertEqual(spans.first?.matched, "4111 1111 1111 1111")
    }

    func testFindsLuhnValidCardNoSeparators() {
        let spans = PIIDetector.detect(in: "5555555555554444")   // Mastercard test, Luhn-valid
        XCTAssertEqual(spans.map(\.kind), [.card])
    }

    // --- NON-OVER-MASKING (the load-bearing contract) ------------------------

    func testBenignTextUntouched() {
        XCTAssertTrue(PIIDetector.detect(in: "The quick brown fox jumps over the lazy dog.").isEmpty)
        XCTAssertTrue(PIIDetector.detect(in: "Meeting at 3pm in room 204.").isEmpty)
    }

    func testShortNumberNotOverMasked() {
        // A short number (extension / order id / house number) is below the phone
        // floor and must NOT be masked.
        XCTAssertTrue(PIIDetector.detect(in: "ext 4521").isEmpty, "4 digits -> not PII")
        XCTAssertTrue(PIIDetector.detect(in: "order 12345 shipped").isEmpty, "5 digits -> not PII")
    }

    func testNineDigitNumberBelowPhoneFloorNotMasked() {
        // 9 digits (e.g. an SSN-length run) is below the 10-digit phone floor and
        // outside the card scope -> not masked (documented limitation, not a bug).
        XCTAssertTrue(PIIDetector.detect(in: "id 123-45-6789 here").isEmpty)
    }

    func testLongNumberThatFailsLuhnIsNotOverMasked() {
        // 16 digits but Luhn-INVALID -> it is NOT a card and must be left alone.
        // (This is the "a partial/invalid number is not over-masked" contract.)
        XCTAssertFalse(PIIDetector.luhnValid("1234 5678 9012 3456"), "precondition: Luhn-invalid")
        XCTAssertTrue(PIIDetector.detect(in: "ref 1234 5678 9012 3456 end").isEmpty,
                      "a long non-card number is not masked (no fall-through to phone)")
    }

    func testTwentyDigitNumberNotMasked() {
        // > 19 digits is outside every band -> not masked.
        XCTAssertTrue(PIIDetector.detect(in: "12345678901234567890").isEmpty)
    }

    // --- mixed payload: source order + all three kinds -----------------------

    func testMixedPayloadFindsAllThreeInOrder() {
        let text = "Email a@b.com, call 555-123-4567, card 4111111111111111."
        let spans = PIIDetector.detect(in: text)
        XCTAssertEqual(spans.map(\.kind), [.email, .phone, .card],
                       "spans returned in source order")
    }

    // --- Luhn checksum -------------------------------------------------------

    func testLuhnKnownVectors() {
        XCTAssertTrue(PIIDetector.luhnValid("4111 1111 1111 1111"))
        XCTAssertTrue(PIIDetector.luhnValid("5555 5555 5555 4444"))
        XCTAssertTrue(PIIDetector.luhnValid(digits: [7,9,9,2,7,3,9,8,7,1,3]), "classic Luhn example")
        XCTAssertFalse(PIIDetector.luhnValid(digits: [7,9,9,2,7,3,9,8,7,1,0]))
        XCTAssertFalse(PIIDetector.luhnValid("1234 5678 9012 3456"))
        XCTAssertFalse(PIIDetector.luhnValid(digits: []), "empty -> invalid")
        XCTAssertFalse(PIIDetector.luhnValid(digits: [5]), "single digit -> invalid")
    }

    func testDigitRangeConstants() {
        // The bands are non-overlapping and phone sits strictly below card.
        XCTAssertLessThan(PIIDetector.phoneDigitRange.upperBound, PIIDetector.cardDigitRange.lowerBound)
    }

    // --- adjacent-number merge (the review's under-mask leak) ----------------

    func testTwoAdjacentNumbersAreEachRedactedNotMergedAndLeaked() {
        // REGRESSION: two bare-adjacent numbers separated only by a space must NOT
        // merge into one out-of-band run that leaks BOTH. Each is classified.
        let phones = PIIDetector.detect(in: "call 5551234567 5559876543 now")
        XCTAssertEqual(phones.count, 2, "both phones detected, not merged: \(phones)")
        XCTAssertTrue(phones.allSatisfy { $0.kind == .phone })

        // Two Luhn-valid 16-digit cards side by side (a 32-digit merged run would leak).
        let cards = PIIDetector.detect(in: "4539578763621486 4485275742308327")
        XCTAssertEqual(cards.count, 2, "both cards detected, not merged: \(cards)")
        XCTAssertTrue(cards.allSatisfy { $0.kind == .card })
    }

    func testGroupedPhoneAndCardStillDetectAsOne() {
        // A genuine grouped phone / card (small <=4-digit groups) is ONE number.
        XCTAssertEqual(PIIDetector.detect(in: "555 123 4567").count, 1, "spaced phone is one")
        XCTAssertEqual(PIIDetector.detect(in: "4539 5787 6362 1486").first?.kind, .card, "spaced card is one card")
    }

    func testLoneShortNumbersAreNotMasked() {
        // A single short number (< phone band) is left alone. (Two adjacent shorts
        // that sum into the phone band are indistinguishable from a real spaced
        // phone, so masking THEM is the safe over-mask direction — not tested as a
        // no-op here, since the safe choice is to mask.)
        XCTAssertTrue(PIIDetector.detect(in: "code 12345 here").isEmpty, "a lone short number is untouched")
        XCTAssertTrue(PIIDetector.detect(in: "the year 2026 was").isEmpty, "a 4-digit year is untouched")
    }
    /// TWO CARD NUMBERS, WRITTEN THE WAY CARD NUMBERS ARE WRITTEN.
    ///
    /// The candidate run greedily spans both, lands out of every band, and used to
    /// be split into eight 4-digit atoms — none of which is a band on its own. So
    /// NOTHING was emitted: the "redacted copy" came back byte-identical to the
    /// input while the preview reported "No PII detected". The redactor failed open
    /// on exactly the data it exists to protect, and since the user then shares that
    /// copy themselves, the false negative actively induces the leak.
    ///
    /// Asserted on the SCRUBBED TEXT, not just the span count: some orderings would
    /// satisfy a count-only assertion while still leaking a PAN.
    func testTwoGroupedCardsSideBySideAreBothRedacted() {
        // Precondition — if these stopped being Luhn-valid the test would pass
        // vacuously, having proved nothing about regrouping.
        XCTAssertTrue(PIIDetector.luhnValid("4111 1111 1111 1111"))
        XCTAssertTrue(PIIDetector.luhnValid("5555 5555 5555 4444"))

        let text = "cards 4111 1111 1111 1111 5555 5555 5555 4444 ok"
        XCTAssertEqual(PIIDetector.detect(in: text).map(\.kind), [.card, .card])
        let out = ShareGuard.scrub(text: text).redactedText
        XCTAssertFalse(out.contains("4111"), "first PAN leaked: \(out)")
        XCTAssertFalse(out.contains("4444"), "second PAN leaked: \(out)")
        XCTAssertFalse(out.contains(where: { $0.isNumber }), "a digit survived: \(out)")
    }

    /// Mixed bare + grouped, BOTH orderings. The bare one used to redact and the
    /// grouped one used to leak, which is worse than a total failure: the user is
    /// shown a redaction happening and reasonably concludes the scan worked.
    func testMixedBareAndGroupedCardsBothRedacted() {
        for text in [
            "pay 4111111111111111 5555 5555 5555 4444 now",
            "pay 4111 1111 1111 1111 5555555555554444 now",
        ] {
            XCTAssertEqual(PIIDetector.detect(in: text).map(\.kind), [.card, .card], "\(text)")
            let out = ShareGuard.scrub(text: text).redactedText
            XCTAssertFalse(out.contains("4111"), "first PAN leaked from \(text): \(out)")
            XCTAssertFalse(out.contains("4444"), "second PAN leaked from \(text): \(out)")
        }
    }

    /// The regroup must not swallow neighbours. Two spaced PHONES stay two phones —
    /// this is the case the per-atom split was originally added for, and the card
    /// regroup runs in front of it.
    func testTwoAdjacentPhonesAreStillTwoPhones() {
        let text = "call 5551234567 5559876543 today"
        XCTAssertEqual(PIIDetector.detect(in: text).map(\.kind), [.phone, .phone])
        let out = ShareGuard.scrub(text: text).redactedText
        XCTAssertFalse(out.contains("5551234567"), "phone leaked: \(out)")
        XCTAssertFalse(out.contains("5559876543"), "phone leaked: \(out)")
    }

    /// KNOWN RESIDUAL, pinned so it is a documented limit rather than a surprise.
    ///
    /// A spaced PHONE sandwiched between two grouped cards is still missed: the
    /// card regroup consumes greedily around it and a 10-12 digit window has no
    /// checksum to gate a symmetric phone regroup on. Both CARDS redact — strictly
    /// better than before, when only the bare one did — but the phone survives.
    /// If this is ever fixed, this test should flip, not be deleted.
    func testKnownResidual_spacedPhoneBetweenTwoCardsIsMissed() {
        let text = "a@b.com 4111 1111 1111 1111 555 123 4567 4485275742308327"
        let out = ShareGuard.scrub(text: text).redactedText
        XCTAssertFalse(out.contains("4111"), "the first card must redact: \(out)")
        XCTAssertFalse(out.contains("4485275742308327"), "the second card must redact: \(out)")
        XCTAssertTrue(
            out.contains("555 123 4567"),
            "residual changed — a spaced phone between two cards now redacts. That is an "
                + "improvement: flip this assertion rather than deleting the test. Got: \(out)"
        )
    }

}
