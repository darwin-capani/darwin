// ShareGuardEventTests.swift — app->host wire-contract tests: every line is
// token-stamped + framed as the daemon expects, targets a declared share.* topic,
// and the redactions frame is SECRET-FREE (counts + preview + output path only —
// never the PII, never the redacted body).

import XCTest
import Foundation
@testable import share_guard

/// In-memory LineWriter that collects the framed lines (no socket).
private actor LineCollector: LineWriter {
    private var lines: [String] = []
    func writeLine(_ line: String) { lines.append(line) }
    func all() -> [String] { lines }
}

final class ShareGuardEventTests: XCTestCase {

    private func emitAndParse(_ event: ShareGuardEvent, token: String = "cafebabe") async throws -> [String: Any] {
        let collector = LineCollector()
        let sink = OutboundSink(token: token, writer: collector)
        await sink.emit(event)
        let lines = await collector.all()
        XCTAssertEqual(lines.count, 1, "one event -> one framed line")
        let data = try XCTUnwrap(lines[0].data(using: .utf8))
        return try XCTUnwrap(try JSONSerialization.jsonObject(with: data) as? [String: Any])
    }

    // --- redactions frame ----------------------------------------------------

    func testRedactionsFrameIsStampedFramedAndSecretFree() async throws {
        // Scrub a real payload, then emit ONLY the secret-free summary (never the
        // redacted body, which the event API structurally cannot carry).
        let scrub = ShareGuard.scrub(text: "a@b.com and card 4111111111111111")
        let event = ShareGuardEvent.redactions(
            counts: scrub.counts, total: scrub.total, foundPII: scrub.foundPII,
            preview: scrub.preview(), output: "redacted/out.txt",
            artifactId: "99", originalLength: scrub.originalLength)
        let obj = try await emitAndParse(event, token: "cafebabe")

        // Envelope: token stamped, type "items".
        XCTAssertEqual(obj["token"] as? String, "cafebabe")
        XCTAssertEqual(obj["type"] as? String, "items")

        let d = try XCTUnwrap(obj["data"] as? [String: Any])
        XCTAssertEqual(d["topic"] as? String, "share.redactions")
        XCTAssertEqual(d["total"] as? Int, 2)
        XCTAssertEqual(d["found_pii"] as? Bool, true)
        XCTAssertEqual(d["artifact_id"] as? String, "99")
        XCTAssertEqual(d["output"] as? String, "redacted/out.txt")
        let byKind = try XCTUnwrap(d["by_kind"] as? [String: Int])
        XCTAssertEqual(byKind["email"], 1)
        XCTAssertEqual(byKind["card"], 1)
        XCTAssertNil(byKind["phone"], "a kind with zero is omitted")

        // SECRET-FREE: neither the PII nor the redacted body ride the wire.
        let whole = try String(data: JSONSerialization.data(withJSONObject: obj), encoding: .utf8) ?? ""
        XCTAssertFalse(whole.contains("a@b.com"), "the email never reaches telemetry")
        XCTAssertFalse(whole.contains("4111"), "no card digits reach telemetry")
        XCTAssertFalse(whole.contains("[EMAIL REDACTED]"), "the redacted body is not on the wire")
    }

    // --- status + error frames -----------------------------------------------

    func testStatusFrame() async throws {
        let obj = try await emitAndParse(.status(state: .scrubbing, message: "working"))
        XCTAssertEqual(obj["type"] as? String, "status")
        let d = try XCTUnwrap(obj["data"] as? [String: Any])
        XCTAssertEqual(d["topic"] as? String, "share.status")
        XCTAssertEqual(d["state"] as? String, "scrubbing")
        XCTAssertEqual(d["message"] as? String, "working")
    }

    func testErrorFrame() async throws {
        let obj = try await emitAndParse(.error(code: "path_denied", message: "nope"))
        XCTAssertEqual(obj["type"] as? String, "status")
        let d = try XCTUnwrap(obj["data"] as? [String: Any])
        XCTAssertEqual(d["topic"] as? String, "share.error")
        XCTAssertEqual(d["code"] as? String, "path_denied")
        XCTAssertEqual(d["message"] as? String, "nope")
    }

    // --- topic declarations match the manifest -------------------------------

    func testDeclaredTopics() {
        XCTAssertEqual(ShareTopic.all, ["share.redactions", "share.status", "share.error"])
        XCTAssertEqual(ShareTopic.all.first, ShareTopic.redactions, "first declared = default topic")
    }
}
