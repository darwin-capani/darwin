// Frontmost.swift — AX-FREE frontmost app/window attribution for screen reads.
//
// SCREEN GROUNDING: a screen OCR snapshot is far more useful when it knows
// WHICH app/window it came from ("what was that error in the terminal"). This
// reader supplies that attribution WITHOUT any new permission:
//   * the APP comes from NSWorkspace.frontmostApplication — plain AppKit, no
//     TCC of any kind;
//   * the WINDOW TITLE comes from ScreenCaptureKit's SCShareableContent — the
//     SAME Screen-Recording consent the capture itself already holds (if we may
//     see the pixels, we may see the title). NO Accessibility TCC, no
//     AXUIElement — the repo's read-only/no-actuator stance is untouched.
//
// HONESTY: every failure degrades to ABSENCE (nil), never a fabricated
// attribution — headless, unauthorized, or no frontmost app all yield nil, and
// a missing title yields app-only. The provider seam on Pipeline defaults to
// nil (tests stay hermetic; the production socket path wires the real reader,
// mirroring the FrameSource factory discipline).

import Foundation
#if os(macOS)
import AppKit
import ScreenCaptureKit
#endif

/// The frontmost app (+ its focused window's title, when known) at one instant.
public struct FrontmostWindow: Sendable, Equatable {
    /// The frontmost application's user-visible name (e.g. "Terminal").
    public let app: String
    /// The frontmost layer-0 window title of that app, when one was readable
    /// under the existing Screen-Recording consent. nil = honestly unknown.
    public let window: String?

    public init(app: String, window: String?) {
        self.app = app
        self.window = window
    }
}

/// The production frontmost reader. DEVICE-gated by construction: off macOS it
/// returns nil, and on macOS every failure path returns nil/absence.
public enum FrontmostReader {
    public static func read() async -> FrontmostWindow? {
        #if os(macOS)
        guard let front = NSWorkspace.shared.frontmostApplication,
              let name = front.localizedName, !name.isEmpty
        else { return nil }
        var title: String? = nil
        // Title via the SAME Screen-Recording consent the capture holds. An
        // enumeration failure (no consent / headless) just means app-only.
        if let content = try? await SCShareableContent
            .excludingDesktopWindows(true, onScreenWindowsOnly: true)
        {
            title = content.windows.first(where: { w in
                w.owningApplication?.processID == front.processIdentifier
                    && w.windowLayer == 0
                    && !(w.title ?? "").isEmpty
            })?.title
        }
        return FrontmostWindow(app: name, window: title)
        #else
        return nil
        #endif
    }
}
