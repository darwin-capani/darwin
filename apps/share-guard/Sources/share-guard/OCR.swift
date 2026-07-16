// OCR.swift — the DEVICE-GATED runner: the live, on-device VNRecognizeTextRequest
// OCR path (the SAME built-in Apple engine the Vision app uses). This is the ONE
// impure seam in Share Guard: it reads pixels from a supplied image and returns
// recognized GLYPH TEXT, which the PURE PIIDetector/Redaction seam then scrubs.
//
// HONESTY + SCOPE:
//   - On-device + OFFLINE. Built-in Apple Vision request; no external model
//     download, no network. Recognized text never leaves the device on this path.
//   - GLYPHS ONLY. VNRecognizeTextRequest reads text; it is never turned into a
//     face / person identity. There is no identity path here.
//   - This runner operates on a SUPPLIED image the host staged inside the app's
//     own input dir — it does NOT open the camera or capture the live screen.
//     Share Guard's manifest declares camera=false, screen=false, audio=false.
//   - NOT UNIT-TESTED (per the app's test contract): recognition QUALITY is
//     device/Vision-model-dependent, so the live OCR is exercised only through the
//     `share-guard scrub-image` CLI mode and the daemon-launched scrub.image op —
//     never in `swift test`. The pure scrub seam it feeds IS exhaustively tested.

import Foundation

#if canImport(Vision)
import Vision
import CoreGraphics
import ImageIO

/// The on-device text recognizer. Wraps the built-in VNRecognizeTextRequest and
/// returns the recognized lines in reading order (top-to-bottom). Reused from the
/// Vision app's OCR configuration (.accurate + language correction, explicit
/// offline language list).
public struct OCRTextRecognizer: Sendable {

    /// The languages VNRecognizeTextRequest is asked to recognize, in priority
    /// order — an explicit, offline, OS-bundled set so OCR is deterministic.
    public let recognitionLanguages: [String]

    /// Latin-script Western default (bundled with the OS, no model download).
    public static let defaultRecognitionLanguages =
        ["en-US", "fr-FR", "de-DE", "es-ES", "it-IT", "pt-BR"]

    public init(recognitionLanguages: [String] = OCRTextRecognizer.defaultRecognitionLanguages) {
        self.recognitionLanguages = recognitionLanguages
    }

    /// Recognize text lines in a CGImage, in reading order (top-to-bottom; Vision
    /// boxes use a bottom-left origin, so a higher `y` reads first). Non-throwing +
    /// total: a request failure or empty result yields `[]` (never a fabricated
    /// line). DEVICE-GATED behavior lives entirely in the real Vision `perform`.
    public func recognizeLines(cgImage: CGImage) -> [String] {
        let request = VNRecognizeTextRequest()
        request.recognitionLevel = .accurate
        request.usesLanguageCorrection = true
        request.recognitionLanguages = recognitionLanguages

        let handler = VNImageRequestHandler(cgImage: cgImage, options: [:])
        do {
            try handler.perform([request])
        } catch {
            return []
        }
        guard let observations = request.results else { return [] }

        // Sort by vertical position (bottom-left origin: higher y = higher on
        // screen = reads first), then left-to-right, and take each block's top
        // candidate string.
        let ordered = observations.sorted { a, b in
            let ay = a.boundingBox.origin.y, by = b.boundingBox.origin.y
            if abs(ay - by) > 0.02 { return ay > by }
            return a.boundingBox.origin.x < b.boundingBox.origin.x
        }
        return ordered.compactMap { obs -> String? in
            guard let top = obs.topCandidates(1).first else { return nil }
            let s = top.string
            return s.isEmpty ? nil : s
        }
    }

    /// Recognize the full text of an image FILE (the supplied payload), joined in
    /// reading order by newlines. Returns nil if the file cannot be decoded (the
    /// caller reports an honest "could not read image" rather than fabricating
    /// text). Offline; the pixels are read locally and never leave the device.
    public func recognizeText(imagePath path: String) -> String? {
        guard let image = Self.loadCGImage(path: path) else { return nil }
        return recognizeLines(cgImage: image).joined(separator: "\n")
    }

    /// Decode an image file (PNG/JPEG/etc.) into a CGImage via ImageIO. Offline,
    /// no network. Returns nil on any failure (missing/corrupt/unsupported).
    public static func loadCGImage(path: String) -> CGImage? {
        let url = URL(fileURLWithPath: path)
        guard let src = CGImageSourceCreateWithURL(url as CFURL, nil),
              CGImageSourceGetCount(src) > 0,
              let img = CGImageSourceCreateImageAtIndex(src, 0, nil)
        else { return nil }
        return img
    }
}

#endif // canImport(Vision)
