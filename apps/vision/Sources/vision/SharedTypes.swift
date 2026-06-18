// SharedTypes.swift — the CANONICAL shared vocabulary for the Vision micro-app.
//
// FROZEN: module agents (inference, capture, pipeline, ipc, main) build AGAINST
// these types and MUST NOT change them. Anything that crosses a module boundary
// — a Detection, a Frame, a capture source, an Op, the AppEnv, a telemetry
// event — lives here so the modules stay disjoint and wire-compatible.
//
// Defensive-only invariants baked into the type system:
//   - Detection has NO identity field (no name, no faceprint, no person id).
//     We detect WHAT (human-as-rectangle, animal, object, salient region,
//     motion), never WHO. There is deliberately no API to attach an identity.
//   - Nothing here serializes a Frame's pixels for transport. Telemetry carries
//     COUNTS and BOUNDING BOXES, never image bytes — frames never leave device.

import Foundation
import CoreGraphics
import CoreVideo

// ===========================================================================
// Detection
// ===========================================================================

/// One thing the vision pipeline found in a frame.
///
/// `kind` says WHAT category; `label` is a coarse, model-provided class string
/// (e.g. "dog", "keyboard") for object/animal/classification results — it is a
/// generic category, NEVER an identity. `boundingBox` is in Vision's normalized
/// coordinate space (origin bottom-left, 0...1 on each axis); a detection with
/// no spatial extent (a whole-frame classification) uses `DetectionBox.full`.
public struct Detection: Sendable, Codable, Equatable {
    /// What category of thing was detected. Deliberately coarse + non-identifying.
    public enum Kind: String, Sendable, Codable, CaseIterable {
        case human          // VNDetectHumanRectanglesRequest — a person as a RECTANGLE, not an identity
        case animal         // VNRecognizeAnimalsRequest — cat/dog class only
        case object         // VNClassifyImageRequest / objectness — generic object class
        case salientRegion  // VNGenerate*SaliencyImageRequest — attention/objectness region
        case motion         // pipeline-derived: frame-to-frame change region
        case text           // VNRecognizeTextRequest — recognized GLYPHS as a string, NOT a person/face id
    }

    public var kind: Kind
    /// Normalized bounding box (Vision convention: origin bottom-left, 0...1).
    public var boundingBox: DetectionBox
    /// Model/pipeline confidence in 0...1.
    public var confidence: Double
    /// Coarse category label (generic class, never an identity). May be empty
    /// for kinds that have no class string (e.g. a bare human rectangle).
    /// For `.text` this carries the RECOGNIZED GLYPH STRING (the read text) —
    /// still NOT an identity: OCR reads on-screen/printed glyphs, never a face,
    /// name, or person id. This text is sensitive + transient (see the wiring
    /// stage) and must not be persisted to lifelong memory by default.
    public var label: String

    public init(kind: Kind, boundingBox: DetectionBox, confidence: Double, label: String = "") {
        self.kind = kind
        self.boundingBox = boundingBox
        self.confidence = confidence
        self.label = label
    }
}

/// A normalized bounding box in Vision's coordinate space (origin bottom-left,
/// each component in 0...1). A Codable, Sendable mirror of CGRect so detections
/// cross module + wire boundaries without dragging CoreGraphics value-identity
/// quirks; convert with `.cgRect` / `init(cgRect:)`.
public struct DetectionBox: Sendable, Codable, Equatable {
    public var x: Double
    public var y: Double
    public var width: Double
    public var height: Double

    public init(x: Double, y: Double, width: Double, height: Double) {
        self.x = x
        self.y = y
        self.width = width
        self.height = height
    }

    public init(cgRect r: CGRect) {
        self.init(x: Double(r.origin.x), y: Double(r.origin.y),
                  width: Double(r.size.width), height: Double(r.size.height))
    }

    public var cgRect: CGRect {
        CGRect(x: x, y: y, width: width, height: height)
    }

    /// The whole frame (a classification with no spatial extent).
    public static let full = DetectionBox(x: 0, y: 0, width: 1, height: 1)
}

// ===========================================================================
// Capture source
// ===========================================================================

/// Where frames come from. The user's OWN devices/files only.
public enum CaptureSource: Sendable, Codable, Equatable {
    case camera     // the user's OWN camera (AVFoundation; TCC: Camera)
    case screen     // the user's OWN screen (ScreenCaptureKit; TCC: Screen Recording)
    case file(path: String)   // a user-provided video file under videos/input

    /// Stable tag used on Frames + telemetry ("camera" | "screen" | "file").
    public var tag: String {
        switch self {
        case .camera: return "camera"
        case .screen: return "screen"
        case .file:   return "file"
        }
    }
}

// ===========================================================================
// Frame
// ===========================================================================

/// One captured/decoded frame handed from capture -> pipeline -> inference.
///
/// Holds EITHER a CVPixelBuffer (live capture path) OR a CGImage (decoded file
/// / synthesized-test path); accessors normalize to a CGImage for Vision.
/// `pixels`/`image` are NEVER serialized — Frame is intentionally not Codable.
/// Frames stay in-process and never reach telemetry or disk.
public struct Frame: @unchecked Sendable {
    /// Live-capture pixel buffer, when present (camera/screen path).
    public let pixelBuffer: CVPixelBuffer?
    /// Decoded/synthesized image, when present (file/test path).
    public let cgImage: CGImage?
    /// Capture time, monotonic-ish, seconds since the run started or media PTS.
    public let timestamp: TimeInterval
    /// Which source produced this frame.
    public let source: CaptureSource
    /// Monotonic frame index within the current watch/analyze run (0-based).
    public let index: UInt64

    public init(pixelBuffer: CVPixelBuffer, timestamp: TimeInterval, source: CaptureSource, index: UInt64) {
        self.pixelBuffer = pixelBuffer
        self.cgImage = nil
        self.timestamp = timestamp
        self.source = source
        self.index = index
    }

    public init(cgImage: CGImage, timestamp: TimeInterval, source: CaptureSource, index: UInt64) {
        self.pixelBuffer = nil
        self.cgImage = cgImage
        self.timestamp = timestamp
        self.source = source
        self.index = index
    }

    /// Pixel dimensions, from whichever backing the frame carries.
    public var pixelSize: CGSize {
        if let img = cgImage {
            return CGSize(width: img.width, height: img.height)
        }
        if let pb = pixelBuffer {
            return CGSize(width: CVPixelBufferGetWidth(pb), height: CVPixelBufferGetHeight(pb))
        }
        return .zero
    }
}
