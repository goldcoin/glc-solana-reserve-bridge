# Destination-bound admission

**Hotfix, 2026-09-20.** No schema change. Branch
`fix/destination-bound-admission`.

**What this is, and is not.** Large elastic payouts are INTENDED: at a live
rate of ~17 Solana units per Goldcoin unit, 50 000 GLC (L1) paying out
~830 000 GLC (Solana) is correct bridge economics. This document adds a
*guard*, not an economic cap: the bridge refuses — before a deposit address
exists — only what the destination chain literally cannot pay in one
release, and it publishes that figure so a UI can cap its entry at it. The
figure is derived from the destination's LIVE per-transfer limit, so it is
whatever the operators make it: the Solana program's `per_transfer_limit`
is admin-gated-immediate (`glc-admin set-limit --field per-transfer`, no
redeploy), the Robinhood contract's `outboundMax` is a 2-of-3 `setLimits`.
The 2026-09-18 incident was a LIMIT sized for a unit rate (50 000) meeting
an elastic one; the remedy is to size the limit for the intended payouts,
and then this guard only fires on a rate spike beyond that sizing.

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

The buffer shaves the admitted maximum to `(1 − b)` of the limit-derived
figure. With a limit sized for the intended payouts this is the intended
headroom against a rate move between admission and lock; if the founder
wants the full limit admissible, size L with the `1/(1 − b)` factor above
or lower `destination_limit_buffer_bps` — no code change either way.

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

## The limit is the economic knob

The published maximum scales linearly with the destination limit, with no
code or config change (`api::tests::destination_bound::raising_per_
transfer_limit_raises_the_maximum_without_a_code_change`). At the
2026-09-20 live rate (Goldcoin 758 565 116 e12, Solana 44 009 955 e12,
i.e. 17.24; 300 bps; 25 % buffer):

| `per_transfer_limit` (GLC on Solana) | max source (GLC L1) |
|---|---|
| 50 000 (today) | 2 242.94 |
| 1 000 000 | 44 858.79 |
| 2 000 000 | 89 717.59 |

Sizing rule for an intended maximum source S, a rate ceiling R to plan
for, fee f and buffer b: `L ≥ S · R · (1 − f) / (1 − b)`. Nothing here
decides L; `glc-admin set-limit` does, and docs/09-runbook.md's
per-transfer-limit section owns the operational consequences (deposit
direction, UTXO chunk target, per-transaction blast radius).

## Availability probe

`SolToGlc`'s public availability is probed at a "normal large deposit"
(docs/09-runbook.md, 2026-09-12). Before this change the probe WAS the
program's `per_transfer_limit`, which was right while the limit was a
normal size. A limit sized for elastic payouts (millions of Solana units)
would make the probe ask whether the Goldcoin reserve could fund the
largest deposit the program permits — and its "no" would close the
route's advertisement while every real deposit still admits. The probe is
now `min(per_transfer_limit, [service] sol_to_glc_probe_gross_atomic)`
(canonical units; default 5 000 000 000 000 = 50 000 GLC, today's limit —
so today's behaviour is bit-identical). The liquidity gates still decide
every deposit at its own size; the probe only decides what "available"
claims.

## Operator notes

- `glc-admin set-limit` and a Robinhood `setLimits` move the maxima
  immediately (the limits are read live on every listing, quote and
  create; the fold reads once per tick). Nothing about today's 50 000 is
  assumed anywhere but in tests that reproduce the incident.
- A `GlcToSol`/`GlcToRhn` route reading `destination_limit_unavailable`
  means the program config, the mint's decimals, the contract or the
  rate could not be read — the same reads a create would fail on.
- Requests 4438 / 4483 (locked nets 947 017.34 and 828 816.01 GLC on
  Solana) pay at any limit ≥ 947 018; pinned in `bridge_rate::tests::
  requests_4438_and_4483_against_candidate_per_transfer_limits` for
  1 000 000 and 2 000 000, settlement and admission verdicts separately.
