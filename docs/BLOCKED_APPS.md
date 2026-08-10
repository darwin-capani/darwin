# Blocked apps — designs this OS cannot grant, and what each would need

Two Phase-4 launch apps, **Fab-Link** and **Algo-Core**, were carried in `apps/`
as spec-only directories (`SPEC.md` + `manifest.toml`, no implementation) with
⛔ BLOCKED banners. Both are now **removed from the tree**.

**Why removal, not retention.** Each manifest declared a direct-egress
`net_hosts` scope, which is **not grantable on macOS at all** (SBPL has no host
or IP filtering primitive — see `docs/SANDBOX.md` → *A net scope is not
grantable*), so both were **refused at validation**: they never loaded, never
appeared on the App Deck, and — before the refusal landed — never launched
either, because their sandbox profile failed to compile and the process died at
`sandbox-exec` (exit 65). They were retained once, with banners, as the cautious
call. It was not the precise one: the tree carried two apps that can never run,
every audit re-derived the same conclusion about them, and any "shipped apps"
count that includes them is wrong.

**Nothing is lost.** The full source of both apps — every line of both specs and
both manifests — is recoverable from git:

```
git show 39c8101f7a359607825087e298adcea6fc8ec260:apps/fab-link/SPEC.md
git show 39c8101f7a359607825087e298adcea6fc8ec260:apps/fab-link/manifest.toml
git show 39c8101f7a359607825087e298adcea6fc8ec260:apps/algo-core/SPEC.md
git show 39c8101f7a359607825087e298adcea6fc8ec260:apps/algo-core/manifest.toml
# or restore the whole tree state:
git checkout 39c8101f7a359607825087e298adcea6fc8ec260 -- apps/fab-link apps/algo-core
```

`39c8101` is the last commit in which both directories exist.

**The rule outlives the examples.** These two were only ever the *instances*.
The property — *a `net_hosts` scope is refused at validation, whatever hosts it
names* — is pinned against **synthetic** manifests, not against these apps, in:

- `plugin_sdk::tests::a_net_scope_is_refused_as_not_grantable_not_as_over_privileged`
- `apps::tests::ceiling_refuses_any_net_hosts_declaration_however_well_formed`
- `apps::tests::a_net_hosts_declaration_is_refused_and_can_no_longer_emit_uncompilable_sbpl`
  (the only one that proves it against the **real OS compiler** rather than
  against our own string literals)
- `apps::tests::shipped_manifests_all_validate_and_declared_tools_are_served`
  — which now injects a net scope into a **real shipped manifest** and requires
  the refusal, so the fleet gate proves the validator *refuses*, not merely that
  no shipped app happens to declare one.

---

## Fab-Link — 3D-printing telemetry overlay

**What it was for.** A HUD `overlay`-class surface for a Klipper/Voron printer:
live job progress, extruder/bed/chamber temperatures and ETA; a toolpath render
parsed from the active g-code and synced to print progress; a thermal panel; a
timelapse frame cache pulled from the printer's webcam; and a reserved hook for
the Phase-3 ANE vision failure-detector. `gpu = false` — it parsed and published
geometry, and the HUD (which owns the GPU) drew it.

**The exact endpoints it needed.**

| Purpose | Endpoint | Transport |
|---|---|---|
| Moonraker telemetry (primary) | `ws://voron.local:7125/websocket` | WebSocket JSON-RPC, `printer.objects.subscribe` on `print_stats`, `virtual_sdcard`, `extruder`, `heater_bed`, `temperature_sensor chamber`, `display_status`, `toolhead` |
| G-code + thumbnails | `http://voron.local:7125/server/files/gcodes/...`, `/server/files/thumbnails` | HTTP GET |
| Webcam snapshot (timelapse) | `http://voron.local:7125/webcam/?action=snapshot` | HTTP GET, on layer change |
| Control ops | Moonraker JSON-RPC `pause` / `resume` / `cancel` / `set_temp` | same WebSocket |
| OctoPrint fallback | `octoprint.local` — SockJS socket, else REST `/api/job`, `/api/printer` at 2 s | WebSocket / HTTP |

Declared scope was `net_hosts = ["voron.local", "octoprint.local"]`.

**Why it cannot work.** Two independent walls.

1. **The declared scope does not exist.** `net_hosts` is a direct-egress net
   scope. macOS SBPL cannot express a host or IP filter, so a non-empty list
   only ever produced an uncompilable profile. Measured on this machine, both
   spellings and both messages:
   `(remote tcp (host-name "example.com"))` → `sandbox-exec: unbound variable: host-name`;
   `(remote ip "1.2.3.4:443")` → `sandbox-exec: host must be * or localhost in network address`.
   The scope is now refused at validation (`apps::NET_SCOPE_REFUSAL`).
2. **The one supported egress route cannot carry it.** The daemon-mediated fetch
   proxy (`fetch_hosts`) is the only way an app reaches the network, and
   Moonraker fails it on **three independent axes at once**:
   - **scheme** — the proxy is HTTPS-only; Moonraker is `ws://` (and its file
     API is plain `http://`);
   - **port** — the proxy allows 443 only; Moonraker is 7125;
   - **address** — `voron.local` is an mDNS name that resolves to a private LAN
     address, which the proxy's SSRF / DNS-rebinding guard refuses **by design**
     (that guard is the reason an app cannot be talked into reaching the
     owner's router, NAS, or `169.254.169.254`).

   The control ops and the webcam pulls are the same shape, so there is no
   subset of the app that migrates.

**The exact mechanism it would need.** Either
(a) a **daemon-side WebSocket relay** — a new long-lived, per-app, host-scoped
proxy channel with its own consent and audit surface, plus a plaintext-`ws`
allowance; **or**
(b) an explicit **LAN-scoped exception to the SSRF guard**, admitting
RFC1918/mDNS destinations for named apps.

Both **widen DARWIN's egress posture**. Both are owner decisions, not validator
changes. Neither has been taken.

---

## Algo-Core — algorithmic trading daemon

> **THIS APP PLACES REAL ORDERS ON REAL VENUES.** Its spec (§5 exchange
> adapters, §6 signed order audit log, §7 kill-switch) defines an
> order-management system that submits live orders against Binance, Kraken and a
> Polymarket-style prediction-market CLOB, with API credentials on disk under
> `apps/algo-core/data/keys.toml`. **Granting this app egress is a financial
> decision as much as a sandbox one.** Nothing about it is a lint or a
> configuration question: the mechanism that unblocks it is the mechanism that
> lets autonomous code spend the owner's money. No order has ever been placed by
> this code, because this code was never written — the app has no `main.py` and
> never launched on any machine.
>
> The spec is an **engineering** spec. It makes no profitability claims; a
> strategy passing every harness in it can still lose money.

**What it was for.** A single-threaded deterministic event-driven engine over
one ordered event stream; strategies as **WASM** modules with a mandatory
walk-forward validation certificate; a risk gate between signals and adapters
(per-instrument position caps, gross/net exposure, daily loss kill-switch, order
rate limits, price sanity band); an append-only SQLite audit log with an
Ed25519-signed SHA-256 hash chain; and a HUD `panel` publishing
`algo.prices` / `algo.signals` / `algo.orders` / `algo.positions` / `algo.pnl`.

**The exact endpoints it needed.**

| Purpose | Endpoint | Transport |
|---|---|---|
| Binance market data | `stream.binance.com` | **persistent WebSocket subscription** |
| Kraken market data | `ws.kraken.com` | **persistent WebSocket subscription** |
| Binance orders | `api.binance.com` | HTTPS REST (place / cancel / cancel_all / positions) |
| Kraken orders | `api.kraken.com` | HTTPS REST |
| Prediction-market CLOB | `clob.polymarket.com` | HTTPS REST + book stream |

Declared scope was
`net_hosts = ["api.binance.com", "stream.binance.com", "api.kraken.com", "ws.kraken.com", "clob.polymarket.com"]`.

**Why it cannot work.** The same first wall — a `net_hosts` scope is not
grantable — plus a second one specific to its shape:

- The fetch proxy is **one-shot request/response**. It cannot carry a
  **subscription**. Market data here is persistent WebSocket streaming
  (`stream.binance.com`, `ws.kraken.com`); the proxy has no channel that stays
  open, no back-pressure model, and no way to deliver server-pushed frames.
- The REST **order** path (`api.binance.com`, `api.kraken.com`,
  `clob.polymarket.com`) would proxy fine on its own — HTTPS, port 443, public
  hosts. That is the uncomfortable half of this finding: **the only part of this
  app that migrates today is the part that spends money**, and an order path
  with no market-data path is not a working app, it is a loaded gun with no
  sights.

**The exact mechanism it would need.** A **daemon-side WebSocket relay** — a
long-lived, per-app, host-scoped streaming channel through the daemon, with its
own consent gate, its own rate/quantity accounting, and an audit surface — and,
separately and explicitly, an owner decision to let sandboxed code place orders
with real credentials. The second is not implied by the first.

---

## If the owner reverses either decision

Restoring the files is `git checkout 39c8101 -- apps/<name>` (above). It is not
sufficient, and must not be treated as sufficient:

1. **The mechanism has to exist first.** A restored manifest is still refused,
   because the refusal is the OS's, not a policy toggle. Building the relay (or
   the LAN exception) is the actual work; restoring the spec is bookkeeping.
2. **`shipped_manifests_all_validate_and_declared_tools_are_served` will fail**
   on a restored directory — it requires every manifest under `apps/` to
   validate. That failure is the gate working, not an obstacle to route around.
3. For Algo-Core, the egress grant and the **order authority** are two separate
   decisions. Do not let the first quietly carry the second.
