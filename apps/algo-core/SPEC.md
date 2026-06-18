# Algo-Core — SPEC

Event-driven trading engine with sandboxed WASM strategies, mandatory walk-forward validation, hard risk limits, and a signed audit log. Phase-4 implementation against `docs/SANDBOX.md`; HUD panel contract per `docs/HUD.md` §5.

**Scope statement: this is an engineering spec.** It defines correctness, isolation, auditability, and safety properties of an order-management system. It makes **no profitability claims** — a strategy passing every harness in this document can still lose money.

## Sandbox contract (binding: `manifest.toml`)

- `net_hosts`: `api.binance.com`, `stream.binance.com`, `api.kraken.com`, `ws.kraken.com`, `clob.polymarket.com` — adapters may speak to these and nothing else.
- `fs_read = apps/algo-core/strategies` (WASM modules + configs), `fs_write = apps/algo-core/data` (SQLite, journals, keys).
- IPC: JSONL over `state/ipc/apps/algo-core.sock`, capability token per message.
- UI: `surface = "panel"`. Topics: `algo.prices`, `algo.signals`, `algo.orders`, `algo.positions`, `algo.pnl`.

## 1. Event-driven engine

Single-threaded deterministic core loop over one ordered event stream:

```
MarketEvent(seq, ts, venue, instrument, book/trade)   ← adapters
TimerEvent(seq, ts, interval)                          ← scheduler
  └─▶ strategy.on_event() ─▶ [SignalEvent]
        └─▶ risk gate ─▶ [OrderIntent] ─▶ adapter ─▶ FillEvent / RejectEvent
              └─▶ portfolio state, audit log, telemetry
```

- Every event carries a monotonic `seq`; the engine is a pure function of the event sequence + initial state. The same stream replayed produces the same orders — this is what makes backtest, walk-forward, paper, and live the *same code path* with different event sources.
- Adapters and telemetry run on separate threads/tasks feeding the core via bounded queues; the core never blocks on I/O.
- Clock discipline: event time (`ts`) is venue time; the engine never reads the wall clock inside strategy or risk code.

## 2. Strategies as sandboxed WASM modules

- Runtime: **wasmtime** with no WASI filesystem, network, clock, or random imports. A strategy is pure compute: it sees only what the engine passes in.
- ABI (versioned, `algo_abi = 1`): exported `on_event(ptr, len) -> (ptr, len)`; JSON in (event + read-only views of its own positions/working orders), JSON out (list of `SignalEvent`s). Linear-memory handshake via exported `alloc`/`free`.
- **Fuel metering**: per-call fuel cap (~10 ms equivalent); exhaustion = strategy fault. Memory cap 64 MiB. A faulting strategy is quarantined (engine keeps running, its orders are cancelled) and reported on `algo.signals` with `state: "quarantined"`.
- Strategies cannot place orders — they emit signals; only the engine's risk gate turns signals into `OrderIntent`s. The WASM boundary plus the manifest's network allowlist means strategy code can never exfiltrate or trade around the gate.
- Loaded from `apps/algo-core/strategies/<name>/{module.wasm, config.toml}`; config declares instruments, max signal rate, and the walk-forward certificate (§3).

## 3. Walk-forward validation harness

No strategy reaches paper or live without a current walk-forward pass.

- Rolling windows over recorded event streams: train `T` days → validate `V` days → roll forward by `V` (anchored or rolling, per config). Parameter selection happens only inside each train window; the validate window is touched once.
- A **purge gap** (≥ 1 day, configurable) between train and validate windows prevents leakage through overlapping horizons.
- Output: per-window metrics (return, max drawdown, turnover, fill assumptions used) and a pass/fail against configured floors. Pass produces a **certificate**: hash of (module.wasm + config + data range + harness version) stored in SQLite; the engine refuses to arm a strategy whose current wasm/config hash has no certificate. Stale data range (> 30 days old) = expired certificate.
- The harness replays through the identical engine + risk gate (§1), with explicit, conservative fill models (taker: cross + fee; maker: queue-position pessimistic; prediction markets: fill only inside the recorded book depth).

## 4. Risk limits and kill-switch

The risk gate sits between signals and adapters. All limits are per-config, enforced in the engine, not in strategies:

| Limit | Action on breach |
|---|---|
| Max position per instrument (units + notional) | Clip or reject intent |
| Max gross/net exposure across book | Reject |
| Max daily realized + unrealized loss | **Kill-switch** |
| Max order rate (per strategy, per venue) | Reject + quarantine on repeat |
| Price sanity band (vs last trade) | Reject |
| Prediction markets: max stake per market and per outcome, no order at price ≤ 0.01 or ≥ 0.99 | Reject |

**Kill-switch** (automatic on breach, or manual via IPC `kill` op, or voice through the daemon): cancel all working orders on every venue, halt the engine's order path, optionally flatten (config `flatten_on_kill`), latch. The latch survives restart (state in SQLite) and clears only via an explicit `arm` op carrying the capability token. Time-to-cancel target after trigger: < 2 s per venue, measured and logged.

## 5. Exchange adapters

Common trait: `subscribe(instruments) -> MarketEvents`, `place(OrderIntent) -> OrderAck`, `cancel`, `cancel_all`, `positions()`. All order placement idempotent via client order ids (`algo-<strategy>-<seq>`); reconnect logic replays open-order state before re-arming.

- **Binance / Kraken**: WS market data (`stream.binance.com`, `ws.kraken.com`), REST orders. Standard CLOB semantics.
- **Polymarket-style prediction markets** (`clob.polymarket.com`): binary-outcome CLOB; price ∈ (0,1) is implied probability; "sell" of an outcome is modeled as buying the complement — the adapter normalizes both venues' conventions into one book model so strategies and risk see a single representation. Resolution events close positions at 0/1 and are journaled like fills.
- Credentials (API keys) live in `apps/algo-core/data/keys.toml` (chmod 600, inside the app's own write grant — never in repo, never readable by other apps per seatbelt).

## 6. Signed order audit log

Append-only SQLite tables in `apps/algo-core/data/audit.db`:

```sql
orders(seq, ts, strategy, venue, instrument, side, qty, price, client_oid,
       state,            -- intent|acked|filled|cancelled|rejected|killed
       prev_hash, hash, sig)
```

- `hash = SHA-256(prev_hash || canonical_json(row))` — a hash chain; `sig = Ed25519(hash)` with a key generated on first run in `apps/algo-core/data/` . Every state transition is a new row; rows are never updated or deleted.
- `algo-audit verify` walks the chain and signatures and reports the first break. Tamper evidence, not tamper proofing: an attacker with the app's own write grant and key can rewrite history — the chain protects against partial edits and anything outside the sandbox grant, and pins what the engine *believed* it did.

## 7. Telemetry → HUD panel

| Topic | Rate | Payload |
|---|---|---|
| `algo.prices` | ≤ 10 Hz/instrument | `{venue, instrument, bid, ask, last}` |
| `algo.signals` | on event | `{strategy, instrument, direction, strength, state}` |
| `algo.orders` | on transition | audit-row mirror (minus sig) |
| `algo.positions` | on change + 1 Hz | `{instrument, qty, avg_px, mark, upnl}` |
| `algo.pnl` | 1 Hz | `{equity, realized, unrealized, drawdown, killed: bool}` |

Panel: equity curve (pnl), positions table, working-orders strip, a prominent KILLED banner when latched, and an `ARM/KILL` control that round-trips through the daemon socket with the token.

## 8. Milestones

1. Engine + event replay + SQLite journal; deterministic replay test (same stream → identical order log hash).
2. WASM host + fuel/memory caps + one reference strategy; quarantine path tested.
3. Walk-forward harness + certificates; engine refuses uncertified strategies.
4. Risk gate + kill-switch (latency measured); paper adapter.
5. Live adapters (Binance, Kraken, Polymarket-style) behind paper-first config; audit verify tool; HUD panel live.
