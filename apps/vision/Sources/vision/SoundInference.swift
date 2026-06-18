// SoundInference.swift — AUDIO SCENE understanding (the SoundEngine).
//
// The AUDIO analog of Inference.swift's VisionEngine. Where VisionEngine runs
// Apple's BUILT-IN Vision requests (VNClassifyImageRequest etc.) over a frame,
// SoundEngine runs Apple's BUILT-IN Sound Analysis request
// (SNClassifySoundRequest with SNClassifierIdentifier.version1, the ~300-class
// on-device classifier) over an AUDIO CLIP/BUFFER -> [SoundClass{label,
// confidence}]. Built-in only — NO external model download (fully offline).
//
// COMPUTE / ANE: SNClassifySoundRequest is ANE/GPU-eligible — Apple schedules
// the execution unit and exposes no residency readout, so this is preferred
// placement, not an observed fact (same honesty caveat as VisionEngine).
//
// DEFENSIVE / PRIVACY: ONLY the derived sound-class LABELS (+ confidence) ever
// leave this engine — the AUDIO ITSELF NEVER LEAVES the device. There is
// deliberately NO code path that returns/serializes audio samples. This is
// audio SCENE understanding (dog bark, doorbell, alarm, music), DISTINCT from
// STT (speech-to-text): SNClassifySoundRequest does NOT transcribe speech, it
// classifies the SOUND. The classifier knows a FIXED ~300-class vocabulary —
// it does NOT "recognize any sound".
//
// HONESTY — HEADLESS PROOF: the established Vision pattern proved
// VNClassifyImageRequest/VNRecognizeTextRequest over a SYNTHESIZED in-memory
// image with NO camera/display/TCC. The audio analog: `classify(buffers:)`
// runs the REAL SNClassifySoundRequest over SYNTHESIZED in-memory PCM buffers
// (a tone) via SNAudioStreamAnalyzer — verified to return real classifications
// headlessly with NO microphone, NO TCC, NO continuous capture, NO network.
// (The classifier window is ~3s; a clip must be at least that long to score.)
// CONTINUOUS LIVE MIC MONITORING + TCC consent are DEVICE-GATED and are NOT
// exercised here — the proof covers the classifier over a synthetic clip, not
// live ambient capture.

import Foundation
import AVFoundation
import SoundAnalysis

// ===========================================================================
// SoundClass — one classified sound (the audio analog of a Detection.object)
// ===========================================================================

/// One classified sound: a coarse model-provided CLASS string from the built-in
/// ~300-class classifier (e.g. "dog_bark", "doorbell", "music") and a confidence
/// in 0...1. A generic acoustic category, NEVER an identity and NEVER speech
/// content — SNClassifySoundRequest classifies the SOUND, it does not transcribe.
/// Codable/Sendable so the label crosses the module + wire boundary; the AUDIO
/// never does.
public struct SoundClass: Sendable, Codable, Equatable {
    /// The built-in classifier's class identifier (a fixed-vocabulary acoustic
    /// category, never an identity, never transcribed words).
    public var label: String
    /// Confidence in 0...1.
    public var confidence: Double

    public init(label: String, confidence: Double) {
        self.label = label
        self.confidence = confidence
    }
}

// ===========================================================================
// SoundClassifier protocol — the seam (mirrors `Detector`)
// ===========================================================================

/// Runs the built-in Sound Analysis classifier over an audio clip and returns
/// the top sound classes. Errors are recoverable (a bad clip yields []), so the
/// seam is non-throwing and total — mirroring `Detector.detect`.
public protocol SoundClassifier: Sendable {
    /// Classify in-memory PCM buffers (the daemon's captured clip). `minConfidence`
    /// gates results (0...1). Returns the top sound classes over the clip.
    func classify(buffers: [AVAudioPCMBuffer], minConfidence: Double) -> [SoundClass]

    /// Classify the audio clip at `path` (a wav/audio file the host wrote from its
    /// captured buffer). The seam the Pipeline's classify.sound op drives. The
    /// default DECODES the file to PCM buffers locally and delegates to
    /// `classify(buffers:)` — the AUDIO never leaves the device; only labels are
    /// returned. Tests may override with a canned-by-path stub (no real decode).
    func classify(audioClipPath path: String, minConfidence: Double) -> [SoundClass]
}

extension SoundClassifier {
    /// Default: decode the clip to PCM buffers and run `classify(buffers:)`.
    /// Returns [] (never throws) on a missing/corrupt/unsupported clip — the
    /// op then emits an honest "nothing classified" rather than fake labels.
    public func classify(audioClipPath path: String, minConfidence: Double) -> [SoundClass] {
        guard let buffers = SoundEngine.decodeFile(path: path), !buffers.isEmpty else { return [] }
        return classify(buffers: buffers, minConfidence: minConfidence)
    }
}

// ===========================================================================
// SoundEngine — the real built-in SNClassifySoundRequest classifier
// ===========================================================================

/// The production `SoundClassifier`: runs Apple's BUILT-IN SNClassifySoundRequest
/// (SNClassifierIdentifier.version1) over an audio clip via SNAudioStreamAnalyzer
/// (ANE/GPU-eligible; Apple picks the unit) and maps results into [SoundClass].
/// Stateless + `Sendable` (each call builds its own analyzer/observer), safe to
/// share. Offline: zero external model downloads. ONLY labels are returned — the
/// audio never leaves this engine.
public struct SoundEngine: SoundClassifier {

    /// How many top sound classes to surface per clip.
    public let maxClasses: Int

    /// The compute-unit tag reported on telemetry ("all" = ANE/GPU eligible).
    /// Mirrors VisionEngine.computeUnitTag (Apple schedules the unit; this is the
    /// requested/preferred placement, not an observed residency fact).
    public static let computeUnitTag = "all"

    /// The built-in classifier identifier string, surfaced for honesty in
    /// telemetry/CLI so the consumer knows EXACTLY which fixed-vocabulary
    /// classifier produced the labels (the ~300-class version1, not "any sound").
    public static let classifierTag = "SNClassifierIdentifier.version1"

    public init(maxClasses: Int = 5) {
        self.maxClasses = max(0, maxClasses)
    }

    // --- SoundClassifier seam ----------------------------------------------

    /// Classify the concatenation of in-memory PCM buffers. Builds a fresh
    /// SNAudioStreamAnalyzer for the (first buffer's) format, adds the built-in
    /// SNClassifySoundRequest, feeds every buffer with a running frame position,
    /// completes the analysis, and merges the per-window classification results.
    /// Total + non-throwing: any failure (empty clip, bad format, request build
    /// failure) yields []. ONLY labels/confidence are returned; the audio buffers
    /// are read and discarded — never serialized, never returned, never emitted.
    public func classify(buffers: [AVAudioPCMBuffer], minConfidence: Double) -> [SoundClass] {
        let floor = max(0.0, min(1.0, minConfidence))
        guard maxClasses > 0, let first = buffers.first else { return [] }
        let format = first.format

        guard let request = try? SNClassifySoundRequest(classifierIdentifier: .version1) else {
            // The built-in classifier could not be constructed (unavailable in
            // this OS build). Honest: nothing classified rather than fake labels.
            return []
        }
        let analyzer = SNAudioStreamAnalyzer(format: format)
        let observer = SoundResultCollector(maxClasses: maxClasses, floor: floor)
        do {
            try analyzer.add(request, withObserver: observer)
        } catch {
            return []
        }

        // Feed every buffer with a CONTIGUOUS running frame position. The window
        // (~3s) slides over the accumulated audio; short clips below the window
        // may produce no result (honest — see the engine doc + the headless
        // proof, which uses a clip longer than the window).
        var position: AVAudioFramePosition = 0
        for buffer in buffers {
            // Only feed buffers that share the analyzer's format (mixed formats
            // would corrupt the stream); skip mismatches rather than throw.
            guard buffer.format.sampleRate == format.sampleRate,
                  buffer.format.channelCount == format.channelCount else { continue }
            analyzer.analyze(buffer, atAudioFramePosition: position)
            position += AVAudioFramePosition(buffer.frameLength)
        }
        analyzer.completeAnalysis()

        return observer.topClasses()
    }

    /// Convenience: classify a SINGLE in-memory PCM buffer (e.g. a daemon clip
    /// already coalesced into one buffer).
    public func classify(buffer: AVAudioPCMBuffer, minConfidence: Double = 0.0) -> [SoundClass] {
        classify(buffers: [buffer], minConfidence: minConfidence)
    }

    // --- WAV/audio file decode (-> the SAME proven stream engine) -----------
    //
    // The audio-file entry point is the protocol default `classify(audioClipPath:
    // minConfidence:)` (in the SoundClassifier extension): it decodes the file to
    // PCM buffers via `decodeFile` below and feeds the SAME SNAudioStreamAnalyzer
    // path as the in-memory clip. We deliberately do NOT use SNAudioFileAnalyzer
    // (it did not run headlessly in this build env during de-risk; the stream
    // path did). The file is decoded LOCALLY and the AUDIO never leaves the
    // device; only labels are returned. Backs the `vision classify-sound
    // <audiopath>` CLI mode + the Pipeline's classify.sound op.

    /// Decode an audio file into a list of PCM buffers (chunked). On-device,
    /// offline. Returns nil on any failure. The decoded samples stay in-process
    /// (handed to the classifier and discarded) — they are never serialized out.
    static func decodeFile(path: String, chunkFrames: AVAudioFrameCount = 16000) -> [AVAudioPCMBuffer]? {
        let url = URL(fileURLWithPath: path)
        guard let file = try? AVAudioFile(forReading: url) else { return nil }
        let format = file.processingFormat
        let total = file.length
        guard total > 0 else { return nil }
        var out: [AVAudioPCMBuffer] = []
        var read: AVAudioFramePosition = 0
        while read < total {
            let remaining = AVAudioFrameCount(min(Int64(chunkFrames), total - read))
            guard let buf = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: remaining) else { break }
            do {
                try file.read(into: buf, frameCount: remaining)
            } catch {
                break
            }
            if buf.frameLength == 0 { break }
            out.append(buf)
            read += AVAudioFramePosition(buf.frameLength)
        }
        return out.isEmpty ? nil : out
    }

    // --- Synthesized-tone clip (the headless-proof + test fixture) ----------

    /// Synthesize an in-memory mono PCM clip of a pure sine tone — the AUDIO
    /// analog of VisionEngine's synthesized CGImage. Used by the headless proof
    /// (and any caller) to exercise the REAL classifier with NO microphone, NO
    /// TCC, NO continuous capture. `seconds` should exceed the classifier window
    /// (~3s) so a window actually scores. Returns one buffer (the whole clip).
    public static func synthesizeToneClip(frequency: Double = 440.0,
                                          seconds: Double = 4.0,
                                          sampleRate: Double = 16000.0,
                                          amplitude: Float = 0.6) -> AVAudioPCMBuffer? {
        guard seconds > 0, sampleRate > 0,
              let format = AVAudioFormat(standardFormatWithSampleRate: sampleRate, channels: 1)
        else { return nil }
        let frames = AVAudioFrameCount(max(1.0, sampleRate * seconds))
        guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frames),
              let channel = buffer.floatChannelData?[0] else { return nil }
        buffer.frameLength = frames
        let dt = 1.0 / sampleRate
        var t = 0.0
        for i in 0..<Int(frames) {
            channel[i] = amplitude * Float(sin(2.0 * Double.pi * frequency * t))
            t += dt
        }
        return buffer
    }
}

// ===========================================================================
// SoundResultCollector — SNResultsObserving sink that merges window results
// ===========================================================================

/// Collects SNClassificationResult callbacks from the analyzer and merges them
/// into the top sound classes over the whole clip. The classifier emits one
/// result per analysis window; for each class label we keep the HIGHEST
/// confidence across windows, then return the top-N over the floor. `@unchecked
/// Sendable`: it is only mutated on the analyzer's callback queue and read after
/// `completeAnalysis()` (synchronous in this build), guarded by a lock so the
/// read is safe. ONLY labels/confidence are retained — never audio.
final class SoundResultCollector: NSObject, SNResultsObserving, @unchecked Sendable {
    private let maxClasses: Int
    private let floor: Double
    private let lock = NSLock()
    /// label -> highest confidence seen across windows.
    private var best: [String: Double] = [:]

    init(maxClasses: Int, floor: Double) {
        self.maxClasses = maxClasses
        self.floor = floor
    }

    func request(_ request: SNRequest, didProduce result: SNResult) {
        guard let classification = result as? SNClassificationResult else { return }
        lock.lock(); defer { lock.unlock() }
        for c in classification.classifications {
            let conf = Double(c.confidence)
            if let existing = best[c.identifier], existing >= conf { continue }
            best[c.identifier] = conf
        }
    }

    func request(_ request: SNRequest, didFailWithError error: Error) {
        // Recoverable: a failed window leaves whatever earlier windows produced.
    }

    func requestDidComplete(_ request: SNRequest) {}

    /// The top sound classes over the clip, gated by the floor and capped at
    /// maxClasses, sorted by descending confidence then label (deterministic).
    func topClasses() -> [SoundClass] {
        lock.lock(); defer { lock.unlock() }
        return best
            .filter { $0.value >= floor }
            .map { SoundClass(label: $0.key, confidence: $0.value) }
            .sorted { a, b in
                if a.confidence != b.confidence { return a.confidence > b.confidence }
                return a.label < b.label
            }
            .prefix(maxClasses)
            .map { $0 }
    }
}

// ===========================================================================
// Stub classifier (kept for the seam; default wiring uses SoundEngine)
// ===========================================================================

/// Stub classifier — returns no sound classes. Retained as the trivial
/// `SoundClassifier` for tests/wiring that want a deterministic no-op.
public struct StubSoundClassifier: SoundClassifier {
    public init() {}
    public func classify(buffers: [AVAudioPCMBuffer], minConfidence: Double) -> [SoundClass] {
        return []
    }
}
