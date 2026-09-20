# Destination-bound admission

**Hotfix, 2026-09-20.** No schema change. Branch
`fix/destination-bound-admission`.

This document is the canonical cross-reference target
("docs/40-destination-bound-admission.md") named in
`service/src/bridge_rate.rs`, `service/src/api.rs`,
`service/src/solana/indexer.rs` and `service/src/config.rs`.

## The defect

Every destination chain has a per-transfer ceiling the bridge does not
control: the Solana program's `BridgeConfig.per_transfer_limit` (set only
by `glc-admin set-limit`) and the Robinhood custody contract's
`limits().outboundMax`. Until this change **nothing at admission compared
a quoted payout against them.** At the fixed unit rate of Phase 2A that
did not matter — a Goldcoin deposit of X netted X less the fee, and the
program limit was far above any real deposit. Under the live bridge rate
(docs/38-elastic-bridge-rate.md, Phase 2B) a Goldcoin deposit is worth
many more Solana units: on 2026-09-18 the rate was ~16.7, so a 50 000 GLC
`GlcToSol` request quoted a payout of ~812 123 GLC (Solana) against a
50 000 program limit. `POST /transfers` admitted it, issued a deposit
address, the user funded it, and the request could only be discovered
unpayable at settlement — `Orchestrator::release_out_of_bounds` parked it
`destination_payout_out_of_bounds` (requests 4438 and 4483). The user's
funds were already in the vault.

The same shape exists on `GlcToRhn` (contract `outboundMax`) and, for a
Solana deposit that is already irreversible when it is seen, on
`SolToRhn`.

## The rule

**A route with a bounded destination admits a gross amount only if the
payout it quotes fits the destination's per-transfer limit with the
configured buffer to spare — decided BEFORE any ledger row, capacity
reservation or deposit address exists.** The settlement-time checks
(`Orchestrator::release_out_of_bounds`, `Settler::authorize_payout` and
the `destination_payout_out_of_bounds` park) are untouched and remain the
second layer: admission makes the park rare; it cannot make it
unreachable (see "Why the buffer is sufficient").

The contracts are not changed. Neither `per_transfer_limit` nor
`outboundMax` is raised or lowered by this work; the bridge adapts to
what its destinations will pay.

## One derivation

`bridge_rate::max_source_for_destination_limit(limit_canonical, prices,
fee_bps, destination_scale, buffer_bps)` is **the one canonical maximum**:
the largest source amount (canonical 8-decimal units, the figure the
depositor sends) whose quoted destination net does not exceed
`buffered_destination_limit(limit, buffer_bps) = ⌊limit × (10000 −
buffer_bps) / 10000⌋`.

It does not rearrange the quote formula. It binary-searches the monotone
integer function `gross_in ↦ net_out` computed by
`bridge_rate::quoted_breakdown` — the SAME arithmetic every quote, lock
and settlement verification uses (`gross_out = ⌊gross_in · src / dst⌋`,
`fee = ⌊gross_out · bps / 10000⌋`, `net = (gross_out − fee)` floored to
the destination's precision, J-7). There is therefore no second,
approximate formula that could disagree with the real one by a rounding
unit; `bridge_rate::tests::max_source_is_the_exact_boundary` proves
`net(max) ≤ buffered < net(max + 1)` over 4 000 deterministic samples of
prices (six orders of magnitude each way), fees, destination precisions
and limits from one unit to `u64::MAX`, with integer arithmetic only.

Inputs, and where each comes from:

| Input | Solana destination | Robinhood destination |
|---|---|---|
| limit | `per_transfer_limit` widened from the mint's live decimals (`SolanaAtomic::to_canonical`) | `outboundMax` floored from 18 dp (`RobinhoodAtomic::to_canonical_floor`); a value above the canonical `u64` range is clamped to `u64::MAX` (effectively unbounded — what the contract would say) |
| prices | the rate book's verdict for the route: unit at `fixed_unit`, the live `RailPrices` when it quotes, none when it refuses | same |
| fee | this route's `[fees]` entry | same |
| destination scale | `10^(8 − mint decimals)` (100 today) | 1 |
| buffer | `[bridge_rate] destination_limit_buffer_bps` | same |

`api::max_transfer_from(route, limits, fee, prices, buffer)` applies it
per route and takes the smaller of two bounds:

- the **destination-derived** maximum above, when the destination is
  bounded (Solana, or Robinhood with a contract configured); and
- the **source chain's own** per-deposit ceiling, which the chain enforces
  itself: the Robinhood contract's `inboundMax` for `RhnToSol`/`RhnToGlc`,
  the program's `per_transfer_limit` for `SolToGlc`/`SolToRhn` (the
  program applies the same figure to deposits and releases).

The answer is one of three: `Unbounded` (no bound on either end),
`Known(max)`, or `Unknown` — a bound exists but its limit, decimals, fee
or prices could not be read. **`Unknown` is never treated as
unbounded.**

## Where it is enforced

| Surface | Routes | Behaviour |
|---|---|---|
| `POST /transfers` (`BridgeApi::create_goldcoin_deposit_transfer`) | `GlcToSol`, `GlcToRhn` | After the quote is struck and the band checked, before the rolling window, before `create_request_from`: `gross > max` → **400** `reason = destination_payout_out_of_bounds` with `max_transfer_atomic` / `max_transfer_display` in the body. `Unknown` → **503** (`Upstream`): refused, never admitted on a guess. No row, no deposit address. |
| `POST /quote` | all six | The same check at the quote's own prices, so a quote is never published for an amount a create would refuse. Successful quotes carry `max_transfer_atomic`. |
| `SolanaIndexer` fold (`fold_to_robinhood`) | `SolToRhn` | A Solana deposit is irreversible when it is folded, so this is the EARLIEST park, not a refusal: `gross > max` at the fold's locked prices → parked `destination_payout_out_of_bounds` at the fold, before any Robinhood capacity is held, refundable on Solana. The contract is read once per tick and only on a tick with a Robinhood-bound deposit; an unreadable contract makes no decision (logged) — the settler's check still refuses — because a read failure must not park every deposit of a healthy route. |
| `GET /chains`, `GET /robinhood/reserve` | all six | Every `RouteView` carries `max_transfer_atomic` and `max_transfer_display`. A bounded route whose maximum is `Unknown` is **not advertised**: `available = false`, `availability_reason = destination_limit_unavailable` (ranked after every operator gate). |
| `GET /limits` | `GlcToSol` | `max_transfer_atomic` beside the program's own `per_transfer_limit`. |

Settlement (`Orchestrator::release_out_of_bounds`,
`Settler::authorize_payout`, `Ledger::park_for_destination_bounds`) is
unchanged.

`RhnToSol` needed no new enforcement: the contract refuses a deposit above
`inboundMax` before the bridge sees it, and the program limit is the
larger of the two at every rate seen so far. The published maximum is the
smaller of the two bounds so that, should the rate ever make the
program-derived figure the tighter one, the UI cap and the quote follow
it (`api::tests::destination_bound::rhn_to_sol_is_bounded_by_the_smaller_
of_inbound_max_and_the_program_limit`).

## The buffer, and why it is sufficient

`[bridge_rate] destination_limit_buffer_bps` (default: `rate_band_pct ×
100`, i.e. 2 500 = the established band; validated ≤ 10 000; honoured in
both rate modes). Admission stays this far below the destination limit.

Why one band is the right size: the quote a Goldcoin-sourced request is
admitted with is indicative (J-4); the settlement quote is locked at the
first deposit observation, and the live book refuses to lock a deposit
whose rate moved more than `rate_band_bps` against the reference one
window ago (`RouteRate::band_exceeded` → parked `bridge_rate_band_
exceeded`). An order admitted at the buffered maximum therefore locks, if
it locks at all, at a rate at most one band above admission, and

    net_locked ≤ net_admitted × (1 + band) ≤ limit × (1 − buffer) × (1 + band)

which with `buffer = band = 25 %` is `limit × 0.9375 < limit`. The
counter-case is the incident: without the buffer the same order nets
`limit × 1.25` after the same move and parks.
`api::tests::destination_bound::the_buffer_keeps_an_admitted_order_
payable_through_the_allowed_band` pins both.

What the buffer does NOT cover, deliberately: a deposit observed long
after admission can have drifted more than one band in total (each window
is bounded, the sum is not) — and the band check compares against one
window ago, not against the quote. That residue is exactly what the
settlement park exists for, and it is why the park stays.

## Public API contract (for the bridge UI)

- `GET /chains` → `routes[].max_transfer_atomic` (string, canonical
  8-decimal units of the SOURCE asset) and `routes[].max_transfer_display`
  (decimal string). **The UI must cap its amount entry at this value,
  render it ("Max … GLC"), and never derive or hardcode it.** It moves
  with the rate, the fee, the destination bound, the decimals and the
  buffer, and it is absent for an unbounded route or one that is not
  currently advertised.
- `POST /quote` → the same two fields on success; a gross above the
  maximum fails `400` with `reason = "destination_payout_out_of_bounds"`,
  `max_transfer_atomic` and `max_transfer_display` in the error body, so
  the UI can show the maximum in its validation message before the user
  can create the transfer.
- `POST /transfers` → the same refusal, with the same body. The UI is
  never the only enforcement layer.
- `GET /limits` → `max_transfer_atomic` / `max_transfer_display` for
  `GlcToSol`.
- Existing fields, `min_transfer_atomic` included, are unchanged.

## Operator notes

- The reported maxima at the 2026-09-18 live rate (Goldcoin 731 245 672
  e12, Solana 43 669 983 e12, 300 bps, limit 50 000 GLC): **2 308.76243559
  GLC** with the 25 % buffer (3 078.34991410 GLC unbuffered). Both are
  pinned in `bridge_rate::tests`; neither appears anywhere but a test.
- `glc-admin set-limit` and a Robinhood `setLimits` move the maxima
  immediately (the limits are read live on every listing, quote and
  create; the fold reads once per tick).
- A `GlcToSol`/`GlcToRhn` route reading `destination_limit_unavailable`
  means the program config, the mint's decimals, the contract or the
  rate could not be read — the same reads a create would fail on.
