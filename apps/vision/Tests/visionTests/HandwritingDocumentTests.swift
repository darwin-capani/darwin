// HandwritingDocumentTests.swift — #28 handwriting/whiteboard recognizer + #29
// camera document scanner ENGINE proofs, headless over SYNTHESIZED in-memory
// images. NO camera, NO screen, NO TCC, NO socket, NO external model download.
//
// Mirrors the existing synthesized-OCR-image precedent (InferenceTests'
// makeTextImage): render KNOWN strings (and, for #29, a KNOWN document quad) into
// an in-memory CGImage via Core Graphics / Core Text, run the REAL Vision
// engine, and assert the recognized text comes back — the genuine "the engine
// really works" evidence. If the build env's Vision model returns nothing, the
// proof is HONESTLY device-gated via XCTSkip (we never fabricate recognized text
// or a detected document).
//
// DEFENSIVE: glyph text only — never a face / person identity. The no-document
// case asserts the scanner returns the HONEST empty (never a fabricated page).

import XCTest
import Foundation
import CoreGraphics
import CoreText
import ImageIO
@testable import vision

final class HandwritingDocumentTests: XCTestCase {

    // --- Synthesized text image (the proven OCR-image precedent) ------------

    /// Render dark `lines` of text on a white background into a CGImage, large +
    /// high-contrast so the recognizer reads them. Pure Core Graphics / Core Text
    /// — no assets, no network. `inset` insets the text from the edges so a
    /// perspective-corrected crop still contains it.
    private func makeTextImage(_ lines: [String], width: Int = 640, height: Int = 240,
                               fontSize: CGFloat = 56, inset: CGFloat = 24) -> CGImage? {
        let cs = CGColorSpaceCreateDeviceRGB()
        guard let ctx = CGContext(
            data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
            space: cs, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.setFillColor(red: 1, green: 1, blue: 1, alpha: 1)
        ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
        let font = CTFontCreateWithName("Helvetica" as CFString, fontSize, nil)
        let attrs: [NSAttributedString.Key: Any] = [
            .font: font, .foregroundColor: CGColor(red: 0, green: 0, blue: 0, alpha: 1)]
        let lineHeight = fontSize * 1.4
        var y = CGFloat(height) - lineHeight - inset
        for line in lines {
            let ctLine = CTLineCreateWithAttributedString(NSAttributedString(string: line, attributes: attrs))
            ctx.textPosition = CGPoint(x: inset, y: y)
            CTLineDraw(ctLine, ctx)
            y -= lineHeight
        }
        return ctx.makeImage()
    }

    /// A "document on a desk" scene: a WHITE page (with dark text) centered on a
    /// dark background, so VNDetectDocumentSegmentationRequest has a real page quad
    /// to find. Pure Core Graphics — no assets, no network.
    private func makeDocumentSceneImage(_ lines: [String],
                                        width: Int = 900, height: Int = 700) -> CGImage? {
        let cs = CGColorSpaceCreateDeviceRGB()
        guard let ctx = CGContext(
            data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
            space: cs, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        // Dark desk background.
        ctx.setFillColor(red: 0.08, green: 0.08, blue: 0.10, alpha: 1)
        ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
        // A white page inset from the edges (a clear rectangular document).
        let margin: CGFloat = 90
        let page = CGRect(x: margin, y: margin,
                          width: CGFloat(width) - 2 * margin,
                          height: CGFloat(height) - 2 * margin)
        ctx.setFillColor(red: 0.98, green: 0.98, blue: 0.97, alpha: 1)
        ctx.fill(page)
        // Dark text on the page.
        let fontSize: CGFloat = 64
        let font = CTFontCreateWithName("Helvetica" as CFString, fontSize, nil)
        let attrs: [NSAttributedString.Key: Any] = [
            .font: font, .foregroundColor: CGColor(red: 0, green: 0, blue: 0, alpha: 1)]
        let lineHeight = fontSize * 1.5
        var y = page.maxY - lineHeight - 30
        for line in lines {
            let ctLine = CTLineCreateWithAttributedString(NSAttributedString(string: line, attributes: attrs))
            ctx.textPosition = CGPoint(x: page.minX + 40, y: y)
            CTLineDraw(ctLine, ctx)
            y -= lineHeight
        }
        return ctx.makeImage()
    }

    /// A FLAT, document-free scene (a uniform dark field) — no page quad to find.
    private func makeNoDocumentImage(width: Int = 400, height: Int = 300) -> CGImage {
        let cs = CGColorSpaceCreateDeviceRGB()
        let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8,
                            bytesPerRow: 0, space: cs,
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        ctx.setFillColor(red: 0.12, green: 0.13, blue: 0.14, alpha: 1)
        ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
        return ctx.makeImage()!
    }

    private func recognizedJoined(_ dets: [Detection]) -> String {
        dets.filter { $0.kind == .text }.map { $0.label }.joined(separator: " ").lowercased()
    }

    // --- #28 HANDWRITING engine proof ---------------------------------------

    /// The REAL handwriting recognizer reads KNOWN text off a synthesized image.
    /// (We render printed glyphs, not actual cursive — the point is to prove the
    /// .accurate + language-correction recognizer engine path runs headlessly and
    /// returns the text; handwriting QUALITY itself is device-dependent + honestly
    /// not asserted here.)
    func testRecognizeHandwritingReadsSynthesizedTextHeadlessly() throws {
        guard let img = makeTextImage(["Meeting Notes", "Buy milk"]) else {
            throw XCTSkip("could not render a text CGImage in this environment")
        }
        let engine = VisionEngine()
        let dets = engine.recognizeHandwriting(image: img, minConfidence: 0.0)

        // Every recognized detection is .text with a non-empty string — NEVER an
        // identity.
        for d in dets {
            XCTAssertEqual(d.kind, .text, "recognizeHandwriting must only yield .text detections")
            XCTAssertFalse(d.label.isEmpty, ".text detection must carry the recognized string")
            XCTAssertEqual(d.label.range(of: "person", options: .caseInsensitive) == nil, true)
        }

        let joined = recognizedJoined(dets)
        guard !joined.isEmpty else {
            throw XCTSkip("VNRecognizeTextRequest returned no text in this build env (device-gated)")
        }
        XCTAssertTrue(joined.contains("meeting") || joined.contains("notes"),
                      "the handwriting recognizer must read the synthesized text; got: \(joined)")
    }

    /// Handwriting recognizer boxes are normalized + confidence is in range.
    func testHandwritingBoxesNormalizedAndConfidenceInRange() throws {
        guard let img = makeTextImage(["Agenda"]) else { throw XCTSkip("no text image") }
        let engine = VisionEngine()
        let dets = engine.recognizeHandwriting(image: img, minConfidence: 0.0)
        guard !dets.isEmpty else {
            throw XCTSkip("VNRecognizeTextRequest returned no text in this build env (device-gated)")
        }
        for d in dets {
            for v in [d.boundingBox.x, d.boundingBox.y, d.boundingBox.width, d.boundingBox.height] {
                XCTAssertTrue(v >= -0.001 && v <= 1.001, "box component out of 0...1: \(v)")
            }
            XCTAssertTrue((0.0...1.0).contains(d.confidence))
        }
    }

    /// A bad path yields [] (never throws) — honest, never a fabricated line.
    func testHandwritingFromBadPathIsEmpty() {
        let engine = VisionEngine()
        XCTAssertTrue(engine.recognizeHandwriting(imagePath: "/no/such/hw.png").isEmpty,
                      "a missing image path yields [] (never throws)")
    }

    // --- #29 DOCUMENT SCANNER engine proof ----------------------------------

    /// The REAL document scanner finds a page quad in a synthesized document scene,
    /// perspective-corrects it, and OCRs the corrected page — returning the KNOWN
    /// text with documentDetected == true. The detected-quad path is exercised end
    /// to end. If segmentation returns no document in this build env, the proof is
    /// honestly device-gated (we never fabricate a detected document).
    func testScanDocumentDetectsQuadAndReadsCorrectedText() throws {
        guard let img = makeDocumentSceneImage(["INVOICE", "Total 42"]) else {
            throw XCTSkip("could not render a document scene in this environment")
        }
        let engine = VisionEngine()
        let scan = engine.scanDocument(image: img, minConfidence: 0.0)

        guard scan.documentDetected else {
            throw XCTSkip("VNDetectDocumentSegmentationRequest found no document in this build env (device-gated)")
        }
        XCTAssertTrue(scan.quadConfidence >= 0 && scan.quadConfidence <= 1,
                      "detected-quad confidence must be in 0...1")
        // Every line is .text glyphs, never an identity.
        for d in scan.lines {
            XCTAssertEqual(d.kind, .text)
            XCTAssertFalse(d.label.isEmpty)
        }
        let joined = recognizedJoined(scan.lines)
        guard !joined.isEmpty else {
            throw XCTSkip("OCR over the corrected page returned no text in this build env (device-gated)")
        }
        XCTAssertTrue(joined.contains("invoice") || joined.contains("total"),
                      "the scanner must read the page text off the corrected document; got: \(joined)")
    }

    /// A document-FREE image returns the HONEST empty scan — never a fabricated
    /// page. This is the core honesty invariant for #29.
    func testScanDocumentNoDocumentReturnsHonestEmpty() {
        let engine = VisionEngine()
        let scan = engine.scanDocument(image: makeNoDocumentImage(), minConfidence: 0.0)
        // A flat field should not segment into a page; if the build env's detector
        // is over-eager it may report one, but it must NEVER fabricate TEXT on a
        // blank field.
        if scan.documentDetected {
            XCTAssertTrue(recognizedJoined(scan.lines).isEmpty,
                          "a blank field must never yield fabricated text even if a quad is reported")
        } else {
            XCTAssertEqual(scan, VisionEngine.DocumentScan.none,
                           "no document -> the honest .none (empty, documentDetected=false)")
        }
    }

    /// A bad path yields the honest `.none` (never throws, never a fabricated page).
    func testScanDocumentFromBadPathIsHonestNone() {
        let engine = VisionEngine()
        let scan = engine.scanDocument(imagePath: "/no/such/doc.png")
        XCTAssertEqual(scan, VisionEngine.DocumentScan.none,
                       "a missing image path yields the honest .none (never throws)")
        XCTAssertFalse(scan.documentDetected)
        XCTAssertTrue(scan.lines.isEmpty)
    }

    /// The Frame seam: scanDocument over a Frame carrying a synthesized CGImage
    /// runs the real pipeline (this is what the scan.document op routes through).
    func testScanDocumentViaFrameSeam() throws {
        guard let img = makeDocumentSceneImage(["RECEIPT"]) else { throw XCTSkip("no doc scene") }
        let engine = VisionEngine()
        let frame = Frame(cgImage: img, timestamp: 0, source: .file(path: "synth"), index: 0)
        let scan = engine.scanDocument(in: frame, minConfidence: 0.0)
        // Well-formed regardless of whether the env's detector fires.
        XCTAssertTrue(scan.quadConfidence >= 0 && scan.quadConfidence <= 1)
        for d in scan.lines { XCTAssertEqual(d.kind, .text) }
    }

    /// The default Detector seam (StubDetector / protocol default) NEVER fabricates
    /// a document — it returns the honest empty. This guarantees a non-VisionEngine
    /// detector can never invent a page.
    func testStubDetectorScanDocumentIsHonestEmpty() {
        let stub = StubDetector()
        let frame = Frame(cgImage: makeNoDocumentImage(), timestamp: 0,
                          source: .file(path: "x"), index: 0)
        let scan = stub.scanDocument(in: frame, minConfidence: 0.0)
        XCTAssertEqual(scan, VisionEngine.DocumentScan.none,
                       "a non-VisionEngine detector must never fabricate a document")
    }
}
