/**
 * SYSTEM ACCESS — the macOS Privacy & Security permissions JARVIS uses, plus the
 * honest contract around (re-)requesting them.
 *
 * HONESTY CONTRACT (do not regress): macOS does NOT let any app grant itself a
 * TCC permission — Full Disk Access, Accessibility, Screen Recording, the
 * Microphone, etc. are a hard security boundary with NO programmatic grant. The
 * MOST an app may do is (1) DEEP-LINK the user to the exact Privacy pane and
 * (2) trigger the native PROMPT for the few categories that have one, the first
 * time it actually uses them. So this surface OPENS the right pane and explains
 * each permission — the user flips the switch. We NEVER imply JARVIS can grant
 * itself anything; "re-request" means re-open the pane / re-trigger the prompt.
 *
 * Each pane KEY below MUST exist in the Rust allowlist (hud/src-tauri/src/
 * permissions.rs → PRIVACY_PANES). The backend maps the key to a FIXED
 * `x-apple.systempreferences:` anchor and shells `open` itself, so the frontend
 * can never open an arbitrary URL — an unknown key is rejected with no shell-out.
 * The `anchor` field here is a documentation mirror of the Rust map; the key set
 * is locked on BOTH sides (system-access.test.ts here + a Rust test there) so a
 * drift on either end fails CI.
 *
 * Anchors verified live on macOS 26.5.1 (Tahoe): the classic
 * `com.apple.preference.security?Privacy_*` scheme still resolves to each pane.
 */
export interface PermissionPane {
  /** Stable key sent to the backend; MUST match the Rust allowlist. */
  key: string;
  /** The macOS Privacy anchor (documentation mirror of the Rust map). */
  anchor: string;
  /** Pane name exactly as it reads in System Settings. */
  label: string;
  /** Plain-English reason JARVIS uses it. */
  why: string;
}

export const PERMISSION_PANES: PermissionPane[] = [
  {
    key: "full_disk",
    anchor: "Privacy_AllFiles",
    label: "Full Disk Access",
    why: "Read and organize files across your whole Mac — long-term memory, on-device document search, and the file actions you ask for.",
  },
  {
    key: "accessibility",
    anchor: "Privacy_Accessibility",
    label: "Accessibility",
    why: "Control your Mac — click, type, and drive other apps when you ask JARVIS to act for you.",
  },
  {
    key: "screen",
    anchor: "Privacy_ScreenCapture",
    label: "Screen & System Audio Recording",
    why: "See and read what's on your screen — “what am I looking at”, screen understanding, and visual help.",
  },
  {
    key: "microphone",
    anchor: "Privacy_Microphone",
    label: "Microphone",
    why: "Hear you — the wake word, dictation, and the on-device voice match.",
  },
  {
    key: "input_monitoring",
    anchor: "Privacy_ListenEvent",
    label: "Input Monitoring",
    why: "Catch the wake word or a hotkey from any app, so “Jarvis…” works anywhere.",
  },
  {
    key: "automation",
    anchor: "Privacy_Automation",
    label: "Automation",
    why: "Drive other apps through Apple Events — open a Terminal, run a script, automate a workflow.",
  },
  {
    key: "camera",
    anchor: "Privacy_Camera",
    label: "Camera",
    why: "On-device vision when you ask JARVIS to watch the camera. Stays off until you ask.",
  },
];

/** The exact key set, locked on BOTH sides (this file's test + a Rust test) so a
 *  drift on either end fails CI. */
export const PERMISSION_KEYS: string[] = PERMISSION_PANES.map((p) => p.key);

export const PERMISSIONS_COPY = {
  title: "SYSTEM ACCESS // macOS PERMISSIONS",
  lede:
    "JARVIS is most capable with broad access to your Mac. macOS keeps these behind Privacy & Security — and for your safety, NO app (JARVIS included) can switch them on for itself. What JARVIS can do is take you straight to the right switch.",
  howTitle: "How this works",
  how: [
    "Click a permission to open its exact pane in System Settings.",
    "Turn JARVIS on there. If JARVIS isn't listed yet, click the + to add it — apps appear once they first ask for that access.",
    "Microphone, Screen Recording, and Accessibility also show a one-time prompt the first time JARVIS uses them; this panel re-opens the pane so you can change your answer afterward.",
  ],
  requestAll: "RE-REQUEST ALL PERMISSIONS",
  requestAllHint:
    "Opens System Settings → Privacy & Security, where every category is listed. Use it any time access stops working, or after a macOS update resets a permission.",
  footnote:
    "This is the maximum an app is allowed to do — open the panes and explain them honestly. The switches are always yours to flip, and you can revoke any of them in the same place at any time.",
} as const;
