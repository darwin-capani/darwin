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

    /// WAS A KNOWN RESIDUAL, now closed — kept as the regression that pins the fix.
    ///
    /// A spaced PHONE sandwiched between two grouped cards used to be missed: the
    /// card regroup consumed greedily around it and there was no symmetric phone
    /// regroup, so "555"/"123"/"4567" reached the per-atom pass as three sub-floor
    /// atoms and nothing was emitted. The shape-gated phone regroup now claims it.
    /// (The previous version of this test asserted the leak; it was flipped, not
    /// deleted, exactly as its own comment instructed.)
    func testSpacedPhoneBetweenTwoCardsIsRedacted() {
        let text = "a@b.com 4111 1111 1111 1111 555 123 4567 4485275742308327"
        let out = ShareGuard.scrub(text: text).redactedText
        XCTAssertFalse(out.contains("4111"), "the first card must redact: \(out)")
        XCTAssertFalse(out.contains("4485275742308327"), "the second card must redact: \(out)")
        XCTAssertFalse(out.contains("555 123 4567"), "the sandwiched phone leaked: \(out)")
    }

    // --- glued PHONE under-mask (the review's second under-mask leak) --------

    /// THE CANONICAL `scrub.image` SHAPE — A BUSINESS CARD / LETTERHEAD.
    ///
    /// VNRecognizeTextRequest joins recognized lines with `\n`, and the number
    /// candidate class contains `\s`, so a suite/zip/extension number on one line
    /// glues to the phone on the next into ONE candidate: "1200\n555 123 4567" is 14
    /// digits — out of the phone band, into the card band, Luhn-invalid — so no card
    /// window claimed it and the per-atom fallback saw only 3-4 digit atoms. The
    /// "redacted" copy came back carrying the full phone number while the preview
    /// said "No PII detected", and the user is the one who then shares that copy.
    func testPhoneGluedToPrecedingNumberAcrossANewlineIsRedacted() {
        let text = "Acme Corp\n1 Market St Suite 1200\n555 123 4567\nbilling@acme.com"
        let spans = PIIDetector.detect(in: text)
        XCTAssertEqual(spans.map(\.kind), [.phone, .email],
                       "the glued phone must be found, in source order: \(spans)")
        XCTAssertEqual(spans.first?.matched, "555 123 4567",
                       "the span must cover exactly the phone, not the suite number")

        let out = ShareGuard.scrub(text: text).redactedText
        XCTAssertFalse(out.contains("555 123 4567"), "phone leaked: \(out)")
        XCTAssertTrue(out.contains("Suite 1200"), "the suite number is not PII and must survive: \(out)")
    }

    /// The same glue on ONE line, and with the paren/dash formatting OCR actually
    /// emits — the shape gate reads DIGIT RUNS, not whitespace atoms, so "(555)
    /// 123-4567" is the same 3-3-4 as "555 123 4567".
    func testPhoneGluedToPrecedingNumberOnOneLineIsRedacted() {
        for (text, leaked) in [
            ("1234 555 123 4567", "555 123 4567"),
            ("Zip 94105 555 123 4567", "555 123 4567"),
            ("Suite 1200 (555) 123-4567", "(555) 123-4567"),
        ] {
            let spans = PIIDetector.detect(in: text)
            XCTAssertEqual(spans.map(\.kind), [.phone], "expected exactly one phone in \(text): \(spans)")
            XCTAssertEqual(spans.first?.matched, leaked, "span must cover only the phone in \(text)")
            let out = ShareGuard.scrub(text: text).redactedText
            XCTAssertFalse(out.contains(leaked), "phone leaked from \(text): \(out)")
        }
    }

    /// THE GATE THAT KEEPS THE PHONE REGROUP HONEST. A 10-12 digit window has no
    /// checksum, so the regroup stands on the window's DIGIT-RUN SHAPE instead. 4-4-4
    /// is not a dialling grouping — if it were accepted, the first three groups of
    /// "ref 1234 5678 9012 3456 end" (12 digits) would be masked and the
    /// non-over-masking contract would be gone.
    func testPhoneRegroupShapeGate() {
        XCTAssertTrue(PIIDetector.isPhoneGrouping("555 123 4567"))
        XCTAssertTrue(PIIDetector.isPhoneGrouping("(555) 123-4567"))
        XCTAssertTrue(PIIDetector.isPhoneGrouping("1 555 123 4567"), "1-digit country code")
        XCTAssertTrue(PIIDetector.isPhoneGrouping("44 555 123 4567"), "2-digit country code")
        XCTAssertFalse(PIIDetector.isPhoneGrouping("1234 5678 9012"), "4-4-4 is not a phone shape")
        XCTAssertFalse(PIIDetector.isPhoneGrouping("12345 12345"), "5-5 is not a phone shape")
        XCTAssertFalse(PIIDetector.isPhoneGrouping("555 123 4567 8901"), "3-3-4-4 is not a phone shape")

        // End to end: the long Luhn-invalid run stays untouched even though a 4-4-4
        // sub-window of it lands squarely inside the 10-12 phone band.
        XCTAssertTrue(PIIDetector.detect(in: "ref 1234 5678 9012 3456 end").isEmpty,
                      "the phone regroup must not swallow a long non-card number")
    }

    // --- separator-run blowup (the cubic regroup) ----------------------------

    /// A WALL OF DIGIT-FREE SEPARATORS USED TO PEG A CORE FOR MINUTES.
    ///
    /// The card regroup's window loop bounded `j` by ACCUMULATED DIGITS only. A group
    /// of pure `-`/`.`/`(`/`)` contributes zero digits, so `digits + 0 <= 19` was
    /// always true and `j` ran to the end of the array; the k-loop then walked O(N)
    /// windows, each re-slicing O(N) characters, inside an O(N) outer loop. Measured
    /// before the fix on this exact input shape: 0.62 s at n=400, 4.81 s at n=800,
    /// 38.4 s at n=1600. `Pipeline` is an actor with no cancellation, so the app
    /// answered nothing at all — not even `{"type":"stop"}` — for the duration.
    ///
    /// n=2000 here is ~2x the n=1600 that took 38 s, i.e. ~5 minutes unfixed. The
    /// budget below is deliberately loose so this measures the COMPLEXITY CLASS and
    /// not this machine's clock.
    func testSeparatorRunDoesNotBlowUpTheRegroup() {
        let text = "1 " + String(repeating: "- ", count: 2000) + "1"
        let started = Date()
        let out = ShareGuard.scrub(text: text)
        let elapsed = Date().timeIntervalSince(started)

        // Precondition: the input really does reach the regroup (the candidate spans
        // the whole wall). If the candidate pattern ever stops matching this, the
        // timing assertion would pass vacuously.
        XCTAssertGreaterThan(text.count, 4000)
        XCTAssertEqual(out.total, 0, "two lone 1-digit numbers are not PII")
        XCTAssertLessThan(elapsed, 2.0,
                          "regroup is superlinear in separator groups again (took \(elapsed)s)")
    }

    /// Dot leaders and checkbox glyphs — the same blowup in the shapes it actually
    /// arrives in from OCR — and a control proving the separators are still allowed
    /// INSIDE a card window (the fix skips them only as window ENDPOINTS).
    func testSeparatorRunRealisticShapesAndLooselyWrittenCardStillDetected() {
        for text in [
            "Total 1 " + String(repeating: ". ", count: 1500) + "2 pages",
            "Item 1 " + String(repeating: "( ) ", count: 1000) + "2",
        ] {
            let started = Date()
            _ = ShareGuard.scrub(text: text)
            XCTAssertLessThan(Date().timeIntervalSince(started), 2.0,
                              "regroup blew up on a realistic separator run")
        }

        // CONTROL: separator-only atoms are still allowed INSIDE a card window — the
        // fix refuses them only as window ENDPOINTS. Two loosely written cards glued
        // together (32 digits, out of every band) must both regroup, and the first
        // span must stop at the last DIGIT group, not swallow the trailing " -".
        let loose = "4111 - 1111 - 1111 - 1111 - 5555 5555 5555 4444"
        let spans = PIIDetector.detect(in: loose)
        XCTAssertEqual(spans.map(\.kind), [.card, .card], "loosely written cards lost: \(spans)")
        XCTAssertEqual(spans.first?.matched, "4111 - 1111 - 1111 - 1111",
                       "the card span must start and end on a digit group")
    }

    /// REGRESSION: BOUNDING THE CARD WINDOW BY GROUP COUNT FAILS OPEN.
    ///
    /// The separator-run fix originally capped the card window at 20 whitespace
    /// groups. A Luhn-valid card written with a separator group between every
    /// digit occupies 31 groups, so whenever the WHOLE candidate was out of every
    /// band — a neighbouring number glued on, which is the same OCR glue the
    /// phone tests above are about — no window could reach the 13-19 digit band.
    /// The scrub returned "No PII detected" with the card verbatim in the
    /// "redacted" copy: the exact under-mask this regroup exists to prevent, and
    /// the pristine code caught it. Only the DIGIT cap may bound the window; the
    /// seed/endpoint skips are what make the pass linear.
    func testWidelySeparatedCardStillRedactsWhenTheWholeCandidateIsOutOfBand() {
        let card = "4111111111111111"   // 16 digits, Luhn-valid
        for sep in [" - ", " . ", " ( ", " "] {
            let spelled = card.map(String.init).joined(separator: sep)
            let text = "A " + spelled + " 9 9 9 9 end"

            // Precondition: the whole candidate is OUT of every band, so the
            // whole-candidate short-circuit cannot claim it and the regroup is
            // the only path. Without this the test could pass without ever
            // exercising the window loop.
            let digits = text.reduce(0) { $0 + ($1.isNumber ? 1 : 0) }
            XCTAssertEqual(digits, 20, "candidate must be out of band for \(sep.debugDescription)")

            let spans = PIIDetector.detect(in: text)
            XCTAssertTrue(
                spans.contains { $0.kind == .card },
                "the card was lost with separator \(sep.debugDescription): \(spans)"
            )
            let out = ShareGuard.scrub(text: text).redactedText
            XCTAssertFalse(out.contains(spelled), "card leaked verbatim: \(out)")
        }
    }

    // --- separator-run blowup (the cubic regroup) ----------------------------

}
