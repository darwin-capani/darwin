// App.swift — entry point for the Share Guard micro-app binary (manifest entry =
// "apps/share-guard/.build/release/share-guard"). Mirrors the vision app's App.swift.
//
// NOTE: named App.swift, NOT main.swift — SwiftPM treats a file literally named
// main.swift as top-level code, which collides with @main. The @main async entry
// lives here.
//
// THREE run modes (chosen by argv):
//   1. socket-served runtime (default, no args): the daemon launches us under
//      sandbox-exec with DARWIN_APP_TOKEN / _SOCKET / _NAME in the env. We connect
//      to the per-app Unix socket, pump host Ops (scrub.text / scrub.image /
//      status) into the Pipeline, and emit token-stamped share.* telemetry until
//      the host stops us / the socket closes. This is the path daemon/src/apps.rs
//      drives (after resolving an artifact id -> payload against artifact.rs).
//   2. `share-guard scrub-text <textpath>` — headless CLI: read a text file, run
//      the PURE scrub seam once, print the redacted copy + counts as JSON. NO OCR,
//      NO socket, NO token — the proof the redaction seam is real.
//   3. `share-guard scrub-image <imagepath>` — headless CLI: OCR the image with the
//      built-in on-device VNRecognizeTextRequest, scrub the recognized text, print
//      the result as JSON. Exercises the DEVICE-GATED OCR runner (never run in
//      `swift test`).
//
// SAFETY: this CONNECTS to the daemon socket (binds nothing), opens no window,
// touches no GPU directly, opens no camera/screen/mic. It reads a supplied payload
// and writes a REDACTED COPY only under its own sandbox dir. DARWIN cannot SEND —
// the user shares the scrubbed copy themselves. Redaction is BEST-EFFORT text
// detection, not a guarantee.

import Foundation

@main
struct ShareGuardApp {
    static func main() async {
        let args = CommandLine.arguments

        // 0. Headless CLI modes — NO socket/token.
        if args.count >= 2, args[1] == "scrub-text" {
            exit(runScrubTextCLI(args: Array(args.dropFirst(2))))
        }
        if args.count >= 2, args[1] == "scrub-image" {
            exit(runScrubImageCLI(args: Array(args.dropFirst(2))))
        }
        if args.count >= 2, ["help", "--help", "-h"].contains(args[1]) {
            printUsage()
            exit(0)
        }

        // 1. socket-served runtime (the daemon-launched path).
        await runSocketServed()
    }

    // -----------------------------------------------------------------------
    // Mode 1 — socket-served runtime.
    // -----------------------------------------------------------------------

    static func runSocketServed() async {
        // Launch env (token + socket). Missing env -> clean non-zero exit.
        let env: AppEnv
        do {
            env = try AppEnv.loadFromProcess()
        } catch {
            FileHandle.standardError.write(Data(
                "share-guard: \(error) (this app runs under darwind, not standalone; or use `share-guard scrub-text <path>`)\n".utf8))
            exit(2)
        }

        // Open the REAL socket connection (split reader/writer over a dup of the fd).
        let connection: AppConnection
        let writer: LineWriter
        #if canImport(Darwin)
        do {
            let conn = try SocketConnection(env: env)
            connection = SocketAppConnection(reader: conn.reader)
            writer = SocketLineWriter(writer: conn.writer)
        } catch {
            FileHandle.standardError.write(Data("share-guard: cannot connect to app socket: \(error)\n".utf8))
            exit(2)
        }
        #else
        connection = StubAppConnection(env: env)
        writer = StubLineWriter()
        #endif

        // Wiring: token-stamping sink -> Pipeline (sandbox writer + injected OCR).
        let sink: EventSink = OutboundSink(token: env.token, writer: writer)
        let sandbox = SandboxRoot(projectRoot: FileManager.default.currentDirectoryPath)
        let pipeline = Pipeline(sink: sink, sandbox: sandbox, recognizeImageText: Self.ocrRunner())

        // Announce we're up (idle).
        await sink.emit(.status(state: .idle, message: "share-guard started"))

        // Pump host ops into the pipeline until the connection closes / we stop.
        do {
            try await connection.run { op in
                await pipeline.handle(op)
            }
        } catch {
            FileHandle.standardError.write(Data("share-guard: connection error: \(error)\n".utf8))
            exit(1)
        }
    }

    /// The injected OCR runner — the built-in on-device VNRecognizeTextRequest path
    /// when Vision is available, else nil (scrub.image then reports OCR unavailable
    /// rather than reaching a device path). DEVICE-GATED; never run in tests.
    static func ocrRunner() -> ImageTextRecognizer? {
        #if canImport(Vision)
        return { path in OCRTextRecognizer().recognizeText(imagePath: path) }
        #else
        return nil
        #endif
    }

    // -----------------------------------------------------------------------
    // Mode 2 — `share-guard scrub-text <textpath>` (headless, pure seam).
    // -----------------------------------------------------------------------

    /// Read a text file and run the PURE scrub seam, printing the redacted copy +
    /// counts as JSON. NO OCR / socket / token. Returns the exit code.
    static func runScrubTextCLI(args: [String]) -> Int32 {
        guard let path = args.first, !path.isEmpty else {
            FileHandle.standardError.write(Data("usage: share-guard scrub-text <textpath>\n".utf8))
            return 2
        }
        guard let text = try? String(contentsOfFile: path, encoding: .utf8) else {
            FileHandle.standardError.write(Data("share-guard: could not read text at \(path)\n".utf8))
            return 3
        }
        let result = ShareGuard.scrub(text: text)
        guard printJSON(encodeResult(result, artifactId: nil, output: nil)) else {
            FileHandle.standardError.write(Data("share-guard: failed to encode result\n".utf8))
            return 4
        }
        return 0
    }

    // -----------------------------------------------------------------------
    // Mode 3 — `share-guard scrub-image <imagepath>` (headless, DEVICE-GATED OCR).
    // -----------------------------------------------------------------------

    /// OCR an image with the built-in on-device recognizer, scrub the recognized
    /// text, and print the result as JSON. Exercises the device-gated OCR runner
    /// (never in `swift test`). Returns the exit code.
    static func runScrubImageCLI(args: [String]) -> Int32 {
        guard let path = args.first, !path.isEmpty else {
            FileHandle.standardError.write(Data("usage: share-guard scrub-image <imagepath>\n".utf8))
            return 2
        }
        #if canImport(Vision)
        guard let text = OCRTextRecognizer().recognizeText(imagePath: path) else {
            FileHandle.standardError.write(Data("share-guard: could not read image at \(path)\n".utf8))
            return 3
        }
        let result = ShareGuard.scrub(text: text)
        guard printJSON(encodeResult(result, artifactId: nil, output: nil)) else {
            FileHandle.standardError.write(Data("share-guard: failed to encode result\n".utf8))
            return 4
        }
        return 0
        #else
        FileHandle.standardError.write(Data("share-guard: OCR (Vision) unavailable on this platform\n".utf8))
        return 5
        #endif
    }

    // -----------------------------------------------------------------------
    // helpers
    // -----------------------------------------------------------------------

    /// Encode a ScrubResult to a SECRET-FREE-ish CLI JSON. NOTE: for the CLI proof
    /// modes the redacted (PII-removed) body IS printed so a human can inspect it —
    /// that is safe (the PII is gone). The daemon TELEMETRY path (share.redactions)
    /// deliberately omits the body and prints only counts + preview + output path.
    static func encodeResult(_ r: ScrubResult, artifactId: String?, output: String?) -> [String: Any] {
        var byKind: [String: Int] = [:]
        for kind in PIIKind.allCases where r.count(kind) > 0 { byKind[kind.rawValue] = r.count(kind) }
        var d: [String: Any] = [
            "topic": ShareTopic.redactions,
            "total": r.total,
            "found_pii": r.foundPII,
            "by_kind": byKind,
            "preview": r.preview(),
            "original_length": r.originalLength,
            "redacted": r.redactedText,
        ]
        if let artifactId { d["artifact_id"] = artifactId }
        if let output { d["output"] = output }
        return d
    }

    static func printUsage() {
        let usage = """
        share-guard — DARWIN on-device PII auto-redactor (defensive, offline).

        Run BEFORE sharing an artifact: it detects PII (emails / phone numbers /
        Luhn-valid card & account numbers) with an on-device text scan and writes a
        REDACTED copy inside its own sandbox dir. DARWIN cannot send — YOU share the
        scrubbed copy. Redaction is BEST-EFFORT text detection, NOT a guarantee;
        review the copy before sharing.

        Run modes:
          share-guard                     socket-served runtime (launched by darwind;
                                          needs DARWIN_APP_TOKEN / _SOCKET / _NAME)
          share-guard scrub-text <path>   headless: redact PII in a text file, print
                                          the redacted copy + counts as JSON
          share-guard scrub-image <path>  headless: OCR an image on-device, redact
                                          PII in the recognized text, print JSON
          share-guard help                this message

        Defensive: on-device only, no upload, no identity recognition (glyph text
        only). Writes ONLY inside the app's sandbox dir, never your original.
        """
        FileHandle.standardError.write(Data((usage + "\n").utf8))
    }

    /// Serialize a payload dict to one JSON line on stdout (sorted keys for
    /// deterministic output). Returns false on a serialization failure.
    @discardableResult
    static func printJSON(_ payload: [String: Any]) -> Bool {
        guard JSONSerialization.isValidJSONObject(payload),
              let data = try? JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys]),
              let s = String(data: data, encoding: .utf8)
        else { return false }
        FileHandle.standardOutput.write(Data((s + "\n").utf8))
        return true
    }
}
