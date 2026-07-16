// swift-tools-version: 6.0
// Share Guard — DARWIN on-device PII auto-redactor micro-app.
//
// Defensive, on-device ONLY. Run BEFORE sharing an artifact: detect PII
// (emails / phone numbers / Luhn-valid card & account numbers) with an on-device
// text scan (the SAME built-in Apple VNRecognizeTextRequest OCR the Vision app
// uses, for the image path) and write a REDACTED copy inside the app's OWN
// sandbox dir. Fully offline (no external model download, no network). DARWIN
// cannot send — the user shares the scrubbed copy themselves.
//
// Target platform: macOS 14+ (modern Vision). swiftc 6.3.x (arm64) verified.

import PackageDescription

let package = Package(
    name: "share-guard",
    platforms: [
        // macOS 14 (Sonoma) floor: the current Vision request set is available and
        // the daemon hosts on macOS 26 (verified in env).
        .macOS(.v14)
    ],
    products: [
        // The micro-app binary the daemon launches (runtime = "binary").
        .executable(name: "share-guard", targets: ["share-guard"])
    ],
    targets: [
        // Single executable target holding all modules (pure detector/redaction
        // seam, sandbox confinement, device-gated OCR runner, ipc, main).
        .executableTarget(
            name: "share-guard",
            path: "Sources/share-guard",
            linkerSettings: [
                .linkedFramework("Vision"),
                .linkedFramework("CoreGraphics"),
                .linkedFramework("ImageIO"),
                .linkedFramework("Foundation"),
                // EMBED Info.plist into the binary's __TEXT,__info_plist section so
                // macOS reads CFBundleDisplayName="D.A.R.W.I.N." + the bundle id for
                // this binary (it opens no TCC-gated device, so it carries no usage
                // strings). SwiftPM resolves this path relative to the PACKAGE ROOT,
                // so it works from apps/share-guard and via `--package-path`.
                .unsafeFlags([
                    "-Xlinker", "-sectcreate",
                    "-Xlinker", "__TEXT",
                    "-Xlinker", "__info_plist",
                    "-Xlinker", "Info.plist",
                ])
            ]
        ),
        // XCTest target. Tests drive the PURE logic: the PII-span detector
        // (email/phone/Luhn-card found + masked; benign text untouched; a partial
        // number not over-masked), the redaction composition, the sandbox write
        // confinement, the op decoder, the launch-env parser, and the event
        // framing — with NO OCR, NO capture, NO socket, NO TCC.
        .testTarget(
            name: "share-guardTests",
            dependencies: ["share-guard"],
            path: "Tests/share-guardTests"
        )
    ]
)
