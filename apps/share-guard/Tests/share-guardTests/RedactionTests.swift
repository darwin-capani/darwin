// RedactionTests.swift — PURE-logic tests for the redaction composition + the
// top-level scrub. Asserts that detected PII is MASKED (the original substrings
// are gone, the markers present), benign text survives verbatim, counts are
// right, and the preview is honest + secret-free.

import XCTest
@testable import share_guard

final class RedactionTests: XCTestCase {

    // --- masking: PII removed, markers present, surrounding text intact -------

    func testEmailIsMasked() {
        let r = ShareGuard.scrub(text: "write to jane.doe@example.com now")
        XCTAssertFalse(r.redactedText.contains("jane.doe@example.com"), "the email is gone")
        XCTAssertTrue(r.redactedText.contains("[EMAIL REDACTED]"))
        XCTAssertEqual(r.redactedText, "write to [EMAIL REDACTED] now", "surrounding text intact")
        XCTAssertEqual(r.count(.email), 1)
        XCTAssertTrue(r.foundPII)
    }

    func testPhoneIsMasked() {
        let r = ShareGuard.scrub(text: "call 555-123-4567 please")
        XCTAssertFalse(r.redactedText.contains("555-123-4567"))
        XCTAssertEqual(r.redactedText, "call [PHONE REDACTED] please")
        XCTAssertEqual(r.count(.phone), 1)
    }

    func testCardIsMasked() {
        let r = ShareGuard.scrub(text: "card 4111 1111 1111 1111 exp")
        XCTAssertFalse(r.redactedText.contains("4111 1111 1111 1111"))
        XCTAssertFalse(r.redactedText.contains("4111"), "no digit of the card survives")
        XCTAssertEqual(r.redactedText, "card [CARD REDACTED] exp")
        XCTAssertEqual(r.count(.card), 1)
    }

    func testAllThreeMaskedInOnePass() {
        let text = "Email a@b.com, call 555-123-4567, card 4111111111111111."
        let r = ShareGuard.scrub(text: text)
        XCTAssertEqual(r.total, 3)
        XCTAssertEqual(r.count(.email), 1)
        XCTAssertEqual(r.count(.phone), 1)
        XCTAssertEqual(r.count(.card), 1)
        XCTAssertFalse(r.redactedText.contains("a@b.com"))
        XCTAssertFalse(r.redactedText.contains("555-123-4567"))
        XCTAssertFalse(r.redactedText.contains("4111111111111111"))
        XCTAssertEqual(r.redactedText,
                       "Email [EMAIL REDACTED], call [PHONE REDACTED], card [CARD REDACTED].")
    }

    // --- benign text survives untouched --------------------------------------

    func testBenignTextUnchanged() {
        let text = "The quick brown fox jumps over the lazy dog. Meet at 3pm, room 204."
        let r = ShareGuard.scrub(text: text)
        XCTAssertFalse(r.foundPII)
        XCTAssertEqual(r.redactedText, text, "no PII -> the text is returned verbatim")
        XCTAssertEqual(r.total, 0)
    }

    func testLongNonCardNumberSurvives() {
        // A 16-digit Luhn-INVALID number must not be masked.
        let text = "Reference 1234 5678 9012 3456 for the ticket."
        let r = ShareGuard.scrub(text: text)
        XCTAssertEqual(r.redactedText, text, "an invalid long number is left alone")
        XCTAssertFalse(r.foundPII)
    }

    func testEmptyInput() {
        let r = ShareGuard.scrub(text: "")
        XCTAssertEqual(r.redactedText, "")
        XCTAssertFalse(r.foundPII)
        XCTAssertEqual(r.originalLength, 0)
    }

    // --- mask style ----------------------------------------------------------

    func testFixedMaskStyle() {
        let r = ShareGuard.scrub(text: "a@b.com", mask: .fixed)
        XCTAssertEqual(r.redactedText, "[REDACTED]")
    }

    // --- preview honesty (secret-free) ---------------------------------------

    func testPreviewIsSecretFreeAndCountsKinds() {
        let text = "Email a@b.com, call 555-123-4567, card 4111111111111111."
        let r = ShareGuard.scrub(text: text)
        let preview = r.preview()
        // The preview names WHAT was redacted, never the PII itself.
        XCTAssertTrue(preview.contains("3 items"))
        XCTAssertTrue(preview.contains("1 email"))
        XCTAssertTrue(preview.contains("1 phone"))
        XCTAssertTrue(preview.contains("1 card"))
        XCTAssertTrue(preview.lowercased().contains("best-effort"), "honest framing")
        XCTAssertFalse(preview.contains("a@b.com"))
        XCTAssertFalse(preview.contains("4111"))
        XCTAssertFalse(preview.contains("555"))
    }

    func testPreviewWhenNothingFound() {
        let r = ShareGuard.scrub(text: "nothing to see here")
        XCTAssertTrue(r.preview().lowercased().contains("no pii"))
    }

    func testOriginalLengthIsReported() {
        let text = "hello a@b.com"
        let r = ShareGuard.scrub(text: text)
        XCTAssertEqual(r.originalLength, text.count)
    }
}
