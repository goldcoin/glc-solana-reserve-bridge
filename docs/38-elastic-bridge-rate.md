# Elastic bridge rate

Two phases, both in this document. **Phase 2A** (2026-09-14, schema v37)
introduced the bridge-quote math, its persistence, and quote-aware
verification across all six routes at a fixed bridge rate of exactly 1.0.
**Phase 2B** (2026-09-15, no schema change) turns that fixed rate into a
live one: three verified price feeds, time-weighted smoothing, a staleness
gate, a warm-up gate, a band check, and the parks around them. The Phase
2A sections are kept as written; everything Phase 2B changed is in the
second half, starting at "Phase 2B".

This document is the canonical cross-reference target ("docs/38-elastic-
bridge-rate.md") named throughout `service/src/bridge_rate.rs`,
`service/src/ledger/`, `service/src/api.rs` and the settlement paths.

## Terminology

Bridge **rate**, bridge **fee**, bridge **quote** — and only those words.
Nothing in this design converts one asset into another: the bridge still
transfers existing GLC from pre-funded reserves. The rate is the ratio of
the two rails' prices, and a quote is the rate applied to one deposit.

## What was decided (2026-09-14)

| # | Decision |
|---|---|
| J-2 | The per-route `[fees]` table (bps) stays the fee. No `bridge_fee_pct` key is added, and no production fee value changes in Phase 2A. |
| J-3 | Phase 2B sources are fixed: Goldcoin L1 → NonKYC, Solana → Jupiter, Robinhood Chain → Uniswap pool. Exact endpoints, symbols, quote currencies, pool addresses and fee tiers are researched and documented in Phase 2B. **No live feeds in Phase 2A.** |
| J-4 | Goldcoin-sourced routes lock the settlement quote at the **first deposit observation**. The `POST /transfers` quote is indicative only. |
| J-5 | Fee accounting is **option (a)**: the bridge fee is persisted and accrued in the destination-canonical interpretation. No second, source-denominated fee. |
| J-6 | Phase 2B: with no valid "one window ago" reference (restart, feed gap) the affected routes **halt** until the full reference window exists. No degraded fallback, no last-known-price admission. |
| J-7 | Phase 2B may floor to destination precision with less than one destination atomic unit of residual retained by the bridge. **Phase 2A at rate 1.0 preserves current behaviour exactly** — no dust behaviour in live route processing. |
| J-8 | Phase 2A now, Phase 2B afterwards. **When Phase 2B ships, all six route `[fees]` are set to 300 bps.** Not before. |

## The math

```text
PRICE_SCALE = 1_000_000_000_000                       // a price of 1.0

gross_out = floor(gross_in * source_price_e12 / destination_price_e12)
fee_out   = floor(gross_out * fee_bps / 10_000)       // the existing fee rule, unchanged
net_out   = gross_out - fee_out
```

`gross_in` is what the depositor sent — anchored to the real deposit
exactly as before (`gross_amount_atomic`, checked byte-for-byte against
the observed Goldcoin output, or widened from the immutable on-chain
obligation). `gross_out`, `fee_out` and `net_out` are the destination-
asset figures in the canonical 8-decimal unit; `net_out` then goes through
the unchanged decimal conversion to the destination chain's own unit
(`net_destination_atomic`), with the unchanged exactness rule.

Every intermediate is `u128`, every step is checked, and no floating
point exists anywhere in `service/src/bridge_rate.rs` — a test asserts the
module also contains no HTTP client, no RPC client and no async runtime.
Two processes given the same integers produce the same integers.

**Phase 2A pins both prices to `PRICE_SCALE`** (`RateBook::fixed_unit`).
At a unit rate `gross_out == gross_in`, so `fee_out`/`net_out` are
bit-for-bit what `compute_fee_at_bps` produced before this change — the
signer messages, the reserve accounting and every API amount are
unchanged. `bridge_rate::tests::a_unit_rate_reproduces_the_fee_rule_bit_for_bit`
pins this across the fee rule's whole range; the synthetic 0.8 / 1.25 /
1/3 cases pin the math Phase 2B will drive.

## Where a quote is struck, and where it is locked

There is ONE `RateBook` per daemon process (`glc-bridge-daemon` builds it
from `[bridge_rate]` and hands the same value to the public API, both
Solana folds, both Robinhood folds and the Goldcoin deposit observation),
so no two components can quote one deposit differently.

| Route | Struck | Locked (= the settlement quote) |
|---|---|---|
| `GlcToSol`, `GlcToRhn` | `POST /transfers` — **indicative**, stored on the row unlocked | `Ledger::record_glc_deposit_observed_from`, when the deposit is first seen in a block; re-struck at that instant at the row's own `fee_bps` snapshot |
| `SolToGlc`, `SolToRhn` | the Solana indexer's fold | the fold (the obligation is already final) |
| `RhnToGlc`, `RhnToSol` | the Robinhood fold (`robinhood::fold`) | the fold (the observation is already `Final`) |
| `GET /quote` | a preview, through the same `RateBook::quote` | never |

A reorg that orphans the observing block (`mark_glc_reorged`,
`goldcoin_rollback_reorg`) clears `quote_locked_at` together with the
outpoint; the re-observation strikes and locks a fresh quote. A quoted row
whose quote is not locked cannot settle
(`ConversionError::QuoteNotLocked`).

Phase 2A invariant at the lock: the observation-time quote reproduces the
reservation's fee and net exactly (it must — the rate is 1.0), so only the
quote columns are written and the row's amounts and reservation are
untouched. A lock that would NOT reproduce them (a row with corrupted
amounts, or a fee snapshot the fee rule refuses) is not written; the
observation is still recorded — the deposit is real and must stay visible
and refundable — and settlement then refuses the row exactly as a
corrupted reservation was refused before v37. Phase 2B replaces that
branch with re-reserving from the lock and parking an unquotable deposit.

## Schema v37

Eight nullable columns on `bridge_requests`, added in place with
column-level idempotent `ALTER TABLE ... ADD COLUMN`:

| column | meaning |
|---|---|
| `quote_source_price_e12` | source rail price × 10¹² |
| `quote_destination_price_e12` | destination rail price × 10¹² |
| `quote_gross_out_atomic` | `floor(gross_in × src / dst)`, canonical |
| `quoted_at` | when the quote was struck |
| `quote_expires_at` | `quoted_at + quote_lifetime_secs` — metadata; nothing reads it |
| `quote_source_feed_at`, `quote_destination_feed_at` | feed read times (audit; the quoting instant at a fixed rate) |
| `quote_locked_at` | when the quote became the settlement quote; NULL while indicative |

`CHECK`s make the first seven all-NULL or all-set, prices positive, and a
lock impossible without a quote. **NULL means legacy**: no row is
backfilled, and a request created before v37 keeps every quote column
NULL and keeps settling through `amount_conversion::verify_fee_breakdown`
at an implicit unit rate, exactly as before.

The existing amount columns keep their names with sharpened meaning under
a quote: `gross_amount_atomic` = `gross_in`; `fee_amount_atomic` =
`fee_out`; `net_amount_atomic` = `net_out`; `net_destination_atomic` =
`net_out` in the destination chain's unit. `ledger::RequestAmounts` gained
`quote: Option<BridgeQuote>`; every production pricing site supplies one,
and the ledger refuses to store amounts that are not the quote's own
figures (`LedgerError::BridgeQuote`).

## Verification — one entry point

`BridgeRequest::verify_breakdown` is the canonical amount verification for
a row, and every settlement, recovery and reconciliation path calls it
instead of choosing a verifier itself:

- quoted row → must be locked, and `gross_out`/fee/net must reproduce
  from `(gross_in, prices, fee_bps)` — `bridge_rate::verify_quoted_breakdown`,
  refusing with `ConversionError::QuoteMismatch`;
- legacy row → `verify_fee_breakdown`, refusing with `AccountingMismatch`.

The returned breakdown is always the freshly recomputed one; the stored
figures are only ever compared against. Callers: `signing::attestation`
(release and completion), `orchestrator::submit_release`,
`signing::goldcoin_vault::DevLedgerPayoutSource::rederive_plan`,
`goldcoin::payout_recovery`, `robinhood::settlement::authorize_payout`,
`solana::reconcile_request`. The Solana completion attestation's
cross-check against the on-chain obligation amount prices that amount
through `BridgeRequest::expected_net_for_gross` — the row's own persisted
rate, never a live one.

## Signers

Unchanged: wire formats (`shared::claim`), signer keys, multisig, custody
policy, the Solana program, the Robinhood contract. Remote signers never
query a price source; they sign what the daemon re-derives from the
persisted row, exactly as before. At a unit rate the bytes are identical
to a legacy row's —
`signing::attestation::tests::a_quoted_request_signs_exactly_the_bytes_a_legacy_request_signs`
proves it for both the release claim and the completion claim, and the
pre-existing golden-layout tests now run against quoted rows.

## Accounting (J-5, option a)

`fee_amount_atomic` is `fee_out` — the bridge fee in destination-asset
canonical units — and it is what `reserve_ledger.accrued_fees_atomic`
accrues, on the SOURCE reserve, in `mark_release_confirmed` /
`mark_goldcoin_completion_confirmed`, exactly as before. The fee is still
physically retained on the source reserve; the accrued figure reports that
retention valued in the destination asset at the request's own quoted
rate. At a unit rate the two are the same number. There is deliberately
no second, source-denominated fee.

## API

- `GET /quote` gains `bridge_quote` (`bridge_rate`, `source_price_e12`,
  `destination_price_e12`, `gross_in_amount`, `gross_out_amount`,
  `fee_bps`, `bridge_fee_amount`, `net_out_amount`, `quoted_at`,
  `quote_expires_at`). The pre-quote fields (`gross_amount`, `fee_amount`,
  `net_amount`, display strings) are kept for existing clients;
  `gross_amount` keeps meaning what the user sends.
- `POST /transfers` returns the indicative quote as `bridge_quote`
  (`locked_at` absent).
- `GET /transfers/{id}` reports the row's quote as `bridge_quote`, with
  `locked_at` once it is the settlement quote; absent for a legacy row.

## Config

```toml
[bridge_rate]
mode = "fixed_unit"        # Phase 2B made the section and its mode REQUIRED
quote_lifetime_secs = 60   # default
```

Metadata only in Phase 2A. `[fees]` is unchanged and remains the fee.
(Phase 2A originally allowed the section to be absent; since Phase 2B a
config without `[bridge_rate].mode` does not load — see the Phase 2B
"Config" section.)

## Rounding

Unchanged in live route processing: at rate 1.0 every figure is what it
was, the fee floors as before, and the destination exactness rule
(`NotExactlyRepresentable`) still refuses rather than rounds. The
`bridge_rate` unit tests exercise the generic math at 1.0 / 0.8 / 1.25 /
1/3 (`gross_out` floors; `gross_out == fee_out + net_out` structurally).
Destination-precision flooring with sub-unit residual (J-7) is Phase 2B.

## Explicitly NOT in Phase 2A

No live feeds. No smoothing window. No staleness gate. No band check. No
rate-dependent route halt. No non-1.0 rate reachable through production
configuration.

# Phase 2B — the live bridge rate (2026-09-15)

Implemented on the founder's approval of 2026-09-14 ("FOUNDER APPROVED —
PROCEED WITH PHASE 2B"). No schema change: Phase 2B writes the v37 quote
columns with real prices instead of `PRICE_SCALE`. The Phase 2A math,
verification entry point, signer bytes and accounting are unchanged; what
changes is where the two prices come from and what happens when they
cannot be trusted.

Nothing in Phase 2B is live in production until the daemon is restarted
on a config whose `[bridge_rate]` says `mode = "live"`. **This change is
not deployed** — see "Rollout" for the sequence.

## Verified feed manifest

Three rails, three sources, fixed by J-3. Every endpoint below was read
live during implementation; the evidence column is what came back. No
feed takes a credential. USDT is taken at par with USD throughout (the
one stated assumption).

### A. Goldcoin L1 — NonKYC

| | |
|---|---|
| Endpoint | `GET https://api.nonkyc.io/api/v2/market/getbysymbol/GLC_USDT` |
| Provenance | the `market/getbysymbol/` call of NonKYC's own client library (the `nonkycapinode` package, `nonkycApi.js`, base `https://api.nonkyc.io/api/v2`); `GLC_USDT` is the URL-safe spelling the endpoint accepts alongside `GLC%2FUSDT` |
| Fields read | `lastPrice` (decimal **string**, USDT per GLC), `updatedAt` (unix ms → `feed_at`), `isActive` / `isPaused` (either wrong ⇒ no sample), `symbol` (cross-checked against the configured market) |
| Not read | `lastPriceNumber` and every float-typed duplicate — the string is the exact figure |
| Evidence 2026-09-14 | `{"symbol":"GLC/USDT","lastPrice":"0.000156705","lastTradeAt":1789429916125,"isActive":true,"isPaused":false,"updatedAt":1789429924581}` |
| Evidence 2026-09-15T01:16Z | `{"symbol":"GLC/USDT","lastPrice":"0.000155836","updatedAt":1789434984754,"isActive":true,"isPaused":false}` |

The `GLC_BTC` market also exists on NonKYC (`lastPrice` ≈ 1.99e-9 BTC on
2026-09-14); it is NOT used — the USDT market gives USD directly with no
second conversion.

### B. Solana — Jupiter Price API v3

| | |
|---|---|
| Endpoint | `GET https://lite-api.jup.ag/price/v3?ids=Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump` |
| Provenance | `developers.jup.ag/docs/api-reference/price/v3/price` ("GET https://api.jup.ag/price/v3?ids={mints}"). `lite-api.jup.ag` is Jupiter's keyless host and answers the identical document; `api.jup.ag` takes an `x-api-key`, which this bridge never sends |
| Mint | the reserve token mint from `[solana].reserve_token_mint` — the same mint the Solana program holds |
| Fields read | `usdPrice` (JSON **number**, read as raw text through `serde_json::value::RawValue` and parsed by `decimal::parse_price_e12`, never through `f64`), `blockId` (logged), `decimals` (cross-checked = 6) |
| `feed_at` | receipt time. Jupiter's document has no timestamp; `blockId` is the slot of the last swap the price was computed at, and on this thin market that slot was ~2 h old while the document was fresh — using it as the staleness clock would halt the Solana rail whenever nobody trades, which is not what staleness means. A mint absent from the response is a refusal, not a zero |
| Evidence 2026-09-14 | `{"Hn6K…pump":{"usdPrice":0.000044367007702948316,"blockId":447085739,"decimals":6,"liquidity":9033.73,"priceChange24h":4.96,"launchpad":"pump.fun"}}` |
| Evidence 2026-09-15T01:16Z | `{"Hn6K…pump":{"usdPrice":0.000045416004635705604,"blockId":447115380,"decimals":6,"liquidity":9024.74}}` |

### C. Robinhood Chain — Uniswap v4 pool (ETH / GLC)

The Robinhood GLC market is a Uniswap **v4** pool: a `bytes32` `PoolId`
inside the singleton `PoolManager`, not a contract of its own, paired
against NATIVE ETH, with a hook. Read on-chain through the official
`StateView` lens.

| | |
|---|---|
| Chain | Robinhood Chain mainnet, `chainId 4663` (`cast chain-id` against `[robinhood.indexer].rpc_url`) |
| PoolManager | `0x8366a39cc670b4001a1121b8f6a443a643e40951` |
| StateView | `0xf3334192d15450cdd385c8b70e03f9a6bd9e673b` |
| PoolId | `0x70028c45e0efeea7c73d9144310f8f5cd76a7e0a0e03f945cf21b59dc55ac955` |
| PoolKey (from the `Initialize(bytes32,address,address,uint24,int24,address,uint160,int24)` event at block 47917900) | `currency0` = native ETH (`0x0000…0000`), `currency1` = GLC `0xaf0172DDEa4ce60dB3EBab05748A00B14fC8e433` (= `[robinhood].expected_token`, 18 dp), `fee` = 0 (dynamic, hooked), `tickSpacing` = 200, `hooks` = `0xe5e702641ea86f4ae6cc3cdaed2b886f976be044` |
| Calls per poll | `StateView.getSlot0(poolId) → (sqrtPriceX96, tick, protocolFee, lpFee)`, `StateView.getLiquidity(poolId) → uint128` (zero ⇒ no market ⇒ no sample), `eth_getBlockByNumber(latest)` for the head timestamp |
| ETH/USD leg | NonKYC `ETH_USDT` (`lastPrice` `"2499"` at 2026-09-15T01:16Z, `updatedAt` 1789434983758) — the same client and host the Goldcoin rail already depends on |
| `feed_at` | the OLDER of the head block timestamp and the ETH market's `updatedAt`, so a stale leg on either side ages the whole rail |
| Evidence 2026-09-14 | `sqrtPriceX96 = 666733903468254117567079170768462`, `liquidity = 29277002188455995766012` |
| Evidence 2026-09-15T01:16Z | `getSlot0 → (671699865855380224739509192226459, 180913, 0, 0)`, `getLiquidity → 29277002188455995766012`, head block 63261240 |

Price derivation (`feeds/uniswap_v4.rs`), exact integers only:

```text
P            = sqrtPriceX96² / 2^192          // GLC-wei per ETH-wei (currency1 per currency0)
GLC_usd_e12  = ETH_usd_e12 · 2^192 / sqrtPriceX96²
             = floor( floor(ETH_usd_e12 · 2^96 / sqrtPriceX96) · 2^96 / sqrtPriceX96 )
```

Two exact 256÷128 divisions (`bridge_rate::bigmath::U256`) so no
intermediate leaves 256 bits. At the 2026-09-15 reading this gives
`34_767_615` e12 = $0.000034767615 per GLC on Robinhood.

A Uniswap **v3** factory also exists on Robinhood Chain
(`0x1f7d7550b1b028f7571e69a784071f0205fd2efa`); the GLC market is the v4
pool above, and the v3 factory is not used.

### What the three prices imply today

| rail | USD per GLC (2026-09-15T01:16Z) |
|---|---|
| Goldcoin L1 | 0.000155836 |
| Solana | 0.000045416 |
| Robinhood | 0.000034768 |

So `GlcToSol ≈ 3.43`, `SolToGlc ≈ 0.291`, `GlcToRhn ≈ 4.48`, `RhnToGlc ≈
0.223`, `SolToRhn ≈ 1.31`, `RhnToSol ≈ 0.766`. **These are the rates the
live book will strike on day one.** The rails have been far from 1.0 for
some time and move tens of percent intraday, so the band gate will fire
on real traffic; that is the design, not a fault — a parked deposit is
resumed at its locked quote by an operator, or refunded.

## Config

```toml
# Both keys of this section are REQUIRED (a daemon must never price at
# 1.0 because an operator forgot a line). Everything else has the
# documented default.
[bridge_rate]
mode                 = "live"   # "live" | "fixed_unit"
price_window_secs    = 360      # W: the smoothing window and the reference distance
quote_lifetime_secs  = 60       # GET /quote validity; POST /transfers indicative-quote validity
rate_band_pct        = 25       # movement vs one window ago beyond which new deposits are parked
price_staleness_secs = 120      # a newest sample older than this halts the rail
poll_interval_secs   = 20       # default; must not exceed price_staleness_secs

[bridge_rate.feeds.goldcoin]
kind     = "nonkyc"
base_url = "https://api.nonkyc.io/api/v2"
market   = "GLC_USDT"

[bridge_rate.feeds.solana]
kind     = "jupiter"
base_url = "https://lite-api.jup.ag/price/v3"
mint     = "Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump"

[bridge_rate.feeds.robinhood]
kind               = "uniswap_v4"
state_view         = "0xf3334192d15450cdd385c8b70e03f9a6bd9e673b"
pool_id            = "0x70028c45e0efeea7c73d9144310f8f5cd76a7e0a0e03f945cf21b59dc55ac955"
glc_is_currency1   = true
currency0_decimals = 18
currency1_decimals = 18
eth_usd_market     = "ETH_USDT"
```

Validation (`config::resolve_bridge_rate`): every `*_secs` > 0, window ≤
one day, `poll_interval_secs ≤ price_staleness_secs`, band 1–10000 %,
feeds required in `live` mode and refused in `fixed_unit` mode, `https://`
only, market spelled `BASE_QUOTE`, mint a valid pubkey, `state_view` an
EVM address, `pool_id` 32 nonzero bytes. The Robinhood feed reads through
`[robinhood.indexer].rpc_url` — no second RPC endpoint. **No secrets: no
API keys, no signing material, nothing new in the config that must be
protected.**

`fixed_unit` mode is exactly Phase 2A: `RateBook::fixed_unit`, both prices
`PRICE_SCALE`, no feeds, no poller, no gates. The production config today
has no `[bridge_rate]` section at all; the section is now required, so
**step 1 of any rollout is adding `[bridge_rate] mode = "fixed_unit"`**
(see "Rollout").

`[fees]` is unchanged in this PR and remains the fee. **At the Phase 2B
production release all six routes are set to 300 bps (J-8)** — the exact
edit is under "Rollout".

## The live book (`bridge_rate::live`, `bridge_rate::smoothing`)

One `LiveBook` per daemon (`Arc`, shared by the API, both indexers, the
Robinhood folds, the settler, the admin API and the ops collector), three
`PriceHistory` rails inside it, one `Mutex`. The poller
(`bridge_rate::feeds::run_poller`) polls each feed once per
`poll_interval_secs` and records either a `Sample {feed_at, observed_at,
price_e12}` or a failure; a failing feed never delays the others.

**Smoothing.** Each rail's price over a window `[end − W, end)` is the
time-weighted average of its samples treated as a step function
(`PriceHistory::twap`): each sample's price holds from its `feed_at` until
the next sample's, weights are whole seconds, the sum is `u128`, the
result is `floor(Σ price·secs / Σ secs)`. Irregular intervals are the
normal case and need no special handling. The history is append-only in
`feed_at` (a non-newer sample is dropped), pruned to `now − 2W −
staleness`, and capped at `MAX_SAMPLES = 4096` (at 20 s polls two windows
hold ~36 samples; the cap is a bound, not a target). Deterministic:
`smoothing::tests::irregular_intervals_weight_each_price_by_exactly_how_long_it_held`,
`…::a_gap_longer_than_the_staleness_bound_invalidates_the_history_behind_it`,
`…::the_average_is_deterministic_and_cannot_overflow_at_the_price_ceiling`
and the other pins there.

**Verdicts, in order**, for a route at `now` (`LiveBook::route_rate`):

1. `FeedUnavailable` — the rail has no samples, or its last poll failed
   and nothing usable is held.
2. `FeedStale` — the rail's newest sample is older than
   `price_staleness_secs`.
3. `WarmingUp` — the CURRENT window `[now − W, now)` or the REFERENCE
   window `[now − 2W, now − W)` is not covered by gap-free history (a
   gap wider than `price_staleness_secs` inside a window invalidates the
   history behind it). This is J-6: after a start or a feed gap the route
   stays halted until two full windows of continuous history exist —
   `2 × price_window_secs = 12 min` at the defaults. Nothing stands in
   for the missing reference.
4. Otherwise the rate is struck: `rate = source_twap / destination_twap`,
   and its movement against the reference rate is measured in basis
   points with exact 256-bit math (`movement_bps = |a·d − b·c| · 10 000 /
   (b·c)` over the four prices). Movement STRICTLY GREATER than
   `rate_band_pct × 100` bps is a breach; exactly at the band is admitted.

A breach does not refuse the rate: the quote is struck and returned
flagged (`StruckQuote::band`), because a deposit observed under a breach
must be parked WITH its locked quote (below).

**No fallback, ever.** The history is the only state and it only holds
what a feed printed. A stopped feed ages into `FeedStale`; a hole ages
into `WarmingUp` until it leaves both windows. There is no "last known
good price" anywhere in the service.

## Where each verdict lands

| Site | Refused (`feed_unavailable` / `feed_stale` / `warming_up`) | Band breach |
|---|---|---|
| `GET /quote` | `503` `{"error":…,"reason":"bridge_rate_feed_stale"}` | quote returned with `movement_bps`, `band_exceeded: true` |
| `POST /transfers` | `503` with the reason | `503` `{"reason":"bridge_rate_band_exceeded","movement_bps":…,"band_bps":…}` — no reservation is created |
| `GET /routes` | route `available: false`, `availability_reason` = the reason, `bridge_rate.status` = the same reason | `available: false`, `availability_reason: "bridge_rate_band_exceeded"`, `bridge_rate.status: "ok"` with `band_exceeded: true`, `movement_bps`, `band_bps` and the live `bridge_rate` |
| Goldcoin deposit observation (`GlcToSol`, `GlcToRhn`) | deposit recorded, NO quote written, parked `ManualReview` under the reason (`GlcObservationOutcome::BridgeRateParked`) | quote LOCKED, amounts re-reserved, parked `bridge_rate_band_exceeded` |
| Solana fold (`SolToGlc`, `SolToRhn`) | row created at unit-rate figures with no quote, parked under the reason | quote locked, parked `bridge_rate_band_exceeded` |
| Robinhood fold (`RhnToGlc`, `RhnToSol`) | same as the Solana fold | same |

Route availability ranking in `GET /routes` (`api::route_availability`):
operator gates (`route_disabled`, `onchain_paused`, `reserve_unavailable`
and the admission gates) → bridge-rate reason → missing liquidity probe
(`probe_unavailable`) → liquidity gates. A rail outage therefore shows as the rate reason, not as
`probe_unavailable`; and only the routes touching the failed rail are
halted — `SolToRhn`/`RhnToSol` keep quoting through a Goldcoin feed
outage (`api::tests::live_bridge_rate::only_the_routes_on_a_stale_rail_halt_and_a_fresh_sample_reopens_them`).

## Lock semantics (J-4)

Unchanged in principle from Phase 2A, now with real prices:

- **Goldcoin-sourced** (`GlcToSol`, `GlcToRhn`): `POST /transfers` strikes
  an INDICATIVE quote (stored unlocked, `quote_expires_at = quoted_at +
  quote_lifetime_secs`). The settlement quote is struck at the deposit's
  FIRST observation in a block (`Ledger::record_glc_deposit_observed_from`)
  at the row's own `fee_bps` snapshot, and locked in the same
  transaction. When that quote moves the destination figures (the rate
  changed since creation) the row's `fee_amount_atomic`,
  `net_amount_atomic`, `net_destination_atomic` are rewritten from the
  lock and `reserved_liquidity` on the destination reserve is adjusted by
  the delta, in the same transaction. A delta the reserve cannot absorb
  (`delta > balance − protected − reserved`) leaves the reservation as it
  was and parks `insufficient_capacity_at_lock` — the locked quote then
  disagrees with the row's amounts and nothing can settle it until an
  operator refunds.
- **Solana- and Robinhood-sourced**: struck and locked at the fold — the
  obligation / observation is already final.
- A reorg that orphans the observing block clears the lock with the
  outpoint; re-observation strikes and locks afresh.
- **Settlement never reads live prices.** `BridgeRequest::verify_breakdown`
  recomputes from the persisted quote columns; `orchestrator::submit_release`,
  both attestation claims, the Robinhood payout authorization, Goldcoin
  payout recovery and reconciliation all go through it. Remote signers
  never contact a feed. Proof: `robinhood::fold::tests::bridge_quote::a_fold_under_a_band_breach_parks_with_the_locked_quote_and_resumes_at_it`
  moves the live book after the lock and shows the resumed row carries
  the locked figures; `…::the_locked_quote_survives_a_reopen_and_is_what_recovery_verifies`
  reopens the ledger; `signing::attestation::tests` pin the claim bytes.

## ManualReview under the live rate

Reasons (`Ledger::BRIDGE_RATE_MANUAL_REVIEW_REASONS`):

| reason | when | resume |
|---|---|---|
| `bridge_rate_feed_unavailable` | deposit seen while the rail had no usable price | no quote on the row ⇒ **not resumable**; refund |
| `bridge_rate_feed_stale` | deposit seen while the rail's newest sample was stale | same |
| `bridge_rate_warming_up` | deposit seen during warm-up (start, feed gap) | same |
| `bridge_rate_band_exceeded` | deposit priced under a band breach | Sol/Rhn-sourced: resumable, **at the locked quote** (`resume_manual_review_inbound` / `_cross_route`, guarded by `refuse_unless_quote_locked_in`) — no live repricing. Goldcoin-sourced: refund-only (existing policy) |
| `insufficient_capacity_at_lock` | Goldcoin lock moved the net beyond free capacity | refund-only |
| `destination_payout_out_of_bounds` | quoted net outside the program's / contract's live bounds (below) | refund-only for Goldcoin-sourced; Sol/Rhn-sourced refundable |

Rules: nothing auto-resumes (`auto_resume_manual_review = false` is not
touched and these reasons are never in the auto-resume set); a resume
uses the LOCKED quote and only that; a row without a locked quote
(`quote_locked_at` NULL, or quote columns stripped) is not resumable —
`LedgerError` from `refuse_unless_quote_locked_in`, pinned by
`robinhood::fold::tests::bridge_quote::a_band_park_stripped_of_its_quote_cannot_be_resumed`.

## Minimum / maximum checks before any signer

Under a live rate the destination net can leave the destination's bounds
even when the deposit itself was in range, so both settlement paths check
BEFORE asking a signer and park `destination_payout_out_of_bounds`:

- Solana releases: `Orchestrator::release_out_of_bounds` reads
  `min_transfer_amount` / `per_transfer_limit` live from `bridge_config`
  and parks via `Ledger::park_for_destination_bounds`; counted in
  `TickReport::releases_parked_out_of_bounds`, never as a submitted
  release (`orchestrator::tests::a_quoted_release_outside_the_programs_bounds_is_parked_before_any_signer_is_asked`).
- Robinhood payouts: `Settler::authorize_payout` reads `limits()` and
  parks when the amount is below `outbound_min` or above `outbound_max`
  (`robinhood::settlement::tests::a_quoted_payout_outside_the_contracts_bounds_is_parked_before_any_signer_is_asked`).

The Solana program's 200 000 floor, confirmation depths, throttles and
daily caps are untouched.

## Destination precision (J-7)

`net_out` is floored to the destination's own precision at quote time:
`destination_scale = 10^(8 − destination_decimals)` (100 for the 6-dp
Solana mint, 1 for Goldcoin and the 18-dp Robinhood token), `net_out =
floor(raw_net / scale) · scale`, and `dust_out = raw_net − net_out < one
destination unit` is recorded on the quote (`BridgeQuote::dust_out`,
`GET /quote` → `bridge_quote.dust_amount`). The residual stays on the
source reserve with the fee; it is never paid, never lost, never rounded
up. `bridge_rate::tests` hold a 10 000-case property test that `net_out %
scale == 0`, `dust_out < scale`, `gross_out == fee_out + net_out +
dust_out`; a zero scale is refused. At rate 1.0 (`fixed_unit`) every
figure is what Phase 2A produced.

## API additions

- `GET /quote` and the transfer views: `bridge_quote` gains
  `dust_amount`, `movement_bps`, `band_exceeded`.
- `GET /routes`: each route gains `bridge_rate` `{status: "fixed" |
  "live" | "refused", bridge_rate, source_price_e12,
  destination_price_e12, movement_bps, band_bps, band_exceeded}`.
- `ErrorBody` gains `reason`, `movement_bps`, `band_bps` on `503`s.

## Observability

- Admin API `GET /bridge-rate` (read-only): `mode`, `as_of`, the three
  tunables, per rail (`chain`, `status`, `smoothed_price_e12`,
  `reference_price_e12`, `newest_feed_at`, `newest_observed_at`,
  `sample_age_secs`, `sample_count`, `last_error`, `last_error_at`), per
  route (`route`, `status`, `bridge_rate`, `source_price_e12`,
  `destination_price_e12`, `reference_bridge_rate`, `movement_bps`,
  `band_exceeded`, `detail`).
- Metrics (`ops::collector::bridge_rate_gauges`, static names): per rail
  `glc_bridge_rate_{goldcoin,solana,robinhood}_price_e12`,
  `_sample_age_secs`, `_ok`; per route
  `glc_bridge_rate_{route}_rate_e12`, `_movement_bps`, `_admitting`.
- Logs: the daemon logs the resolved `[bridge_rate]` at start, every feed
  failure with the redacted URL (query strings are stripped from error
  text), every park with its reason. Never a credential.

## Feed HTTP hardening (`bridge_rate::feeds::FeedHttp`)

Connect and request timeouts; `redirect(Policy::none())`;
`https_only(true)` (tests use a loopback-only constructor); response body
bounded at 64 KiB and refused past it; only an `accept` header is sent; no
retries inside a poll — the next tick is the retry, and a failure is a
recorded state, not a loop. `feeds/tests.rs` drives every failure class
(malformed, 500, 404, redirect, zero, negative, non-numeric, overflow,
paused market, wrong market, oversized body, timeout, unreachable, stale
timestamp) against a loopback server and asserts no sample is recorded.

## Proofs in the test suite

| claim | test |
|---|---|
| a route halts on a failed/stale rail and only that rail's routes | `api::tests::live_bridge_rate::only_the_routes_on_a_stale_rail_halt_and_a_fresh_sample_reopens_them`, `feeds::tests` staleness and failure cases |
| a cold book halts every route with its reason; warm-up halts until two full windows | `api::tests::live_bridge_rate::a_cold_book_halts_every_route_with_its_reason_and_a_warm_one_opens_them`, `bridge_rate::tests::a_live_book_refuses_until_warm_then_quotes_and_flags_a_breach` |
| band at exactly the limit admits, one bp over parks; band reported separately and outranked by an operator pause | `bridge_rate::live::tests`, `api::tests::live_bridge_rate::{a_band_breach_is_reported_separately_and_refuses_new_requests, an_operator_pause_outranks_the_bridge_rate_in_the_reason}` |
| a fold under a refused rate parks unquoted, refundable, unresumable | `robinhood::fold::tests::bridge_quote::a_fold_under_a_refused_rate_parks_the_deposit_unquoted_refundable_and_unresumable` |
| Goldcoin observation parks without a lock / relocks and re-reserves / parks a band breach with the lock | `robinhood::fold::tests::bridge_quote::{a_goldcoin_observation_under_a_refused_rate_parks_the_deposit_without_a_lock, a_goldcoin_observation_at_a_moved_rate_relocks_and_rereserves_the_destination_net, a_goldcoin_observation_under_a_band_breach_locks_the_quote_and_parks}` |
| a parked deposit settles at the persisted quote after a restart | `robinhood::fold::tests::bridge_quote::{the_locked_quote_survives_a_reopen_and_is_what_recovery_verifies, a_fold_under_a_band_breach_parks_with_the_locked_quote_and_resumes_at_it}` |
| signer bytes unchanged | `signing::attestation::tests::a_quoted_request_signs_exactly_the_bytes_a_legacy_request_signs` |
| bounds parked before any signer | the two tests named under "Minimum / maximum checks" |
| flooring property | `bridge_rate::tests::{a_non_unit_rate_floors_the_net_to_the_destination_unit_and_keeps_the_dust, the_dust_is_always_below_one_destination_unit_and_never_reaches_the_net}` (10 000 cases), `robinhood::fold::tests::cross_route::rhn_to_sol_floors_a_net_that_cannot_be_spelled_at_the_mints_precision` |
| config: section/mode required, live manifest validated, feeds refused in fixed mode | `config::tests::{a_config_without_a_bridge_rate_mode_does_not_load, fixed_unit_mode_quotes_at_one_with_the_documented_defaults, live_mode_resolves_the_feed_manifest_and_validates_every_key}` |

## glc-admin in live mode

`glc-admin` never runs the poller. On a `mode = "live"` config it builds
an EMPTY live book, so any admin path that would strike a quote
(Robinhood deposit recovery) parks the row `bridge_rate_feed_unavailable`
rather than pricing it at 1.0 — the daemon's next fold is the only
quoting authority. Resumes and refunds do not quote and are unaffected.

## Rollout (proposed — NOT executed)

Preconditions: PR #110 (Phase 2A) and this PR merged; ledger already at
v37 from the Phase 2A deploy (Phase 2B has no migration).

1. **Config, fixed mode.** Add to `/etc/glc-bridge/config.toml`:
   ```toml
   [bridge_rate]
   mode = "fixed_unit"
   ```
   Restart the daemon on the new binary. Behaviour is Phase 2A exactly;
   confirm `GET /bridge-rate` reports `mode: "fixed_unit"` and `GET /routes`
   shows `bridge_rate.status: "fixed"` on all six.
2. **Fees.** Set all six routes to 300 bps (J-8) — the exact edit is
   below. Restart. Confirm `GET /quote` on every route reports
   `fee_bps: 300`.
3. **Live mode, admission closed.** Close inbound admission on all six
   routes (the existing operator gate), switch `[bridge_rate]` to the
   `mode = "live"` block above, restart. Watch `GET /bridge-rate`: every
   rail should go `empty → warming_up → ok` within `2 × price_window_secs`
   (12 min); `last_error` must stay null on all three. Compare each
   rail's `smoothed_price_e12` against the sources by hand.
4. **Open one route.** Reopen admission on `GlcToSol` only. Watch the
   first deposit: the observation log line, the locked quote on
   `GET /transfers/{id}`, the release amount in the attestation. Confirm
   the paid amount equals `net_destination_atomic` from the row.
5. **Open the rest** in the order `SolToGlc`, `GlcToRhn`, `RhnToGlc`,
   then the cross routes if enabled, one at a time, each after one
   settled transfer on the previous.
6. Leave `auto_resume_manual_review = false`. Brief the operators on the
   four `bridge_rate_*` reasons and on `resume` being at the locked
   quote.

## Rollback (proposed)

Any step can be reversed without touching the ledger:

- From live mode: set `mode = "fixed_unit"`, remove the `feeds` tables,
  restart. Quotes struck under live mode stay on their rows and settle at
  their locked figures — a rollback never reprices an existing request.
  Deposits parked `bridge_rate_*` stay parked for the operator.
- From the fee change: restore the previous `[fees]` values, restart.
  Existing rows keep their `fee_bps` snapshot.
- From the binary: the Phase 2A binary reads v37 rows written by Phase 2B
  (same columns, same verification); a row parked with a live quote
  settles under 2A at that quote. Rolling back past Phase 2A is the
  Phase 2A rollback (PR #110) — not affected by this change.

## The six-route 300 bps edit (J-8, at release, NOT in this PR)

Production `[fees]` today → at the Phase 2B release:

```diff
 [fees]
-SolToGlc = 600
+SolToGlc = 300
 GlcToRhn = 300
-RhnToGlc = 600
+RhnToGlc = 300
 GlcToSol = 300
 SolToRhn = 300
 RhnToSol = 300
```

Or, per route through the existing tool (restart afterwards):

```sh
glc-admin fees-set --config /etc/glc-bridge/config.toml --route SolToGlc --fee-percent 3 --note "elastic bridge rate release (J-8)" --execute
glc-admin fees-set --config /etc/glc-bridge/config.toml --route RhnToGlc --fee-percent 3 --note "elastic bridge rate release (J-8)" --execute
```

## Destination bounds (added 2026-09-20)

A live rate makes a destination payout rate-dependent, and the
destination chains' per-transfer limits do not move with it. The
admission-time check that keeps a quoted payout inside those limits —
and the `max_transfer_atomic` every route now publishes — is
docs/40-destination-bound-admission.md. Its buffer defaults to
`rate_band_pct`, for the reason given there.

## Explicitly NOT in Phase 2B

No change to signer keys, multisig, the Solana program, the Robinhood
contract, reserve wallet structure, the 200 000 floor, confirmation
depths, throttles, daily caps, the manual refund system, the
retained-cancel policy, the UI or the dashboards. No auto-resume. No
fallback price. No production fee value changed by this PR.
