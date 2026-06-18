# Fab-Link — SPEC

3D-printing telemetry: Moonraker/OctoPrint live data, a toolpath render synced to print progress, thermal and timelapse panels, and a reserved hook for Phase-3 vision-based failure detection. Phase-4 implementation against `docs/SANDBOX.md`; HUD surface contract per `docs/HUD.md` §5. This app is also the worked example in SANDBOX.md.

## Sandbox contract (binding: `manifest.toml`)

- `net_hosts = ["voron.local", "octoprint.local"]` — printer endpoints only.
- `fs_read = apps/fab-link/gcode-previews`, `fs_write = state/tmp/fab-link` (frame cache, parsed-toolpath cache).
- `gpu = false` — Fab-Link does **no GPU rendering**. It parses and publishes geometry; the HUD (which owns the GPU) draws it. This is the honest split for an `overlay`-class surface.
- IPC: JSONL over `state/ipc/apps/fab-link.sock`, capability token per message.
- UI: `surface = "overlay"`. Topics: `fab.progress`, `fab.temps`, `fab.eta`, `fab.alerts`.

## 1. Printer connectivity

**Moonraker (primary, `voron.local`):** WebSocket JSON-RPC at `ws://voron.local:7125/websocket`.

- `printer.objects.subscribe` on: `print_stats` (state, filename, print_duration, filament_used), `virtual_sdcard` (progress, file_position), `extruder`, `heater_bed`, `temperature_sensor chamber`, `display_status`, `toolhead` (position, max velocities).
- Files via the HTTP file API (`/server/files/gcodes/...`) for g-code download; `/server/files/thumbnails` for embedded preview images.
- Reconnect with exponential backoff (1 s → 30 s cap); `klippy_state` transitions surface as alerts.

**OctoPrint (fallback, `octoprint.local`):** push via the SockJS socket when available, else REST polling (`/api/job`, `/api/printer`) at 2 s. API key in `state/tmp/fab-link/octoprint.key` is not acceptable for a secret — key lives in an env var passed by the daemon at launch (same convention as capability tokens).

One internal `PrinterState` model normalizes both backends; everything downstream is backend-agnostic.

## 2. Toolpath render, synced to progress

Fab-Link prepares geometry; the HUD draws it on the overlay layer:

- **Parse** the active g-code (downloaded once per job, cached parsed form in `state/tmp/fab-link/<job-hash>.toolpath`): extrusion moves → polylines bucketed per layer, annotated with feature type when slicer comments allow (`;TYPE:` for PrusaSlicer/Cura, `; FEATURE` for others); travels kept separately (thin, dimmer).
- **Publish** geometry over the overlay surface channel as binary-friendly JSON chunks: `{layer, z, polylines: [[x,y],...], kind}` — streamed layer-by-layer so a 200 MB g-code never serializes at once. The HUD widget registry's `polyline` widget (HUD.md §M4) renders it with GPU line drawing, pan/zoom/rotate handled HUD-side.
- **Progress sync**: `virtual_sdcard.file_position` (byte offset) maps through the parser's byte→segment index; Fab-Link publishes `fab.progress` with `{percent, layer, total_layers, segment}`. The HUD colors completed segments bright (`--holo-bright`), the active layer animated, the remainder dim — the render is always exactly as far along as the printer.

## 3. Thermal panel

- Ring buffers (4 h at 1 Hz) for hotend/bed/chamber actual+target; published on `fab.temps` at 1 Hz as `{hotend: {actual, target}, bed: {...}, chamber: {...}}` plus a 5-minute sparkline array every 30 s.
- Threshold logic → `fab.alerts`: deviation from target > 10 °C for > 30 s (thermal runaway-adjacent, severity high), heater off while printing, chamber over limit. Alerts are `{severity, code, message, ts}`; the HUD overlay shows high-severity as an amber banner.

## 4. ETA + timelapse

- `fab.eta`: blended estimate — slicer estimate (from file metadata) weighted against observed progress rate, published `{eta_iso, remaining_s, basis: "blend|slicer|rate"}` on change > 30 s.
- Timelapse: snapshot pull from the printer's webcam endpoint (Moonraker webcam/`/webcam/?action=snapshot`) on layer change, cached in `state/tmp/fab-link/frames/<job>/`; latest frame published (path + dimensions) for the HUD `image` widget; cache pruned to the last 2 jobs. Fab-Link does not encode video — frames only; encoding is a manual export.

## 5. Failure-detection hook (reserved, Phase 3)

Reserved interface — **explicitly not implemented in Phase 4**:

- `FrameSink`: on layer-change snapshot, Fab-Link POSTs the frame path over its daemon socket as `{op: "vision.submit", path, job, layer}`. A Phase-3 **Core ML vision model running on the ANE** (per ARCHITECTURE.md's ANE policy: small always-on aux models belong to Core ML/ANE, not MLX/GPU) is hosted **daemon-side** — not in this sandbox; Fab-Link keeps `gpu = false` and gains no model runtime.
- Responses come back as `{op: "vision.result", verdict: "ok|spaghetti|detached|blob", confidence}` and map to `fab.alerts` (severity high, with the frame path) and, at high confidence + config opt-in, a `pause` command to the printer.
- Until Phase 3 the daemon answers `vision.submit` with `not_implemented`; Fab-Link treats that as a permanent no-op for the session. The hook ships so the data path (frames on layer change) is already exercised and recorded.

## 6. Printer control ops (JSONL, token-bearing)

`pause`, `resume`, `cancel`, `set_temp {heater, target}` — forwarded to Moonraker/OctoPrint. The daemon's intent router can drive these by voice ("pause the print"); destructive ops (`cancel`) require the daemon to confirm with the user first (router-side policy, not Fab-Link's).

## 7. Milestones

1. Moonraker client + `PrinterState` + `fab.temps`/`fab.progress`/`fab.eta` live in a HUD panel (data only, no toolpath).
2. G-code parser + cached toolpath + layer-streamed geometry; HUD overlay renders synced progress.
3. Alerts + timelapse frames + OctoPrint fallback.
4. Control ops by voice end-to-end; `vision.submit` no-op hook wired and logging.

Non-goals: slicing, multi-printer farms (one printer per instance), video encoding, any local CV inference (Phase 3, daemon-side).
