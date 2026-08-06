# Global-Scan

First micro-app on the DARWIN micro-app runtime substrate (`docs/SANDBOX.md`).
A world **intel feed aggregator**: it polls open, reputable, non-paywalled
RSS/Atom feeds, dedupes and ranks the latest items newest-first, optionally adds
a neutral one-line summary per top item plus a short overall brief from the
local LLM, and renders an intel digest panel in the HUD.

Honest framing: it **aggregates and summarizes public syndication feeds**. It
predicts nothing and surveils no one.

## Files

| File | Purpose |
|---|---|
| `manifest.toml` | Sandbox manifest (SANDBOX.md schema). `fetch_hosts` lists exactly the feed hostnames in `feeds.toml`; `net_hosts` is deliberately **empty** — the app has no direct egress, so its seatbelt profile is a flat `(deny network*)`. |
| `feeds.toml` | Category → list of RSS/Atom feed URLs. Every URL verified to return a parseable feed over HTTPS on 2026-06-13. |
| `main.py` | The app. Runs under `darwind` + `sandbox-exec`; reads `DARWIN_APP_SOCKET` / `DARWIN_APP_TOKEN` from env. |

## How it runs

`darwind` launches `main.py` under a generated seatbelt profile and hands it a
per-launch capability token. The app:

1. Connects to its per-app Unix socket (`state/ipc/apps/global-scan.sock`).
2. Loads `feeds.toml` (falls back to a built-in default set if missing).
3. Fetches every feed through the **daemon-mediated fetch proxy** at
   `state/ipc/apps/fetch.sock` (`op=fetch` only, token-gated, https-only, each
   URL authorized against `fetch_hosts`, body-capped; the daemon resolves,
   SSRF/rebind-guards, follows redirects inside the allow-list and returns the
   body). The app holds NO direct network of its own — there is no `urllib` in
   it. It parses the returned body with `xml.etree` (no heavy deps), reads
   title/link/published/source, dedupes by URL, sorts newest-first, keeps the
   top 20.
4. If the daemon-mediated generate proxy at `state/ipc/apps/generate.sock` is
   reachable, asks the local LLM through it (`op=generate` only, token-gated,
   256-token cap, rate-limited — the raw `inference.sock` is not reachable) with
   a neutral instruction and low `max_tokens` for a one-line summary per top
   item and a 2-sentence brief; on any failure falls back to extractive summaries
   (headline + first feed sentence). Real headline/source/time always shown.
5. Emits to the host socket, every line stamped with the token:
   - `{"type":"items","data":{"brief","items":[…],"fetched_at"}}`
   - `{"type":"status","data":{"feeds_ok","feeds_failed"}}`
6. Re-polls every ~10 min or on a host `refresh` command; stops on `stop`.

Each item: `{title, source, url, published, category, summary, flag}` where
`flag` is `"alert"` for breaking/urgent headlines (drives the HUD red accent),
else `"normal"`.

## Self-test (offline pipeline — no sandbox, no network)

```sh
.venv/bin/python3.11 apps/global-scan/main.py --selftest
```

Reports how many feeds resolved, item count after dedupe/rank, whether LLM
enhancement was used, the brief, and a sample parsed item.

Fetching is daemon-mediated, so a **standalone** run — no `DARWIN_APP_TOKEN`,
no fetch-proxy socket — resolves **0 feeds** and exercises the OFFLINE pipeline
(load / parse-shape / rank / summary) instead. It says so and exits 0 rather
than reporting a false failure for the direct egress this app deliberately gave
up; see `selftest()` in `main.py`. A live feed pull needs `darwind`.
