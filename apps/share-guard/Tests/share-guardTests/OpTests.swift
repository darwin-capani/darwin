// OpTests.swift — decoding tests for the host->app op vocabulary. Total decoding:
// a malformed/unknown line becomes .unknown, never a crash.

import XCTest
@testable import share_guard

final class OpTests: XCTestCase {

    // --- control verbs -------------------------------------------------------

    func testControlVerbs() {
        XCTAssertEqual(Op.decode(line: #"{"type":"start"}"#), .start)
        XCTAssertEqual(Op.decode(line: #"{"type":"refresh"}"#), .refresh)
        XCTAssertEqual(Op.decode(line: #"{"type":"stop"}"#), .stop)
    }

    // --- scrub.text ----------------------------------------------------------

    func testScrubTextWithArtifactId() {
        let op = Op.decode(line: #"{"type":"op","op":"scrub.text","text":"a@b.com","artifact_id":"42"}"#)
        XCTAssertEqual(op, .scrubText(text: "a@b.com", artifactId: "42"))
    }

    func testScrubTextWithoutArtifactId() {
        let op = Op.decode(line: #"{"type":"op","op":"scrub.text","text":"hi"}"#)
        XCTAssertEqual(op, .scrubText(text: "hi", artifactId: nil))
    }

    func testScrubTextEmptyStringIsValid() {
        // An empty text is a valid (no-PII) payload; only a MISSING text is bad.
        XCTAssertEqual(Op.decode(line: #"{"type":"op","op":"scrub.text","text":""}"#),
                       .scrubText(text: "", artifactId: nil))
    }

    func testScrubTextMissingTextIsUnknown() {
        if case .unknown = Op.decode(line: #"{"type":"op","op":"scrub.text"}"#) {} else {
            XCTFail("scrub.text without text must be .unknown")
        }
    }

    func testEmptyArtifactIdBecomesNil() {
        XCTAssertEqual(Op.decode(line: #"{"type":"op","op":"scrub.text","text":"x","artifact_id":""}"#),
                       .scrubText(text: "x", artifactId: nil))
    }

    // --- scrub.image ---------------------------------------------------------

    func testScrubImage() {
        XCTAssertEqual(Op.decode(line: #"{"type":"op","op":"scrub.image","path":"staged.png"}"#),
                       .scrubImage(path: "staged.png", artifactId: nil))
        XCTAssertEqual(Op.decode(line: #"{"type":"op","op":"scrub.image","path":"s.png","artifact_id":"7"}"#),
                       .scrubImage(path: "s.png", artifactId: "7"))
    }

    func testScrubImageMissingPathIsUnknown() {
        if case .unknown = Op.decode(line: #"{"type":"op","op":"scrub.image"}"#) {} else {
            XCTFail("scrub.image without path must be .unknown")
        }
    }

    // --- status + unknowns ---------------------------------------------------

    func testStatus() {
        XCTAssertEqual(Op.decode(line: #"{"type":"op","op":"status"}"#), .status)
    }

    func testUnknownOpName() {
        if case .unknown = Op.decode(line: #"{"type":"op","op":"delete.everything"}"#) {} else {
            XCTFail("an unknown op must be .unknown (no phantom action)")
        }
    }

    func testMalformedLineIsUnknown() {
        if case .unknown = Op.decode(line: "not json at all") {} else { XCTFail() }
        if case .unknown = Op.decode(line: "") {} else { XCTFail() }
        if case .unknown = Op.decode(line: #"{"type":"op"}"#) {} else { XCTFail("op with no name") }
    }

    // --- wire names ----------------------------------------------------------

    func testWireNames() {
        XCTAssertEqual(Op.scrubText(text: "x", artifactId: nil).wireName, "scrub.text")
        XCTAssertEqual(Op.scrubImage(path: "p", artifactId: nil).wireName, "scrub.image")
        XCTAssertEqual(Op.status.wireName, "status")
        XCTAssertNil(Op.unknown(raw: "x").wireName)
    }
}
