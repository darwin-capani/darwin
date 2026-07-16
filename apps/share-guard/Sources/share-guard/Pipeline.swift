// Pipeline.swift — routes decoded host Ops through the scrub seam and writes the
// redacted copy into the sandbox (mirrors the vision app's Pipeline actor).
//
// The Pipeline is Vision-AGNOSTIC: the (device-gated) OCR runner is INJECTED as a
// closure, so the pipeline itself compiles + reasons everywhere and the live
// VNRecognizeTextRequest lives only in the wiring (App.swift, under
// canImport(Vision)). Every op path is total: a decode/scrub/write failure emits a
// share.error, never a crash.
//
// SAFETY: the ONLY write is the redacted copy under the app's own sandbox dir
// (SandboxRoot.writeRedacted); the user's original is never touched. A scrub.image
// read is confined to the granted input dir first. DARWIN cannot SEND — the user
// shares the scrubbed copy themselves.

import Foundation

/// The OCR seam: recognize the text of a supplied image FILE, or nil if it cannot
/// be read. Injected so the live Vision path stays out of the pipeline (and out of
/// the pure tests). Returns the recognized glyph text (reading order).
public typealias ImageTextRecognizer = @Sendable (String) -> String?

/// Routes host Ops to the scrub seam + sandbox writer, emitting share.* events.
public actor Pipeline {
    private let sink: EventSink
    private let sandbox: SandboxRoot
    /// The injected OCR runner (nil => scrub.image reports "OCR unavailable" here
    /// rather than reaching a device path; the wiring injects the real recognizer).
    private let recognizeImageText: ImageTextRecognizer?
    private let mask: MaskStyle

    public init(sink: EventSink, sandbox: SandboxRoot,
                recognizeImageText: ImageTextRecognizer? = nil,
                mask: MaskStyle = .labeled) {
        self.sink = sink
        self.sandbox = sandbox
        self.recognizeImageText = recognizeImageText
        self.mask = mask
    }

    /// Handle one decoded op.
    public func handle(_ op: Op) async {
        switch op {
        case .start, .refresh:
            await sink.emit(.status(state: .idle, message: "share-guard ready"))
        case .stop:
            await sink.emit(.status(state: .stopped, message: "share-guard stopped"))
        case .status:
            await sink.emit(.status(state: .idle, message: "share-guard ready"))
        case let .scrubText(text, artifactId):
            await scrubAndPublish(text: text, artifactId: artifactId)
        case let .scrubImage(path, artifactId):
            await scrubImage(path: path, artifactId: artifactId)
        case let .unknown(raw):
            // Keep the diagnostic bounded — never echo an unbounded/possibly
            // sensitive line back onto telemetry.
            let snippet = String(raw.prefix(80))
            await sink.emit(.error(code: "bad_op", message: "unrecognized op line: \(snippet)"))
        }
    }

    // -- op handlers ----------------------------------------------------------

    /// Scrub a text payload, write the redacted copy to the sandbox, and emit the
    /// secret-free share.redactions readout.
    private func scrubAndPublish(text: String, artifactId: String?) async {
        await sink.emit(.status(state: .scrubbing, message: nil))
        let result = ShareGuard.scrub(text: text, mask: mask)
        var output: String?
        do {
            let name = Self.outputName(artifactId: artifactId)
            let written = try sandbox.writeRedacted(name: name, contents: result.redactedText)
            // Report the sandbox-RELATIVE path (never the absolute host path).
            output = Self.relativize(written, under: sandbox.root)
        } catch let e as SandboxError {
            await sink.emit(.error(code: e.code, message: e.description))
        } catch {
            await sink.emit(.error(code: "write_failed", message: "\(error)"))
        }
        await sink.emit(.redactions(
            counts: result.counts, total: result.total, foundPII: result.foundPII,
            preview: result.preview(), output: output, artifactId: artifactId,
            originalLength: result.originalLength))
    }

    /// Confine + OCR a supplied image, then scrub the recognized text. The read is
    /// confined to the granted input dir; the OCR itself is the injected
    /// device-gated runner.
    private func scrubImage(path: String, artifactId: String?) async {
        guard let recognize = recognizeImageText else {
            await sink.emit(.error(code: "ocr_unavailable",
                                   message: "OCR runner not available in this build"))
            return
        }
        let confined: String
        do {
            confined = try sandbox.confinedInputPath(path)
        } catch let e as SandboxError {
            await sink.emit(.error(code: e.code, message: e.description))
            return
        } catch {
            await sink.emit(.error(code: "path_denied", message: "\(error)"))
            return
        }
        await sink.emit(.status(state: .scrubbing, message: nil))
        guard let text = recognize(confined) else {
            await sink.emit(.error(code: "ocr_failed",
                                   message: "could not read text from the supplied image"))
            return
        }
        await scrubAndPublish(text: text, artifactId: artifactId)
    }

    // -- helpers --------------------------------------------------------------

    /// A safe redacted-copy filename derived from the artifact id (sanitized to
    /// `[A-Za-z0-9_-]` — no dots, so a `..` can never survive) or a timestamp when
    /// none is supplied. Always ends `.txt`.
    static func outputName(artifactId: String?) -> String {
        let stem: String
        if let id = artifactId, !id.isEmpty {
            let safe = id.map { ch -> Character in
                (ch.isLetter || ch.isNumber || ch == "_" || ch == "-") ? ch : "-"
            }
            stem = "artifact-" + String(String(safe).prefix(64))
        } else {
            stem = "scrub-\(Int(Date().timeIntervalSince1970))"
        }
        return "redacted-\(stem).txt"
    }

    /// Present an absolute written path as sandbox-relative (`redacted/…`) for
    /// telemetry, never leaking the absolute host path. Falls back to the last
    /// component if the prefix does not match.
    static func relativize(_ absolute: String, under root: String) -> String {
        let rootWithSep = root.hasSuffix("/") ? root : root + "/"
        if absolute.hasPrefix(rootWithSep) {
            return String(absolute.dropFirst(rootWithSep.count))
        }
        return (absolute as NSString).lastPathComponent
    }
}
