# Micro-App Sandboxing Blueprint

Status: **IMPLEMENTED.** The runtime substrate (`daemon/src/apps.rs` — manifest parsing, SBPL profile generation, capability tokens, per-app socket, supervised lifecycle, telemetry relay) is live, and the first app, **Global-Scan** (`apps/global-scan/`), runs on it. **Nexus** (`apps/nexus/` — full Python app + `core/` + tests) and **Silicon Canvas** (`apps/silicon-canvas/` — a full Rust crate) also run on this substrate, alongside vision, mark-forge, example-plugin and the utility micro-apps under `apps/`.

The remaining two launch apps, **Algo-Core** and **Fab-Link**, are **⛔ BLOCKED — spec-only AND REFUSED**. Their manifests declare `net_hosts`, which is not grantable on this OS at all, so validation refuses them: they never load, they are absent from the App Deck, and they never launched even before the refusal (their profile failed to compile, exit 65). They are not "not built yet" — they cannot be built as specified until the owner decides on a mechanism that would widen egress. See *A net scope is not grantable* below, and the ⛔ banner at the top of each `SPEC.md`.

Implementation notes (read these — they record the real boundary, not the ideal one):

- **`sandbox-exec` is deprecated-but-functional.** The host launches apps via `/usr/bin/sandbox-exec -f <profile>`. Apple has deprecated the *CLI* (it prints a notice) but the underlying seatbelt *kernel enforcement* is fully live and is what Apple's own daemon profiles use. The manifest→profile derivation in `generate_sbpl` is the stable part; Phase-4+ may migrate the launch mechanism to a `sandboxd` profile or App Sandbox entitlements without changing the derivation.
- **The generated profile is default-deny.** Every profile opens with `(deny default)`, imports Apple's stock `bsd.sb` (so the process can boot — dyld, frameworks, base syscalls — without opening the filesystem, network, mic, or GPU), then adds *only* the grants the manifest declares. See `daemon/src/apps.rs::generate_sbpl` and its unit tests (default-deny asserted, exact allows asserted, no stray grants asserted).
- **Honest SBPL limitations** (documented rather than hidden — see the Threat-model caveats below): SBPL has **no network filtering primitive at all** (no host, no IP), so the sandbox's only expressible network posture is *none*; and same-UID is the trust boundary for the per-app socket.

## Model

- **Process isolation.** Each micro-app is a separate process launched by `darwind`. Apps never run in the daemon's address space.
- **Seatbelt sandboxing.** At launch, `darwind` generates a macOS `sandbox-exec` (seatbelt) profile derived from the app's manifest permissions and starts the app under it. Anything not granted by the manifest is denied by the profile.
- **IPC.** Newline-delimited JSON (one object per line) over a per-app Unix socket at `state/ipc/apps/<name>.sock`. The daemon creates and owns the socket (bound `0600`, parent dir `0700`); the sandbox profile grants the app access to its own socket path only. The wire protocol (exact, mirrored in `daemon/src/apps.rs`, `apps/global-scan/main.py`, and the HUD reducer):
  - **app → host:** `{"token": <str>, "type": "items"|"status"|"log"|"modules", "data": <obj>}` — every line carries the capability token. (`modules` is the OPTIONAL, READ-ONLY dyld loaded-module self-report — `data.modules = [{path, uuid?}, …]` — attested against a trust-on-first-use baseline in `daemon/src/introspect.rs`; see docs/INTROSPECT.md. The reference stub is `apps/_sdk/dyld_report.py`.)
  - **host → app:** `{"type": "start"|"refresh"|"stop"}` — no token (the daemon is the trust root; the app trusts its own socket).
- **Capability tokens.** Every line an app sends carries a capability token minted by `darwind` at launch: `HMAC-SHA256(session_key, name ‖ canonical(permissions) ‖ nonce)`. The session key is 32 bytes of OS entropy generated once per daemon boot, held in a process-lifetime `OnceLock`, and is **never logged, never on telemetry, and never handed to an app** — only the derived per-app token reaches the app's environment (`DARWIN_APP_TOKEN`, alongside `DARWIN_APP_SOCKET`). The daemon verifies the token (constant-time) on **every inbound line**; a bad/missing/forged/stale/cross-app token drops the line and emits `("system","app.auth_failed",{name})`. The nonce rotates per launch, so a leaked token is dead after restart and cannot be replayed by another app or after a permission change.
- **Telemetry relay.** Apps do **not** connect to the `7177` telemetry WS. The host relays each accepted `items`/`status` line as `("system","app.data",{name,topic,payload})` (topic is one the app *declared* in `telemetry_topics`, else its first declared topic, else `"feed"` — an app can never publish to an undeclared topic), `log` lines as `("system","app.log",{name,line})`, and lifecycle as `("system","app.started"|"app.stopped"|"app.crashed",{name,...})`. The HUD panel renders purely from these relayed events.
- **Lifecycle.** The host writes the profile, binds the socket, spawns the sandboxed child, sends `{"type":"start"}`, and supervises it. On child exit it restarts, bounded to **≤3 restarts / 5 min**, then gives up with `app.crashed`. `stop()` kills the child (`kill_on_drop`) and removes the socket.
- **UI.** Micro-apps never open their own windows. UI surfaces render inside the HUD; the app declares which surface class it needs (`panel`|`overlay`|`fullscreen`) and the HUD composites it. (v1 renders `panel` apps as FUI-styled React panels driven by the `app.data` relay — e.g. `hud/src/components/GlobalScanPanel.tsx`; wgpu-texture/embedded-webview compositing is reserved for richer surfaces.)

## manifest.toml schema

Location: `apps/<name>/manifest.toml`. All paths are relative to the project root unless absolute.

```toml
[app]
name        = ""        # string, required — must match the directory name; used for socket and token
version     = ""        # string, required — semver
description = ""        # string, required
entry       = ""        # string, required — path to the entry script (python/node) or the binary, relative to the project root
runtime     = ""        # "python" | "binary" | "node", required

[permissions]
audio     = false       # bool — microphone / audio-route access via the daemon's audio API
net_hosts = []          # MUST BE EMPTY. A direct-egress net scope is NOT GRANTABLE: macOS SBPL has no
                        # host or IP filtering primitive, so a non-empty list is REFUSED at validation.
                        # For network access declare `fetch_hosts` below instead.
fetch_hosts = []        # list of hostnames the app may fetch THROUGH the daemon-mediated fetch proxy
                        # (https-only, exact-host, SSRF-guarded). This is the ONLY supported egress.
fs_read   = []          # list of paths the app may read (beyond its own app dir, which is implicit)
fs_write  = []          # list of paths the app may write; everything else is read-only or denied
gpu       = false       # bool — Metal/GPU access for the app process
camera    = false       # bool — DECLARES AVFoundation capture of the user's OWN camera (TCC: Camera)
screen    = false       # bool — DECLARES ScreenCaptureKit capture of the user's OWN screen (TCC: Screen Recording)
jit       = false       # bool — DECLARES dynamic code generation (JIT / dynamic-code-generation); consequential to enable — see docs/INTROSPECT.md

[ui]
surface          = ""   # "panel" | "overlay" | "fullscreen" — how the HUD composites the app
telemetry_topics = []   # list of topic strings the app may publish to the telemetry stream
```

Derivation rules from manifest to seatbelt profile:

| Manifest field | Seatbelt effect |
|---|---|
| `audio = false` | `(deny device-microphone)`; audio data only via daemon-mediated IPC if granted |
| `net_hosts = []` | `(deny network*)` — the only valid state |
| `net_hosts = [...]` | **REFUSED at validation.** SBPL cannot express a host filter, so this used to emit a profile `sandbox-exec` rejects (exit 65) and the app never launched. See *A net scope is not grantable* below. |
| `fetch_hosts = [...]` | **No SBPL network effect at all.** The app keeps a flat `(deny network*)`; egress rides the daemon over `state/ipc/apps/fetch.sock`. |
| `fs_read` / `fs_write` | deny-by-default filesystem; allow subpath reads/writes for the listed paths, plus implicit read of the app's own directory and read/write of `state/ipc/apps/<name>.sock` |
| `gpu = false` | deny IOKit GPU clients (no Metal device access) |
| `jit = false` / `jit = true` | `(deny dynamic-code-generation)` / `(allow dynamic-code-generation)` — explicit + reorder-safe, like `gpu`; the bit is token-bound and consequential to enable (see docs/INTROSPECT.md). Only `dynamic-code-generation` is emitted; legacy `dynamic-signature` is never written. |
| `camera = true` / `screen = true` | **DECLARATION ONLY — TCC IS THE REAL GATE.** macOS Camera / Screen Recording consent is enforced by TCC, which requires a runtime USER-CONSENT prompt and is **not grantable by an SBPL/seatbelt profile** (there is no `(allow camera)` / `(allow screen)` operation). The profile at most grants the best-effort mach-lookup/device plumbing the capture frameworks need to *reach* the consent prompt; it never enables capture. `= false` keeps the deny explicit. So a `true` here lets the daemon surface the need in the launch UI/status and binds it into the per-app token — it grants nothing. No consent → no frames, profile notwithstanding. |

### Vision OCR screen read (`read.screen` — READ ON REQUEST, on-device)

The Vision micro-app (`apps/vision`; needs the screen capability — declared as `screen = true` in `apps/vision/manifest.toml`, which the daemon `AppManifest` parses under `deny_unknown_fields` — the manifest is the authoritative declaration channel. It grants nothing by itself: TCC is the real gate and is requested on-device) exposes a one-shot OCR **screen read** behind the FROZEN op `read.screen`. The daemon routes `"what's on my screen"` / `"read my screen"` / `"read this"` / `"where's the <X> button"` to it (`router::vision_command` → `read.screen`, with an optional `query` for a where-is locate), forwards the structured op verbatim, and the app runs Apple's built-in `VNRecognizeTextRequest` on **one** captured frame, structures the result, and relays a `vision.screen` telemetry event (recognized text in reading order, per-block boxes/centers, control-candidate labels, and — for a where-is — the best-matching located block). Honest properties:

- **On-device OCR, fully offline.** Built-in Apple Vision request; `net_hosts = []`. The recognized glyph text never leaves the device on the on-device brain path.
- **TCC is the real gate.** Live ScreenCaptureKit capture needs the Screen Recording consent prompt — not SBPL-grantable, requested on-device at first use. Headless test environments prove the OCR engine over a synthesized in-memory image; live capture is **device-gated** and never exercised in CI.
- **READ-ONLY = locate, not click.** A where-is query returns a control's box/center so the readout can *describe/locate* it. There is **no click/actuate op anywhere in the contract** — actuation is a separate, out-of-scope, gated surface.
- **DEFENSIVE: glyphs only.** OCR reads text glyphs; it is never turned into a face/person identifier. No identity path.
- **TRANSIENT by default (privacy).** The recognized screen text is sensitive (it can contain on-screen passwords/messages), so the daemon keeps it **off lifelong memory (fact extraction) and optimizer traces** — `router::is_screen_read` gates `main.rs`'s learning loop, and the text rides the `vision.screen` telemetry event (HUD readout only), never the persisted reply. The HUD surfaces it live only and never persists it either, labeled `READ ON REQUEST · TRANSIENT`.
- **Cloud-if-cloud-brain note.** If a turn is answered by the CLOUD brain, any on-screen text the user includes goes to the cloud exactly like any other user content — so the **on-device brain is the privacy-preferring path** for reading your screen.
- **Op-gated, never proactive.** The read fires only on an explicit request; there is no continuous/background screen-watching.

## Worked example

**This example is a SHIPPED, RUNNING app.** It used to be `apps/fab-link/manifest.toml` — which was a bad choice on two counts: that app has no implementation, and its manifest is now *refused* (see *A net scope is not grantable*), so the schema doc's canonical example was a manifest the daemon rejects and a launch sequence that never happened. The example below is `apps/global-scan/manifest.toml`, abridged: the first app on the substrate, live today, and the one that demonstrates the *only* supported egress route.

```toml
[app]
name        = "global-scan"
version     = "0.1.0"
description = "Intel feed aggregator: polls reputable public RSS/Atom feeds, dedupes and ranks the latest items, optionally adds a neutral local-LLM summary, and renders the result as a HUD panel."
entry       = "apps/global-scan/main.py"
runtime     = "python"

[permissions]
audio     = false
gpu       = false
# NO direct network. This is the ONLY valid value: a direct-egress net scope is
# not grantable on this OS, and a non-empty list is REFUSED at validation.
net_hosts = []
# Egress rides the daemon-mediated fetch proxy instead: the app names hostnames,
# the DAEMON makes the request (https-only, exact-host, SSRF-guarded).
fetch_hosts = ["feeds.npr.org", "feeds.bbci.co.uk", "hnrss.org"]  # …9 in the real file
fs_read   = [
  "state/ipc/apps/fetch.sock",    # the fetch proxy (feed bodies; NO direct net)
  "state/ipc/apps/generate.sock", # op-restricted generate proxy (NOT raw inference.sock)
  "apps/_sdk",                    # shared read-only dyld module-report stub
]
fs_write  = ["state/apps/global-scan"]

[ui]
surface          = "panel"
telemetry_topics = ["feed"]
```

At launch, `darwind`:

1. Parses the manifest and validates it (name matches directory, runtime known, paths inside allowed roots, **`net_hosts` empty**). A manifest that fails any of these is skipped at discovery and surfaced as `app.manifest_invalid` on the HUD's App Deck — it never launches.
2. Mints the capability token: `HMAC-SHA256(secret, "global-scan" || canonical(permissions) || nonce)`.
3. Writes a seatbelt profile allowing: read of `apps/global-scan/`, read of each `fs_read` path, write of `state/apps/global-scan`, and socket access to `state/ipc/apps/global-scan.sock` plus the `.sock` paths it declared. **No outbound IP network at all** — the profile is a flat `(deny network*)`, unconditionally, for every app. Everything else denied — no mic, no GPU, no other filesystem.
4. Executes `sandbox-exec -f <profile> <venv python3> apps/global-scan/main.py` (the daemon resolves the `.venv/bin/python3` interpreter and appends the project-root-relative entry path) with the token passed via the launch environment.
5. The app connects to its socket and includes the token in every JSON request; the daemon verifies before acting. Telemetry the app publishes is accepted only on its declared `telemetry_topics` and re-broadcast on 127.0.0.1:7177 to the HUD. Feed fetches go out over `fetch.sock` and are performed **by the daemon**, authorized against `fetch_hosts`.

## Threat model

What the sandbox prevents:

| Escape attempt | Why it fails |
|---|---|
| **Arbitrary filesystem access** — reading `~/.ssh`, `state/darwin.db`, other apps' dirs; writing outside its grant | Seatbelt is deny-by-default; only manifest-listed `fs_read`/`fs_write` paths (plus the app's own dir and socket) are allowed. The daemon's secrets and the memory DB are never in any app's grant. |
| **Arbitrary network** — exfiltration, C2, scanning the LAN | `(deny network*)`, unconditionally, for every app. There is no host allow-list to widen: a net scope is refused at validation and the generator never emits a network grant. The only egress is the daemon-mediated fetch proxy, where the *daemon* makes the request against the app's declared `fetch_hosts`. |
| **Mic access without grant** — eavesdropping via the microphone | Direct device access is denied by the profile unless `audio = true`. Even with `audio = true`, audio flows through the daemon's audio API over the app socket — the daemon can mute, indicate, and log it. |
| **IPC impersonation** — one app speaking as another, or replaying old credentials | Per-app sockets plus per-launch HMAC capability tokens bound to name + permission set + session nonce. Wrong app, wrong permission set, or stale nonce → verification fails and the daemon drops the connection. |
| **Privilege escalation via UI** — spawning windows, capturing the screen, key-logging | Apps have no window-server allowance; their only display path is a surface composited by the HUD (`wgpu` texture or embedded webview). Input reaches an app only when the HUD routes it to that surface. |

What it does not protect against (out of scope): kernel exploits in macOS itself, a compromised `darwind` (it is the trust root), and side channels between processes on shared hardware. Manifests are reviewed before an app is installed; the sandbox enforces the manifest, it does not judge it.

### Self-heal GUI apply — a human-gated mutation of the trust root

The one place a model-authored change can reach `darwind`'s own source tree is the self-heal apply path (full detail in `docs/ARCHITECTURE.md` → *Self-heal v2*). It is **not** auto-heal and it is **not** in the micro-app sandbox's scope — it is a deliberate, **human-gated** mutation of the daemon source:

- **`self_heal` ships `enabled = true` but PROPOSE-ONLY** (`mode = "propose"`, inert without a cloud key). With the gate off, an error burst only emits `heal.suppressed`; even on, nothing touches the live tree without the human-gated `scripts/apply_heal.sh`.
- **The GUI Accept button is human-gated and two-step.** The HUD's SELF-REPAIR // PROPOSALS modal fetches and shows the **actual staged diff** (`heal_proposal_detail`) for review. **ACCEPT & APPLY** arms a distinct **CONFIRM — APPLY & REBUILD** state; only a second click (after a re-arm window so a double-click cannot skip it) calls `heal_apply`.
- **Re-validation is mandatory and non-bypassable.** `heal_apply` spawns `scripts/apply_heal.sh <ts> --yes` (args-only, `ts` validated **digits-only** — no path traversal). `--yes` skips **only** the script's `read -r` keystroke; the script still stages a fresh copy of `daemon/` and re-runs `/usr/bin/patch -p1 --batch` + `cargo check` + `cargo clippy -D warnings` + full `cargo test` — then prints the **responsiveness** verdict (`darwind --heal-responsiveness` — advisory only; every gate here is blind to the diagnosis, so a patch that fixes something else entirely passes all of them) — then enforces the **review-confidence floor** (`darwind --heal-confidence` on the proposal's `report.md` — the same `CONFIDENCE_FLOOR` the daemon refuses to *propose* below, so an older or hand-edited proposal cannot be installed under a weaker bar; unlike the responsiveness verdict, this one **refuses**) — then runs the mutation probe (fix reversed, the patch's own test must fail). Both of those probes run **before** the mutation probe on purpose: that probe reverse-applies the fix into the staged crate and leaves it there, and a crate with its fix lifted out often no longer compiles, so `darwind` could not be run from it. The script then **refuses to touch `daemon/src` if anything fails** (the UI surfaces the failure in alert-red; the live code stays untouched). There is no flag that weakens this gate.
- This GUI apply, and the doubly-opt-in `mode = "auto"`, are the **only** sanctioned paths that change the live `daemon/` tree. The HUD reaches the daemon only over the one-way telemetry WS; the apply work is done by the HUD-Tauri backend spawning the repo script directly (filesystem + `cargo`), after which the daemon restarts to run the healed binary.

## Honest limitations of the current seatbelt implementation

These were surfaced by an isolation review of `daemon/src/apps.rs`. The first three are *closed* in the generator. The fourth is not a limitation of the sandbox but of the OS — SBPL cannot filter network at all — and the fifth is inherent to the single-UID model. Together they are the boundary of what this sandbox claims.

- **Metadata side channel — CLOSED.** Earlier the profile emitted a bare `(allow file-read-metadata)` (no path filter), which let an app `stat`/test-existence on the *entire* filesystem (probe that `~/.ssh/id_rsa` exists and its size/mtime) even though contents stayed denied. The generator now scopes `file-read-metadata` to the *same subpaths* it grants `file-read*` on (app dir, runtime install prefix, venv, `fs_read`, socket) — never a blanket grant. dyld's startup stats of `/` and the firmlink ancestors are covered by the `bsd.sb`/`system.sb` import, so no blanket grant is needed to boot.
- **Over-broad exec — CLOSED.** Earlier python/node apps were granted `process-exec*` on the *entire* `/opt/homebrew` and `/usr/local` trees (to reach the symlinked venv interpreter), letting an app exec any `bash`/`curl`/`git`/compiler planted under those user-writable prefixes. The generator now resolves the interpreter once (`std::fs::canonicalize`) and grants `process-exec*` only on the configured interpreter path *literal* plus its *resolved* path literal — never a prefix subpath. Read of the stdlib is scoped to the interpreter's own install prefix (the Cellar version dir holding `lib/pythonX.Y`), not all of Homebrew.
- **Socket ownership — CLOSED (defense-in-depth).** The per-app Unix socket at `state/ipc/apps/<name>.sock` is now `chmod 0600` after bind, and its parent dir `state/ipc/apps` is `0700`, so an unrelated same-UID process cannot casually `connect()` to read the host's start/refresh/stop command stream or wedge the accept path (a local DoS). Token verification already blocked *injection* (a connector cannot forge the per-launch HMAC), so this only closes the casual-connect leak. It does not stop a same-UID attacker who can `chmod` — same-UID is the trust boundary either way.
- **Filtered network egress — NOT AVAILABLE (this entry replaces two that were wrong).** This section used to list *coarse host-name filtering* and a *DNS exfiltration side channel* as the two INHERENT costs of SBPL egress, describing the first as "a meaningful narrowing, not an IP allow-list". **Both descriptions were fiction: SBPL has no host or IP filtering rule whatsoever.** The rules the generator emitted for a non-empty `net_hosts` did not narrow anything — the profile compiler rejected them, so `sandbox-exec` refused the whole profile and the app never launched (full account in *A net scope is not grantable* below). There is therefore no coarse filter to reason about and no DNS channel to raise the bar on, because no app was ever granted DNS. The seatbelt's only expressible network posture is `(deny network*)`, which is what every profile now emits unconditionally. **Filtered egress exists in DARWIN, but it lives in the daemon, not the sandbox:** the fetch proxy below does the host allow-listing, the SSRF/rebinding guard, and the redirect re-authorization in Rust, where those checks can actually run.
- **Same-UID trust boundary — INHERENT.** The per-app socket is `0600` under a `0700` dir, but a same-UID attacker who can `chmod` is inside the boundary either way; see the socket-ownership entry above. The daemon is the trust root, and nothing here defends against a compromised `darwind`.

### Direct app egress — CLOSED (daemon-mediated fetch proxy)

**Was:** Global-Scan (the only shipped net-declaring app) held `net_hosts = [9 RSS hosts]`, which the generator turned into `(system-network)` + pinned DNS + per-host `(remote tcp (host-name …))` allows. **Note the correction:** that text is what was *emitted*, not what was *enforced* — those rules do not compile (see *A net scope is not grantable*), so the profile was rejected outright. The proxy below was built to close two caveats that, as it turned out, described a mechanism that never ran. It is still the right design; it is simply the *only* egress design, not the better of two.

**Now:** the daemon fronts app fetches with a **daemon-mediated fetch proxy** (`daemon/src/fetchproxy.rs`), a sibling of the generate proxy on its own socket `state/ipc/apps/fetch.sock` (`0600`, parent `0700`). Global-Scan's manifest declares `fetch_hosts = [the same 9 hosts]` and `net_hosts = []` — its SBPL is now a **flat `(deny network*)` with no `(system-network)`, no DNS, and no host-name allows at all**. The proxy enforces, daemon-side:

- **Only `op=fetch`, structurally** (no other op has a code path to a fetch); token-gated via the same `AppRegistry::verify_token` machinery; per-app rate limit (60/60 s); bounded request lines.
- **Exact-host allowlist** from the app's own manifest `fetch_hosts`: https-only, case-insensitive exact host match (no subdomains/wildcards), no userinfo, port 443 only, IP literals rejected.
- **SSRF/rebinding guard:** the host is resolved first and every address must be public (loopback/private/link-local/ULA/etc. refused); the verified address is **pinned** for the actual request, so DNS rebinding between check and connect is closed.
- **Redirects re-validated:** at most 3 hops, each re-authorized against the same allowlist; a cross-host or non-listed redirect is refused.
- **Bounded bodies:** responses stream with a hard 2 MiB cap; errors are secret-free kinds (never the URL or body).

The **agent-tool surface invariant is preserved by construction**: `agent_tools` skips any app with non-empty `net_hosts` **or** non-empty `fetch_hosts`, so a tool offered to the model still provably has no network side effects.

**Residual (honest register):** the daemon (the trust root) now performs the fetches, so a compromised daemon fetches as before — same single-UID boundary as everything above. The declared `fetch_hosts` are trusted to be the operator's intent (the manifest is reviewed, not judged, exactly like `net_hosts` was); a hostile *feed server* can still return hostile *content*, which the app must treat as untrusted data (unchanged from direct egress). `net_hosts` is **no longer supported at all** — see the next section.

### A net scope is not grantable — `net_hosts` REFUSED at validation

**The primitive does not exist.** macOS SBPL has no host or IP filtering rule. `(remote tcp (host-name "x"))` is not valid syntax, and neither is `(remote ip "1.2.3.4:443")` — the compiler accepts only `*` or `localhost` as a host. So the derivation this document used to describe was never enforceable.

**What actually happened.** A non-empty `net_hosts` made `generate_sbpl` emit those rules, `sandbox-exec` rejected the whole profile with **exit 65**, and the app **never launched at all**. (Measured on this machine, both forms and both messages: `(remote tcp (host-name "example.com"))` → `sandbox-exec: unbound variable: host-name`; `(remote ip "1.2.3.4:443")` → `sandbox-exec: host must be * or localhost in network address`. This document previously quoted only the second, for a failure caused by the first.) It failed *closed*, so there was no security exposure — but two shipped apps (`fab-link`, `algo-core`) were unlaunchable, and the failure presented as a crash-loop rather than a permission error. Two string-matching tests over these rules passed the entire time, because they asserted the literals the generator emitted: the generator agreeing with itself. Only handing the profile to `sandbox-exec` ever caught it.

**The decision, now landed.** A net scope is refused at **validation**, in one voice across all three gates (`apps::NET_SCOPE_REFUSAL`):

| Gate | Behaviour |
|---|---|
| `AppManifest::validate` (runtime capability ceiling) | a non-empty `net_hosts` is refused; the app is skipped at discovery and surfaced as `app.manifest_invalid` |
| `forge::validate_permissions` (author-time + `darwind --validate-forge-manifest`) | same refusal; `scripts/apply_forge.sh` will not deploy the app |
| `plugin_sdk::scope_backed_by_permissions` | a `net` tool scope is **never** backed, with or without hosts |

The old guidance was actively harmful: an author who hit *"tool scope `net` is over-privileged"* was being told to declare `net_hosts`, which is precisely what killed the app. The DLS rule was renamed `net_scope_without_hosts` → **`net_scope_not_grantable`** for the same reason.

`generate_sbpl` now emits `(deny network*)` **unconditionally**, so even a manifest constructed in-process that bypassed validation produces a profile that compiles and grants nothing.

**The supported route is the fetch proxy** (previous section): declare `fetch_hosts`, read `state/ipc/apps/fetch.sock`, and let the daemon make the request.

#### The same primitive in `[[mcp.servers]]` — also REFUSED

An MCP stdio server's config carried the identical unenforceable key, and it was left un-refused when the app surface was closed. It is closed now, the same way and for the same reason: `mcp::stdio_sandbox_profile` no longer has a `net_hosts` branch (it emits `(deny network*)` unconditionally, like `generate_sbpl`), a stdio server declaring `net_hosts` is **reported at config load and dropped from `McpManager::connectable_servers`** so it is never spawned, and the boot log says so with the same message (`mcp::MCP_NET_SCOPE_REFUSAL`). The DLS raises `net_scope_not_grantable` on it, exactly as it does on a manifest.

Three end states were considered. **Refuse** (chosen): the server does not run, and the operator is told why, in three places. **Start it with a silently-denied network** (rejected): this is the worst of the three — it is *precisely* the "claims a rule the code does not enforce" defect, and an operator who had written a host list would carry on believing a filter was live. **Leave it** (rejected): the config key would keep documenting a capability that does not exist. Nothing that worked was taken away — a stdio server in this state could never start, because its profile did not compile.

The remedy is **not** the fetch proxy — that is a micro-app mechanism and an MCP server has no access to it. It is: remove `net_hosts` and run the server sandboxed with no network (the only posture a seatbelt profile can express), or accept that a server which genuinely needs the network cannot be sandboxed here. Running it as a remote `transport = "http"` server is a **different trust posture** (TLS + Keychain token, explicitly *not* sandboxed) and is an operator decision, not a substitute grant. On an `http` server the key is INERT rather than fatal — there is no local process to filter — so it is reported but not refused; refusing it would break a configuration that works today.

No config shipped in this repo declares `net_hosts` on any MCP server (`config/darwin.toml` ships the key commented out and `servers` empty), so nothing in-tree needed migrating. That is pinned by `config::tests::shipped_config_declares_no_mcp_net_scope`.

#### Open owner decision — two apps cannot take any route

`fab-link` and `algo-core` are spec-only (`SPEC.md` + `manifest.toml`, no implementation) and were already unlaunchable, so nothing regressed — but neither can be migrated as written:

- **`fab-link`** — Moonraker is `ws://voron.local:7125/websocket`. That fails the proxy on three independent axes: scheme (`ws`, not `https`), port (7125, not 443), and the SSRF guard (a `.local` mDNS name resolves to a private LAN address, which is refused by design). Its control ops (`pause`/`cancel`/`set_temp`) and webcam snapshot pulls are the same shape.
- **`algo-core`** — market data is persistent WebSocket streaming (`stream.binance.com`, `ws.kraken.com`). The proxy is one-shot request/response and cannot carry a subscription. Its REST order path would proxy fine; the market-data half cannot.

Granting either of them direct egress would require a new mechanism (a daemon-side WebSocket relay, or an explicit LAN-scoped exception to the SSRF guard). **That is an owner decision, not a validator change**, and it is deliberately not taken here. `algo-core` additionally **places real orders on real venues** per its spec, which makes its egress a financial-risk decision as much as a sandbox one.

**Where that state is now stated, so nobody meets these apps believing they work:** a ⛔ banner at the top of each `manifest.toml` *and* each `SPEC.md`; the *Status* line of this document; the Phase-4 section of `docs/ROADMAP.md` (they are listed as **BLOCKED**, not "still to build"); and the App Deck, where they are absent from the fleet and appear only under **MANIFEST ERRORS**. Their retention in the tree is deliberate — see the note below.

**Why the source is retained rather than deleted.** The owner's open decision is *"a new mechanism, or deletion"*. Deleting the specs would quietly take the second branch, and it would destroy the design record that a future decision needs — while the refusal already makes them harmless (they cannot load, cannot appear on the deck, and `apps::tests::shipped_manifests_all_validate_and_declared_tools_are_served` asserts *positively* that each is refused, so the state cannot rot into silence). The confusion risk that argues for deletion is addressed by marking, which is cheap and reversible; deletion is not. If the owner decides against the mechanism, `rm -rf apps/fab-link apps/algo-core` plus that test arm is the whole change.

### Confused-deputy via the inference socket — CLOSED (daemon-mediated generate proxy)

**Was:** Global-Scan's manifest granted `fs_read = ["state/ipc/inference.sock"]` so it could ask the local LLM for a neutral one-line summary (`op=generate`). The seatbelt grant was socket *reachability*, **not** op-level scope, and the inference server multiplexes *all* ops on that one socket (`transcribe`/`classify`/`generate`/`extract_facts`/`speak`/`converse`/`consolidate`) **without caller authorization**. A compromised app holding that grant could call `op=speak` (make DARWIN talk), drive `op=extract_facts`/`consolidate` (write into the user's memory DB by proxy), or spam the model to exhaustion — none of which the manifest implied.

**Now:** the daemon fronts micro-app generation with a **daemon-mediated `generate` proxy** (`daemon/src/genproxy.rs`), and micro-apps are granted **only** that proxy — never the raw `inference.sock`. The inference server is **unchanged**; the gate lives in the daemon:

- **Separate, op-restricted socket.** The proxy listens on `state/ipc/apps/generate.sock` (own JSONL socket, `chmod 0600`, parent dir `0700`), distinct from `inference.sock`. Global-Scan's manifest now reads `fs_read = ["state/ipc/apps/generate.sock"]`; it has **no grant to `inference.sock` at all**.
- **Only `op=generate`, structurally.** The proxy accepts `op == "generate"` and nothing else — every other value (`speak`/`extract_facts`/`consolidate`/`transcribe`/`classify`/`converse`, or any unknown string) returns `ok=false error=op_not_permitted` and emits `app.proxy_denied`. This is not a blocklist: the proxy has **no code path** that forwards anything but generate (it calls `InferenceClient::generate` directly, never a generic op dispatch), so the privileged ops are *unrouteable*, not merely *rejected*.
- **Token-gated.** Every line is verified with `AppRegistry::verify_token` — the *same* per-launch HMAC capability-token machinery the per-app relay uses (no duplicate token logic). A forged/tampered/cross-app/stale/missing token returns `ok=false error=unauthorized` and emits `app.auth_failed {via:genproxy}`. Fail-closed.
- **256-token cap.** `max_tokens` is clamped to a hard `PROXY_MAX_TOKENS = 256` regardless of the requested value (a missing/zero/negative value floors to a sane default), so no single proxied call can request an outsized generation.
- **Rate-limited.** At most `PROXY_RATE = 30` calls / 60 s rolling **per app name** — the LLM-exhaustion guard; beyond it the call returns `ok=false error=rate_limited`. An inference failure relays as `ok=false error=inference_unavailable`.

On any `ok=false` reply, or an unreachable proxy, the app falls back to extractive summaries exactly as before — the enhancement is best-effort, never required.

**Residual (honest register):** this closes the confused-deputy vector *for micro-apps* — the sandboxed, untrusted processes the threat model is about. It does **not** harden `inference.sock` itself: the server still trusts any local caller, so `darwind` (the trust root) and anything else able to reach that socket retain the full unauthenticated op surface. That is by design — the daemon *is* the trust root — and is the same single-UID boundary called out above. If a future non-micro-app component needs scoped LLM access, it should route through this proxy (or a sibling) rather than the raw socket.

## MCP external tool servers

DARWIN is an **opt-in MCP _host_**: it can connect to external [Model Context Protocol](https://modelcontextprotocol.io) tool servers and expose their tools to agents. This is the most dangerous surface in the system — an MCP server is **external code running on your machine as you**, not a sandboxed micro-app — so it is fenced by four independent layers and an honest residual-trust note. The per-server sandbox profiles generated by `daemon/src/mcp.rs` (`stdio_sandbox_profile`) cite this section.

**Ships ON, but INERT WITH ZERO SERVERS by default.** `[mcp].enabled = true` is the default, but the `servers` list ships EMPTY — so no server connects and no MCP tool exists for any agent until you add at least one `[[mcp.servers]]` entry. There is no auto-discovery and no bundled server.

**Layer 1 — per-server default-deny sandbox (stdio).** Each stdio server is wrapped by the **same `sandbox-exec`/SBPL machinery as micro-apps** (`apps.rs`). `stdio_sandbox_profile` derives a per-server `.sb` profile that is **deny-by-default** and grants only: exec of the configured `command` and the paths the server's config declares in `fs_read`/`fs_write`. **It grants no network whatsoever** — `(deny network*)`, unconditionally. A `net_hosts` list is not a narrowing this OS can express (see *A net scope is not grantable*), so a stdio server that declares one is **refused**: reported at config load, dropped from `connectable_servers`, never spawned. The profile filename stem is the strict-validated server name. **Honest residual trust:** `sandbox-exec` is Apple-deprecated-but-functional, and same-UID remains the trust boundary. The profile **bounds** an untrusted server; it does **not** make a malicious server binary *safe* — and a sandboxed stdio server that needs the network cannot be run here at all.

**Layer 1 (remote `http`) — TLS + token, NOT sandboxed.** A server configured with `transport = "http"` is a **remote** MCP server speaking MCP Streamable-HTTP/SSE (`daemon/src/mcp.rs::HttpTransport`, wired into `McpManager::connect_one`). It runs on **someone else's machine**, so — stated plainly — it **cannot be SBPL-sandboxed**: there is no local process to wrap in seatbelt, and we do **not** claim a remote server is sandboxed. Its protections are a *different, still-layered* set: **TLS-only** (the url **must** be `https://` — a non-https url is refused at construction so a bearer token never rides plaintext); **Keychain bearer auth** (the token resolves from `mcp_<server>_token` and rides the `Authorization` header **only** — never the URL, a log, or `Debug`); the **same** confirmation gate + per-agent allowlist + per-call bounds (timeout / output-size cap, plus a hard cap on SSE events and total bytes) as stdio; and a friendly, **secret-free** error map (a 4xx/5xx body is never echoed). **Honest residual trust:** the layers above bound the blast radius and keep the secret clean, but ultimately **you trust the remote operator** with the arguments you send and the results you receive — they do not neutralize a malicious operator. The single network leg (`HttpTransport::post`) is **runtime-gated**: it is reached only when `[mcp].enabled = true` **and** an `http` server with an `https://` url is configured; **no test ever touches the wire** — the SSE/JSON-RPC reply parsing is a pure function (`parse_sse_events` / `extract_rpc_response`) unit-tested with canned bytes, and the manager path is driven by a `MockTransport`.

**Layer 2 — confirmation gate + armed-by-default master switch.** A **CONSEQUENTIAL** MCP tool **parks** behind the cross-turn spoken-confirmation gate, identical to the built-in consequential tools, and only acts after the user confirms. Parking is additionally fenced by the master switch `[integrations].allow_consequential`, which ships **ON** — but even armed, a confirmed action still requires a fresh per-action confirm + voice-id + `!lockdown`; the switch alone never executes. **Fail-safe classification:** any unknown or mutating MCP tool defaults to CONSEQUENTIAL — a tool is treated as read-only (ungated) **only** when the server config explicitly marks it so.

**Layer 3 — per-agent allowlist.** A server is usable only by agents on its allowlist. An **empty** `agents = []` (the shipped default, and exactly what `connector_add` writes) is **fully inert — no agent may use the server, not even the orchestrator**, which is what the confirmation the user approves promises. Once the user grants **any** agent, that server admits the listed agents **plus the orchestrator** (the delegation fallback / tool owner) — the 27 personas are **never** auto-granted. An unlisted agent (or an unknown server name) is refused before any tool dispatch.

**Layer 4 — bounds.** Per-call timeout, output-size cap, max servers, and max tools/server are all enforced, so a slow or chatty server cannot wedge or flood the host.

**Secrets — Keychain only.** A server's auth token resolves from the macOS Keychain under the allowlisted account stem `mcp_<server>_token`, where `<server>` must pass `integrations::is_safe_mcp_server_name` (strict `[a-z0-9_-]+`, no leading/trailing or consecutive separator — the `__` ban keeps the flat tool id `mcp__<server>__<tool>` unambiguous). The token is **never** logged, never in `Debug`, never on argv, and never in a URL. A name that fails validation mints no account and the server is filtered out of `connectable_servers` — so a hostile name never spawns a subprocess or reaches `security(1)`.

**Out of scope (unchanged):** a malicious server binary's behavior *within* its granted fs/net bounds, kernel exploits, a compromised `darwind`, and cross-process side channels on shared hardware — the same boundary the rest of this document claims. The MCP layers **bound and gate** an untrusted server; they do not vouch for it.

## Plugin SDK — the formalized capability-module contract (#36)

The optional `[intents]` / `[tools]` block a plugin's `manifest.toml` may declare — *what intents it answers and what tools it exposes, with the capability scopes each requests* — is formalized and **validated** by `daemon/src/plugin_sdk.rs`. Full detail in [`PLUGIN_SDK.md`](PLUGIN_SDK.md). In short: `validate_manifest` (pure) rejects a malformed manifest (bad intent/tool name) and an **over-privileged** one (a tool requesting a scope outside `ALLOWED_SCOPES`, or a scope the `[permissions]` block does not back; and `net` **in every state**, because a net scope is not grantable at all — with hosts or without); the register-on-launch handshake (`register_plugin`, gated by `[plugin_sdk].enabled`, ships **ON**) re-validates the manifest and **verifies the capability token** with the same HMAC/nonce machinery the per-app relay uses before scoping the plugin's intents. Declaring an intent grants nothing — the `generate_sbpl` derivation above is unchanged, and a consequential tool still rides the confirmation gate. Reference plugin: `apps/example-plugin/`.

## Webhook triggers — an inbound, authenticated, loopback-default surface (#35)

`daemon/src/webhooks.rs` adds the daemon's first **inbound** network surface, the most security-sensitive thing in this layer. It **ships ON** (`[webhooks].enabled = true`) but is **INERT WITHOUT MAPPINGS + a Keychain HMAC secret** — `mappings` ships empty (an unmapped event is rejected) and the secret resolves from the Keychain; the live receiver binds **127.0.0.1 loopback** by default (a non-loopback bind is refused) and is **runtime-gated** (the bind/accept-loop is wired behind the flag, not exercised in tests — the mic-loop / vision-capture precedent). Every request is authenticated by a **constant-time HMAC-SHA256 over the raw body** (`X-Darwin-Signature: sha256=<hex>`, secret from the Keychain at `webhook_hmac_secret` — never in config/log/Debug); a missing/forged/stale signature **never routes**. An authenticated event is mapped to an intent only via the **explicit `[[webhooks.mappings]]` allowlist** (an unmapped event is rejected, not guessed), and a mapped **consequential** intent **PARKS** for the user's spoken confirm — a webhook can never satisfy the cross-turn confirm, so it can never auto-execute a side-effecting action. The pure decision (`handle_webhook`) is proven hermetically with synthetic signed requests; the body and the secret are never logged.
