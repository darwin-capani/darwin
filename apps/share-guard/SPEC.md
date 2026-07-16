# Share Guard — SPEC

An on-device PII **auto-redactor** you run **before sharing** an artifact. It takes an artifact id (or a text/image payload) from the Artifact Registry (`daemon/src/artifact.rs`), detects PII spans in the text, composes a marked-up redaction, and writes a **redacted copy inside its own sandbox dir** — never the user's original. Phase-4 implementation against `docs/SANDBOX.md`; HUD panel contract per `docs/HUD.md`.

## Honesty contract (LOAD-BEARING)

- **On-device + offline.** Built-in Apple `VNRecognizeTextRequest` OCR for the image path (the same engine the Vision app uses); `net_hosts = []`. The payload, the recognized glyphs, and the redacted copy never leave the device.
- **Glyph text only.** OCR reads text glyphs; it is never turned into a face / person identity. There is no identity path.
- **Best-effort, NOT a guarantee.** Redaction is text-pattern detection (emails / phone numbers / Luhn-valid card & account numbers). Unusual formats will be missed — the preview says so, and the copy must be reviewed before sharing.
- **Writes only its own sandbox dir.** The redacted copy is written under `state/tmp/share-guard/redacted/` (the manifest `fs_write` grant). The user's original file is never touched.
- **DARWIN cannot send.** Share Guard produces a scrubbed COPY; the **user** shares it themselves. There is no egress on this path.
- **Secret-free telemetry.** The `share.redactions` event carries per-kind counts, a bounded best-effort preview, and the sandbox-relative output path — **never** the raw PII and **never** the full redacted body (that lives in the sandbox copy).

## Sandbox contract (binding: `manifest.toml`)

- Runtime `binary` (a prebuilt Swift binary). `entry = apps/share-guard/.build/release/share-guard` (project-root-relative, inside the app dir — required by `AppRegistry::discover`).
- Opens **no device**: `audio = false`, `camera = false`, `screen = false`. It scrubs a supplied payload, not live capture.
- `gpu = true`: ANE/GPU for the built-in OCR request. `net_hosts = []` — fully offline.
- `fs_read = ["state/tmp/share-guard/input"]` — the host stages a to-be-scrubbed **image** payload here (`scrub.image`).
- `fs_write = ["state/tmp/share-guard"]` — the app's own sandbox dir; redacted copies land under `redacted/`.
- IPC: JSONL over `state/ipc/apps/share-guard.sock`, capability token in every app→host line.
- UI: `surface = "panel"`. Telemetry topics: `share.redactions`, `share.status`, `share.error`.

## 1. The pure seam (unit-tested)

The redaction decision is a **pure, value-transform seam** — no OCR, no capture, no socket, no filesystem — so it is exhaustively unit-testable:

- **`PIIDetector.detect(in:)`** → `[PIISpan]`. Finds:
  - **email** — `local@domain.tld`.
  - **phone** — a **10–12** digit run (NANP ± country code), typically with `+`, spaces, dashes, dots, parens.
  - **card / long account** — a **13–19** digit run that **passes a Luhn check**. The Luhn gate is what separates a real card/account number from an arbitrary long number.
  - **Non-over-masking (the load-bearing property):** a 13–19 digit run that **fails** Luhn is **not** masked; phone is capped strictly **below** the card band so a long non-card number never falls through to a phone mask; runs `< 10` digits (dates, house numbers, extensions, SSN-length runs) are below the phone floor and are left alone; number spans overlapping an email are dropped.
- **`Redaction.compose(text:spans:mask:)`** → `ScrubResult`. Splices each detected span out and inserts its mask marker (`[EMAIL REDACTED]` etc.), copying the between-span text verbatim. `ScrubResult` carries the redacted copy, per-kind counts, and a secret-free `preview()`.
- **`ShareGuard.scrub(text:)`** = detect + compose — the one call both the text payload and the OCR output funnel through.

## 2. The device-gated runner (NOT unit-tested)

`OCRTextRecognizer` (in `OCR.swift`, under `#if canImport(Vision)`) runs the built-in `VNRecognizeTextRequest` (`.accurate` + language correction, explicit offline language list) over a supplied image and returns the recognized lines in reading order. This is the one impure seam; its recognition **quality is device/Vision-model-dependent**, so it is exercised only through the `share-guard scrub-image` CLI mode and the daemon-launched `scrub.image` op — **never** in `swift test`. The pure scrub seam it feeds IS fully tested.

## 3. IPC ops (JSONL, token-bearing)

| op | request | effect |
|---|---|---|
| `scrub.text` | `{text, artifact_id?}` | Redact PII in a text payload; write the redacted copy to the sandbox; emit `share.redactions`. |
| `scrub.image` | `{path, artifact_id?}` | Confine `path` to the input dir, OCR it on-device, redact the recognized text, write the copy; emit `share.redactions`. |
| `status` | `{}` | Emit a `share.status` snapshot. |

**Artifact Registry integration (daemon-side, deferred).** "Scrub artifact `<id>` before I share it" is resolved by the daemon: it reads the `ArtifactRef` by id (`artifact::peek`), forwards its text as `scrub.text` (or stages its image under `state/tmp/share-guard/input/` and forwards `scrub.image`), and passes the artifact id along in `artifact_id` for HUD correlation. The app **never** reaches into the registry — keeping it sandbox-honest — and tags its `share.redactions` readout with the id it was given.

## 4. Telemetry → HUD panel

| Topic | Payload |
|---|---|
| `share.redactions` | `{total, found_pii, by_kind:{email,phone,card}, preview, original_length, output, artifact_id?}` — **secret-free** (no PII, no redacted body) |
| `share.status` | `{state: idle\|scrubbing\|stopped, message?}` |
| `share.error` | `{code, message}` |

Panel: the redaction summary (counts by kind), the best-effort preview line, and the sandbox-relative output path the user opens to share. Rendered HUD-side from these payloads; Share Guard ships no UI code.

## 5. Headless verification

`cd apps/share-guard && swift build && swift test` — the pure detector/redaction/confinement/op/env/event tests run headlessly (no OCR, no capture, no socket). The live OCR is proven separately via `share-guard scrub-image <path>` on a device.

Non-goals: sending/sharing (DARWIN cannot send — the user shares the copy), SSN / passport / other id classes (out of the initial email/phone/card scope), image redaction of pixels (this produces a redacted **text** copy of the recognized content, not a pixel-masked image).
