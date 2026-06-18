// Inference.swift — INFERENCE module (the VisionEngine).
//
// Responsibility: run Apple's BUILT-IN Vision requests on a Frame and return
// [Detection]. Built-ins only — NO external model download (fully offline):
//   - VNDetectHumanRectanglesRequest                -> Detection.Kind.human (rectangles, NOT identity)
//   - VNRecognizeAnimalsRequest                     -> .animal  (cat/dog class)
//   - VNClassifyImageRequest                        -> .object  (top-N generic classes)
//   - VNGenerateObjectnessBasedSaliencyImageRequest -> .salientRegion (attention/objectness)
//
// COMPUTE / ANE: the built-in requests above are ANE/GPU-eligible — Apple
// schedules the execution unit and exposes no residency readout, so this is
// preferred placement, not an observed fact.
// Where a VNCoreMLRequest is ever introduced, its MLModelConfiguration must use
// .computeUnits = .all so it is ANE/GPU eligible. We expose that configuration
// (`MLModelConfiguration.aneVision`) and a `computeUnit` tag ("all") so the perf
// telemetry + tests can assert the ANE path is requested.
//
// DEFENSIVE: we detect WHAT (human-as-rectangle, animal class, object class,
// salient region), never WHO. No face matching, no identity, no re-ID. A
// VNDetectHumanRectanglesRequest result is a bare rectangle with empty label —
// there is deliberately no code path that attaches a name/identity.
//
// DE-RISKED this session: a headless VNClassifyImageRequest over a synthesized
// CGImage compiled + ran with NO camera/display/TCC and returned 1303 classes.
// `analyze(image:)` mirrors that proof and is exposed as the `vision analyze
// <imagepath>` CLI mode — the headlessly-verifiable evidence the pipeline is real.

import Foundation
import CoreGraphics
import Vision
import CoreML
import ImageIO
import CoreImage
import UniformTypeIdentifiers

// ===========================================================================
// DetectorSet — which built-in detectors to run on each frame
// ===========================================================================

/// Which built-in detectors to run on each frame.
public struct DetectorSet: OptionSet, Sendable {
    public let rawValue: Int
    public init(rawValue: Int) { self.rawValue = rawValue }

    public static let humans         = DetectorSet(rawValue: 1 << 0)
    public static let animals        = DetectorSet(rawValue: 1 << 1)
    public static let classification = DetectorSet(rawValue: 1 << 2)
    public static let saliency       = DetectorSet(rawValue: 1 << 3)
    /// VNRecognizeTextRequest — read on-screen/printed TEXT GLYPHS (OCR). Built-in,
    /// ANE/GPU-eligible, OFFLINE. DEFENSIVE: reads text, never a face/person id.
    public static let text           = DetectorSet(rawValue: 1 << 4)

    /// Everything except classification (classification is noisy at high fps).
    /// Text is NOT in liveDefault/all: OCR is op-gated (read-on-request), never
    /// part of the continuous live-watch detector set.
    public static let liveDefault: DetectorSet = [.humans, .animals, .saliency]
    public static let all: DetectorSet = [.humans, .animals, .classification, .saliency]
}

/// Runs built-in Vision requests on a frame. Errors are recoverable (a bad
/// frame yields []), so the protocol is non-throwing and total.
public protocol Detector: Sendable {
    /// Run the configured detectors on one frame and return detections.
    /// `minConfidence` gates results (0...1, from the current sensitivity).
    func detect(in frame: Frame, detectors: DetectorSet, minConfidence: Double) -> [Detection]

    /// Run the detectors AND report the real measured inference time (ms). The
    /// production `VisionEngine` measures strictly around the on-device Vision
    /// `perform`; the default below times the `detect` call with a monotonic
    /// clock so every Detector reports a real (never placeholder) number.
    func detectTimed(in frame: Frame, detectors: DetectorSet,
                     minConfidence: Double) -> (detections: [Detection], inferenceMs: Double)

    /// #29 DOCUMENT SCAN seam: detect a document quad in the frame, perspective-
    /// correct it, and OCR the corrected page into a `DocumentScan` (the HONEST
    /// document-detected bool + the recognized lines). The production `VisionEngine`
    /// runs the real VNDetectDocumentSegmentationRequest -> CIPerspectiveCorrection
    /// -> VNRecognizeTextRequest; the default below finds NO document (honest empty)
    /// so a stub/test detector never fabricates a page. Total + non-throwing.
    func scanDocument(in frame: Frame, minConfidence: Double) -> VisionEngine.DocumentScan
}

extension Detector {
    /// Default measured path: time the `detect` call itself with a monotonic
    /// clock. `VisionEngine` overrides this to bracket only the inner Vision
    /// `perform`, which is the tighter (and the production) measurement.
    public func detectTimed(in frame: Frame, detectors: DetectorSet,
                            minConfidence: Double) -> (detections: [Detection], inferenceMs: Double) {
        let t0 = DispatchTime.now().uptimeNanoseconds
        let dets = detect(in: frame, detectors: detectors, minConfidence: minConfidence)
        let t1 = DispatchTime.now().uptimeNanoseconds
        return (dets, Double(t1 &- t0) / 1_000_000.0)
    }

    /// Default document-scan seam: find NO document (honest empty). A detector
    /// that does not implement the real segmentation/correction/OCR pipeline
    /// (e.g. `StubDetector`) never fabricates a page. `VisionEngine` overrides
    /// this with the real pipeline.
    public func scanDocument(in frame: Frame, minConfidence: Double) -> VisionEngine.DocumentScan {
        .none
    }
}

// ===========================================================================
// ANE compute configuration
// ===========================================================================

extension MLModelConfiguration {
    /// The compute configuration the Vision engine requests for any Core ML
    /// backed request: `.all` makes the model ANE/GPU eligible (the ANE is
    /// preferred for supported ops). Built-in Vision requests already schedule
    /// on the ANE/GPU; this is the explicit knob for any VNCoreMLRequest we add.
    public static var aneVision: MLModelConfiguration {
        let cfg = MLModelConfiguration()
        cfg.computeUnits = .all
        return cfg
    }
}

// ===========================================================================
// VisionEngine — the real built-in-Vision Detector
// ===========================================================================

/// The production `Detector`: runs Apple's built-in Vision requests over a frame
/// (ANE/GPU-eligible; Apple picks the unit) and maps results into `[Detection]`.
/// Stateless + `Sendable`
/// (each `detect` call builds its own request handler), so it is safe to share
/// across the pipeline actor. Offline: zero external model downloads.
public struct VisionEngine: Detector {

    /// How many top classification results to surface as `.object` detections.
    public let maxClassifications: Int
    /// Salient-region cap (objectness saliency can return many small regions).
    public let maxSalientRegions: Int
    /// Recognized-text block cap (a dense screen can yield hundreds of lines).
    public let maxTextBlocks: Int
    /// Minimum text height as a fraction of image height (0 = no floor). Filters
    /// out tiny noise glyphs; passed to VNRecognizeTextRequest.minimumTextHeight.
    public let minimumTextHeight: Float
    /// The languages VNRecognizeTextRequest is asked to recognize, in priority
    /// order. An EXPLICIT list (not the system default) so OCR behavior is
    /// deterministic + offline; all are bundled with the OS (no model download).
    public let recognitionLanguages: [String]

    /// The compute-unit tag reported on `vision.perf` ("all" = ANE/GPU eligible).
    public static let computeUnitTag = "all"

    /// Default recognition languages — Latin-script Western set bundled with the
    /// OS. Explicit so the OCR is reproducible; extend per locale as needed.
    public static let defaultRecognitionLanguages = ["en-US", "fr-FR", "de-DE", "es-ES", "it-IT", "pt-BR"]

    public init(maxClassifications: Int = 5, maxSalientRegions: Int = 8,
                maxTextBlocks: Int = 64, minimumTextHeight: Float = 0,
                recognitionLanguages: [String] = VisionEngine.defaultRecognitionLanguages) {
        self.maxClassifications = max(0, maxClassifications)
        self.maxSalientRegions = max(0, maxSalientRegions)
        self.maxTextBlocks = max(0, maxTextBlocks)
        self.minimumTextHeight = max(0, minimumTextHeight)
        self.recognitionLanguages = recognitionLanguages
    }

    // --- Detector seam -----------------------------------------------------

    public func detect(in frame: Frame, detectors: DetectorSet, minConfidence: Double) -> [Detection] {
        detectTimed(in: frame, detectors: detectors, minConfidence: minConfidence).detections
    }

    /// #29 DOCUMENT SCAN over a Frame: normalize the frame to a CGImage (the live
    /// CVPixelBuffer path renders via Core Image, the file/synth path uses its
    /// CGImage directly) and run the real `scanDocument(image:)` pipeline. A
    /// backing-less frame yields the honest `.none` (no document fabricated).
    public func scanDocument(in frame: Frame, minConfidence: Double) -> DocumentScan {
        guard let img = Self.encodableCGImage(for: frame) else { return .none }
        return scanDocument(image: img, minConfidence: minConfidence)
    }

    /// Run the detectors AND report the REAL wall-clock inference time. The
    /// elapsed value is measured with a monotonic clock around the actual
    /// `VNImageRequestHandler.perform` call (see `run(handler:…)`), so it is a
    /// genuine measured per-frame inference latency in milliseconds — never a
    /// placeholder. A frame with no usable backing pixels does no inference and
    /// reports `inferenceMs == 0` (nothing was timed).
    public func detectTimed(in frame: Frame, detectors: DetectorSet,
                            minConfidence: Double) -> (detections: [Detection], inferenceMs: Double) {
        guard let handler = Self.makeHandler(for: frame) else {
            // No usable backing pixels -> no detections, nothing timed.
            return ([], 0)
        }
        return runTimed(handler: handler, detectors: detectors, minConfidence: minConfidence)
    }

    /// Headless entry: run the full detector set over a CGImage and return
    /// detections. Mirrors the proven probe and backs the `vision analyze
    /// <imagepath>` CLI mode — verifiable with NO camera/screen/TCC.
    public func analyze(image: CGImage,
                        detectors: DetectorSet = .all,
                        minConfidence: Double = 0.0) -> [Detection] {
        analyzeTimed(image: image, detectors: detectors, minConfidence: minConfidence).detections
    }

    /// Headless entry that ALSO reports the real measured inference time (ms),
    /// timed around the actual `VNImageRequestHandler.perform`. Backs the
    /// `vision analyze` / `analyze-video` perf telemetry — a genuine number, not
    /// a placeholder.
    public func analyzeTimed(image: CGImage,
                             detectors: DetectorSet = .all,
                             minConfidence: Double = 0.0) -> (detections: [Detection], inferenceMs: Double) {
        let handler = VNImageRequestHandler(cgImage: image, options: [:])
        return runTimed(handler: handler, detectors: detectors, minConfidence: minConfidence)
    }

    /// Convenience headless entry: decode an image file from disk and analyze it.
    /// Returns [] (never throws) if the file can't be decoded — the CLI prints a
    /// distinguishable error separately. Used by the `analyze` CLI mode + tests.
    public func analyze(imagePath path: String,
                        detectors: DetectorSet = .all,
                        minConfidence: Double = 0.0) -> [Detection] {
        guard let image = Self.loadCGImage(path: path) else { return [] }
        return analyze(image: image, detectors: detectors, minConfidence: minConfidence)
    }

    /// Headless OCR entry: run ONLY VNRecognizeTextRequest over a CGImage and
    /// return the `.text` Detections (recognized string + box + confidence). Backs
    /// the `vision ocr <imagepath>` CLI mode and the synthetic-text OCR proof —
    /// verifiable with NO camera/screen/TCC. This is the genuine "OCR really
    /// works" path: it runs the REAL Vision recognizer over an in-memory image.
    public func recognizeText(image: CGImage, minConfidence: Double = 0.0) -> [Detection] {
        analyze(image: image, detectors: .text, minConfidence: minConfidence)
    }

    /// Headless OCR entry from a file path. Returns [] (never throws) if the file
    /// can't be decoded. Used by the `vision ocr <imagepath>` CLI mode.
    public func recognizeText(imagePath path: String, minConfidence: Double = 0.0) -> [Detection] {
        guard let image = Self.loadCGImage(path: path) else { return [] }
        return recognizeText(image: image, minConfidence: minConfidence)
    }

    // --- #28 HANDWRITING / WHITEBOARD ---------------------------------------
    //
    // recognizeHandwriting: the SAME built-in VNRecognizeTextRequest, run in the
    // config that best reads handwriting / whiteboard text — recognitionLevel
    // .accurate + usesLanguageCorrection true (the on-device state-of-the-art for
    // free-form / handwritten strokes; the recognizer is a single neural reader
    // and `.accurate` + language correction is the setting Apple documents for
    // hard text). Returns the recognized lines as `.text` Detections (string +
    // box + confidence), EXACTLY like the screen OCR, so the existing structuring
    // (reading order) and the existing vision.screen readout carry it unchanged.
    //
    // This is DISTINCT from `recognizeText` only in intent + the explicit honest
    // framing: handwriting recognition QUALITY is device/Vision-model-dependent
    // (a messy scrawl may not read), and the live capture is TCC-gated — the
    // engine itself is proven headlessly here over a synthesized image. DEFENSIVE:
    // glyph text only, NEVER a face / person identity. The recognized text is
    // sensitive + transient (kept off lifelong memory by default).

    /// Headless HANDWRITING/WHITEBOARD recognition over a CGImage: run ONLY
    /// VNRecognizeTextRequest with `.accurate` + language correction (the config
    /// best for handwriting/whiteboard text) and return the recognized lines as
    /// `.text` Detections (string + box + confidence). Identical wire shape to
    /// `recognizeText` (so the existing structuring + vision.screen readout carry
    /// it), distinct in intent. Verifiable with NO camera/screen/TCC. Honesty:
    /// recognition QUALITY is device/Vision-model-dependent; a scrawl may not read
    /// (then this returns []), and live capture is TCC-gated — never fabricates a
    /// line. DEFENSIVE: glyph text only, never a person id.
    public func recognizeHandwriting(image: CGImage, minConfidence: Double = 0.0) -> [Detection] {
        // The .accurate + usesLanguageCorrection config IS the engine default
        // (see runTimed's .text branch). recognizeHandwriting is the handwriting/
        // whiteboard-INTENT entry over that same proven recognizer.
        analyze(image: image, detectors: .text, minConfidence: minConfidence)
    }

    /// Headless handwriting entry from a file path. Returns [] (never throws) if
    /// the file can't be decoded. Backs the `vision handwriting <imagepath>` CLI.
    public func recognizeHandwriting(imagePath path: String, minConfidence: Double = 0.0) -> [Detection] {
        guard let image = Self.loadCGImage(path: path) else { return [] }
        return recognizeHandwriting(image: image, minConfidence: minConfidence)
    }

    // --- #29 CAMERA DOCUMENT SCANNER ----------------------------------------
    //
    // scanDocument: VNDetectDocumentSegmentationRequest finds the document QUAD ->
    // CIPerspectiveCorrection flattens it using the detected corners -> a second
    // VNRecognizeTextRequest reads the CORRECTED page -> structured `.text`
    // Detections (lines/blocks). When NO document is detected, this returns an
    // HONEST empty result (`documentDetected == false`, no text) — it NEVER
    // fabricates a page. All on-device, offline (built-in Vision + Core Image, no
    // model download). Verifiable headlessly over a synthesized image with a known
    // quad of text. Honesty: the segmentation/correction QUALITY is device-
    // dependent, and the live camera capture is TCC-gated. DEFENSIVE: glyph text
    // only, never a face / person id. The text is sensitive + transient.

    /// The structured result of a document scan. `documentDetected` is the HONEST
    /// signal of whether VNDetectDocumentSegmentationRequest found a page quad;
    /// when false, `lines` is empty (we never fabricate a page). `lines` are the
    /// `.text` Detections read off the PERSPECTIVE-CORRECTED page (string + box +
    /// confidence in the corrected image's normalized coords). DEFENSIVE: glyph
    /// text only, never an identity.
    public struct DocumentScan: Sendable, Equatable {
        /// Whether a document page quad was detected (the honest "is there a
        /// document" bool). When false, `lines` is empty.
        public let documentDetected: Bool
        /// The recognized text lines off the corrected page (`.text` Detections).
        public let lines: [Detection]
        /// The detected page quad's confidence (0 when no document). Device-
        /// dependent; reported honestly, never fabricated.
        public let quadConfidence: Double

        public init(documentDetected: Bool, lines: [Detection], quadConfidence: Double) {
            self.documentDetected = documentDetected
            self.lines = lines
            self.quadConfidence = quadConfidence
        }

        /// The honest "no document found" result — never a fabricated page.
        public static let none = DocumentScan(documentDetected: false, lines: [], quadConfidence: 0)
    }

    /// Headless DOCUMENT SCAN over a CGImage: detect the page quad
    /// (VNDetectDocumentSegmentationRequest), perspective-correct it
    /// (CIPerspectiveCorrection using the detected corners), then OCR the corrected
    /// page (VNRecognizeTextRequest) into structured `.text` lines. Returns a
    /// `DocumentScan` carrying the HONEST `documentDetected` bool + the read lines.
    /// When NO document is found, returns `.none` (empty, documentDetected=false)
    /// — never fabricates a page. Verifiable with NO camera/screen/TCC. Honesty:
    /// segmentation/correction QUALITY is device-dependent; live camera capture is
    /// TCC-gated. DEFENSIVE: glyph text only, never a person id.
    public func scanDocument(image: CGImage, minConfidence: Double = 0.0) -> DocumentScan {
        let floor = max(0.0, min(1.0, minConfidence))

        // 1. Detect the document segmentation quad. macOS 13+ has the request;
        //    on an older build it is honestly unavailable -> no document found.
        guard #available(macOS 13.0, *) else { return .none }
        let segReq = VNDetectDocumentSegmentationRequest()
        let segHandler = VNImageRequestHandler(cgImage: image, options: [:])
        do {
            try segHandler.perform([segReq])
        } catch {
            return .none
        }
        // Highest-confidence detected rectangle (the page). No observation -> honest
        // "no document found" (we never fabricate a page).
        guard let obs = segReq.results?.max(by: { $0.confidence < $1.confidence }) else {
            return .none
        }
        let quadConfidence = Double(obs.confidence)

        // 2. Perspective-correct the detected quad to a flat page via Core Image.
        //    The observation corners are normalized (Vision coords, origin
        //    bottom-left); CIPerspectiveCorrection wants image-space points, which
        //    is the same bottom-left origin CIImage uses. If correction fails we
        //    fall back to OCR over the original image (still honest: a document was
        //    detected, we just could not flatten it).
        let ciInput = CIImage(cgImage: image)
        let extent = ciInput.extent
        func pointFor(_ normalized: CGPoint) -> CIVector {
            CIVector(x: extent.origin.x + normalized.x * extent.width,
                     y: extent.origin.y + normalized.y * extent.height)
        }
        let corrected: CIImage
        if let filter = CIFilter(name: "CIPerspectiveCorrection") {
            filter.setValue(ciInput, forKey: kCIInputImageKey)
            filter.setValue(pointFor(obs.topLeft), forKey: "inputTopLeft")
            filter.setValue(pointFor(obs.topRight), forKey: "inputTopRight")
            filter.setValue(pointFor(obs.bottomLeft), forKey: "inputBottomLeft")
            filter.setValue(pointFor(obs.bottomRight), forKey: "inputBottomRight")
            corrected = filter.outputImage ?? ciInput
        } else {
            corrected = ciInput
        }

        // 3. Render the corrected CIImage to a CGImage for the OCR handler. A
        //    render failure honestly degrades to OCR over the original image.
        let ocrSource: VNImageRequestHandler
        let ciCtx = CIContext(options: nil)
        if let correctedCG = ciCtx.createCGImage(corrected, from: corrected.extent) {
            ocrSource = VNImageRequestHandler(cgImage: correctedCG, options: [:])
        } else {
            ocrSource = VNImageRequestHandler(cgImage: image, options: [:])
        }

        // 4. OCR the corrected page with the same .accurate + language-correction
        //    recognizer the screen/handwriting paths use.
        let textReq = VNRecognizeTextRequest()
        textReq.recognitionLevel = .accurate
        textReq.usesLanguageCorrection = true
        textReq.recognitionLanguages = recognitionLanguages
        if minimumTextHeight > 0 { textReq.minimumTextHeight = minimumTextHeight }
        do {
            try ocrSource.perform([textReq])
        } catch {
            // A document WAS detected but OCR failed: honest — detected, no lines.
            return DocumentScan(documentDetected: true, lines: [], quadConfidence: quadConfidence)
        }
        let lines = mapText(textReq, floor: floor)
        return DocumentScan(documentDetected: true, lines: lines, quadConfidence: quadConfidence)
    }

    /// Headless document-scan entry from a file path. Returns the honest `.none`
    /// (never throws) if the file can't be decoded. Backs the `vision scan
    /// <imagepath>` CLI.
    public func scanDocument(imagePath path: String, minConfidence: Double = 0.0) -> DocumentScan {
        guard let image = Self.loadCGImage(path: path) else { return .none }
        return scanDocument(image: image, minConfidence: minConfidence)
    }

    // --- Core run ----------------------------------------------------------

    private func run(handler: VNImageRequestHandler,
                     detectors: DetectorSet,
                     minConfidence: Double) -> [Detection] {
        runTimed(handler: handler, detectors: detectors, minConfidence: minConfidence).detections
    }

    /// Core run that also returns the REAL elapsed inference time in ms, measured
    /// with a monotonic clock (`DispatchTime.now()`) strictly around the actual
    /// `handler.perform(requests)` call — the on-device Vision inference. The
    /// timer brackets ONLY the inference, not request construction or result
    /// mapping, so the value is the genuine per-frame inference latency. When no
    /// requests are built (empty detector set) nothing runs and the time is 0.
    private func runTimed(handler: VNImageRequestHandler,
                          detectors: DetectorSet,
                          minConfidence: Double) -> (detections: [Detection], inferenceMs: Double) {
        let floor = max(0.0, min(1.0, minConfidence))
        var requests: [VNRequest] = []

        // Build only the requested built-in requests.
        let humanReq: VNDetectHumanRectanglesRequest?
        if detectors.contains(.humans) {
            let r = VNDetectHumanRectanglesRequest()
            // Upper-body too, so a partial person still registers as presence.
            if #available(macOS 12.0, *) { r.upperBodyOnly = false }
            humanReq = r
            requests.append(r)
        } else { humanReq = nil }

        let animalReq: VNRecognizeAnimalsRequest?
        if detectors.contains(.animals) {
            let r = VNRecognizeAnimalsRequest()
            animalReq = r
            requests.append(r)
        } else { animalReq = nil }

        let classifyReq: VNClassifyImageRequest?
        if detectors.contains(.classification) {
            let r = VNClassifyImageRequest()
            classifyReq = r
            requests.append(r)
        } else { classifyReq = nil }

        let saliencyReq: VNGenerateObjectnessBasedSaliencyImageRequest?
        if detectors.contains(.saliency) {
            let r = VNGenerateObjectnessBasedSaliencyImageRequest()
            saliencyReq = r
            requests.append(r)
        } else { saliencyReq = nil }

        // OCR: read TEXT GLYPHS with the built-in recognizer. .accurate +
        // language correction is the state-of-the-art on-device setting; the
        // explicit language list keeps it deterministic + offline. DEFENSIVE:
        // this reads glyphs, NOT a face/identity.
        let textReq: VNRecognizeTextRequest?
        if detectors.contains(.text) {
            let r = VNRecognizeTextRequest()
            r.recognitionLevel = .accurate
            r.usesLanguageCorrection = true
            r.recognitionLanguages = recognitionLanguages
            if minimumTextHeight > 0 { r.minimumTextHeight = minimumTextHeight }
            textReq = r
            requests.append(r)
        } else { textReq = nil }

        guard !requests.isEmpty else { return ([], 0) }

        // Perform all requested built-ins in one handler pass. A failure of the
        // whole batch is recoverable -> [] (callers/tests expect non-throwing).
        // MEASURE: bracket ONLY the inference call with a monotonic clock so the
        // reported ms is the genuine on-device inference latency for this frame.
        let t0 = DispatchTime.now().uptimeNanoseconds
        do {
            try handler.perform(requests)
        } catch {
            return ([], 0)
        }
        let t1 = DispatchTime.now().uptimeNanoseconds
        let inferenceMs = Double(t1 &- t0) / 1_000_000.0

        var detections: [Detection] = []
        detections += Self.mapHumans(humanReq, floor: floor)
        detections += Self.mapAnimals(animalReq, floor: floor)
        detections += mapClassifications(classifyReq, floor: floor)
        detections += mapSaliency(saliencyReq, floor: floor)
        detections += mapText(textReq, floor: floor)
        return (detections, inferenceMs)
    }

    // --- Result mappers (each total; a nil request -> []) ------------------

    private static func mapHumans(_ req: VNDetectHumanRectanglesRequest?, floor: Double) -> [Detection] {
        guard let observations = req?.results else { return [] }
        return observations.compactMap { obs in
            let conf = Double(obs.confidence)
            guard conf >= floor else { return nil }
            // Bare rectangle, EMPTY label — presence, never identity.
            return Detection(kind: .human,
                             boundingBox: DetectionBox(cgRect: obs.boundingBox),
                             confidence: conf,
                             label: "")
        }
    }

    private static func mapAnimals(_ req: VNRecognizeAnimalsRequest?, floor: Double) -> [Detection] {
        guard let observations = req?.results else { return [] }
        var out: [Detection] = []
        for obs in observations {
            // Highest-confidence animal label on this observation (cat/dog).
            let top = obs.labels.max(by: { $0.confidence < $1.confidence })
            let conf = Double(top?.confidence ?? obs.confidence)
            guard conf >= floor else { continue }
            out.append(Detection(kind: .animal,
                                 boundingBox: DetectionBox(cgRect: obs.boundingBox),
                                 confidence: conf,
                                 label: top?.identifier ?? ""))
        }
        return out
    }

    private func mapClassifications(_ req: VNClassifyImageRequest?, floor: Double) -> [Detection] {
        guard let observations = req?.results, maxClassifications > 0 else { return [] }
        // Built-in classifier yields ~1000+ classes; surface the top-N over the
        // floor. Whole-frame, so the box is the full frame (no spatial extent).
        let top = observations
            .filter { Double($0.confidence) >= floor }
            .sorted { $0.confidence > $1.confidence }
            .prefix(maxClassifications)
        return top.map { obs in
            Detection(kind: .object,
                      boundingBox: .full,
                      confidence: Double(obs.confidence),
                      label: obs.identifier)
        }
    }

    private func mapSaliency(_ req: VNGenerateObjectnessBasedSaliencyImageRequest?, floor: Double) -> [Detection] {
        guard let observations = req?.results, maxSalientRegions > 0 else { return [] }
        var out: [Detection] = []
        for obs in observations {
            guard let salientObjects = obs.salientObjects else { continue }
            for region in salientObjects.prefix(maxSalientRegions) {
                let conf = Double(region.confidence)
                guard conf >= floor else { continue }
                out.append(Detection(kind: .salientRegion,
                                     boundingBox: DetectionBox(cgRect: region.boundingBox),
                                     confidence: conf,
                                     label: ""))
            }
        }
        return out
    }

    /// Map recognized-text observations -> `.text` Detections. Each observation
    /// is one recognized text block; we take its top candidate (the recognizer's
    /// best read) and carry the STRING in `label`, the per-observation normalized
    /// boundingBox (Vision coords, origin bottom-left), and the candidate's
    /// confidence. DEFENSIVE: this is glyph text, never a face/person id; we
    /// never attach an identity. Capped at `maxTextBlocks`. Blocks are returned
    /// in the recognizer's observation order (the structuring stage re-orders for
    /// reading order); empty candidates are dropped.
    private func mapText(_ req: VNRecognizeTextRequest?, floor: Double) -> [Detection] {
        guard let observations = req?.results, maxTextBlocks > 0 else { return [] }
        var out: [Detection] = []
        for obs in observations.prefix(maxTextBlocks) {
            guard let top = obs.topCandidates(1).first else { continue }
            let conf = Double(top.confidence)
            guard conf >= floor else { continue }
            let string = top.string
            guard !string.isEmpty else { continue }
            out.append(Detection(kind: .text,
                                 boundingBox: DetectionBox(cgRect: obs.boundingBox),
                                 confidence: conf,
                                 label: string))
        }
        return out
    }

    // --- Frame -> request handler ------------------------------------------

    /// Build a `VNImageRequestHandler` from whichever backing the Frame carries.
    /// CVPixelBuffer (live capture) is preferred; falls back to CGImage
    /// (file/synthesized). Returns nil if the Frame has neither.
    static func makeHandler(for frame: Frame) -> VNImageRequestHandler? {
        if let pb = frame.pixelBuffer {
            return VNImageRequestHandler(cvPixelBuffer: pb, options: [:])
        }
        if let img = frame.cgImage {
            return VNImageRequestHandler(cgImage: img, options: [:])
        }
        return nil
    }

    /// Decode an image file (PNG/JPEG/etc.) into a CGImage via ImageIO. Offline,
    /// no network. Returns nil on any failure (missing/corrupt/unsupported).
    static func loadCGImage(path: String) -> CGImage? {
        let url = URL(fileURLWithPath: path)
        guard let src = CGImageSourceCreateWithURL(url as CFURL, nil),
              CGImageSourceGetCount(src) > 0,
              let img = CGImageSourceCreateImageAtIndex(src, 0, nil)
        else { return nil }
        return img
    }

    /// Normalize whichever backing a Frame carries into a CGImage suitable for
    /// PNG encoding. A CGImage backing (file/synthesized path) is returned
    /// directly; a CVPixelBuffer backing (live camera/screen capture) is rendered
    /// to a CGImage via a CoreImage context (on-device, no network). Returns nil if
    /// the Frame has neither backing or the render fails. Used by describe.capture
    /// to write a captured frame to a confined PNG for the host's on-device VLM.
    static func encodableCGImage(for frame: Frame) -> CGImage? {
        if let img = frame.cgImage { return img }
        if let pb = frame.pixelBuffer {
            let ci = CIImage(cvPixelBuffer: pb)
            let ctx = CIContext(options: nil)
            return ctx.createCGImage(ci, from: ci.extent)
        }
        return nil
    }

    /// Encode a CGImage to a PNG file at `path` via ImageIO. Offline, no network —
    /// the pixels are written to a LOCAL file and never leave the device. The
    /// parent directory is created if needed. Returns true on success. Used by the
    /// describe.capture op to hand a captured frame to the host's on-device VLM as
    /// a confined file. Returns false on any IO/encode failure (so the caller can
    /// report an honest "frame not written" rather than fabricate success).
    static func writeCGImagePNG(_ image: CGImage, to path: String) -> Bool {
        let url = URL(fileURLWithPath: path)
        // Ensure the parent directory exists (the daemon names a state/vision/...
        // path that may not be created yet on first run).
        try? FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        guard let dest = CGImageDestinationCreateWithURL(
            url as CFURL, UTType.png.identifier as CFString, 1, nil) else { return false }
        CGImageDestinationAddImage(dest, image, nil)
        return CGImageDestinationFinalize(dest)
    }
}

// ===========================================================================
// Stub detector (kept for the seam; default wiring uses VisionEngine)
// ===========================================================================

/// Stub detector — returns no detections. Retained as the trivial `Detector`
/// for tests/wiring that want a deterministic no-op; production wiring uses
/// `VisionEngine`.
public struct StubDetector: Detector {
    public init() {}
    public func detect(in frame: Frame, detectors: DetectorSet, minConfidence: Double) -> [Detection] {
        return []
    }
}
