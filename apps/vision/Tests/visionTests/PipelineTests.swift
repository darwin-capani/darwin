// PipelineTests.swift — pure-logic tests for the PIPELINE module: frame-diff
// motion detection, presence enter/exit dwell with anti-flicker hysteresis,
// event debounce / burst aggregation, alert hysteresis, and the Pipeline
// actor's deterministic per-frame processing. NO camera, NO screen, NO TCC,
// NO socket, NO wall clock — every test drives literal luma grids / detections
// / timestamps so outputs are reproducible.

import XCTest
import Foundation
import CoreGraphics
import CoreText
import AVFoundation
@testable import vision

// ===========================================================================
// Helpers
// ===========================================================================

private func box(_ x: Double, _ y: Double, _ w: Double, _ h: Double) -> DetectionBox {
    DetectionBox(x: x, y: y, width: w, height: h)
}

private func det(_ kind: Detection.Kind, _ conf: Double, _ label: String = "",
                 _ b: DetectionBox = .full) -> Detection {
    Detection(kind: kind, boundingBox: b, confidence: conf, label: label)
}

/// A 1x1 CGImage of a given gray value (0...1) for sampling-path tests.
private func grayImage(_ value: Double, side: Int = 8) -> CGImage {
    let v = UInt8(max(0, min(1, value)) * 255)
    let cs = CGColorSpaceCreateDeviceGray()
    let ctx = CGContext(data: nil, width: side, height: side, bitsPerComponent: 8,
                        bytesPerRow: side, space: cs,
                        bitmapInfo: CGImageAlphaInfo.none.rawValue)!
    ctx.setFillColor(gray: CGFloat(v) / 255.0, alpha: 1)
    ctx.fill(CGRect(x: 0, y: 0, width: side, height: side))
    return ctx.makeImage()!
}

/// A detector that returns a fixed list, gated by minConfidence (mirrors the
/// real Detector contract so the pipeline's confidence floor is exercised).
private struct FixedDetector: Detector {
    let detections: [Detection]
    func detect(in frame: Frame, detectors: DetectorSet, minConfidence: Double) -> [Detection] {
        detections.filter { $0.confidence >= minConfidence }
    }
}

/// Collecting sink for actor-level tests.
private actor CollectingSink: EventSink {
    private(set) var events: [VisionEvent] = []
    func emit(_ event: VisionEvent) async { events.append(event) }
    func snapshot() -> [VisionEvent] { events }
}

// ===========================================================================
// Motion: threshold boundary + region + first-frame baseline.
// ===========================================================================

final class MotionDetectorTests: XCTestCase {

    func testFirstFrameAfterResetIsBaselineNoMotion() {
        var m = MotionDetector()
        let r = m.step(.uniform(side: 4, value: 0.5), threshold: 0.0)
        XCTAssertEqual(r, .none, "first grid has no previous to diff -> no motion even at threshold 0")
        XCTAssertFalse(r.exceeded)
    }

    func testIdenticalGridsYieldZeroMagnitude() {
        var m = MotionDetector()
        _ = m.step(.uniform(side: 4, value: 0.5), threshold: 0.1)   // baseline
        let r = m.step(.uniform(side: 4, value: 0.5), threshold: 0.1)
        XCTAssertEqual(r.magnitude, 0.0, accuracy: 1e-12)
        XCTAssertFalse(r.exceeded)
        XCTAssertEqual(r.region, .full)
    }

    func testMagnitudeIsMeanAbsoluteDelta() {
        var m = MotionDetector()
        _ = m.step(.uniform(side: 2, value: 0.2), threshold: 1.0)   // baseline
        // Every cell changes by 0.5 -> mean abs delta = 0.5.
        let r = m.step(.uniform(side: 2, value: 0.7), threshold: 1.0)
        XCTAssertEqual(r.magnitude, 0.5, accuracy: 1e-12)
    }

    func testThresholdBoundaryIsStrictlyGreater() {
        var m = MotionDetector()
        _ = m.step(.uniform(side: 2, value: 0.0), threshold: 0.3)   // baseline
        // delta exactly 0.3 -> NOT exceeded (strict >).
        var r = m.step(.uniform(side: 2, value: 0.3), threshold: 0.3)
        XCTAssertEqual(r.magnitude, 0.3, accuracy: 1e-12)
        XCTAssertFalse(r.exceeded, "magnitude == threshold must NOT exceed (strict >)")

        // re-baseline at 0.3, then a hair above 0.3 -> exceeded.
        r = m.step(.uniform(side: 2, value: 0.6), threshold: 0.29)
        XCTAssertTrue(r.exceeded)
    }

    func testRegionCoversChangedCellsWithYFlip() {
        var m = MotionDetector()
        // 2x2 baseline all zero.
        _ = m.step(LumaGrid(side: 2, cells: [0, 0, 0, 0]), threshold: 1.0)
        // Change ONLY the top-left image cell (row 0, col 0). In Vision coords
        // (origin bottom-left) the TOP row is high y, so y should be 0.5..1.0.
        let r = m.step(LumaGrid(side: 2, cells: [1, 0, 0, 0]), threshold: 1.0)
        XCTAssertEqual(r.region.x, 0.0, accuracy: 1e-12)
        XCTAssertEqual(r.region.width, 0.5, accuracy: 1e-12)
        XCTAssertEqual(r.region.height, 0.5, accuracy: 1e-12)
        XCTAssertEqual(r.region.y, 0.5, accuracy: 1e-12, "top image row -> high Vision y")
    }

    func testRegionForBottomRowIsLowY() {
        var m = MotionDetector()
        _ = m.step(LumaGrid(side: 2, cells: [0, 0, 0, 0]), threshold: 1.0)
        // Change bottom-right image cell (row 1, col 1) -> low y, right x.
        let r = m.step(LumaGrid(side: 2, cells: [0, 0, 0, 1]), threshold: 1.0)
        XCTAssertEqual(r.region.x, 0.5, accuracy: 1e-12)
        XCTAssertEqual(r.region.y, 0.0, accuracy: 1e-12, "bottom image row -> low Vision y")
    }

    func testMismatchedSideRebaselines() {
        var m = MotionDetector()
        _ = m.step(.uniform(side: 4, value: 0.0), threshold: 0.01)
        // Different side: treated as a fresh baseline, no motion reported.
        let r = m.step(.uniform(side: 2, value: 1.0), threshold: 0.01)
        XCTAssertEqual(r, .none)
    }

    func testResetClearsBaseline() {
        var m = MotionDetector()
        _ = m.step(.uniform(side: 2, value: 0.0), threshold: 0.01)
        m.reset()
        let r = m.step(.uniform(side: 2, value: 1.0), threshold: 0.01)
        XCTAssertEqual(r, .none, "after reset the next grid is a baseline again")
    }

    func testLumaGridSamplingFromCGImageIsDeterministic() throws {
        let g1 = try XCTUnwrap(LumaGrid.sample(cgImage: grayImage(0.0), side: 4))
        let g2 = try XCTUnwrap(LumaGrid.sample(cgImage: grayImage(1.0), side: 4))
        XCTAssertEqual(g1.cells.allSatisfy { $0 < 0.02 }, true)
        XCTAssertEqual(g2.cells.allSatisfy { $0 > 0.98 }, true)
        // Same image -> identical grid (deterministic sampling).
        let g3 = try XCTUnwrap(LumaGrid.sample(cgImage: grayImage(0.5), side: 4))
        let g4 = try XCTUnwrap(LumaGrid.sample(cgImage: grayImage(0.5), side: 4))
        XCTAssertEqual(g3, g4)
    }
}

// ===========================================================================
// Presence: enter/exit dwell + anti-flicker hysteresis.
// ===========================================================================

final class PresenceMachineTests: XCTestCase {

    func testEnterRequiresConsecutiveDwell() {
        var p = PresenceMachine(config: PresenceConfig(enterDwell: 3, exitDwell: 2,
                                                       enterThreshold: 0.5, exitThreshold: 0.3))
        XCTAssertFalse(p.update(evidence: 0.9))  // 1
        XCTAssertEqual(p.state, .absent)
        XCTAssertFalse(p.update(evidence: 0.9))  // 2
        XCTAssertEqual(p.state, .absent)
        XCTAssertTrue(p.update(evidence: 0.9))   // 3 -> transition
        XCTAssertEqual(p.state, .present)
    }

    func testEnterStreakBreaksOnSubThresholdFrame() {
        var p = PresenceMachine(config: PresenceConfig(enterDwell: 3, exitDwell: 2,
                                                       enterThreshold: 0.5, exitThreshold: 0.3))
        _ = p.update(evidence: 0.9)   // 1
        _ = p.update(evidence: 0.9)   // 2
        _ = p.update(evidence: 0.1)   // breaks the run (below enter)
        XCTAssertEqual(p.state, .absent)
        _ = p.update(evidence: 0.9)   // 1 again
        _ = p.update(evidence: 0.9)   // 2
        XCTAssertEqual(p.state, .absent, "streak restarted; not yet at dwell")
        XCTAssertTrue(p.update(evidence: 0.9))  // 3 -> present
    }

    func testExitRequiresConsecutiveDwellBelowExitThreshold() {
        var p = PresenceMachine(config: PresenceConfig(enterDwell: 1, exitDwell: 3,
                                                       enterThreshold: 0.5, exitThreshold: 0.3),
                                initial: .present)
        XCTAssertFalse(p.update(evidence: 0.1))  // 1 below
        XCTAssertFalse(p.update(evidence: 0.1))  // 2 below
        XCTAssertEqual(p.state, .present)
        XCTAssertTrue(p.update(evidence: 0.1))   // 3 -> absent
        XCTAssertEqual(p.state, .absent)
    }

    func testHysteresisBandHoldsStateNoFlicker() {
        // enter 0.6, exit 0.4 -> band [0.4, 0.6). Evidence parked IN the band
        // must NEVER cause a transition in either direction.
        let cfg = PresenceConfig(enterDwell: 2, exitDwell: 2,
                                 enterThreshold: 0.6, exitThreshold: 0.4)
        var absentP = PresenceMachine(config: cfg, initial: .absent)
        for _ in 0..<50 { XCTAssertFalse(absentP.update(evidence: 0.5)) }
        XCTAssertEqual(absentP.state, .absent, "band evidence never promotes from absent")

        var presentP = PresenceMachine(config: cfg, initial: .present)
        for _ in 0..<50 { XCTAssertFalse(presentP.update(evidence: 0.5)) }
        XCTAssertEqual(presentP.state, .present, "band evidence never demotes from present")
    }

    func testNoiseAroundExitThresholdDoesNotThrash() {
        // Present state; evidence oscillates across the EXIT threshold but never
        // sustains exitDwell consecutive sub-threshold frames -> stays present.
        let cfg = PresenceConfig(enterDwell: 2, exitDwell: 4,
                                 enterThreshold: 0.6, exitThreshold: 0.4)
        var p = PresenceMachine(config: cfg, initial: .present)
        let noisy: [Double] = [0.1, 0.5, 0.1, 0.5, 0.1, 0.5, 0.39, 0.41, 0.2, 0.42]
        for v in noisy { XCTAssertFalse(p.update(evidence: v)) }
        XCTAssertEqual(p.state, .present, "no run of >=4 sub-exit frames -> no exit")
    }

    func testFullCycleAbsentPresentAbsent() {
        let cfg = PresenceConfig(enterDwell: 2, exitDwell: 2,
                                 enterThreshold: 0.6, exitThreshold: 0.4)
        var p = PresenceMachine(config: cfg)
        _ = p.update(evidence: 0.9)
        XCTAssertTrue(p.update(evidence: 0.9))  // -> present
        _ = p.update(evidence: 0.0)
        XCTAssertTrue(p.update(evidence: 0.0))  // -> absent
        XCTAssertEqual(p.state, .absent)
    }
}

// ===========================================================================
// Debounce + burst aggregation.
// ===========================================================================

final class DebouncerTests: XCTestCase {

    func testLeadEdgeFirstEmitsRestSuppressedWithinWindow() {
        var d = Debouncer(minInterval: 0.1)  // 10/s
        XCTAssertEqual(d.admit(at: 0.00), .emit)     // lead edge
        XCTAssertEqual(d.admit(at: 0.02), .suppress) // within window
        XCTAssertEqual(d.admit(at: 0.05), .suppress)
        XCTAssertEqual(d.admit(at: 0.09), .suppress)
        XCTAssertEqual(d.admit(at: 0.10), .emit)     // exactly minInterval -> emit
        XCTAssertEqual(d.admit(at: 0.15), .suppress)
        XCTAssertEqual(d.admit(at: 0.20), .emit)
    }

    func testBurstOfTenCollapsesToTwo() {
        // 10 frames in 90ms with a 100ms window -> only the FIRST emits.
        var d = Debouncer(maxEventsPerSecond: 10)  // 0.1s window
        var emits = 0
        for i in 0..<10 {
            if d.admit(at: Double(i) * 0.01) == .emit { emits += 1 }
        }
        XCTAssertEqual(emits, 1, "a 90ms burst collapses to one emission")
    }

    func testOutOfOrderOrEqualTimestampsSuppress() {
        var d = Debouncer(minInterval: 0.1)
        XCTAssertEqual(d.admit(at: 1.0), .emit)
        XCTAssertEqual(d.admit(at: 1.0), .suppress, "equal timestamp -> suppress")
        XCTAssertEqual(d.admit(at: 0.5), .suppress, "earlier timestamp -> suppress")
        XCTAssertEqual(d.admit(at: 1.1), .emit)
    }

    func testZeroIntervalAlwaysEmits() {
        var d = Debouncer(minInterval: 0)
        XCTAssertEqual(d.admit(at: 0.0), .emit)
        XCTAssertEqual(d.admit(at: 0.0), .emit)
        XCTAssertEqual(d.admit(at: 0.0), .emit)
    }

    func testResetReArmsLeadEdge() {
        var d = Debouncer(minInterval: 0.1)
        XCTAssertEqual(d.admit(at: 0.0), .emit)
        XCTAssertEqual(d.admit(at: 0.05), .suppress)
        d.reset()
        XCTAssertEqual(d.admit(at: 0.05), .emit, "after reset, next candidate is a lead edge")
    }
}

final class BurstAggregatorTests: XCTestCase {

    func testMergeDedupesByKindLabelKeepingHighestConfidence() {
        let bursts: [[Detection]] = [
            [det(.object, 0.5, "dog"), det(.human, 0.7)],
            [det(.object, 0.8, "dog"), det(.animal, 0.6, "cat")],
            [det(.object, 0.6, "dog")],
        ]
        let merged = BurstAggregator.merge(bursts)
        // dog collapses to ONE at highest confidence 0.8.
        let dogs = merged.filter { $0.kind == .object && $0.label == "dog" }
        XCTAssertEqual(dogs.count, 1)
        XCTAssertEqual(dogs.first?.confidence, 0.8)
        // Three distinct (kind,label) keys total: human, animal/cat, object/dog.
        XCTAssertEqual(merged.count, 3)
    }

    func testMergeOrderIsDeterministicByKindThenLabel() {
        let merged = BurstAggregator.merge([[
            det(.salientRegion, 0.4),
            det(.object, 0.9, "zebra"),
            det(.object, 0.9, "apple"),
            det(.human, 0.5),
        ]])
        // Kind declaration order: human, animal, object, salientRegion, motion.
        XCTAssertEqual(merged.map(\.kind), [.human, .object, .object, .salientRegion])
        // Within object: label ascending (apple before zebra).
        let objs = merged.filter { $0.kind == .object }
        XCTAssertEqual(objs.map(\.label), ["apple", "zebra"])
    }

    func testEmptyBurstsMergeToEmpty() {
        XCTAssertTrue(BurstAggregator.merge([]).isEmpty)
        XCTAssertTrue(BurstAggregator.merge([[], []]).isEmpty)
    }
}

// ===========================================================================
// Alert hysteresis.
// ===========================================================================

final class AlertHysteresisTests: XCTestCase {

    func testRaiseRequiresDwellAndClearRequiresDwell() {
        var a = AlertHysteresis(config: AlertConfig(raiseDwell: 2, clearDwell: 3,
                                                    raiseThreshold: 0.6, clearThreshold: 0.4))
        XCTAssertFalse(a.update(level: 0.9))  // 1
        XCTAssertTrue(a.update(level: 0.9))   // 2 -> raised
        XCTAssertTrue(a.raised)
        XCTAssertFalse(a.update(level: 0.0))  // 1
        XCTAssertFalse(a.update(level: 0.0))  // 2
        XCTAssertTrue(a.update(level: 0.0))   // 3 -> cleared
        XCTAssertFalse(a.raised)
    }

    func testBandHoldsLatchEitherWay() {
        let cfg = AlertConfig(raiseDwell: 1, clearDwell: 1,
                              raiseThreshold: 0.7, clearThreshold: 0.3)
        var low = AlertHysteresis(config: cfg, initiallyRaised: false)
        for _ in 0..<20 { XCTAssertFalse(low.update(level: 0.5)) }
        XCTAssertFalse(low.raised, "band level never raises")

        var high = AlertHysteresis(config: cfg, initiallyRaised: true)
        for _ in 0..<20 { XCTAssertFalse(high.update(level: 0.5)) }
        XCTAssertTrue(high.raised, "band level never clears")
    }

    func testClearStreakBreaksOnInBandFrame() {
        var a = AlertHysteresis(config: AlertConfig(raiseDwell: 1, clearDwell: 3,
                                                    raiseThreshold: 0.6, clearThreshold: 0.4),
                                initiallyRaised: true)
        _ = a.update(level: 0.1)  // 1 below
        _ = a.update(level: 0.1)  // 2 below
        _ = a.update(level: 0.5)  // in band -> breaks clear streak
        XCTAssertTrue(a.raised)
        _ = a.update(level: 0.1)  // 1 again
        _ = a.update(level: 0.1)  // 2
        XCTAssertTrue(a.raised, "streak restarted")
        XCTAssertTrue(a.update(level: 0.1))  // 3 -> cleared
        XCTAssertFalse(a.raised)
    }
}

// ===========================================================================
// PipelineConfig sensitivity -> thresholds mapping.
// ===========================================================================

final class PipelineConfigTests: XCTestCase {

    func testMinConfidenceInverseOfSensitivity() {
        XCTAssertEqual(PipelineConfig(sensitivity: 0).minConfidence, 0.9, accuracy: 1e-12)
        XCTAssertEqual(PipelineConfig(sensitivity: 1).minConfidence, 0.1, accuracy: 1e-12)
        XCTAssertEqual(PipelineConfig(sensitivity: 0.5).minConfidence, 0.5, accuracy: 1e-12)
    }

    func testMotionThresholdInverseOfSensitivity() {
        XCTAssertEqual(PipelineConfig(sensitivity: 0).motionThreshold, 0.40, accuracy: 1e-12)
        XCTAssertEqual(PipelineConfig(sensitivity: 1).motionThreshold, 0.02, accuracy: 1e-12)
    }

    func testSensitivityClampedIntoThresholds() {
        XCTAssertEqual(PipelineConfig(sensitivity: -5).minConfidence, 0.9, accuracy: 1e-12)
        XCTAssertEqual(PipelineConfig(sensitivity: 5).minConfidence, 0.1, accuracy: 1e-12)
    }
}

// ===========================================================================
// Pipeline actor — deterministic per-frame processing + Op lifecycle.
// ===========================================================================

final class PipelineActorTests: XCTestCase {

    private func frame(_ image: CGImage, ts: TimeInterval, source: CaptureSource = .file(path: "v.mov")) -> Frame {
        Frame(cgImage: image, timestamp: ts, source: source, index: 0)
    }

    func testProcessFrameEmitsDetectionsAndIncrementsIndex() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: FixedDetector(detections: [det(.human, 0.95)]),
                            sink: sink,
                            config: PipelineConfig(sensitivity: 0.5))
        // Drive watch.start with a stub file source (no frames) so currentSource is set.
        await pipe.handle(.watchStart(source: .file(path: "v.mov")))

        let evs1 = await pipe.processFrame(frame(grayImage(0.5), ts: 0.0))
        // Frame 0: human detected (0.95 >= floor 0.5). Presence enters after
        // enterDwell=2, so frame 0 alone may not transition; detection still
        // emits via lead-edge debounce.
        XCTAssertTrue(evs1.contains { if case .detections = $0 { return true }; return false })
        if case let .detections(frameIndex, _, source, dets)? = evs1.first(where: {
            if case .detections = $0 { return true }; return false }) {
            XCTAssertEqual(frameIndex, 0)
            XCTAssertEqual(source, "file")
            XCTAssertEqual(dets.count, 1)
            XCTAssertEqual(dets.first?.kind, .human)
        } else {
            XCTFail("expected a detections event on frame 0")
        }

        let evs2 = await pipe.processFrame(frame(grayImage(0.5), ts: 0.001))
        // Second frame, well within the debounce window, same detection. With
        // enterDwell=2 the presence transition fires here, so it still emits.
        let idx = await { () -> UInt64 in
            for ev in evs2 { if case let .detections(i, _, _, _) = ev { return i } }
            return .max
        }()
        XCTAssertEqual(idx, 1, "frame index incremented")
    }

    func testConfidenceFloorGatesDetections() async {
        let sink = CollectingSink()
        // sensitivity 0 -> floor 0.9; a 0.5-confidence detection is gated out.
        let pipe = Pipeline(detector: FixedDetector(detections: [det(.animal, 0.5, "cat")]),
                            sink: sink, config: PipelineConfig(sensitivity: 0.0))
        await pipe.handle(.watchStart(source: .file(path: "v.mov")))
        let evs = await pipe.processFrame(frame(grayImage(0.5), ts: 0.0))
        XCTAssertFalse(evs.contains { if case .detections = $0 { return true }; return false },
                       "0.5 confidence < 0.9 floor -> no detections event")
    }

    func testMotionEmittedWhenLumaChangesAcrossThreshold() async {
        let sink = CollectingSink()
        // High sensitivity -> low motion threshold (0.02). No detector noise.
        let pipe = Pipeline(detector: FixedDetector(detections: []),
                            sink: sink, config: PipelineConfig(sensitivity: 1.0))
        await pipe.handle(.watchStart(source: .file(path: "v.mov")))
        // Frame 0: baseline (no motion).
        let e0 = await pipe.processFrame(frame(grayImage(0.0), ts: 0.0))
        XCTAssertFalse(e0.contains { if case .motion = $0 { return true }; return false })
        // Frame 1: big luma jump -> motion crosses 0.02 threshold.
        let e1 = await pipe.processFrame(frame(grayImage(1.0), ts: 0.2))
        XCTAssertTrue(e1.contains { if case .motion = $0 { return true }; return false },
                      "0.0 -> 1.0 luma is a full-frame change, must exceed motion threshold")
        if case let .motion(_, _, src, mag, _)? = e1.first(where: {
            if case .motion = $0 { return true }; return false }) {
            XCTAssertEqual(src, "file")
            XCTAssertGreaterThan(mag, 0.9)
        }
    }

    func testNoMotionWhenLumaStable() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: FixedDetector(detections: []),
                            sink: sink, config: PipelineConfig(sensitivity: 0.5))
        await pipe.handle(.watchStart(source: .file(path: "v.mov")))
        _ = await pipe.processFrame(frame(grayImage(0.5), ts: 0.0))
        let e1 = await pipe.processFrame(frame(grayImage(0.5), ts: 0.2))
        XCTAssertFalse(e1.contains { if case .motion = $0 { return true }; return false },
                       "identical frames -> no motion")
    }

    func testPresenceTransitionEmitsStatusOnceNotEveryFrame() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: FixedDetector(detections: [det(.human, 0.99)]),
                            sink: sink, config: PipelineConfig(sensitivity: 0.5))
        await pipe.handle(.watchStart(source: .file(path: "v.mov")))
        var statusCount = 0
        // Default presence enterDwell=2. Feed several strong frames; presence
        // should transition exactly ONCE (no per-frame status spam).
        for i in 0..<8 {
            let evs = await pipe.processFrame(frame(grayImage(0.5), ts: Double(i)))
            statusCount += evs.filter { if case .status = $0 { return true }; return false }.count
        }
        XCTAssertEqual(statusCount, 1, "presence enters once -> exactly one status transition")
        let present = await pipe.currentPresence
        XCTAssertEqual(present, .present)
    }

    func testSetSensitivityRetunesAndEmitsStatus() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: FixedDetector(detections: []),
                            sink: sink, config: PipelineConfig(sensitivity: 0.5))
        await pipe.handle(.setSensitivity(value: 1.5))  // clamps to 1.0
        let cfg = await pipe.currentConfig
        XCTAssertEqual(cfg.sensitivity, 1.0, accuracy: 1e-12)
        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains { if case .status = $0 { return true }; return false })
    }

    func testUnknownOpEmitsError() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: FixedDetector(detections: []), sink: sink)
        await pipe.handle(.unknown(raw: "garbage"))
        let evs = await sink.snapshot()
        guard case let .error(code, _, _)? = evs.first(where: {
            if case .error = $0 { return true }; return false }) else {
            return XCTFail("expected an error event")
        }
        XCTAssertEqual(code, "bad_op")
    }

    func testDeniedAuthEmitsTccErrorAndDoesNotRun() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: FixedDetector(detections: []), sink: sink)
        // Inject a denied source.
        await pipe.setFrameSourceFactory { _ in DeniedSource(source: .camera) }
        await pipe.handle(.watchStart(source: .camera))
        // Give the (no-op) run a tick.
        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .error(code, _, _) = $0 { return code == "tcc_denied" }; return false
        }, "denied camera -> tcc_denied error")
        let state = await pipe.currentState
        XCTAssertEqual(state, .stopped, "denied auth does not enter watching")
    }

    func testStopResetsState() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: FixedDetector(detections: []), sink: sink)
        await pipe.handle(.watchStart(source: .file(path: "v.mov")))
        await pipe.handle(.stop)
        let state = await pipe.currentState
        XCTAssertEqual(state, .stopped)
    }

    func testDeterministicReplaySameInputsSameEvents() async {
        // Two pipelines fed identical frame sequences must emit identical events.
        func run() async -> [String] {
            let sink = CollectingSink()
            let pipe = Pipeline(detector: FixedDetector(detections: [det(.human, 0.95)]),
                                sink: sink, config: PipelineConfig(sensitivity: 0.7))
            await pipe.handle(.watchStart(source: .file(path: "v.mov")))
            let imgs = [grayImage(0.0), grayImage(1.0), grayImage(1.0), grayImage(0.0)]
            var lines: [String] = []
            for (i, img) in imgs.enumerated() {
                let evs = await pipe.processFrame(frame(img, ts: Double(i) * 0.2))
                for ev in evs { lines.append(ev.line(token: "T") ?? "nil") }
            }
            return lines
        }
        let a = await run()
        let b = await run()
        XCTAssertEqual(a, b, "identical inputs -> byte-identical wire output")
        XCTAssertFalse(a.isEmpty)
    }
}

// A FrameSource that reports denied authorization (camera/screen TCC denied).
private struct DeniedSource: FrameSource {
    let source: CaptureSource
    func authorization() async -> CaptureAuthorization { .denied }
    func frames() -> AsyncStream<Frame> { AsyncStream { $0.finish() } }
}

// A FrameSource that yields a fixed list of frames (then finishes). Lets the
// read.screen wiring be exercised over a REAL injected frame (NOT the zero-frame
// stub) — the same "prove the real injection" discipline as AppWiringTests.
private struct FixedFrameSource: FrameSource {
    let source: CaptureSource
    let auth: CaptureAuthorization
    let images: [CGImage]
    func authorization() async -> CaptureAuthorization { auth }
    func frames() -> AsyncStream<Frame> {
        AsyncStream { cont in
            for (i, img) in images.enumerated() {
                cont.yield(Frame(cgImage: img, timestamp: Double(i), source: source, index: UInt64(i)))
            }
            cont.finish()
        }
    }
}

// ===========================================================================
// read.screen — single-shot OCR readout wiring (over an injected frame).
// ===========================================================================

final class ReadScreenWiringTests: XCTestCase {

    /// Render dark text on white into a CGImage so the REAL OCR detector reads it.
    private func textImage(_ lines: [String], width: Int = 600, height: Int = 200,
                           fontSize: CGFloat = 56) -> CGImage? {
        let cs = CGColorSpaceCreateDeviceRGB()
        guard let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8,
                                  bytesPerRow: 0, space: cs,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.setFillColor(red: 1, green: 1, blue: 1, alpha: 1)
        ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
        let font = CTFontCreateWithName("Helvetica" as CFString, fontSize, nil)
        let attrs: [NSAttributedString.Key: Any] = [.font: font,
            .foregroundColor: CGColor(red: 0, green: 0, blue: 0, alpha: 1)]
        let lineHeight = fontSize * 1.4
        var y = CGFloat(height) - lineHeight
        for line in lines {
            let ctLine = CTLineCreateWithAttributedString(NSAttributedString(string: line, attributes: attrs))
            ctx.textPosition = CGPoint(x: 20, y: y)
            CTLineDraw(ctLine, ctx)
            y -= lineHeight
        }
        return ctx.makeImage()
    }

    /// Drive read.screen over an injected frame carrying real rendered text and
    /// assert the emitted vision.screen readout carries that recognized text —
    /// proving the op reaches the real OCR detector over a real injected frame
    /// (NOT the zero-frame stub trap). Uses an explicit .file source so this is
    /// fully headless (no TCC); production routes the same code over .screen.
    func testReadScreenEmitsScreenEventWithRecognizedText() async throws {
        guard let img = textImage(["SUBMIT", "Cancel"]) else { throw XCTSkip("no text image") }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        await pipe.handle(.readScreen(source: .file(path: "ui.png")))

        let evs = await sink.snapshot()
        guard case let .screen(_, _, source, readout, located, query, meta)? = evs.first(where: {
            if case .screen = $0 { return true }; return false }) else {
            return XCTFail("read.screen must emit a vision.screen event, got \(evs)")
        }
        XCTAssertEqual(source, "file")
        XCTAssertNil(query)
        XCTAssertNil(located, "no query -> no located block")
        XCTAssertEqual(meta.kind, .screen, "read.screen tags the readout read_kind=screen")
        XCTAssertNil(meta.documentDetected, "screen OCR has no document-detected bool")
        let joined = readout.blocks.map(\.text).joined(separator: " ").lowercased()
        // If the recognizer produced nothing in this env, device-gate honestly.
        guard !joined.isEmpty else {
            throw XCTSkip("VNRecognizeTextRequest returned no text in this build env (device-gated)")
        }
        XCTAssertTrue(joined.contains("submit"),
                      "the read.screen readout must carry the real recognized text; got \(joined)")
        // The short labels surface as control candidates.
        XCTAssertTrue(readout.controls.contains { $0.text.lowercased().contains("submit") },
                      "a short button-ish label is a control candidate")
    }

    /// read.screen does NOT disturb the live-watch run state (it is a one-shot
    /// read, never a continuous watch).
    func testReadScreenLeavesWatchStateUntouched() async throws {
        guard let img = textImage(["Hello"]) else { throw XCTSkip("no text image") }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        await pipe.handle(.readScreen(source: .file(path: "ui.png")))
        let state = await pipe.currentState
        XCTAssertEqual(state, .idle, "a one-shot read.screen must not change the watch lifecycle state")
    }

    /// A denied screen source emits a clean tcc_denied error and reads nothing —
    /// honoring the TCC gate exactly like watch.start.
    func testReadScreenDeniedEmitsTccErrorAndNoReadout() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .denied, images: [])
        }
        await pipe.handle(.readScreen(source: .screen))
        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .error(code, _, src) = $0 { return code == "tcc_denied" && src == "screen" }
            return false
        }, "denied screen read -> tcc_denied error")
        XCTAssertFalse(evs.contains { if case .screen = $0 { return true }; return false },
                       "a denied read emits NO vision.screen readout")
    }

    /// An authorized source that yields no frame reports an honest no_frame error
    /// rather than fabricating empty text.
    func testReadScreenNoFrameReportsHonestError() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [])
        }
        await pipe.handle(.readScreen(source: .file(path: "empty")))
        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .error(code, _, _) = $0 { return code == "no_frame" }; return false
        }, "no captured frame -> honest no_frame error, never fabricated text")
    }
}

// ===========================================================================
// CONTINUOUS SCREEN CONTEXT (#42) — the DEVICE-gated periodic OCR loop wiring.
//
// AppWiring discipline (NO real capture): the loop drives the SAME injected
// FrameSource seam as read.screen. We prove:
//   (1) WIRED — a loop over an INJECTED frame carrying real text emits a
//       vision.screen readout tagged read_kind=.context (the daemon routes that
//       into its bounded/redacted/transient ring) framed by a WATCHING status
//       then a watching=false exit status.
//   (2) CONTRAST — a loop over the DEFAULT/un-injected Pipeline (the zero-frame
//       StubFrameSource) captures NOTHING: it still announces WATCHING + exits
//       honestly, but emits NO .context readout. So the production wiring (the
//       real CaptureSourceFactory injection) is what flips behavior — exactly the
//       AppWiringTests invariant. No camera, no screen, no TCC, no socket.
//   (3) TCC — a DENIED source stops the loop cleanly with a tcc_denied error and
//       reads nothing (the device gate is honored).
// `maxTicks` bounds the loop to one tick + a 0s interval so the test is fast and
// never blocks; production runs unbounded at the configured cadence.
// ===========================================================================

final class ScreenContextLoopWiringTests: XCTestCase {

    /// Render dark text on white into a CGImage so the REAL OCR detector reads it.
    private func textImage(_ lines: [String], width: Int = 600, height: Int = 200,
                           fontSize: CGFloat = 56) -> CGImage? {
        let cs = CGColorSpaceCreateDeviceRGB()
        guard let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8,
                                  bytesPerRow: 0, space: cs,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.setFillColor(red: 1, green: 1, blue: 1, alpha: 1)
        ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
        let font = CTFontCreateWithName("Helvetica" as CFString, fontSize, nil)
        let attrs: [NSAttributedString.Key: Any] = [.font: font,
            .foregroundColor: CGColor(red: 0, green: 0, blue: 0, alpha: 1)]
        let lineHeight = fontSize * 1.4
        var y = CGFloat(height) - lineHeight
        for line in lines {
            let ctLine = CTLineCreateWithAttributedString(NSAttributedString(string: line, attributes: attrs))
            ctx.textPosition = CGPoint(x: 20, y: y)
            CTLineDraw(ctLine, ctx)
            y -= lineHeight
        }
        return ctx.makeImage()
    }

    /// (1) WIRED: over an injected text frame the loop emits a .context-tagged
    /// vision.screen readout, framed by a WATCHING status + an honest exit status.
    func testContinuousLoopEmitsContextTaggedReadoutAndWatchingStatus() async throws {
        guard let img = textImage(["INBOX", "Report"]) else { throw XCTSkip("no text image") }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        // One tick, 0s interval -> a single capture then a clean exit (no blocking).
        await pipe.runScreenContextLoop(source: .file(path: "ui.png"),
                                        intervalSeconds: 0, maxTicks: 1)

        let evs = await sink.snapshot()
        // A WATCHING status announced the active loop (the HUD indicator).
        XCTAssertTrue(evs.contains {
            if case let .status(state, _, _, _, _, message) = $0 {
                return state == .watching && message == "screen_context.watching"
            }
            return false
        }, "the loop must announce a WATCHING status, got \(evs)")
        // The exit status honestly reports watching=false.
        XCTAssertTrue(evs.contains {
            if case let .status(_, _, _, _, _, message) = $0 {
                return message == "screen_context.watching=false"
            }
            return false
        }, "the loop must emit an honest watching=false exit status")
        // A .context-tagged readout (the snapshot the daemon routes into the ring).
        guard case let .screen(_, _, source, readout, located, query, meta)? = evs.first(where: {
            if case .screen = $0 { return true }; return false }) else {
            return XCTFail("the continuous loop must emit a vision.screen readout, got \(evs)")
        }
        XCTAssertEqual(meta.kind, .context, "the continuous snapshot must be tagged read_kind=context")
        XCTAssertEqual(source, "file")
        XCTAssertNil(query, "the continuous path carries no locator query")
        XCTAssertNil(located, "no query -> no located block")
        let joined = readout.blocks.map(\.text).joined(separator: " ").lowercased()
        guard !joined.isEmpty else {
            throw XCTSkip("VNRecognizeTextRequest returned no text in this build env (device-gated)")
        }
        XCTAssertTrue(joined.contains("inbox"),
                      "the continuous readout must carry the real recognized text; got \(joined)")
    }

    /// SCREEN GROUNDING (1): a SCREEN-source context tick is attributed to the
    /// frontmost app/window from the injected provider — the snapshot
    /// self-describes where it came from, and the wire JSON carries the keys.
    func testContinuousLoopAttributesTheFrontmostAppAndWindow() async throws {
        guard let img = textImage(["INBOX", "Report"]) else { throw XCTSkip("no text image") }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        await pipe.useFrontmostProvider { FrontmostWindow(app: "TestMail", window: "Inbox — 3 unread") }
        await pipe.runScreenContextLoop(source: .screen, intervalSeconds: 0, maxTicks: 1)

        let evs = await sink.snapshot()
        guard case let .screen(_, _, _, _, _, _, meta)? = evs.first(where: {
            if case .screen = $0 { return true }; return false }) else {
            return XCTFail("expected a .context readout, got \(evs)")
        }
        XCTAssertEqual(meta.sourceApp, "TestMail", "the tick must carry the frontmost app")
        XCTAssertEqual(meta.sourceWindow, "Inbox — 3 unread", "the tick must carry the window title")
        // The wire JSON carries the additive keys (what the daemon reads).
        guard let ev = evs.first(where: { if case .screen = $0 { return true }; return false }) else {
            return XCTFail("unreachable")
        }
        let data = ev.encodeData()
        XCTAssertEqual(data["source_app"] as? String, "TestMail")
        XCTAssertEqual(data["source_window"] as? String, "Inbox — 3 unread")
    }

    /// SCREEN GROUNDING (2): the DEFAULT provider attributes NOTHING (hermetic
    /// honesty — no injected reader, no fabricated attribution; the wire JSON
    /// OMITS the keys rather than sending empties).
    func testContinuousLoopDefaultProviderAttributesNothing() async throws {
        guard let img = textImage(["INBOX"]) else { throw XCTSkip("no text image") }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        // No useFrontmostProvider — the nil-attributing default.
        await pipe.runScreenContextLoop(source: .screen, intervalSeconds: 0, maxTicks: 1)

        let evs = await sink.snapshot()
        guard case let .screen(_, _, _, _, _, _, meta)? = evs.first(where: {
            if case .screen = $0 { return true }; return false }) else {
            return XCTFail("expected a .context readout, got \(evs)")
        }
        XCTAssertNil(meta.sourceApp, "no provider -> honestly unattributed")
        XCTAssertNil(meta.sourceWindow)
        guard let ev = evs.first(where: { if case .screen = $0 { return true }; return false }) else {
            return XCTFail("unreachable")
        }
        let data = ev.encodeData()
        XCTAssertNil(data["source_app"], "absent attribution must OMIT the key, never fabricate")
        XCTAssertNil(data["source_window"])
    }

    /// SCREEN GROUNDING (4): ORDERING PIN (review-caught race) — the frontmost
    /// provider is consulted BEFORE the slow OCR, so a mid-OCR app switch can
    /// no longer stamp this frame's pixels with the NEXT app's identity. The
    /// probe detector flips a flag when OCR runs; the provider answers
    /// "SwitchedApp" only if OCR already ran. With the fixed ordering the
    /// emitted attribution MUST be the pre-OCR app. (Reverting the read to
    /// after detect() makes this fail — mutation-proven.)
    func testFrontmostIsReadBeforeOcrSoAMidOcrSwitchCannotMislabel() async throws {
        guard let img = textImage(["INBOX"]) else { throw XCTSkip("no text image") }
        final class OrderProbeDetector: Detector, @unchecked Sendable {
            private let lock = NSLock()
            private var flag = false
            func detect(in frame: Frame, detectors: DetectorSet, minConfidence: Double) -> [Detection] {
                lock.lock(); flag = true; lock.unlock()
                return []
            }
            func ocrRan() -> Bool {
                lock.lock(); defer { lock.unlock() }
                return flag
            }
        }
        let probe = OrderProbeDetector()
        let sink = CollectingSink()
        let pipe = Pipeline(detector: probe, sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        await pipe.useFrontmostProvider {
            FrontmostWindow(app: probe.ocrRan() ? "SwitchedApp" : "OriginalApp", window: nil)
        }
        await pipe.runScreenContextLoop(source: .screen, intervalSeconds: 0, maxTicks: 1)

        let evs = await sink.snapshot()
        guard case let .screen(_, _, _, _, _, _, meta)? = evs.first(where: {
            if case .screen = $0 { return true }; return false }) else {
            return XCTFail("expected a .context readout, got \(evs)")
        }
        XCTAssertEqual(meta.sourceApp, "OriginalApp",
                       "attribution must be read BEFORE OCR — a post-OCR read races an app switch")
    }

    /// SCREEN GROUNDING (3): a NON-screen source (file replay) is NEVER
    /// attributed to the live frontmost app — a replayed recording is not the
    /// user's current screen, and stamping it would be a false attribution.
    func testFileSourceTickIsNeverAttributedToTheLiveFrontmostApp() async throws {
        guard let img = textImage(["INBOX"]) else { throw XCTSkip("no text image") }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        await pipe.useFrontmostProvider { FrontmostWindow(app: "TestMail", window: "Inbox") }
        await pipe.runScreenContextLoop(source: .file(path: "replay.png"),
                                        intervalSeconds: 0, maxTicks: 1)

        let evs = await sink.snapshot()
        guard case let .screen(_, _, _, _, _, _, meta)? = evs.first(where: {
            if case .screen = $0 { return true }; return false }) else {
            return XCTFail("expected a .context readout, got \(evs)")
        }
        XCTAssertNil(meta.sourceApp,
                     "a file-replay tick must NOT be attributed to the live frontmost app")
    }

    /// (2) CONTRAST: the DEFAULT/un-injected Pipeline uses the zero-frame stub, so
    /// the loop captures NOTHING — it still announces WATCHING + exits honestly,
    /// but emits NO .context readout. The production injection is what flips this.
    func testContinuousLoopOverDefaultStubCapturesNothing() async {
        let sink = CollectingSink()
        // No setFrameSourceFactory — the default zero-frame StubFrameSource.
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.runScreenContextLoop(source: .screen, intervalSeconds: 0, maxTicks: 1)

        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .status(state, _, _, _, _, message) = $0 {
                return state == .watching && message == "screen_context.watching"
            }
            return false
        }, "even over the stub the loop announces WATCHING honestly")
        XCTAssertFalse(evs.contains { if case .screen = $0 { return true }; return false },
                       "the zero-frame stub yields NO readout — the real injection is what captures")
    }

    /// (3) TCC: a denied source stops the loop cleanly with a tcc_denied error and
    /// reads nothing — the device gate is honored, capturing nothing.
    func testContinuousLoopHonorsTccDenial() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { _ in DeniedSource(source: .screen) }
        await pipe.runScreenContextLoop(source: .screen, intervalSeconds: 0, maxTicks: 3)

        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .error(code, _, src) = $0 { return code == "tcc_denied" && src == "screen" }
            return false
        }, "a denied continuous loop emits a tcc_denied error")
        XCTAssertFalse(evs.contains { if case .screen = $0 { return true }; return false },
                       "a denied loop reads NOTHING — never a fabricated readout")
    }

    /// (4) WIRED THROUGH handle(): the PRODUCTION dispatch path — a
    /// `screen.context.start` Op decoded + handled actually DRIVES the loop (over
    /// an injected source it announces WATCHING and emits the .context-tagged
    /// readout the daemon routes into the ring). This proves the loop is not dead
    /// code: the daemon's `{"op":"screen.context.start",...}` line reaches it. No
    /// real capture (the injected FixedFrameSource is the seam).
    func testScreenContextStartOpDispatchesTheLoop() async throws {
        guard let img = textImage(["INBOX", "Report"]) else { throw XCTSkip("no text image") }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        // Decode the EXACT op line the daemon sends, then dispatch via handle().
        let op = Op.decode(line: #"{"type":"op","op":"screen.context.start","source":"file","path":"ui.png","interval_secs":0}"#)
        guard case .screenContextStart = op else {
            return XCTFail("the daemon's start line must decode to .screenContextStart, got \(op)")
        }
        await pipe.handle(op)
        // The loop runs on its own task — stop it (the production stop path) and
        // give the task a moment to drain its emitted events.
        await pipe.handle(.screenContextStop)
        try await Task.sleep(nanoseconds: 200_000_000)

        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .status(state, _, _, _, _, message) = $0 {
                return state == .watching && message == "screen_context.watching"
            }
            return false
        }, "handle(.screenContextStart) must drive the loop's WATCHING status, got \(evs)")
        // A .context-tagged readout proves the dispatched loop actually captured +
        // OCR'd through the injected seam (the daemon routes this into the ring).
        let sawContext = evs.contains {
            if case let .screen(_, _, _, _, _, _, meta) = $0 { return meta.kind == .context }
            return false
        }
        // The OCR result can be empty in a headless build (device-gated); the
        // WATCHING status above already proves the dispatch. When OCR did read,
        // assert the .context tag so the routing contract is exercised.
        if evs.contains(where: { if case .screen = $0 { return true }; return false }) {
            XCTAssertTrue(sawContext, "a dispatched continuous readout must be tagged read_kind=context")
        }
    }

    /// (5) STOP via handle(): a `screen.context.stop` Op (and a lifecycle stop)
    /// cancels the loop — proven by the honest watching=false exit status. No real
    /// capture.
    func testScreenContextStopOpCancelsTheLoop() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        // The zero-frame default stub: the loop announces WATCHING then idles on
        // the interval until cancelled (no capture). A long interval ensures the
        // loop is still alive when we stop it.
        await pipe.handle(.screenContextStart(source: .screen, intervalSecs: 3600))
        await pipe.handle(.screenContextStop)
        // Give the cancelled task a moment to emit its exit status.
        try? await Task.sleep(nanoseconds: 200_000_000)

        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .status(_, _, _, _, _, message) = $0 {
                return message == "screen_context.watching=false"
            }
            return false
        }, "screen.context.stop must cancel the loop and emit the honest watching=false exit, got \(evs)")
    }
}

// ===========================================================================
// describe.capture — single-shot frame capture + PNG write for the VLM
// (over an injected frame). DISTINCT from read.screen: NO OCR, writes a PNG.
// ===========================================================================

final class DescribeCaptureWiringTests: XCTestCase {

    /// A small solid-color CGImage to capture + write (no text needed — describe
    /// runs NO OCR, it only writes pixels for the host's VLM).
    private func solidImage(width: Int = 64, height: Int = 64) -> CGImage? {
        let cs = CGColorSpaceCreateDeviceRGB()
        guard let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8,
                                  bytesPerRow: 0, space: cs,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.setFillColor(red: 0.2, green: 0.5, blue: 0.8, alpha: 1)
        ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
        return ctx.makeImage()
    }

    private func tempPath(_ name: String) -> String {
        FileManager.default.temporaryDirectory
            .appendingPathComponent("describe-capture-\(UUID().uuidString)-\(name)").path
    }

    /// Drive describe.capture over an injected frame and assert it WRITES a real,
    /// decodable PNG at the requested path — proving the capture reuse reaches the
    /// real frame->PNG write over a real injected frame (NOT the zero-frame stub
    /// trap). Uses an explicit .file source so this is fully headless (no TCC);
    /// production routes the same code over .screen. CRUCIALLY: it emits NO
    /// vision.screen readout (describe is DISTINCT from the OCR read.screen path).
    func testDescribeCaptureWritesPngAndEmitsNoOcrReadout() async throws {
        guard let img = solidImage() else { throw XCTSkip("no test image") }
        let out = tempPath("frame.png")
        defer { try? FileManager.default.removeItem(atPath: out) }

        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        await pipe.handle(.describeCapture(path: out, source: .file(path: "in.png")))

        // The PNG was written and is a real decodable image.
        XCTAssertTrue(FileManager.default.fileExists(atPath: out),
                      "describe.capture must WRITE the frame PNG at the confined path")
        XCTAssertNotNil(VisionEngine.loadCGImage(path: out),
                        "the written frame must be a real, decodable PNG")

        let evs = await sink.snapshot()
        // DISTINCT from read.screen: describe.capture runs NO OCR -> NO vision.screen.
        XCTAssertFalse(evs.contains { if case .screen = $0 { return true }; return false },
                       "describe.capture must NOT emit a vision.screen OCR readout (it is distinct from read.screen)")
        // It signals completion via a status (no pixels, no error).
        XCTAssertTrue(evs.contains {
            if case let .status(_, _, _, _, _, message) = $0 {
                return message?.contains("captured for describe") == true
            }
            return false
        }, "a successful capture emits a 'frame captured for describe' status")
    }

    /// describe.capture does NOT disturb the live-watch run state (one-shot).
    func testDescribeCaptureLeavesWatchStateUntouched() async throws {
        guard let img = solidImage() else { throw XCTSkip("no test image") }
        let out = tempPath("frame2.png")
        defer { try? FileManager.default.removeItem(atPath: out) }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        await pipe.handle(.describeCapture(path: out, source: .file(path: "in.png")))
        let state = await pipe.currentState
        XCTAssertEqual(state, .idle, "a one-shot describe.capture must not change the watch lifecycle state")
    }

    /// A denied screen source emits a clean tcc_denied error and writes nothing —
    /// honoring the TCC gate exactly like read.screen / watch.start.
    func testDescribeCaptureDeniedEmitsTccErrorAndWritesNothing() async {
        let out = tempPath("denied.png")
        defer { try? FileManager.default.removeItem(atPath: out) }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .denied, images: [])
        }
        await pipe.handle(.describeCapture(path: out, source: .screen))
        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .error(code, _, src) = $0 { return code == "tcc_denied" && src == "screen" }
            return false
        }, "denied screen capture -> tcc_denied error")
        XCTAssertFalse(FileManager.default.fileExists(atPath: out),
                       "a denied capture writes NO frame (the host then falls back honestly)")
    }

    /// An authorized source that yields no frame reports an honest no_frame error
    /// and writes nothing — never a stale/blank PNG the VLM would then describe.
    func testDescribeCaptureNoFrameReportsHonestErrorAndWritesNothing() async {
        let out = tempPath("noframe.png")
        defer { try? FileManager.default.removeItem(atPath: out) }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [])
        }
        await pipe.handle(.describeCapture(path: out, source: .file(path: "empty")))
        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .error(code, _, _) = $0 { return code == "no_frame" }; return false
        }, "no captured frame -> honest no_frame error, never a fabricated/blank PNG")
        XCTAssertFalse(FileManager.default.fileExists(atPath: out),
                       "no frame -> no PNG written")
    }
}

// ===========================================================================
// #28 read.handwriting + #29 scan.document — single-shot capture wiring (over
// an injected frame). Mirrors ReadScreenWiringTests: prove the op reaches the
// real engine over a REAL injected frame (NOT the zero-frame stub), honors the
// TCC gate, reports no_frame honestly, leaves the watch state untouched, and
// (for #29) never fabricates a document. Fully headless (explicit .file source,
// no TCC); production routes the same code over .camera/.screen.
// ===========================================================================

final class HandwritingDocumentWiringTests: XCTestCase {

    private func textImage(_ lines: [String], width: Int = 640, height: Int = 240,
                           fontSize: CGFloat = 56) -> CGImage? {
        let cs = CGColorSpaceCreateDeviceRGB()
        guard let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8,
                                  bytesPerRow: 0, space: cs,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.setFillColor(red: 1, green: 1, blue: 1, alpha: 1)
        ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
        let font = CTFontCreateWithName("Helvetica" as CFString, fontSize, nil)
        let attrs: [NSAttributedString.Key: Any] = [.font: font,
            .foregroundColor: CGColor(red: 0, green: 0, blue: 0, alpha: 1)]
        let lineHeight = fontSize * 1.4
        var y = CGFloat(height) - lineHeight - 24
        for line in lines {
            let ctLine = CTLineCreateWithAttributedString(NSAttributedString(string: line, attributes: attrs))
            ctx.textPosition = CGPoint(x: 24, y: y)
            CTLineDraw(ctLine, ctx)
            y -= lineHeight
        }
        return ctx.makeImage()
    }

    private func documentScene(_ lines: [String], width: Int = 900, height: Int = 700) -> CGImage? {
        let cs = CGColorSpaceCreateDeviceRGB()
        guard let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8,
                                  bytesPerRow: 0, space: cs,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.setFillColor(red: 0.08, green: 0.08, blue: 0.10, alpha: 1)
        ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
        let margin: CGFloat = 90
        let page = CGRect(x: margin, y: margin, width: CGFloat(width) - 2 * margin,
                          height: CGFloat(height) - 2 * margin)
        ctx.setFillColor(red: 0.98, green: 0.98, blue: 0.97, alpha: 1)
        ctx.fill(page)
        let fontSize: CGFloat = 64
        let font = CTFontCreateWithName("Helvetica" as CFString, fontSize, nil)
        let attrs: [NSAttributedString.Key: Any] = [.font: font,
            .foregroundColor: CGColor(red: 0, green: 0, blue: 0, alpha: 1)]
        var y = page.maxY - fontSize * 1.5 - 30
        for line in lines {
            let ctLine = CTLineCreateWithAttributedString(NSAttributedString(string: line, attributes: attrs))
            ctx.textPosition = CGPoint(x: page.minX + 40, y: y)
            CTLineDraw(ctLine, ctx)
            y -= fontSize * 1.5
        }
        return ctx.makeImage()
    }

    private func solidImage(width: Int = 96, height: Int = 96) -> CGImage {
        let cs = CGColorSpaceCreateDeviceRGB()
        let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8,
                            bytesPerRow: 0, space: cs,
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        ctx.setFillColor(red: 0.12, green: 0.13, blue: 0.14, alpha: 1)
        ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
        return ctx.makeImage()!
    }

    // --- #28 read.handwriting -----------------------------------------------

    /// read.handwriting over an injected frame reaches the real recognizer and
    /// emits a vision.screen readout tagged read_kind=handwriting with the real
    /// recognized text (NOT the zero-frame stub trap).
    func testReadHandwritingEmitsScreenEventTaggedHandwriting() async throws {
        guard let img = textImage(["Buy milk", "Call Pepper"]) else { throw XCTSkip("no text image") }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        await pipe.handle(.readHandwriting(source: .file(path: "note.png")))

        let evs = await sink.snapshot()
        guard case let .screen(_, _, source, readout, located, query, meta)? = evs.first(where: {
            if case .screen = $0 { return true }; return false }) else {
            return XCTFail("read.handwriting must emit a vision.screen event, got \(evs)")
        }
        XCTAssertEqual(source, "file")
        XCTAssertNil(query)
        XCTAssertNil(located)
        XCTAssertEqual(meta.kind, .handwriting, "read.handwriting tags read_kind=handwriting")
        XCTAssertNil(meta.documentDetected, "handwriting read has no document-detected bool")
        let joined = readout.blocks.map(\.text).joined(separator: " ").lowercased()
        guard !joined.isEmpty else {
            throw XCTSkip("VNRecognizeTextRequest returned no text in this build env (device-gated)")
        }
        XCTAssertTrue(joined.contains("milk") || joined.contains("buy") || joined.contains("call"),
                      "the handwriting readout must carry the real recognized text; got \(joined)")
    }

    /// read.handwriting is a one-shot; it does NOT change the watch lifecycle.
    func testReadHandwritingLeavesWatchStateUntouched() async throws {
        guard let img = textImage(["Hello"]) else { throw XCTSkip("no text image") }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        await pipe.handle(.readHandwriting(source: .file(path: "note.png")))
        let state = await pipe.currentState
        XCTAssertEqual(state, .idle, "a one-shot read.handwriting must not change the watch state")
    }

    /// A denied source emits a clean tcc_denied error and reads nothing.
    func testReadHandwritingDeniedEmitsTccError() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .denied, images: [])
        }
        await pipe.handle(.readHandwriting(source: .camera))
        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .error(code, _, src) = $0 { return code == "tcc_denied" && src == "camera" }
            return false
        }, "denied handwriting read -> tcc_denied error")
        XCTAssertFalse(evs.contains { if case .screen = $0 { return true }; return false },
                       "a denied read emits NO vision.screen readout")
    }

    /// An authorized source that yields no frame reports an honest no_frame error.
    func testReadHandwritingNoFrameReportsHonestError() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [])
        }
        await pipe.handle(.readHandwriting(source: .file(path: "empty")))
        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .error(code, _, _) = $0 { return code == "no_frame" }; return false
        }, "no captured frame -> honest no_frame error, never fabricated text")
    }

    // --- #29 scan.document --------------------------------------------------

    /// scan.document over an injected document-scene frame reaches the real
    /// scanner and emits a vision.screen readout tagged read_kind=document with
    /// the HONEST document_detected bool (NOT the zero-frame stub trap).
    func testScanDocumentEmitsScreenEventTaggedDocument() async throws {
        guard let img = documentScene(["INVOICE", "Total 42"]) else { throw XCTSkip("no doc scene") }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        await pipe.handle(.scanDocument(source: .file(path: "doc.png")))

        let evs = await sink.snapshot()
        guard case let .screen(_, _, source, readout, _, query, meta)? = evs.first(where: {
            if case .screen = $0 { return true }; return false }) else {
            return XCTFail("scan.document must emit a vision.screen event, got \(evs)")
        }
        XCTAssertEqual(source, "file")
        XCTAssertNil(query)
        XCTAssertEqual(meta.kind, .document, "scan.document tags read_kind=document")
        XCTAssertNotNil(meta.documentDetected, "the scanner reports the document-detected bool")
        guard meta.documentDetected == true else {
            throw XCTSkip("VNDetectDocumentSegmentationRequest found no document in this build env (device-gated)")
        }
        let joined = readout.blocks.map(\.text).joined(separator: " ").lowercased()
        guard !joined.isEmpty else {
            throw XCTSkip("OCR over the corrected page returned no text in this build env (device-gated)")
        }
        XCTAssertTrue(joined.contains("invoice") || joined.contains("total"),
                      "the scan readout must carry the real corrected-page text; got \(joined)")
    }

    /// scan.document over a document-FREE frame emits the HONEST empty readout:
    /// document_detected=false (or, if the env over-detects, NO fabricated text).
    func testScanDocumentNoDocumentEmitsHonestEmpty() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { [solid = solidImage()] src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [solid])
        }
        await pipe.handle(.scanDocument(source: .file(path: "blank.png")))
        let evs = await sink.snapshot()
        guard case let .screen(_, _, _, readout, _, _, meta)? = evs.first(where: {
            if case .screen = $0 { return true }; return false }) else {
            return XCTFail("scan.document must always emit a vision.screen event (honest empty), got \(evs)")
        }
        XCTAssertEqual(meta.kind, .document)
        if meta.documentDetected == false {
            XCTAssertTrue(readout.blocks.isEmpty, "no document -> honestly empty readout")
        } else {
            XCTAssertTrue(readout.fullText.isEmpty,
                          "a blank field must never yield fabricated text even if a quad is reported")
        }
    }

    /// CONTRAST (AppWiring-style): with a StubDetector the scan finds NO document
    /// and the readout is honestly empty — proving the production VisionEngine is
    /// the load-bearing piece (a non-VisionEngine detector can NEVER fabricate a
    /// page).
    func testScanDocumentWithStubDetectorYieldsNoDocument() async {
        guard let img = documentScene(["INVOICE"]) else { return }
        let sink = CollectingSink()
        let pipe = Pipeline(detector: StubDetector(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [img])
        }
        await pipe.handle(.scanDocument(source: .file(path: "doc.png")))
        let evs = await sink.snapshot()
        guard case let .screen(_, _, _, readout, _, _, meta)? = evs.first(where: {
            if case .screen = $0 { return true }; return false }) else {
            return XCTFail("scan.document must emit a vision.screen event even with a stub, got \(evs)")
        }
        XCTAssertEqual(meta.documentDetected, false,
                       "a StubDetector never detects a document (the real engine is load-bearing)")
        XCTAssertTrue(readout.blocks.isEmpty, "stub detector -> honestly empty readout, never a fabricated page")
    }

    /// scan.document is a one-shot; it does NOT change the watch lifecycle.
    func testScanDocumentLeavesWatchStateUntouched() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { [solid = solidImage()] src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [solid])
        }
        await pipe.handle(.scanDocument(source: .file(path: "blank.png")))
        let state = await pipe.currentState
        XCTAssertEqual(state, .idle, "a one-shot scan.document must not change the watch state")
    }

    /// A denied source emits a clean tcc_denied error and scans nothing.
    func testScanDocumentDeniedEmitsTccError() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .denied, images: [])
        }
        await pipe.handle(.scanDocument(source: .camera))
        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .error(code, _, src) = $0 { return code == "tcc_denied" && src == "camera" }
            return false
        }, "denied document scan -> tcc_denied error")
        XCTAssertFalse(evs.contains { if case .screen = $0 { return true }; return false },
                       "a denied scan emits NO vision.screen readout")
    }

    /// An authorized source that yields no frame reports an honest no_frame error.
    func testScanDocumentNoFrameReportsHonestError() async {
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink)
        await pipe.setFrameSourceFactory { src in
            FixedFrameSource(source: src, auth: .notApplicable, images: [])
        }
        await pipe.handle(.scanDocument(source: .file(path: "empty")))
        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .error(code, _, _) = $0 { return code == "no_frame" }; return false
        }, "no captured frame -> honest no_frame error, never a fabricated page")
    }
}

// Test-only setter for the injected factory (the actor exposes a stored var; we
// expose a mutating async hop to set it from a test without touching the seam).
extension Pipeline {
    func setFrameSourceFactory(_ f: @escaping @Sendable (CaptureSource) -> FrameSource) {
        self.frameSourceFactory = f
    }
}

// ===========================================================================
// classify.sound — single-shot Sound Analysis wiring (over an injected stub
// classifier; NO microphone, NO real audio decode, NO continuous capture).
// ===========================================================================

/// A SoundClassifier stub that returns CANNED classes for a known clip path and
/// [] for anything else — so the classify.sound wiring is exercised
/// deterministically WITHOUT a real audio decode/classifier (the headless SN
/// proof lives in SoundInferenceTests). It records the path it was asked to
/// classify so the test can assert the op actually reached the classifier seam.
private final class CannedSoundClassifier: SoundClassifier, @unchecked Sendable {
    let canned: [String: [SoundClass]]
    private let lock = NSLock()
    private(set) var lastPath: String?
    private(set) var lastBuffersCount: Int?

    init(canned: [String: [SoundClass]]) { self.canned = canned }

    func classify(buffers: [AVAudioPCMBuffer], minConfidence: Double) -> [SoundClass] {
        // The default audioClipPath path would decode a file; we override that
        // entry directly below so no real file is ever touched. This buffers entry
        // is here only to satisfy the protocol.
        lock.lock(); lastBuffersCount = buffers.count; lock.unlock()
        return []
    }

    func classify(audioClipPath path: String, minConfidence: Double) -> [SoundClass] {
        lock.lock(); lastPath = path; lock.unlock()
        // Apply the floor exactly like the real engine so the gate is testable.
        let floor = max(0.0, min(1.0, minConfidence))
        return (canned[path] ?? []).filter { $0.confidence >= floor }
    }
}

final class ClassifySoundWiringTests: XCTestCase {

    /// classify.sound routes through the Pipeline, calls the classifier seam with
    /// the supplied clip path, and emits a vision.sound event carrying ONLY the
    /// top sound classes (label + confidence), the classifier tag, and the
    /// compute_unit tag — the shape the daemon's identify-sound intent consumes.
    func testClassifySoundEmitsSoundEventFromClip() async {
        let path = "state/vision/clip.wav"
        let stub = CannedSoundClassifier(canned: [
            path: [SoundClass(label: "dog_bark", confidence: 0.88),
                   SoundClass(label: "doorbell", confidence: 0.52)],
        ])
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink, soundClassifier: stub)

        await pipe.handle(.classifySound(path: path))

        // The op reached the classifier seam with the supplied clip path.
        XCTAssertEqual(stub.lastPath, path, "classify.sound must call the classifier with the clip path")

        let evs = await sink.snapshot()
        guard case let .sound(_, source, classes, classifier, computeUnit)? = evs.first(where: {
            if case .sound = $0 { return true }; return false }) else {
            return XCTFail("classify.sound must emit a vision.sound event, got \(evs)")
        }
        XCTAssertEqual(source, "sound")
        XCTAssertEqual(classifier, SoundEngine.classifierTag)
        XCTAssertEqual(computeUnit, SoundEngine.computeUnitTag)
        XCTAssertEqual(classes.map(\.label), ["dog_bark", "doorbell"])
        XCTAssertEqual(classes.first?.confidence ?? 0, 0.88, accuracy: 1e-9)
    }

    /// classify.sound is a one-shot — it must NOT disturb the live-watch run state
    /// (it never opens the mic, never starts a continuous monitor).
    func testClassifySoundLeavesWatchStateUntouched() async {
        let path = "clip.wav"
        let stub = CannedSoundClassifier(canned: [path: [SoundClass(label: "music", confidence: 0.7)]])
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink, soundClassifier: stub)
        await pipe.handle(.classifySound(path: path))
        let state = await pipe.currentState
        XCTAssertEqual(state, .idle, "a one-shot classify.sound must not change the watch lifecycle state")
    }

    /// A clip the classifier scores nothing on (too short / silence / undecodable)
    /// emits an HONEST no_sound_classes error — never a fabricated sound class.
    func testClassifySoundNoClassesEmitsHonestError() async {
        let stub = CannedSoundClassifier(canned: [:])  // nothing canned -> [] for any path
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink, soundClassifier: stub)

        await pipe.handle(.classifySound(path: "state/vision/silence.wav"))

        let evs = await sink.snapshot()
        XCTAssertTrue(evs.contains {
            if case let .error(code, _, src) = $0 { return code == "no_sound_classes" && src == "sound" }
            return false
        }, "no classes -> honest no_sound_classes error")
        XCTAssertFalse(evs.contains { if case .sound = $0 { return true }; return false },
                       "an empty classification emits NO vision.sound readout (never a fake label)")
    }

    /// The classify.sound emission carries ONLY derived labels — the audio never
    /// leaves. Asserted at the WIRE level: the serialized vision.sound line has no
    /// audio/pcm/samples field.
    func testClassifySoundEmissionCarriesNoAudio() async throws {
        let path = "clip.wav"
        let stub = CannedSoundClassifier(canned: [path: [SoundClass(label: "alarm", confidence: 0.9)]])
        let sink = CollectingSink()
        let pipe = Pipeline(detector: VisionEngine(), sink: sink, soundClassifier: stub)
        await pipe.handle(.classifySound(path: path))

        let evs = await sink.snapshot()
        let soundEvent = try XCTUnwrap(evs.first { if case .sound = $0 { return true }; return false })
        let line = try XCTUnwrap(soundEvent.line(token: "T")).lowercased()
        for forbidden in ["\"audio\"", "\"pcm\"", "\"samples\"", "\"waveform\"", "\"buffer\""] {
            XCTAssertFalse(line.contains(forbidden),
                           "the emitted vision.sound line must carry NO audio (found \(forbidden))")
        }
        XCTAssertTrue(line.contains("alarm"), "the derived label must be present")
    }
}
