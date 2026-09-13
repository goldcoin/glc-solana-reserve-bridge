//! The minimal HTTP surface a client (the bridge frontend, or any other
//! caller) needs to submit and track a bridge request
//! (docs/15-post-phase6-audit.md P0 item 4). Before this module, the only
//! way to reach `Ledger::create_request`/`Ledger::get_request` was a
//! direct in-process Rust call or raw SQL — there was no network-facing
//! way for anything external to interact with the bridge at all.
//!
//! # What this exposes, and what it deliberately does not
//!
//! Read/write operations matched to what an external caller (the future
//! bridge UI) actually needs: bridge status (including, per direction,
//! whether new transfers are currently acceptable), transfer limits
//! (including the fixed 3% bridge fee rate), reserve availability, a
//! non-sensitive health summary, a server-authoritative quote, creating a
//! GLC -> Solana transfer (which requires reserving capacity and handing
//! back deposit instructions), looking up a transfer's lifecycle
//! (including confirmation progress) by id, a wallet-scoped list of a
//! caller's own transfers, aggregate bridge statistics, a real
//! reserve-balance history, a public settlement-event feed, a per-route
//! pre-check of the rolling-24h wallet windows on both legs
//! (`GET /routes/{route}/eligibility`, [`RouteWalletEligibilityView`]),
//! and — on their own paths, leaving every endpoint above unchanged —
//! the Robinhood reserve and the Robinhood contract's own transfer
//! limits.
//!
//! # The two Robinhood endpoints are separate on purpose
//!
//! `GET /robinhood/reserve` and `GET /robinhood/limits` are additional
//! paths, not extra fields on `GET /reserve` and `GET /limits`. Those two
//! keep their exact existing response shape, so a client that has never
//! heard of Robinhood sees no change at all.
//!
//! The separation is also an accounting statement. The Robinhood reserve
//! is a THIRD independent pool on a third chain: it is never summed with,
//! differenced against, or defaulted from the Goldcoin or Solana figures,
//! because one cannot cover the other. And Robinhood's limits are the
//! deployed custody contract's, read over `eth_call`
//! ([`crate::robinhood::public`]) — never the Solana program's
//! `BridgeConfig` values re-labelled, which would publish a limit neither
//! chain enforces.
//!
//! Both report `"not_configured"`/`"unavailable"` with null figures when
//! the underlying source is absent or unreachable. A zero would be a
//! claim about a reserve or a limit that this service does not actually
//! know, and it is never made.
//!
//! It never exposes: custody keys or any signing material (this module
//! never touches [`crate::signing`]), privileged admin operations (pause/
//! unpause/limit changes stay on `glc-admin`, gated by possession of the
//! admin keypair; the LOCAL subset of those operations is additionally
//! reachable through the separately-bound, authenticated
//! [`crate::admin_api`] listener — a deliberate boundary change recorded
//! in that module's own docs, and still never through THIS public
//! listener), rebalancing/custody-transition detail (those are
//! operator-only, `glc-admin rebalance-*`/`custody-*`), or infrastructure
//! detail (RPC URLs, database paths, raw indexer internals — that is what
//! `ops::health` is for, and that endpoint's own docs already say to bind
//! it privately for exactly this reason). Reserve figures here are
//! limited to *available capacity* — a derived, bounded number ("how much
//! can currently move") — not the raw `total_reserve_balance`/
//! `protected_minimum`/`reserved_liquidity` breakdown `ops::health`
//! reports for an operator audience. [`PublicHealth`] is likewise a
//! small, derived subset of `ops::health::HealthReport` — halted/not and
//! a couple of counts, nothing an attacker could use to infer
//! infrastructure shape.
//!
//! # Solana -> GLC has no "create" step here
//!
//! A GLC -> Solana transfer must reserve capacity and obtain a
//! request-specific Goldcoin deposit address before any Goldcoin
//! transaction can reference it — a fresh address, unique to that one
//! request, derived from the same 2-of-3 signer set as every other
//! request (`goldcoin::derivation::derive_request_vault`) and persisted
//! against the request (`Ledger::set_goldcoin_deposit_address`).
//! Attribution is by that address alone: no `OP_RETURN`, memo, or
//! amount-matching trick is required, so an ordinary wallet — enter an
//! address and an amount, click send — is enough. (Requests created
//! before this addressing scheme existed still resolve via the legacy
//! shared vault address + `OP_RETURN` path — see
//! `goldcoin::deposit`/`goldcoin::indexer` — but nothing created through
//! this endpoint uses that path anymore.) So `POST /transfers` exists
//! for that direction. A Solana -> Goldcoin
//! transfer works the other way around: the user calls
//! `deposit_to_reserve` directly on-chain themselves (a plain SPL
//! transfer plus this bridge's own instruction, requiring no interaction
//! with this service beforehand), and this service's Solana indexer picks
//! it up automatically. There is nothing to "create" here for that
//! direction — `GET /status`'s `next_solana_obligation_index` is the one
//! piece of information a caller needs to construct that transaction
//! themselves.
//!
//! # No federation-era or wrapped-token language
//!
//! This is a reserve-backed bridge, not a federated one, and it does not
//! wrap or mint anything (docs/15-post-phase6-audit.md §4/§20) — there is
//! deliberately no `/federation`, `/federation/rounds`, or similarly
//! shaped endpoint here, even though a pre-existing frontend built against
//! the old bridge expects some. Connecting that frontend to this service
//! is later integration work, not something this module should paper
//! over by inventing federation-shaped responses that don't correspond to
//! anything this bridge actually does.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};

use self::atomic::{AtomicI64, AtomicU64};
use solana_sdk::pubkey::Pubkey;

use crate::amount_conversion;
use crate::goldcoin::hex as glc_hex;
use crate::ledger::{
    CreateRequestOutcome, Direction, Ledger, LedgerError, RequestState, ReserveDirection,
    RouteWalletEligibility, TransferAddressFilter, WalletRole,
};
use crate::ops::indexer_status::IndexerStatus;
use crate::solana::accounts;
use crate::solana::rpc::SolanaRpc;

/// The exact, approved end-user copy for a direction that currently
/// cannot accept a new transfer — for ANY of the reasons `glc_to_sol_
/// available`/`sol_to_glc_available` can be `false` (operator pause on
/// either layer, rolling-24h-volume quota exhausted, or reserve/
/// protected-minimum capacity insufficient): deliberately a single,
/// cause-agnostic message, never a technical reason code, and never a
/// claim about automatic reopening — there is no midnight reset and no
/// automatic unpause (docs/09-runbook.md's 2026-08-22 update). A UI
/// wanting the specific cause should read the boolean/numeric fields on
/// [`BridgeStatus`]/[`BridgeStats`] instead of parsing this string.
pub const DIRECTION_UNAVAILABLE_MESSAGE: &str = "Bridge capacity reached for this direction.\nTransfers are temporarily paused while reserves are replenished.\nPlease check the official Telegram for reopening updates.";

/// The Solana custody program's OWN circuit breakers (`BridgeConfig.paused`
/// / `release_paused` / `deposit_paused`), as read from the live
/// `bridge_config` account, folded into every availability answer this
/// API gives about a route with a Solana leg.
///
/// These are a separate layer from this service's local reserve gates:
/// `glc-admin pause/unpause --direction solana` moves only the ledger's
/// `reserve_ledger` row, and `onchain-pause`/`onchain-unpause` moves only
/// the program (docs/09-runbook.md, "two pause layers"). Before this type
/// existed, `GET /status` and `GET /chains` consulted only the local
/// layer, so a direction the PROGRAM refuses — every `deposit_to_reserve`
/// reverting with `DepositDirectionPaused` — was still advertised as
/// `available: true`. The 2026-09-12 incident was exactly that: the
/// on-chain `deposit_paused` flag set 2026-09-09 was never cleared by the
/// launch (which only unpaused the local Solana row), and both endpoints
/// kept advertising `SolToGlc` and `SolToRhn` while every deposit failed
/// at simulation.
///
/// Which flag gates which route follows the program, not the route name
/// (`programs/glc-reserve-bridge/src/instructions/{deposit_to_reserve,
/// release_from_reserve}.rs`): `deposit_paused` guards `deposit_to_reserve`
/// — the SOURCE leg of `SolToGlc` AND `SolToRhn` — and `release_paused`
/// guards `release_from_reserve` — the DESTINATION leg of `GlcToSol` AND
/// `RhnToSol`. `paused` (global) guards both. The Goldcoin<->Robinhood
/// routes never touch the program and are never affected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SolanaProgramPause {
    pub paused: bool,
    pub release_paused: bool,
    pub deposit_paused: bool,
}

impl SolanaProgramPause {
    /// The fail-closed answer for when `bridge_config` could not be read
    /// or decoded: every Solana leg is treated as paused. "Unknown" must
    /// never render as `available: true` — the same rule
    /// [`route_availability`] applies to a failed ledger read.
    pub const UNKNOWN: SolanaProgramPause = SolanaProgramPause {
        paused: true,
        release_paused: true,
        deposit_paused: true,
    };

    pub fn from_config(config: &accounts::BridgeConfigSnapshot) -> SolanaProgramPause {
        SolanaProgramPause {
            paused: config.paused,
            release_paused: config.release_paused,
            deposit_paused: config.deposit_paused,
        }
    }

    /// Whether the program would currently refuse `direction`'s Solana
    /// leg. `false` for a direction with no Solana leg.
    pub fn blocks(self, direction: Direction) -> bool {
        (direction.source_is_solana() && (self.paused || self.deposit_paused))
            || (direction.destination_is_solana() && (self.paused || self.release_paused))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BridgeStatus {
    pub goldcoin_paused: bool,
    pub solana_paused: bool,
    pub vault_address: String,
    pub next_solana_obligation_index: u64,
    /// Whether a NEW `GlcToSol` transfer can currently be created: the
    /// Solana reserve (this direction's destination — see
    /// [`Direction::destination_reserve`]) is unpaused, has capacity
    /// above zero, has rolling-24h-volume quota remaining, AND the Solana
    /// program itself is not refusing this direction's leg
    /// ([`SolanaProgramPause`]: `paused`/`release_paused` here). Derived,
    /// never a raw infrastructure detail — an in-flight transfer already
    /// reserved is unaffected either way.
    pub glc_to_sol_available: bool,
    /// Same as [`BridgeStatus::glc_to_sol_available`] for `SolToGlc`,
    /// whose destination is the Goldcoin reserve and whose Solana leg is
    /// gated on-chain by `paused`/`deposit_paused`.
    pub sol_to_glc_available: bool,
    /// Whether `GlcToSol`'s (release, direction byte 0) rolling-24h-volume
    /// window is currently exhausted — `rolling_volume_remaining <
    /// min_transfer_amount`, i.e. no further transfer of any legal size
    /// could succeed right now. A live, read-only projection of on-chain
    /// state (`RollingVolumeWindow` vs `BridgeConfig::rolling_volume_
    /// limit`, [`accounts::rolling_volume_remaining`]) — this field
    /// itself never sets any pause; separately, this service's own
    /// background tick (`crate::quota`) engages this direction's local
    /// pause once it observes the same exhaustion, and unlike the
    /// on-chain window's own automatic reset, that local pause never
    /// clears itself — see [`DIRECTION_UNAVAILABLE_MESSAGE`]'s docs and
    /// docs/09-runbook.md's 2026-08-22 update.
    pub glc_to_sol_quota_exhausted: bool,
    /// Same as [`BridgeStatus::glc_to_sol_quota_exhausted`] for `SolToGlc`
    /// (deposit, direction byte 1).
    pub sol_to_glc_quota_exhausted: bool,
    /// Raw atomic units still available in `GlcToSol`'s current rolling-
    /// 24h-volume window — `0` when fully exhausted, up to the full
    /// `rolling_volume_limit` right after a fresh bucket reset. GLOBAL and
    /// PER DIRECTION: one `rolling_volume_limit` bounds both directions,
    /// each tracked in its own window (docs/09-runbook.md 2026-08-22).
    pub glc_to_sol_rolling_volume_remaining: AtomicU64,
    /// Same as [`BridgeStatus::glc_to_sol_rolling_volume_remaining`] for
    /// `SolToGlc`.
    pub sol_to_glc_rolling_volume_remaining: AtomicU64,
    /// Whether NEW `SolToGlc` obligations are currently admitted — a
    /// separate signal from [`BridgeStatus::goldcoin_paused`] (see
    /// `Ledger::set_admission`/docs/09-runbook.md's "Admission control
    /// (Solana->Goldcoin)" section). `false` means EITHER an operator has
    /// deliberately closed admission (`glc-admin close-admission`) OR the
    /// automatic confirmed-liquidity gate has closed it because confirmed
    /// unreserved Goldcoin headroom fell into the admission safety buffer
    /// (docs/09-runbook.md's "Confirmed-liquidity admission safety
    /// buffer"). A newly observed on-chain deposit still gets folded (its
    /// tokens are already locked on Solana regardless) but parks in
    /// `ManualReview` instead of processing normally. Already-accepted
    /// obligations are never affected by this either way — a UI should
    /// read `false` here as "not accepting new transfers right now",
    /// distinct from a reserve-health failure.
    ///
    /// The two causes are deliberately NOT distinguished here, matching
    /// this API's disclosure posture everywhere else (see
    /// [`DIRECTION_UNAVAILABLE_MESSAGE`] and `RecipientEligibility`'s own
    /// note): the user-facing answer is identical, and which of the two
    /// closed it — along with the raw headroom figures — is an operator
    /// detail, available on `glc-admin status`, the admin API and
    /// `/metrics`, never on the public endpoint.
    ///
    /// # The name became accurate in v25 — it is no longer an alias
    ///
    /// This field's name was wrong from the day `RhnToGlc` shipped until
    /// schema v25: it reported `GoldcoinReserve`'s two reserve-wide
    /// admission axes, which `Ledger::fold_robinhood_deposit` gates on
    /// as well, so a `false` here always meant `RhnToGlc` was parking
    /// too — which no reader could have known from the name.
    /// [`BridgeStatus::goldcoin_destination_admission_open`] was added
    /// as the honestly-named spelling of that same value.
    ///
    /// v25 gave each inbound route its own admission gate, so the two
    /// are now genuinely different questions and this field answers the
    /// one its name asks: the reserve-wide axes AND `SolToGlc`'s own
    /// `route_admission` row. It can be `false` while
    /// `goldcoin_destination_admission_open` is `true` (an operator
    /// closed only `SolToGlc`), and `RhnToGlc` may be open or closed
    /// independently — read `GET /chains`'s [`RouteView::available`] for
    /// that route's own answer.
    pub sol_to_glc_admission_open: bool,
    /// Whether NEW deposits bound for the GOLDCOIN reserve are currently
    /// admitted, as far as the RESERVE-WIDE axes are concerned.
    ///
    /// Was always exactly equal to
    /// [`BridgeStatus::sol_to_glc_admission_open`]; since schema v25 it
    /// is that field's reserve-wide HALF. This one deliberately does NOT
    /// account for either route's own admission gate — it answers for
    /// the reserve, which is what its name says, and a per-route answer
    /// belongs on `GET /chains`. `true` here with a closed route gate
    /// means "the reserve would admit, this route will not".
    ///
    /// It governs BOTH inbound-to-Goldcoin routes, `SolToGlc` and
    /// `RhnToGlc`, because both are folded against the same
    /// `reserve_ledger` row for `GoldcoinReserve`: the operator-only
    /// `admission_closed` and the automatic confirmed-liquidity gate.
    /// `false` means a newly observed deposit on EITHER route still gets
    /// folded — its funds are already committed on the source chain
    /// regardless — but parks in `ManualReview` instead of processing
    /// normally. Already-accepted obligations are never affected either
    /// way.
    ///
    /// `true` does NOT mean either route will admit: each also has its
    /// own v25 `route_admission` gate, ANDed with this one and closable
    /// independently.
    ///
    /// For a per-route answer that also accounts for that gate, pause,
    /// capacity and the mature-UTXO floor, read `available` on `GET
    /// /chains`'s [`RouteView`]; this field is the reserve-wide
    /// admission axis on its own.
    #[serde(default)]
    pub goldcoin_destination_admission_open: bool,
    /// Machine-readable cause when [`BridgeStatus::sol_to_glc_available`]
    /// is `false`; `null` when available. The same vocabulary as
    /// `GET /chains`'s [`RouteView::availability_reason`], plus
    /// [`AVAILABILITY_REASON_QUOTA_EXHAUSTED`] for the one gate this
    /// endpoint applies that `/chains` does not (the program's
    /// rolling-24h-volume window). `liquidity_buffer_low` here is the
    /// 2026-09-12 shape: every reserve gate open, headroom positive, and
    /// still no normal-sized deposit admissible.
    #[serde(default)]
    pub sol_to_glc_availability_reason: Option<String>,
    /// `SolToGlc`'s capacity figures — the same [`RouteCapacityView`]
    /// `GET /chains` carries for the route, so a status page can render
    /// headroom, buffer and "up to N" without a second request. `null`
    /// only when the probe could not be built (see
    /// [`AVAILABILITY_REASON_PROBE_UNAVAILABLE`]).
    #[serde(default)]
    pub sol_to_glc_capacity: Option<RouteCapacityView>,
}

/// One executable route's configured fee, for the surfaces that report
/// the whole table rather than one route's price.
///
/// Exists because a single `bridge_fee_bps` field cannot answer "what
/// does this bridge charge?" once routes are priced independently — and a
/// UI that reads one route's number and shows it beside another route's
/// button is the display half of the same bug `crate::fees` closed in the
/// pricing path.
#[derive(Debug, Serialize, Deserialize)]
pub struct RouteFeeView {
    /// The route's wire spelling, e.g. `"GlcToRhn"`.
    pub route: String,
    pub fee_bps: u64,
    /// Ready-to-display, e.g. `"6%"` — formatted by the same helper the
    /// operator tooling uses, so the UI and the CLI never round
    /// differently.
    pub fee_percent_display: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TransferLimits {
    /// Atomic; string on the wire (operator-set `u64`, unbounded here).
    pub min_transfer_amount: AtomicU64,
    /// Atomic; string on the wire.
    pub per_transfer_limit: AtomicU64,
    /// `GlcToSol`'s configured fee rate in basis points (300 = 3%,
    /// docs/20-bridge-fee.md), exposed here so a UI can display it
    /// without first needing a [`QuoteInput`]/[`QuoteOutput`] round trip
    /// for a specific amount.
    ///
    /// ROUTE-SPECIFIC, not global. This endpoint reports the SOLANA
    /// program's own `min_transfer_amount`/`per_transfer_limit`, so the
    /// fee beside them is the Solana-bound route's — it is not the rate
    /// any Robinhood route charges and must never be displayed as one.
    /// For the whole table see [`BridgeStats::route_fees`].
    pub bridge_fee_bps: u64,
}

/// Non-sensitive operational health, for the same audience as every other
/// endpoint in this module (a public bridge UI) — deliberately a small,
/// derived subset of what [`crate::ops::health::HealthReport`] reports to
/// an operator: no RPC URLs, no database paths, no raw reserve balances,
/// no indexer-internal detail beyond "halted or not". See this module's
/// top-level docs for why that boundary exists.
#[derive(Debug, Serialize, Deserialize)]
pub struct PublicHealth {
    /// `false` iff the Goldcoin indexer is halted or a post-finality
    /// reorg has been detected — both are fail-closed states that require
    /// operator intervention and never auto-clear (docs/10-threat-model.md).
    pub healthy: bool,
    pub goldcoin_indexer_halted: bool,
    /// Requests parked in `ManualReview`, summed across both directions —
    /// visible so a UI can show "some transfers are under manual review"
    /// without exposing which ones or why.
    pub manual_review_backlog: u64,
    /// Cumulative count of post-finality Goldcoin reorg events ever
    /// detected (docs/10-threat-model.md P3) — any nonzero value means
    /// both reserves were paused at least once for this reason and
    /// require operator resolution before resuming.
    pub post_finality_reorg_events: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReserveAvailability {
    /// Atomic; string on the wire. Signed: a capacity below zero is a real,
    /// diagnostic state and is reported rather than clamped.
    pub goldcoin_available_capacity: AtomicI64,
    /// Atomic; string on the wire. Signed, as above.
    pub solana_available_capacity: AtomicI64,
}

/// Request-count breakdown for one bridge [`Direction`], part of
/// [`BridgeStats`]. Counted in SQL (`Ledger::request_state_counts`), not
/// fetched row-by-row, so this stays cheap as `bridge_requests` grows.
#[derive(Debug, Serialize, Deserialize)]
pub struct DirectionStats {
    pub total_requests: i64,
    /// Sum of every non-terminal state (`RequestState::is_active`).
    pub in_progress_requests: i64,
    pub settled_requests: i64,
    pub manual_review_requests: i64,
}

/// Reserve-level aggregate for one [`ReserveDirection`], part of
/// [`BridgeStats`]. `settled_volume_atomic` and `accrued_fees_atomic` are
/// the same cumulative counters `ops::reserve_health`/`glc-admin status`
/// already track for an operator audience — surfaced here in their
/// public, capacity-only-equivalent form (no raw
/// `total_reserve_balance`/`protected_minimum`, matching
/// [`ReserveAvailability`]'s existing scope).
#[derive(Debug, Serialize, Deserialize)]
pub struct ReserveStats {
    pub paused: bool,
    /// Atomic; string on the wire. Signed, as in [`ReserveAvailability`].
    pub available_capacity: AtomicI64,
    /// Cumulative amount ever settled onto this reserve, in its own
    /// native destination units (docs/05-reserve-accounting.md) — real
    /// volume already recorded in `reserve_ledger.settled_liquidity_total`,
    /// never derived by summing individual transfers at request time.
    pub settled_volume_atomic: AtomicU64,
    /// Cumulative bridge-fee revenue accrued on this reserve, canonical
    /// units (docs/20-bridge-fee.md) — never counted toward capacity.
    pub accrued_fees_atomic: AtomicU64,
}

/// The Robinhood reserve's entry in [`BridgeStats`].
///
/// # Why this is not a [`ReserveStats`]
///
/// It cannot be. `ReserveStats`'s fields are non-optional, and the
/// Robinhood reserve has a state Goldcoin and Solana do not: **it may not
/// exist**. Its `reserve_ledger` row is created only when a
/// `[reserve.robinhood]` config section is present, which no production
/// deployment has today. Reading `paused`/`available_capacity` for a
/// direction with no row raises
/// [`LedgerError::ReserveNotInitialized`](crate::ledger::LedgerError), so
/// a non-optional `ReserveStats` here would have turned `GET /stats` —
/// the whole endpoint, for every client, including the two reserves that
/// were working fine — into a 500 on the exact deployments this field was
/// added for.
///
/// # Absent is not zero
///
/// The same rule [`RobinhoodReserveView`] states, for the same reason and
/// in the same encoding: when `ledger_availability` is not `"available"`,
/// every figure below is `null`. Never `0`. A zero would claim an empty
/// reserve exists, and "the bridge holds no Robinhood GLC" and "this
/// deployment has no Robinhood reserve" are different facts that clients
/// must be able to tell apart.
///
/// # Field names
///
/// Deliberately identical to [`ReserveStats`]'s (`paused`,
/// `available_capacity`, `settled_volume_atomic`, `accrued_fees_atomic`)
/// so a client can feed this to the same renderer it already uses for the
/// other two reserves, having only unwrapped the nulls. The one added
/// field is the `ledger_availability` discriminator that makes unwrapping
/// safe.
#[derive(Debug, Serialize, Deserialize)]
pub struct RobinhoodReserveStats {
    /// `"available"` when a `reserve_ledger` row exists,
    /// `"not_configured"` when it does not — the same two constants
    /// [`RobinhoodReserveView::ledger_availability`] uses
    /// ([`crate::robinhood::public`]). When this is not `"available"`,
    /// every field below is `null`.
    pub ledger_availability: String,
    /// This reserve's own pause flag — the LOCAL `RobinhoodReserve.paused`
    /// gate backing `GlcToRhn`, set by `glc-admin robinhood-local-pause`.
    /// Not the contract's `payoutsPaused`, which is a separate axis and is
    /// reported by `GET /robinhood/reserve` under `onchain`.
    pub paused: Option<bool>,
    /// `balance - protected_minimum - reserved_liquidity`, canonical 8dp.
    /// Atomic; string on the wire. Signed, and not clamped at zero, for
    /// the same diagnostic reason [`ReserveStats::available_capacity`] is
    /// not.
    pub available_capacity: Option<AtomicI64>,
    /// Cumulative amount ever settled onto this reserve, canonical 8dp.
    /// Atomic; string on the wire.
    ///
    /// This is a real, authoritatively-maintained counter, not a
    /// placeholder: `reserve_ledger.settled_liquidity_total` for
    /// `RobinhoodReserve` is incremented by
    /// [`Ledger::mark_robinhood_payout_settled`](crate::ledger::Ledger)
    /// on every `GlcToRhn` payout that reaches the configured Robinhood
    /// confirmation depth. It is read through the same
    /// `Ledger::settled_liquidity` accessor the Goldcoin and Solana
    /// entries above already use.
    pub settled_volume_atomic: Option<AtomicU64>,
    /// Cumulative bridge-fee revenue accrued ON THIS RESERVE, canonical
    /// 8dp. Atomic; string on the wire.
    ///
    /// Also a real counter, and its contents are worth stating because
    /// the obvious reading is wrong: a fee is accrued on the reserve where
    /// it was WITHHELD, i.e. the SOURCE side (docs/20-bridge-fee.md). So
    /// this row accumulates `RhnToGlc` fees — deposits that entered on
    /// Robinhood — and NOT `GlcToRhn` fees, which are withheld on Goldcoin
    /// and land on `goldcoin_reserve.accrued_fees_atomic`. Never counted
    /// toward capacity.
    pub accrued_fees_atomic: Option<AtomicU64>,
}

/// Public, non-sensitive aggregate bridge statistics (`GET /stats`).
/// Every figure is either a live derived check (availability) or a
/// cumulative counter already persisted by ordinary settlement/
/// reconciliation bookkeeping — nothing here is computed by scanning
/// history at request time beyond a single `GROUP BY` count query per
/// direction, and nothing is fabricated: an unavailable or zero figure is
/// reported as exactly that, never omitted or guessed.
#[derive(Debug, Serialize, Deserialize)]
pub struct BridgeStats {
    pub goldcoin_paused: bool,
    pub solana_paused: bool,
    pub glc_to_sol_available: bool,
    pub sol_to_glc_available: bool,
    /// See [`BridgeStatus::sol_to_glc_availability_reason`].
    #[serde(default)]
    pub sol_to_glc_availability_reason: Option<String>,
    /// See [`BridgeStatus::glc_to_sol_quota_exhausted`].
    pub glc_to_sol_quota_exhausted: bool,
    /// See [`BridgeStatus::sol_to_glc_quota_exhausted`].
    pub sol_to_glc_quota_exhausted: bool,
    /// See [`BridgeStatus::glc_to_sol_rolling_volume_remaining`].
    pub glc_to_sol_rolling_volume_remaining: AtomicU64,
    /// See [`BridgeStatus::sol_to_glc_rolling_volume_remaining`].
    pub sol_to_glc_rolling_volume_remaining: AtomicU64,
    /// `GlcToSol`'s configured rate, kept under its historical name for
    /// wire compatibility. Read [`BridgeStats::route_fees`] instead: this
    /// field cannot express four independent prices and is only still
    /// here so existing clients keep parsing.
    pub bridge_fee_bps: u64,
    /// Every executable route's configured fee, in registry order — the
    /// authoritative answer to "what does this bridge charge?".
    pub route_fees: Vec<RouteFeeView>,
    pub glc_to_sol: DirectionStats,
    pub sol_to_glc: DirectionStats,
    pub goldcoin_reserve: ReserveStats,
    pub solana_reserve: ReserveStats,
    /// The third physical reserve. Nullable throughout — see
    /// [`RobinhoodReserveStats`] for why it is not a [`ReserveStats`].
    ///
    /// Purely ADDITIVE: `goldcoin_reserve` and `solana_reserve` above are
    /// byte-for-byte what they always were, and a client that does not
    /// know this field ignores it.
    pub robinhood_reserve: RobinhoodReserveStats,
    pub goldcoin_indexer_halted: bool,
    /// Seconds since each chain indexer's last completed tick — a
    /// freshness signal, not an infrastructure detail (no RPC URL, no
    /// host, no port).
    pub goldcoin_indexer_seconds_since_tick: i64,
    pub solana_indexer_seconds_since_tick: i64,
    pub post_finality_reorg_events: i64,
    pub as_of: i64,
}

/// One row of `GET /reserves/history` — a real, already-persisted
/// reconciliation-tick observation (`Ledger::reconciliation_findings_page`),
/// never a fabricated or interpolated data point. A `classification` of
/// `"SKIPPED: ..."` means this tick could not obtain a real chain read
/// (RPC failure, stale height) and recorded that fact rather than a
/// balance — callers should treat it as a gap, not a zero balance.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReserveHistoryEntry {
    pub id: i64,
    pub direction: String,
    pub detected_at: i64,
    /// Atomic; string on the wire.
    pub expected_atomic: AtomicI64,
    /// Atomic; string on the wire.
    pub observed_atomic: AtomicI64,
    /// Atomic; string on the wire. Signed by nature — negative exactly
    /// when the observed balance is short of the expected one.
    pub delta_atomic: AtomicI64,
    pub classification: String,
    pub auto_paused: bool,
}

/// One row of `GET /explorer/events` — a real, already-persisted
/// bridge-request state transition (`Ledger::explorer_events_page`).
/// Deliberately reserve-bridge-native vocabulary (`RequestState`'s own
/// names), never federation/wrapped-token-era event kinds ("mint"/
/// "burn") — see this module's top-level docs. Carries no counterparty
/// address (this bridge does not truncate-and-publish one; see
/// [`TransferView`], which also omits `recipient`) and no operator
/// identity — rebalancing/custody-transition audit trails, which DO
/// carry real approver identities, stay operator-only via `glc-admin`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExplorerEvent {
    pub id: i64,
    pub request_id: i64,
    pub direction: String,
    pub from_state: Option<String>,
    pub to_state: String,
    pub at: i64,
    pub reason: Option<String>,
}

/// Cursor-paginated response envelope, shared by every list endpoint in
/// this module. `next_cursor` is `Some` only when the page returned
/// exactly `limit` items — i.e. there MIGHT be more; a short page is
/// proof there is nothing further, so `next_cursor` is `None` even
/// though a last item exists. Pass it back as `?cursor=` to fetch the
/// next (older) page.
#[derive(Debug, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub as_of: i64,
}

/// One chain in `GET /chains`. Names only — no RPC URL, no contract
/// address, no explorer host, consistent with this module never exposing
/// infrastructure detail to a public audience.
#[derive(Debug, Serialize, Deserialize)]
pub struct ChainView {
    /// Stable identifier (`goldcoin`/`solana`/`robinhood`). Parse this.
    pub id: String,
    /// Human-readable name. Never parse this.
    pub display_name: String,
}

/// One route in `GET /chains`.
///
/// # `enabled` and `available` answer DIFFERENT questions
///
/// Read the wrong one and a UI will offer a transfer the backend will
/// not complete. That is not hypothetical: it is the production
/// launch-blocker these two fields were separated to close, in which
/// `RhnToGlc` reported `enabled: true` (correctly — the route gate was
/// open) while `reserve_ledger.admission_closed` was set on
/// `GoldcoinReserve`, so every newly observed Robinhood deposit folded
/// straight into `ManualReview` with `admission_closed_at_fold`. Users
/// made irreversible on-chain deposits against a UI that had been told
/// the route was available.
///
/// - **`enabled`** — is this route SWITCHED ON in this deployment? The
///   verdict of `crate::routes::RouteGate`: the config file, the
///   `bridge_routes` ledger row, and chain-adapter capability. It is
///   deliberately a statement about configuration, it changes only when
///   an operator changes it, and it is what `POST /transfers`/`POST
///   /quote` enforce before anything else. It says NOTHING about the
///   reserve.
///
/// - **`available`** — would a transfer started right now actually be
///   admitted? `enabled`, AND every runtime gate on this route's
///   destination reserve: `paused`, `admission_closed`, the
///   confirmed-liquidity gate and its safety buffer, the mature-UTXO
///   pool floor, and capacity. Computed from the SAME
///   `crate::ledger::admission::InboundAdmissionGates` the folds gate
///   on, never a re-derivation.
///
/// **A UI must gate its "start a transfer" affordance on `available`.**
/// Use `enabled`/`implemented` only to choose the WORDING — "Coming
/// soon" for a route this build cannot serve, versus "temporarily
/// unavailable" for one that is switched on but currently closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteView {
    /// `GlcToSol` | `SolToGlc` | `GlcToRhn` | `RhnToGlc` | `SolToRhn` |
    /// `RhnToSol`.
    pub id: String,
    pub source_chain: String,
    pub destination_chain: String,
    /// Whether this route is switched on: config + `bridge_routes` +
    /// adapter capability. NOT a statement about the reserve — see the
    /// type docs, and read `available` before offering a transfer.
    pub enabled: bool,
    /// Cause-agnostic end-user copy when `enabled` is `false`; `null` when
    /// enabled. Never names which gate refused.
    pub disabled_reason: Option<String>,
    /// Whether this route has settlement machinery at all
    /// (`Route::as_direction().is_some()`). `false` means the route is
    /// structurally inert in this build, not merely switched off — a UI can
    /// use it to choose "Coming soon" wording over "temporarily paused"
    /// without parsing `disabled_reason`. `true` for all six routes since
    /// Phase H; a route's `enabled`/`available` say whether it is OPEN.
    pub implemented: bool,
    /// **The field to gate a transfer button on.** `true` only when the
    /// route is enabled AND its destination reserve would currently
    /// admit a new deposit.
    ///
    /// Fails closed in every uncertain case: a non-implemented route, a
    /// disabled route, a destination reserve with no `reserve_ledger`
    /// row, or a ledger read that did not complete all report `false`.
    ///
    /// Asked before an amount exists, so it is evaluated at a STATED
    /// size. For `SolToGlc` that is the Solana program's
    /// `per_transfer_limit` — the largest deposit the program accepts —
    /// so `true` means "the largest permitted deposit would be admitted
    /// under every gate, the confirmed-liquidity safety buffer
    /// included", and any smaller one would too (see
    /// [`AdmissionProbe`] and `capacity`). Every other route is still
    /// evaluated at one atomic unit ("would a minimum-sized deposit be
    /// admitted"), so on those a large enough transfer can be held back
    /// by the buffer or by capacity while this is `true`. Two further
    /// things it does not cover, each with its own endpoint: the
    /// per-wallet rolling-24h windows on both legs of every route
    /// (`GET /routes/{route}/eligibility`, and the older
    /// `GET /recipients/{sol,rhn}-to-glc/eligibility`), and — for
    /// `GlcToSol` — the Solana program's own on-chain rolling-volume
    /// window (`GET /status`'s `glc_to_sol_quota_exhausted`; note that
    /// `crate::quota` engages this service's local pause once it
    /// observes that exhaustion, at which point this field does go
    /// `false` and stays there until an operator unpauses).
    #[serde(default)]
    pub available: bool,
    /// Cause-agnostic end-user copy when `available` is `false`; `null`
    /// when available. Never names which gate refused, matching
    /// `disabled_reason` and [`DIRECTION_UNAVAILABLE_MESSAGE`]'s
    /// disclosure posture — a route being closed for capacity reasons is
    /// not something this endpoint tells the public.
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    /// **The smallest GROSS this route accepts**, canonical 8-decimal
    /// units — the figure a UI should render as "Min … GLC".
    ///
    /// # Why it is published per route rather than derived
    ///
    /// Because deriving it is how it went wrong. Before this field
    /// existed there was no server-side minimum at all, and a client
    /// wanting to show one had to reconstruct it from whichever chain
    /// floor governed the route — grossing it up through the fee when
    /// that floor applied to the NET. That produced entry minimums like
    /// "102.061856 GLC", correct arithmetic against the wrong rule, and
    /// silently different every time a fee moved.
    ///
    /// This is the rule itself: `crate::min_transfer`'s policy floor, the
    /// same value `POST /transfers` and `POST /quote` admit against and
    /// the same one a fold parks below. A client renders it and performs
    /// no arithmetic on it — in particular it must NOT be adjusted for
    /// the fee, which is deducted after this check and legitimately
    /// leaves a destination figure below it.
    ///
    /// Canonical units, so a source chain with finer precision
    /// (Robinhood's 18 decimals) scales up and one with coarser precision
    /// (the Solana mint's 6) scales down — the same convention every
    /// other amount on this API uses.
    #[serde(default)]
    pub min_transfer_atomic: AtomicU64,
    /// **Machine-readable cause** when `available` is `false`; `null`
    /// when available. Where `unavailable_reason` carries end-user copy
    /// and deliberately names no gate, this names exactly one: the
    /// highest-ranked gate that refused, in
    /// [`crate::ledger::admission::InboundAdmissionBlocker::as_str`]'s
    /// vocabulary (`liquidity_buffer_low`, `route_admission_closed`,
    /// `reserve_admission_closed`, `reserve_paused`,
    /// `utxo_liquidity_low`, `insufficient_capacity`) plus the cases the
    /// evaluator never sees: [`AVAILABILITY_REASON_NOT_IMPLEMENTED`],
    /// [`AVAILABILITY_REASON_ROUTE_DISABLED`],
    /// [`AVAILABILITY_REASON_ONCHAIN_PAUSED`],
    /// [`AVAILABILITY_REASON_RESERVE_UNAVAILABLE`] and
    /// [`AVAILABILITY_REASON_PROBE_UNAVAILABLE`].
    ///
    /// Added after the 2026-09-12 incident, in which `SolToGlc` was
    /// advertised `available: true` while every normal-sized deposit
    /// parked `liquidity_buffer_low_at_fold`, and nothing on this
    /// endpoint could say so. Operators and integrators need the gate;
    /// end users keep getting `unavailable_reason`.
    #[serde(default)]
    pub availability_reason: Option<String>,
    /// The destination reserve's admission capacity as it bears on THIS
    /// route, for routes with a known normal transfer size — today
    /// `SolToGlc`, probed at the Solana program's `per_transfer_limit`.
    /// `null` for every other route. See [`RouteCapacityView`].
    #[serde(default)]
    pub capacity: Option<RouteCapacityView>,
}

/// Machine-readable `availability_reason` for a route this build cannot
/// serve at all (`Route::as_direction()` is `None`).
pub const AVAILABILITY_REASON_NOT_IMPLEMENTED: &str = "not_implemented";
/// `availability_reason` when the route gate (`crate::routes::RouteGate`)
/// has the route switched off.
pub const AVAILABILITY_REASON_ROUTE_DISABLED: &str = "route_disabled";
/// `availability_reason` when the Solana program's own pause flags refuse
/// this route's Solana leg, or `bridge_config` could not be read
/// (fail-closed).
pub const AVAILABILITY_REASON_ONCHAIN_PAUSED: &str = "onchain_paused";
/// `availability_reason` when the destination reserve's gates could not
/// be read (no `reserve_ledger` row, or a failed read) — fail-closed.
pub const AVAILABILITY_REASON_RESERVE_UNAVAILABLE: &str = "reserve_unavailable";
/// `availability_reason` when a route whose availability must be probed
/// at its normal transfer size had no probe to evaluate against
/// (`per_transfer_limit` or the reserve mint's decimals unreadable) —
/// fail-closed, never "available at an unknown size".
pub const AVAILABILITY_REASON_PROBE_UNAVAILABLE: &str = "probe_unavailable";
/// `GET /status`-only `availability_reason`: every admission gate is
/// open but the Solana program's rolling-24h-volume window for this
/// direction is exhausted (`sol_to_glc_quota_exhausted`).
pub const AVAILABILITY_REASON_QUOTA_EXHAUSTED: &str = "quota_exhausted";

/// One route's view of its destination reserve's admission capacity,
/// canonical 8-decimal units throughout.
///
/// The figures the confirmed-liquidity safety buffer rule is stated in
/// (docs/09-runbook.md's "Confirmed-liquidity admission safety buffer":
/// a deposit is admitted only while `headroom - net >= buffer`), so a
/// client can see WHY a route is closed for capacity and how large a
/// transfer would currently go through — instead of learning it by
/// having a deposit parked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteCapacityView {
    /// Confirmed, unreserved destination headroom right now:
    /// `total_reserve_balance - protected_minimum - reserved_liquidity`
    /// (`InboundAdmissionGates::confirmed_headroom_atomic`). May be
    /// negative when the reserve is over-committed.
    pub confirmed_headroom_atomic: AtomicI64,
    /// The configured admission safety buffer headroom must stay above
    /// AFTER admitting a transfer (`0` = disabled).
    pub liquidity_buffer_atomic: AtomicI64,
    /// The largest GROSS transfer the DESTINATION RESERVE would admit
    /// right now: the buffer rule's `headroom - buffer` (or plain
    /// headroom with no buffer) grossed up through the route fee
    /// ([`crate::amount_conversion::max_gross_for_net_at_bps`]). `0`
    /// when any amount-independent gate is closed or headroom is
    /// already inside the buffer.
    ///
    /// Deliberately NOT capped at the program's own per-transfer limit,
    /// so the two figures can be compared: `available` is `true` exactly
    /// when this is at least `probe_gross_atomic` (and no other gate
    /// refuses). In the 2026-09-12 shape this read ~32,183 GLC against a
    /// 50,000 GLC probe — the sentence an operator needed and no field
    /// could say. A UI offering an amount should still respect the
    /// program's own limits (`GET /limits`) on top.
    pub max_admissible_gross_atomic: AtomicU64,
    /// The automatic confirmed-liquidity gate's persisted hysteresis
    /// state: `true` while it is holding ALL new obligations back, until
    /// headroom recovers to the reopen threshold. Distinct from a
    /// per-amount buffer refusal, which can occur while this is `false`.
    pub liquidity_admission_closed: bool,
    /// The GROSS this route's availability was evaluated at — the
    /// "normal transfer" the `available` verdict answers for.
    pub probe_gross_atomic: AtomicU64,
}

/// The transfer size a route's public availability is evaluated at.
///
/// For `SolToGlc` this is the Solana program's `per_transfer_limit` — the
/// largest deposit the program will accept — widened to canonical units
/// and netted through the route's own fee, so `available: true` means
/// "the largest permitted deposit would be admitted", and a smaller one
/// therefore would too. Built once per request by
/// [`BridgeApi::sol_to_glc_probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionProbe {
    /// The gross the probe stands for, canonical.
    pub gross_canonical: u64,
    /// The route fee it was netted at.
    pub fee_bps: u64,
    /// The net destination amount the evaluator is asked about.
    pub net_destination_atomic: i64,
}

/// Every probe [`RouteView::build`] may need, resolved by the endpoint
/// once so a listing of six routes performs the chain reads once.
/// `None` for a route means that route is evaluated at the weakest form
/// ([`InboundAdmissionGates::route_blocker`]) exactly as before — for
/// the routes that carry no probe, and for `SolToGlc` when the probe
/// could not be built (which then fails closed inside
/// [`route_availability`], never silently falling back to the weak form).
#[derive(Debug, Clone, Copy, Default)]
pub struct RouteProbes {
    pub sol_to_glc: Option<AdmissionProbe>,
}

/// The full availability verdict [`route_availability`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteAvailability {
    pub available: bool,
    /// End-user copy, cause-agnostic — see `RouteView::unavailable_reason`.
    pub unavailable_reason: Option<String>,
    /// Machine-readable gate — see `RouteView::availability_reason`.
    pub availability_reason: Option<String>,
    pub capacity: Option<RouteCapacityView>,
}

impl RouteAvailability {
    fn unavailable(copy: &str, reason: &str) -> RouteAvailability {
        RouteAvailability {
            available: false,
            unavailable_reason: Some(copy.to_string()),
            availability_reason: Some(reason.to_string()),
            capacity: None,
        }
    }
}

impl RouteView {
    /// The ONE place a [`RouteView`] is built.
    ///
    /// Both construction sites (`GET /chains` and `GET
    /// /robinhood/reserve`'s `routes`) call this, so the two can never
    /// answer the same question differently — which they would
    /// otherwise, since the second repeats the first's verdict
    /// deliberately so a client rendering the reserve page need not
    /// correlate two responses.
    ///
    /// Read-only in the strongest sense: it evaluates the
    /// confirmed-liquidity gate's PERSISTED state and never the
    /// hysteresis rule, so listing a route can never move a gate.
    fn build(
        route_gate: &crate::routes::RouteGate,
        ledger: &Ledger,
        onchain: SolanaProgramPause,
        probes: RouteProbes,
        route: crate::routes::Route,
    ) -> RouteView {
        // One gate evaluation per route, same call the write paths make
        // — this listing can never claim a route is open that
        // `POST /transfers` would then refuse.
        let enabled = route_gate.is_enabled(ledger, route);
        let RouteAvailability {
            available,
            unavailable_reason,
            availability_reason,
            capacity,
        } = route_availability(ledger, onchain, probes, route, enabled);
        RouteView {
            id: route.as_str().to_string(),
            source_chain: route.source_chain().as_str().to_string(),
            destination_chain: route.destination_chain().as_str().to_string(),
            enabled,
            disabled_reason: route_gate.disabled_reason(ledger, route),
            implemented: route.as_direction().is_some(),
            available,
            unavailable_reason,
            // Not gated on `implemented`, `enabled` or `available`: the
            // floor is a property of the route's terms, not of whether it
            // happens to be open this minute, and a UI showing a closed
            // route's limits should show the real ones.
            min_transfer_atomic: AtomicU64(crate::min_transfer::source_minimum(route).0),
            availability_reason,
            capacity,
        }
    }
}

/// Whether `route` would currently admit a new transfer, the
/// cause-agnostic copy to show when it would not, the machine-readable
/// gate that refused, and — for a probed route — its capacity figures.
///
/// # Where the answer comes from
///
/// Every runtime gate is read through
/// [`Ledger::inbound_admission_gates`], i.e. through the SAME
/// [`crate::ledger::InboundAdmissionGates`] evaluator
/// `Ledger::fold_sol_deposit` and `Ledger::fold_robinhood_deposit` gate
/// on. Nothing is re-derived here, and nothing is guessed: this function
/// owns only the mapping from "which reserve does this route draw on"
/// to that shared decision, the size the decision is asked at, and the
/// fail-closed cases.
///
/// # The size the decision is asked at
///
/// A route with a probe in `probes` is evaluated at that probe's net —
/// [`InboundAdmissionGates::route_blocker_at`] — so `available: true`
/// means "a normal, permitted transfer would be admitted". For
/// `SolToGlc` the probe is the Solana program's `per_transfer_limit`
/// (see [`AdmissionProbe`]). Before this existed every route was
/// evaluated at one atomic unit, which with a safety buffer configured
/// is `headroom > buffer` — true for a long stretch of headroom in
/// which every real deposit fails `headroom - net >= buffer` and parks.
/// That is the 2026-09-12 incident, and a probed route can no longer
/// report it as open. A route without a probe keeps the weakest form
/// ([`InboundAdmissionGates::route_blocker`]), exactly as before.
///
/// A route that REQUIRES a probe (`SolToGlc`) and has none fails closed
/// with [`AVAILABILITY_REASON_PROBE_UNAVAILABLE`]: "we could not work
/// out what a normal transfer is" must never render as available.
///
/// The reserve is the route's DESTINATION reserve
/// (`Direction::destination_reserve`) because that is the pool the
/// payout comes out of and therefore the one whose gates a fold or a
/// `create_request` consults — `GoldcoinReserve` for both inbound
/// routes, `SolanaReserve` for `GlcToSol`, `RobinhoodReserve` for
/// `GlcToRhn`.
///
/// The lookup is keyed by `Direction`, not by that reserve, because the
/// two inbound routes SHARE `GoldcoinReserve` and each also carries its
/// own route-level admission gate (schema v25's `route_admission`). A
/// reserve alone can no longer answer the question: `RhnToGlc` may be
/// closed while `SolToGlc` is open, out of the same reserve. Both gates
/// are ANDed inside the shared evaluator, so this function still owns
/// only the mapping and the fail-closed cases.
///
/// # Fail-closed, in every branch that can fail
///
/// A non-implemented route, a disabled route, an unconfigured
/// destination reserve, a failed ledger read and a missing required
/// probe all answer `false`. For `RhnToGlc` in particular there is no
/// `POST /transfers` preflight between this answer and an irreversible
/// on-chain deposit, so "unknown" must never render as "available".
///
/// # Read-only
///
/// Evaluates the confirmed-liquidity gate's PERSISTED hysteresis state
/// and never the rule, so asking never moves a gate; and the automatic
/// gate, the buffer thresholds and every resume policy are untouched —
/// this changes what is REPORTED, never what is admitted.
fn route_availability(
    ledger: &Ledger,
    onchain: SolanaProgramPause,
    probes: RouteProbes,
    route: crate::routes::Route,
    enabled: bool,
) -> RouteAvailability {
    // A route with no settlement machinery, or one the route gate
    // refuses, is unavailable for the reason the gate already reports —
    // the same copy `disabled_reason` carries, so a UI showing one
    // message never has to reconcile two.
    let Some(direction) = route.as_direction() else {
        return RouteAvailability::unavailable(
            crate::routes::RouteGateError::UNAVAILABLE_MESSAGE,
            AVAILABILITY_REASON_NOT_IMPLEMENTED,
        );
    };
    if !enabled {
        return RouteAvailability::unavailable(
            crate::routes::RouteGateError::UNAVAILABLE_MESSAGE,
            AVAILABILITY_REASON_ROUTE_DISABLED,
        );
    }
    // The Solana program's own circuit breaker for this route's Solana
    // leg. A local gate can be open while the program refuses the
    // instruction outright (`DepositDirectionPaused` /
    // `ReleaseDirectionPaused` / `BridgeGloballyPaused`); advertising
    // such a route is a lie the user only discovers at simulation. Same
    // cause-agnostic copy as a closed local gate: it is a pause condition,
    // and which layer paused is an operator detail (`glc-admin
    // show-config`, the admin API's `/onchain`).
    if onchain.blocks(direction) {
        return RouteAvailability::unavailable(
            DIRECTION_UNAVAILABLE_MESSAGE,
            AVAILABILITY_REASON_ONCHAIN_PAUSED,
        );
    }
    // Which size to ask at. `SolToGlc` MUST be probed; every other route
    // keeps the weakest form until it too has a stated normal size.
    let probe = match route {
        crate::routes::Route::SolToGlc => match probes.sol_to_glc {
            Some(p) => Some(p),
            None => {
                return RouteAvailability::unavailable(
                    DIRECTION_UNAVAILABLE_MESSAGE,
                    AVAILABILITY_REASON_PROBE_UNAVAILABLE,
                )
            }
        },
        _ => None,
    };
    // Includes `ReserveNotInitialized` — a destination reserve this
    // deployment has no row for can admit nothing, and reporting
    // "available" for it would be the exact inversion of the
    // fail-closed rule everywhere else in this module.
    let gates = match ledger.inbound_admission_gates(direction) {
        Ok(g) => g,
        Err(_) => {
            return RouteAvailability::unavailable(
                DIRECTION_UNAVAILABLE_MESSAGE,
                AVAILABILITY_REASON_RESERVE_UNAVAILABLE,
            )
        }
    };
    let blocker = match probe {
        Some(p) => gates.route_blocker_at(p.net_destination_atomic),
        None => gates.route_blocker(),
    };
    let available = blocker.is_none();
    // The capacity figures travel with every probed verdict, open or
    // closed, so a client can show "closed: headroom X against buffer Y,
    // reserve admits up to N" as readily as "open". `max_admissible_
    // gross_atomic` is the evaluator's own bound grossed up through the
    // route fee — see the field's docs for why it is not capped at the
    // probe. A gross-up that cannot be computed (an unpriceable rate)
    // reports `0`, the fail-closed figure.
    let capacity = probe.map(|p| {
        let max_net = gates.max_admissible_net_destination_atomic().max(0) as u64;
        let max_gross = crate::amount_conversion::max_gross_for_net_at_bps(
            crate::amount_conversion::CanonicalAtomic(max_net),
            p.fee_bps,
        )
        .map(|g| g.0)
        .unwrap_or(0);
        RouteCapacityView {
            confirmed_headroom_atomic: AtomicI64(gates.confirmed_headroom_atomic),
            liquidity_buffer_atomic: AtomicI64(gates.admission_buffer_atomic),
            max_admissible_gross_atomic: AtomicU64(max_gross),
            liquidity_admission_closed: gates.liquidity_admission_closed,
            probe_gross_atomic: AtomicU64(p.gross_canonical),
        }
    });
    RouteAvailability {
        available,
        // A closed runtime gate is a capacity/pause condition, not a
        // "this route does not exist yet" condition, so it gets the
        // capacity copy rather than the route-gate copy. Which gate
        // closed is deliberately not disclosed in THIS field — that is
        // what `availability_reason` is for.
        unavailable_reason: (!available).then(|| DIRECTION_UNAVAILABLE_MESSAGE.to_string()),
        availability_reason: blocker.map(|b| b.as_str().to_string()),
        capacity,
    }
}

/// `GET /chains` — the chain/route registry. Purely additive to this API:
/// no existing endpoint changed shape to accommodate it.
#[derive(Debug, Serialize, Deserialize)]
pub struct ChainsView {
    pub chains: Vec<ChainView>,
    pub routes: Vec<RouteView>,
    pub as_of: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateTransferInput {
    /// Atomic. Accepts a decimal string (preferred) or a JSON integer, so
    /// existing callers are unaffected — see [`atomic`]'s compatibility note.
    pub amount_atomic: AtomicU64,
    /// The address the released funds should be sent to, spelled in the
    /// DESTINATION chain's own notation and parsed as that chain's
    /// address type: a base58 Solana pubkey for `GlcToSol`, a
    /// `0x`-prefixed 20-byte EVM address for `GlcToRhn`. Which one is
    /// expected follows from `route`, so the two are never
    /// interchangeable and a mismatch is a parse failure, not a silently
    /// stored blob.
    pub recipient: String,
    /// OPTIONAL route selector. Absent means `GlcToSol`, which is what
    /// this endpoint has always created — so every existing client keeps
    /// working with no change and no behavioural difference.
    ///
    /// Present values are gated by `crate::routes::RouteGate` BEFORE any
    /// fee computation, chain read, capacity reservation, ledger write or
    /// deposit-address derivation happens. A disabled route therefore
    /// leaves no trace: no row, no reserved liquidity, no derived address.
    ///
    /// The default applies ONLY to an absent field. A route that is
    /// present and refused is an error, never a fallback: there is no
    /// input to this endpoint that names `GlcToRhn` and produces a
    /// `GlcToSol` request.
    #[serde(default)]
    pub route: Option<String>,
    /// OPTIONAL: the Goldcoin address the caller intends to fund the
    /// deposit from (P2PKH or P2SH on this deployment's network). When
    /// given, its rolling-24h source-wallet window is checked BEFORE any
    /// capacity is reserved — a wallet still inside its window gets a
    /// `429` and no request — and it is recorded on the request so the
    /// window is consumed from admission, not from the moment the
    /// deposit lands. Absent means the source leg is not checked here;
    /// every existing caller keeps working unchanged.
    ///
    /// A declaration, not evidence: the wallet that REALLY funds the
    /// deposit is traced from the deposit transaction's own inputs when
    /// the indexer observes it and re-checked then, so omitting this or
    /// declaring a different address gains nothing — the deposit would
    /// only be parked in `ManualReview` instead of refused up front. See
    /// [`RouteWalletEligibilityView`] for the read-only pre-check a UI
    /// should make before asking the user to sign anything.
    #[serde(default)]
    pub source_address: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateTransferOutput {
    pub request_id: i64,
    /// Send Goldcoin to this address to fund the transfer. A fresh
    /// address unique to THIS request, derived from the same 2-of-3
    /// signer set as every other request
    /// (`goldcoin::derivation::derive_request_vault`, Step 1) — never
    /// the shared legacy vault address. Attribution is by this address
    /// alone: no `OP_RETURN`, memo, or exact-amount trick is required or
    /// consulted for a request created through this endpoint. This
    /// service never constructs the deposit transaction itself; building
    /// and broadcasting it is the caller's own wallet's job.
    pub deposit_address: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TransferView {
    pub id: i64,
    pub direction: String,
    pub state: String,
    /// Canonical units (docs/20-bridge-fee.md) — what the user declared/
    /// deposited, before the bridge fee.
    pub gross_amount_atomic: AtomicU64,
    /// Basis points (`0..=10_000`) — bounded, so a plain JSON number.
    pub fee_bps: u64,
    /// Canonical units; string on the wire.
    pub fee_amount_atomic: AtomicU64,
    /// Canonical units — the real-world GLC entitlement delivered; string
    /// on the wire.
    pub net_amount_atomic: AtomicU64,
    pub created_at: i64,
    pub source_txid: Option<String>,
    pub source_confirmations: i64,
    /// The confirmation depth `source_confirmations` must reach before
    /// this request advances to `SourceFinalized`, so a UI can render
    /// "N/required confirmations" progress. Only meaningful for the
    /// Goldcoin-SOURCED directions (a Goldcoin deposit is
    /// confirmation-tracked block by block); `None` for the
    /// contract-sourced ones, whose obligation folds directly to
    /// `SourceFinalized` once observed — there is no confirmation count
    /// to progress through.
    pub required_source_confirmations: Option<i64>,
    pub destination_txid: Option<String>,
    pub failure_reason: Option<String>,
    /// Present exactly when this request is in the refund lifecycle — see
    /// [`RefundView`], and read it INSTEAD of the gross/fee/net trio above
    /// when it is present.
    pub refund: Option<RefundView>,
}

/// The authoritative refund facts for a request whose deposit is being —
/// or has been — returned. Present on [`TransferView`] exactly when the
/// request is in one of the three refund-lifecycle states
/// ([`RequestState::is_refund_lifecycle`]), which is also exactly when a
/// refund row exists: [`Ledger::begin_goldcoin_refund`]/
/// [`Ledger::begin_solana_refund`] insert the row and move the request to
/// `RefundPending` in the same transaction.
///
/// # Why this exists
///
/// A refunded request never settled. [`TransferView`]'s
/// `gross_amount_atomic`/`fee_amount_atomic`/`net_amount_atomic` trio is
/// the QUOTE the request was created under — an intention, not an outcome
/// — so after a refund all three describe something that did not happen.
/// Production request #2477 (`GlcToSol`; expected gross 29 100 GLC,
/// actually deposited 29 050 GLC, parked on `deposit_amount_mismatch`,
/// refunded in full, zero fee, zero Solana payout) consequently rendered
/// as "you bridge 29 100 GLC / bridge fee 873 GLC / you receive 28 227
/// GLC" — three figures that were each false — because the trio was the
/// only amount information this endpoint offered.
///
/// # Provenance
///
/// Every amount here is read from the REFUND ROW — `goldcoin_refunds` for
/// `GlcToSol`, `solana_refunds` for `SolToGlc` — which is written only
/// from independently chain-verified evidence (schema v19/v20,
/// docs/09-runbook.md). Never derived from the request's expected gross,
/// never parsed out of `manual_review_note`, and never recomputed by
/// arithmetic over the quote.
///
/// # What is deliberately absent
///
/// The refund DESTINATION address. `TransferView` is served
/// unauthenticated on the public explorer route and has never carried any
/// party's address (module doc above); a refund destination is exactly
/// such an address, so it stays operator-only in `admin_api`'s
/// `GlcRefundExecuteView`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RefundView {
    /// The REFUND ROW's own lifecycle state, which is finer-grained than
    /// the request's: `"Built"`/`"Signed"`/`"Broadcast"`/`"Refunded"` for
    /// `GlcToSol`, `"Pending"`/`"Broadcast"`/`"Confirmed"` for
    /// `SolToGlc`. A client that only wants the coarse view keeps using
    /// [`TransferView::state`]; this is here so "built but not yet
    /// signed" is distinguishable from "signed but not yet broadcast",
    /// which the request state alone folds together into `RefundPending`.
    pub state: String,
    /// What was ACTUALLY received on the source chain for the deposit
    /// being returned, canonical units (docs/20-bridge-fee.md — the same
    /// unit as the rest of this DTO). This is the figure that differs
    /// from [`TransferView::gross_amount_atomic`] whenever the request
    /// was parked for `deposit_amount_mismatch`: #2477's 29 050 GLC
    /// against an expected 29 100 GLC. String on the wire, like every
    /// other unbounded atomic amount.
    pub observed_amount_atomic: AtomicU64,
    /// The refund principal actually returned to the depositor, canonical
    /// units. Equal to `observed_amount_atomic` by policy — the vault
    /// absorbs the miner fee, and for `GlcToSol` a schema-level CHECK
    /// enforces that equality — but reported as its own field rather than
    /// left to be assumed, so a client renders the right number without
    /// having to know the policy. String on the wire.
    pub refund_amount_atomic: AtomicU64,
    /// The bridge fee actually charged on this request, canonical units.
    /// Always `0`: the fee accrues at SETTLEMENT only
    /// (docs/20-bridge-fee.md) and a refunded request never settles.
    /// Stated explicitly so "no bridge fee was charged" is a fact this
    /// service asserts, not a claim a UI has to originate on its own.
    /// String on the wire, for consistency with the two amounts above.
    pub fee_charged_atomic: AtomicU64,
    /// The refund transaction: a Goldcoin txid (hex, the same encoding
    /// [`TransferView::source_txid`] uses) for `GlcToSol`, a base58
    /// Solana signature for `SolToGlc`. `None` until the refund has been
    /// broadcast.
    pub refund_txid: Option<String>,
    /// Unix seconds the refund transaction was handed to the network.
    pub broadcast_at: Option<i64>,
    /// Unix seconds the refund reached its terminal, confirmed state.
    /// `None` in every earlier state.
    pub refunded_at: Option<i64>,
}

/// `GET /robinhood/reserve` — the Robinhood reserve, reported as a THIRD
/// INDEPENDENT reserve.
///
/// # Never netted, never substituted
///
/// This endpoint exists separately from `GET /reserve` (which reports
/// Goldcoin and Solana) for a reason that is an accounting fact rather
/// than a presentation choice: these are different physical pools on
/// different chains, and one cannot cover the other. Nothing here is
/// summed with, differenced against, or defaulted from a Goldcoin or
/// Solana figure, and `GET /reserve` is deliberately left exactly as it
/// was so that every existing client keeps reading the same two numbers
/// it always did.
///
/// # Absent is not zero
///
/// The Robinhood reserve is configured by a `[reserve.robinhood]`
/// section, which no production deployment has today. Without it there is
/// no `reserve_ledger` row at all, so nothing can be reserved against it
/// and every ledger figure below is `null` with
/// `ledger_availability = "not_configured"` — never `0`, which would
/// claim an empty reserve exists. The same discipline applies to the
/// contract half independently: see [`RobinhoodOnchainView`].
#[derive(Debug, Serialize, Deserialize)]
pub struct RobinhoodReserveView {
    /// `"available"` when a `reserve_ledger` row exists,
    /// `"not_configured"` when it does not — the constants on
    /// [`crate::robinhood::public`]. When this is not `"available"`,
    /// every ledger field below is `null`.
    pub ledger_availability: String,
    /// Canonical 8-decimal units (NOT Robinhood's native 18 — see
    /// [`ReserveDirection::RobinhoodReserve`]'s docs on why an `INTEGER`
    /// column cannot hold the latter). String on the wire.
    pub balance_atomic: Option<AtomicU64>,
    /// GLC that may never be paid out. Canonical units.
    pub protected_minimum_atomic: Option<AtomicU64>,
    /// Capacity currently held against accepted-but-unsettled requests.
    pub reserved_liquidity_atomic: Option<AtomicU64>,
    /// Liquidity committed to `SourceFinalized`-or-later requests — the
    /// pending OUTBOUND obligations this reserve owes.
    pub pending_obligations_atomic: Option<AtomicU64>,
    /// `balance - protected_minimum - reserved`. Signed: a capacity below
    /// zero is a real diagnostic state and is reported, not clamped —
    /// same rule as [`ReserveAvailability`].
    pub available_capacity_atomic: Option<AtomicI64>,
    /// Cumulative bridge-fee revenue accrued on this reserve. Never
    /// counted toward capacity.
    pub accrued_fees_atomic: Option<AtomicU64>,
    /// This reserve's own pause flag. `null` when unconfigured — "not
    /// paused" would be a claim about a reserve that does not exist.
    pub paused: Option<bool>,
    /// The contract's own view of the same reserve, and the rolling
    /// windows only it knows.
    pub onchain: RobinhoodOnchainView,
    /// The two Robinhood routes' entries exactly as `GET /chains`
    /// reports them — built by the same [`RouteView::build`], repeated
    /// here so a client rendering the reserve page does not have to
    /// correlate two responses. Both `enabled` (the
    /// [`crate::routes::RouteGate`] verdict) and `available` (that AND
    /// every runtime gate on the destination reserve) carry their usual
    /// meanings; see [`RouteView`]. Reading this can never change it.
    ///
    /// Note that `paused` above is the ROBINHOOD reserve's pause, which
    /// backs `GlcToRhn` only. `RhnToGlc` pays out of the GOLDCOIN
    /// reserve, so its `available` here is not derived from `paused`
    /// above and the two can legitimately disagree.
    pub routes: Vec<RouteView>,
    /// The Robinhood indexer's liveness, when this deployment has one.
    pub indexer: RobinhoodIndexerView,
    pub as_of: i64,
}

/// The Robinhood custody contract's own figures, in its native
/// 18-decimal units.
///
/// # Why the amounts are strings and not [`AtomicU64`]
///
/// A `uint256` at 18 decimals does not fit a `u64`: one whole GLC is
/// 10^18, already past `u64::MAX` at nineteen tokens. These are decimal
/// strings of the exact word the contract returned — the same
/// "amounts are strings on the wire" contract [`crate::api::atomic`]
/// established, applied to a wider integer.
///
/// # `availability` is load-bearing
///
/// `"available"` means every figure here came from one live read of the
/// deployed contract. `"not_configured"` means this deployment has no
/// Robinhood contract to ask. `"unavailable"` means it has one and the
/// read did not complete. In the latter two, every field is `null`.
/// There is no fourth case in which a number here was derived from
/// anything other than the contract: this service holds no copy of these
/// values to fall back on, deliberately, so that a config file can never
/// disagree with the chain about what a user may transfer.
#[derive(Debug, Serialize, Deserialize)]
pub struct RobinhoodOnchainView {
    /// One of [`crate::robinhood::public::AVAILABILITY_AVAILABLE`],
    /// `AVAILABILITY_NOT_CONFIGURED`, `AVAILABILITY_UNAVAILABLE`.
    pub availability: String,
    /// `encumberedReserve()` — the protected floor plus every unsettled
    /// depositor's principal: GLC the contract physically holds that is
    /// not the bridge's to pay out. Robinhood 18dp, decimal string.
    pub encumbered_reserve_atomic: Option<String>,
    /// `limits().protectedMinReserve`. Robinhood 18dp.
    pub protected_min_reserve_atomic: Option<String>,
    /// Governance's inbound kill switch (`depositsPaused()`).
    pub deposits_paused: Option<bool>,
    /// The outbound one (`payoutsPaused()`). A separate flag on-chain, so
    /// separate here — a paused payout leg is not a paused bridge.
    pub payouts_paused: Option<bool>,
    /// The DEPOSIT direction's rolling 24h window.
    pub inbound_window: Option<RobinhoodWindowView>,
    /// The PAYOUT direction's.
    pub outbound_window: Option<RobinhoodWindowView>,
    /// The fixed rolling-window length in seconds, so a client need not
    /// hardcode "24 hours".
    pub window_seconds: Option<u64>,
}

/// One direction's rolling-window consumption, exactly as the contract
/// accounts for it. Robinhood 18-decimal units, decimal strings.
#[derive(Debug, Serialize, Deserialize)]
pub struct RobinhoodWindowView {
    /// The ceiling for this direction in one bucket.
    pub limit_atomic: String,
    /// Consumed in the bucket `as_of` falls in. Reported as `0` — a real
    /// figure, not a placeholder — once the recorded bucket has expired,
    /// mirroring the contract's own reset on its next write rather than
    /// showing a total that is no longer charged against anything.
    pub used_atomic: String,
    /// `limit - used`, saturating at zero (a lowered limit can leave an
    /// existing bucket above the new value; that is zero remaining, not
    /// negative).
    pub remaining_atomic: String,
    /// Unix seconds at which the current bucket expires and the full
    /// limit becomes available again.
    pub resets_at: u64,
    /// Whether the recorded bucket is the one `as_of` falls in. `false`
    /// means it has rolled over and `used_atomic` is `0` for that reason.
    pub is_current: bool,
}

/// `GET /robinhood/limits` — per-transfer and rolling limits, read from
/// the deployed `GlcRobinhoodBridge` and from nowhere else.
///
/// # Why this is not `GET /limits`
///
/// [`TransferLimits`] reports the SOLANA program's `BridgeConfig`:
/// `min_transfer_amount`/`per_transfer_limit` as that program enforces
/// them, in canonical units. Those figures bound Solana releases. They
/// are not Robinhood's, they are not enforced on Robinhood, and copying
/// them here would publish a limit neither chain applies. `GET /limits`
/// is therefore untouched and keeps its exact existing shape.
///
/// # Unknown is reported as unknown
///
/// When `availability` is not `"available"` every limit below is `null`.
/// This service holds no service-side copy of these values to substitute
/// — `[robinhood.settlement]` carries submitter, gas and quorum policy
/// and deliberately no min, max or rolling limit — so there is nothing
/// here that could be a stale or invented number.
#[derive(Debug, Serialize, Deserialize)]
pub struct RobinhoodLimitsView {
    /// One of the three [`crate::robinhood::public`] availability
    /// constants; see [`RobinhoodOnchainView::availability`].
    pub availability: String,
    /// Minimum accepted DEPOSIT (`RhnToGlc` source leg). Robinhood 18dp,
    /// decimal string.
    pub inbound_min_atomic: Option<String>,
    /// Maximum accepted deposit.
    pub inbound_max_atomic: Option<String>,
    /// The deposit direction's rolling-window ceiling.
    pub inbound_rolling_limit_atomic: Option<String>,
    /// Minimum PAYOUT (`GlcToRhn` destination leg).
    pub outbound_min_atomic: Option<String>,
    pub outbound_max_atomic: Option<String>,
    pub outbound_rolling_limit_atomic: Option<String>,
    /// GLC that may never be paid out, whatever else is true.
    pub protected_min_reserve_atomic: Option<String>,
    /// The rolling-window length in seconds.
    pub rolling_window_seconds: Option<u64>,
    /// `RhnToGlc`'s rolling-window state, as the contract accounts for
    /// it RIGHT NOW — the authoritative answer to "how much more may move
    /// on this route in this window".
    ///
    /// # Why a route name on a contract field
    ///
    /// The contract keeps ONE accumulator per direction, not per route:
    /// this is `inboundWindow()`, charged by `_consumeWindow` inside
    /// `deposit()` against `inboundRollingLimit`. `RhnToGlc` is the
    /// inbound route (`ROUTE_RHN_TO_GLC = 0x02`; `_routeLegs` returns
    /// `inbound = true`), so this window is what bounds it, and naming
    /// the field after the route is what stops a client pairing an
    /// inbound figure with an outbound route. The sibling `inbound_*`
    /// limit fields above are the ceilings THIS window is charged
    /// against.
    ///
    /// The contract's other inbound route, `ROUTE_RHN_TO_SOL = 0x04`,
    /// shares the same accumulator. Since Phase H
    /// (docs/35-solana-robinhood-routes-phase-h.md) this service CAN
    /// settle that route, but it ships disabled on every gate, so in a
    /// deployment that has not opened it nothing this service does
    /// consumes the window behind `RhnToGlc`'s back. That is a property
    /// of the deployment and not of this figure: the number is READ from
    /// the accumulator the contract charges, so it stays true even where
    /// something else — including an opened `RhnToSol` — is charging it.
    ///
    /// `null` under exactly the same rule as every limit above: unread is
    /// unknown, never zero.
    pub rhn_to_glc_rolling_window: Option<RobinhoodWindowView>,
    /// `GlcToRhn`'s rolling-window state — `outboundWindow()`, charged by
    /// `_consumeWindow` inside `executePayout` against
    /// `outboundRollingLimit`. `GlcToRhn` is the outbound route
    /// (`ROUTE_GLC_TO_RHN = 0x01`, `_routeLegs` returns `inbound =
    /// false`), so this is the window that bounds it; `ROUTE_SOL_TO_RHN`
    /// shares it, under the same note as above.
    ///
    /// # What the figure is denominated in
    ///
    /// `executePayout` consumes `req.amount`, which is the NET this
    /// service pays out after `GlcToRhn`'s fee — not the gross a user
    /// spends on the Goldcoin side. `remaining_atomic` is therefore
    /// remaining PAYOUT capacity, in the same unit the contract charges.
    /// A client converting it into gross terms must do so explicitly;
    /// nothing here has been pre-adjusted for a fee.
    pub glc_to_rhn_rolling_window: Option<RobinhoodWindowView>,
    /// The rate `crate::robinhood::fold` applies to an inbound Robinhood
    /// deposit — i.e. `RhnToGlc`'s configured fee, in basis points.
    ///
    /// NOT read from the contract: `GlcRobinhoodBridge` stores no fee at
    /// all (its `Limits` struct carries minimums, maximums, rolling
    /// limits and a protected minimum, and nothing else), so the fee is
    /// purely this service's own. Present even when `availability` is not
    /// `"available"`, because it is known without reaching the chain.
    ///
    /// It used to be the compiled-in global constant, described here as
    /// "the same rate `GET /limits` reports" — which was wrong the moment
    /// `[robinhood.policy].fee_bps` differed from it, and wrong in the
    /// direction that under-reported what a depositor was actually
    /// charged. The two directions are now reported separately below,
    /// because they can differ.
    pub bridge_fee_bps: u64,
    /// `GlcToRhn`'s configured fee in basis points — the OUTBOUND
    /// direction, charged when a Goldcoin-side request is created.
    pub glc_to_rhn_fee_bps: u64,
    /// `RhnToGlc`'s configured fee in basis points — the INBOUND
    /// direction, charged at fold time. Equal to `bridge_fee_bps` above,
    /// which is retained under its historical name.
    pub rhn_to_glc_fee_bps: u64,
    pub as_of: i64,
}

/// The Robinhood deposit indexer's liveness, in the same non-sensitive
/// register as [`PublicHealth`]: whether it is configured, whether its
/// last tick reached the endpoint, how far behind the chain head it is,
/// and whether it has halted. Never an RPC URL, a host, a chain id or an
/// error string — those are `ops::health`'s and `glc-admin`'s to show an
/// operator, and the snapshot's own error text is redacted even there.
#[derive(Debug, Serialize, Deserialize)]
pub struct RobinhoodIndexerView {
    /// `false` when this deployment has no `[robinhood.indexer]` section.
    /// Every other field is then `null`/`false`: no client was built and
    /// no socket was ever opened.
    pub configured: bool,
    /// Whether the LAST attempted tick reached the endpoint.
    pub connected: bool,
    /// `head - cursor`, in blocks, at the last successful read.
    pub lag_blocks: Option<u64>,
    /// Unix seconds of the last tick that completed without erroring.
    pub last_success_at: Option<i64>,
    /// `true` when the indexer has stopped for a condition requiring an
    /// operator. Robinhood-local: it pauses no reserve.
    pub halted: bool,
}

/// Caller input for `GET /quote`: how much GROSS the caller intends to
/// bridge, in the ledger's canonical accounting unit (8 decimals,
/// docs/20-bridge-fee.md), regardless of direction. A future UI converts
/// a user-typed decimal GLC amount to this unit itself (`* 10^8`) before
/// calling — kept as one single, unambiguous unit here rather than one
/// that varies by direction, consistent with `CreateTransferInput`.
#[derive(Debug, Serialize, Deserialize)]
pub struct QuoteInput {
    pub direction: String,
    /// Atomic. Accepts a decimal string (preferred) or a JSON integer.
    pub gross_amount: AtomicU64,
}

/// Server-authoritative bridge quote. The UI displaying "You bridge: X
/// GLC / Bridge fee (3%): Y GLC / You receive: Z GLC" must source X/Y/Z
/// from here, never compute them itself (docs/20-bridge-fee.md) — this
/// endpoint runs the exact same `amount_conversion::compute_fee` the
/// server uses to actually build a settlement, so a quote can never
/// promise something a real transfer would compute differently.
#[derive(Debug, Serialize, Deserialize)]
pub struct QuoteOutput {
    pub direction: String,
    /// Canonical units; string on the wire.
    pub gross_amount: AtomicU64,
    /// Human-readable GLC, computed via checked integer arithmetic (never
    /// a float) — e.g. `"12.34500000"`.
    pub gross_display_amount: String,
    pub fee_bps: u64,
    /// Canonical units.
    pub fee_amount: AtomicU64,
    pub fee_display_amount: String,
    /// Canonical units — the real-world GLC entitlement, before the
    /// destination chain's own decimal precision is applied.
    pub net_amount: AtomicU64,
    pub net_display_amount: String,
    /// The SOURCE chain's own atomic-unit decimals for this direction.
    pub source_decimals: u8,
    /// The DESTINATION chain's own atomic-unit decimals for this
    /// direction — what `net_amount` is actually converted to and
    /// released as, on-chain.
    pub destination_decimals: u8,
    pub source_asset: String,
    pub destination_asset: String,
}

/// `GET /recipients/sol-to-glc/eligibility?address=<Goldcoin p2pkh
/// address>&wallet=<base58 Solana pubkey, optional>` — whether a NEW
/// SolToGlc obligation naming this recipient (and, if `wallet` is given,
/// deposited from this Solana wallet) would currently be admitted, or
/// parked by ONE OF the two independent rolling 24-hour rate limits
/// (docs/09-runbook.md): the per-recipient limit
/// (`Ledger::goldcoin_recipient_rate_limited_until`) and the per-
/// source-wallet limit (`Ledger::sol_to_glc_source_wallet_rate_limited_until`)
/// that closes the bypass where one wallet spreads deposits across many
/// different recipients.
///
/// The recipient leg is ROUTE-AGNOSTIC: a Goldcoin address that recently
/// received an `RhnToGlc` payout reads as ineligible here too, because
/// the destination limit is one window per address across every inbound
/// route. The wallet leg is Solana-specific and says nothing about any
/// EVM wallet.
///
/// `GET /recipients/rhn-to-glc/eligibility?address=<Goldcoin p2pkh
/// address>&wallet=<0x EVM address, optional>` is the exact twin for the
/// Robinhood route, served by
/// [`ApiSource::rhn_to_glc_recipient_eligibility`]: same response shape,
/// same `blocked_reason` values, same `window_seconds`, same optional
/// `wallet` leg. It reads the SAME route-global recipient window this one
/// does, and the `RhnToGlc`-scoped source-wallet window instead of the
/// Solana one.
///
/// Both endpoints answer about RATE LIMITS only. Neither says anything
/// about whether the route is open, the reserve is funded, or the chain
/// adapter is operational — `GET /chains`'s [`RouteView::available`] owns
/// that question (and `GET /robinhood/reserve` repeats it), and a deposit
/// can still be parked for one of those reasons after this endpoint said
/// "eligible". The converse also holds: `available` is route-wide and
/// knows no addresses, so it can be `true` while THIS recipient or THIS
/// wallet is still inside its rolling-24h window. A UI wanting to be sure
/// a specific transfer would be admitted has to read both. Both read through the exact same query
/// `Ledger::fold_sol_deposit`'s admission check applies, so the answer is
/// always the authoritative ledger rule, never a re-implementation.
/// `wallet` is optional so existing callers that only know the recipient
/// so far keep working — omitting it simply means the source-wallet leg
/// is not checked yet. Read-only and purely advisory: the UI calls it to
/// warn a user BEFORE they sign a Solana transaction whose deposit would
/// only be parked in `ManualReview` — admission itself still re-checks at
/// fold time (independently, keyed by the on-chain `WithdrawalObligation`'s
/// own `requester`, never a client-provided string), so a stale (or
/// bypassed) answer here can never weaken either limit.
///
/// Deliberately minimal disclosure, consistent with this API never
/// exposing per-recipient/per-wallet identity elsewhere: a boolean, which
/// limits are blocking, and their reopen times — never which request is
/// blocking, its amount, its state, or anything else about the history.
///
/// # One type, both inbound routes
///
/// `GET /recipients/sol-to-glc/eligibility` and
/// `GET /recipients/rhn-to-glc/eligibility` return this same shape, built
/// by the same [`RecipientEligibility::from_windows`], differing only in
/// `direction`, how `wallet` is spelled, and which source-wallet limiter
/// supplied its window. Two structs would have been two places for the
/// precedence and the retry arithmetic to drift.
#[derive(Debug, Serialize, Deserialize)]
pub struct RecipientEligibility {
    /// `"SolToGlc"` or `"RhnToGlc"` — the route this answer is about.
    /// `GlcToSol`/`GlcToRhn` recipients have no rate limit at all. Note
    /// that the RECIPIENT verdict is route-agnostic either way (a recent
    /// payout to the same address on the OTHER inbound route blocks here
    /// too); only the WALLET verdict is specific to this route's source
    /// chain.
    pub direction: String,
    /// The trimmed address this answer is about — echoed back so a caller
    /// racing form edits can discard a stale response.
    pub address: String,
    /// The wallet this answer also checked, when `?wallet=` was given —
    /// base58 for `SolToGlc`, `0x`-prefixed lowercase hex for `RhnToGlc`.
    /// `null` when it was omitted, so a caller can tell "the wallet leg
    /// was not evaluated" apart from "it was evaluated and found
    /// eligible."
    pub wallet: Option<String>,
    /// `true` only when NEITHER limit currently blocks a new obligation.
    pub eligible: bool,
    /// Which limit is blocking, when `eligible` is `false`:
    /// [`BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED`] or
    /// [`BLOCKED_REASON_RECIPIENT_RATE_LIMITED`]. `null` when eligible.
    /// Checked wallet-first when `wallet` was provided and both would
    /// block, matching `Ledger::fold_sol_deposit`'s own precedence — the
    /// two limits are independently enforced either way, so this only
    /// affects which single reason is surfaced here.
    pub blocked_reason: Option<String>,
    /// EVERY limit currently blocking, not just the one `blocked_reason`
    /// surfaces — `[]` when eligible, one entry when a single limit
    /// applies, and BOTH entries (source wallet first) when both do.
    ///
    /// Added because `blocked_reason` deliberately reports a single
    /// reason and cannot say "both": a caller that wants to tell a user
    /// everything they must wait for needs the full set, and deriving it
    /// client-side would mean re-implementing the precedence. Purely
    /// additive — `blocked_reason` keeps its exact original meaning.
    #[serde(default)]
    pub blocked_reasons: Vec<String>,
    /// Absolute unix second at which the window named by `blocked_reason`
    /// reopens; `null` when eligible.
    pub retry_after: Option<i64>,
    /// The same instant as seconds from now, clamped at zero; `null` when
    /// eligible.
    pub retry_after_seconds: Option<i64>,
    /// Per-limit reopen instants, so a caller showing both reasons can
    /// show both waits. `null` where that limit is not blocking (and, for
    /// the wallet leg, where `?wallet=` was omitted and it was therefore
    /// never evaluated).
    #[serde(default)]
    pub source_wallet_retry_after: Option<i64>,
    #[serde(default)]
    pub recipient_retry_after: Option<i64>,
    /// The rolling window itself (86 400), shared by every limit, so
    /// clients need not hardcode "24 hours" in copy or logic.
    pub window_seconds: i64,
}

impl RecipientEligibility {
    /// Assembles the verdict from two ALREADY-COMPUTED windows.
    ///
    /// This function performs no rate-limit reasoning of its own — it
    /// never touches the ledger, the window length, the exclude-list or
    /// the `created_at` arithmetic. Its inputs are whatever
    /// `Ledger::goldcoin_recipient_rate_limited_until` and the route's
    /// own source-wallet view returned, which are the SAME shared queries
    /// the two fold paths enforce with. All this owns is presentation:
    /// which single reason `blocked_reason` names, and the derived
    /// `retry_after_seconds`.
    ///
    /// `blocked_reason` is wallet-first when both apply, matching both
    /// folds' own `manual_review_note` ranking, so the one reason a UI
    /// shows is the one an actual deposit would have been parked under.
    #[allow(clippy::too_many_arguments)]
    fn from_windows(
        direction: &str,
        address: String,
        wallet: Option<String>,
        source_wallet_retry_after: Option<i64>,
        recipient_retry_after: Option<i64>,
        now: i64,
    ) -> Self {
        let mut blocked_reasons = Vec::new();
        if source_wallet_retry_after.is_some() {
            blocked_reasons.push(BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED.to_string());
        }
        if recipient_retry_after.is_some() {
            blocked_reasons.push(BLOCKED_REASON_RECIPIENT_RATE_LIMITED.to_string());
        }
        let (blocked_reason, retry_after) = match (source_wallet_retry_after, recipient_retry_after)
        {
            (Some(t), _) => (Some(BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED), Some(t)),
            (None, Some(t)) => (Some(BLOCKED_REASON_RECIPIENT_RATE_LIMITED), Some(t)),
            (None, None) => (None, None),
        };
        RecipientEligibility {
            direction: direction.to_string(),
            address,
            wallet,
            eligible: retry_after.is_none(),
            blocked_reason: blocked_reason.map(str::to_string),
            blocked_reasons,
            retry_after,
            retry_after_seconds: retry_after.map(|t| (t - now).max(0)),
            source_wallet_retry_after,
            recipient_retry_after,
            window_seconds: Ledger::RECIPIENT_RATE_LIMIT_WINDOW_SECS,
        }
    }
}

/// `blocked_reason` values [`RecipientEligibility`] reports — named
/// constants (rather than inline string literals at each call site) so the
/// backend answer and the UI's message-selection logic can never drift
/// apart on the exact spelling.
pub const BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED: &str = "source_wallet_rate_limited";
pub const BLOCKED_REASON_RECIPIENT_RATE_LIMITED: &str = "recipient_rate_limited";

/// `GET /routes/{route}/eligibility?source=<address>&destination=<address>`
/// — the route-generic pre-check for the rolling-24h WALLET UNIQUENESS
/// rule (`ledger::wallet_window`), on every one of the six routes: would
/// a NEW request on `route` funded from `source` and paid to
/// `destination` be admitted right now, or is either wallet still inside
/// its 24-hour window?
///
/// Each address is spelled in ITS OWN chain's notation, decided by the
/// route — a Goldcoin P2PKH/P2SH address, a base58 Solana pubkey, a
/// `0x` EVM address — and is validated as that chain's address type
/// before anything is looked up: a malformed one is a `400`, never a
/// misleading verdict about bytes no deposit could carry. At least one
/// of the two must be given; a leg that is omitted is `null` and is
/// simply not evaluated, so a UI can ask about the destination before it
/// knows the connected wallet.
///
/// The answer is the SAME query every admission path enforces with
/// (`Ledger::route_wallet_eligibility`), read-only and purely advisory:
/// a UI calls it to warn the user BEFORE they sign a source-chain
/// transaction whose deposit would only be parked in `ManualReview` (or,
/// on the Goldcoin-sourced routes, before `POST /transfers` refuses with
/// `429`). Admission re-checks for itself — on the Goldcoin-sourced
/// routes against the wallets the deposit was REALLY funded from — so a
/// stale or bypassed answer here can never weaken the rule.
///
/// Like [`RecipientEligibility`] this answers about the wallet windows
/// ONLY. Whether the route is open, funded and operational is
/// `GET /chains`'s [`RouteView::available`]; a UI wanting to be sure a
/// transfer would be admitted reads both.
///
/// Deliberately minimal disclosure: per leg, a boolean, the reason and
/// the reopen time — never which request is blocking, its amount, its
/// state, or anything else about the wallet's history.
#[derive(Debug, Serialize, Deserialize)]
pub struct RouteWalletEligibilityView {
    /// The route asked about, `Route::as_str` spelling.
    pub route: String,
    /// The source-wallet leg, `null` when `?source=` was omitted.
    pub source: Option<WalletLegView>,
    /// The destination-wallet leg, `null` when `?destination=` was
    /// omitted.
    pub destination: Option<WalletLegView>,
    /// `true` only when NO evaluated leg is inside its window.
    pub eligible: bool,
    /// The single limit to show when one or both apply — source first,
    /// matching the folds' own `manual_review_note` ranking:
    /// [`BLOCKED_REASON_WALLET_SOURCE_24H_LIMIT`] or
    /// [`BLOCKED_REASON_WALLET_DESTINATION_24H_LIMIT`]. `null` when
    /// eligible.
    pub blocked_reason: Option<String>,
    /// EVERY blocking limit, source first — `[]` when eligible.
    pub blocked_reasons: Vec<String>,
    /// Absolute unix second at which the window named by
    /// `blocked_reason` reopens; `null` when eligible.
    pub retry_after: Option<i64>,
    /// The same instant as seconds from `as_of`, clamped at zero.
    pub retry_after_seconds: Option<i64>,
    /// The rolling window itself (86 400), shared by every limit on
    /// every route.
    pub window_seconds: i64,
    pub as_of: i64,
}

/// One wallet's verdict inside [`RouteWalletEligibilityView`].
#[derive(Debug, Serialize, Deserialize)]
pub struct WalletLegView {
    /// The address as canonicalized — the exact spelling the ledger keys
    /// this wallet's window on — echoed so a caller racing form edits can
    /// discard a stale response.
    pub address: String,
    pub eligible: bool,
    /// The leg's reason when blocked, `null` when eligible.
    pub reason: Option<String>,
    /// Unix second the leg's window reopens, `null` when eligible.
    pub retry_after: Option<i64>,
    /// Seconds from `as_of` until the leg's window reopens, clamped at
    /// zero, `null` when eligible.
    pub retry_after_seconds: Option<i64>,
}

impl WalletLegView {
    fn from_retry_after(
        address: String,
        role: WalletRole,
        retry_after: Option<i64>,
        now: i64,
    ) -> Self {
        WalletLegView {
            address,
            eligible: retry_after.is_none(),
            reason: retry_after.map(|_| role.limit_reason().to_string()),
            retry_after,
            retry_after_seconds: retry_after.map(|t| (t - now).max(0)),
        }
    }
}

impl RouteWalletEligibilityView {
    /// Assembles the verdict from the ledger's ALREADY-COMPUTED windows.
    /// Presentation only — no window reasoning of its own, exactly like
    /// [`RecipientEligibility::from_windows`].
    fn from_eligibility(
        route: crate::routes::Route,
        source: Option<String>,
        destination: Option<String>,
        eligibility: RouteWalletEligibility,
        now: i64,
    ) -> Self {
        let (blocked_reason, retry_after) = match eligibility.blocker() {
            Some((role, t)) => (Some(role.limit_reason().to_string()), Some(t)),
            None => (None, None),
        };
        RouteWalletEligibilityView {
            route: route.as_str().to_string(),
            source: source.map(|address| {
                WalletLegView::from_retry_after(
                    address,
                    WalletRole::Source,
                    eligibility.source_retry_after,
                    now,
                )
            }),
            destination: destination.map(|address| {
                WalletLegView::from_retry_after(
                    address,
                    WalletRole::Destination,
                    eligibility.destination_retry_after,
                    now,
                )
            }),
            eligible: retry_after.is_none(),
            blocked_reason,
            blocked_reasons: eligibility
                .blocked_reasons()
                .into_iter()
                .map(str::to_string)
                .collect(),
            retry_after,
            retry_after_seconds: retry_after.map(|t| (t - now).max(0)),
            window_seconds: Ledger::WALLET_WINDOW_SECS,
            as_of: now,
        }
    }
}

/// The `blocked_reason`/`reason` values [`RouteWalletEligibilityView`]
/// reports, and the `manual_review_note` a parked deposit carries, and the
/// `blocked_reasons` a `429` from `POST /transfers` names — ONE
/// vocabulary, read from the ledger's own constants so the backend
/// verdict, the durable park reason and the UI's message selection can
/// never drift on the spelling.
pub const BLOCKED_REASON_WALLET_SOURCE_24H_LIMIT: &str =
    Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT;
pub const BLOCKED_REASON_WALLET_DESTINATION_24H_LIMIT: &str =
    Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT;

/// Parses and canonicalizes a wallet a caller asks about, AS the
/// address type of the chain that plays `role` on `route`, returning
/// `(canonical spelling, the bytes the ledger keys its window on)`.
///
/// The bytes are exactly what admission compares: a Goldcoin address's
/// text (its canonical base58check spelling — for a destination, the
/// P2PKH-only rule the payout builder applies; for a source, P2PKH or
/// P2SH, the two forms the indexer spells a funding prevout as), a
/// Solana pubkey's 32 bytes, an EVM address's 20 bytes. The EVM zero
/// address is refused as a destination for the same reason
/// `POST /transfers` refuses it.
fn parse_route_wallet(
    route: crate::routes::Route,
    role: WalletRole,
    raw: &str,
    goldcoin_network: crate::goldcoin::address::Network,
) -> Result<(String, Vec<u8>), ApiError> {
    let raw = raw.trim();
    let chain = match role {
        WalletRole::Source => route.source_chain(),
        WalletRole::Destination => route.destination_chain(),
    };
    let invalid =
        |e: String| ApiError::BadRequest(format!("invalid {} address: {e}", role.as_str()));
    match chain {
        crate::routes::Chain::Goldcoin => {
            let canonical = match role {
                WalletRole::Destination => {
                    crate::goldcoin::address::decode_p2pkh(raw, goldcoin_network)
                        .map_err(|e| invalid(format!("not a Goldcoin p2pkh address: {e}")))?;
                    raw.to_string()
                }
                WalletRole::Source => {
                    crate::goldcoin::address::canonical_standard_address(raw, goldcoin_network)
                        .map_err(|e| invalid(format!("not a Goldcoin address: {e}")))?
                }
            };
            let bytes = canonical.as_bytes().to_vec();
            Ok((canonical, bytes))
        }
        crate::routes::Chain::Solana => {
            let key = raw
                .parse::<Pubkey>()
                .map_err(|e| invalid(format!("not a Solana pubkey: {e}")))?;
            Ok((key.to_string(), key.to_bytes().to_vec()))
        }
        crate::routes::Chain::Robinhood => {
            let address = raw
                .parse::<crate::evm::address::EvmAddress>()
                .map_err(|e| invalid(format!("not an EVM address: {e}")))?;
            if role == WalletRole::Destination && address.is_zero() {
                return Err(invalid(
                    "the zero address is the EVM burn sink, not a payout destination".into(),
                ));
            }
            Ok((address.to_string(), address.to_bytes().to_vec()))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("invalid request: {0}")]
    BadRequest(String),
    /// Reserve/protected-minimum capacity constraint — one of the three
    /// direction-unavailable causes (see [`DIRECTION_UNAVAILABLE_MESSAGE`]).
    /// `available` is retained on the variant for server-side logging/
    /// introspection; the user-facing text is the same generic message as
    /// every other cause, never this raw number (this API's own module
    /// docs already avoid leaking raw reserve figures elsewhere).
    #[error("{}", DIRECTION_UNAVAILABLE_MESSAGE)]
    InsufficientLiquidity { available: i64 },
    /// Operator pause (on-chain `PauseScope::Release`/`Deposit`/`Global`,
    /// or this service's own local ledger gate) — see
    /// [`DIRECTION_UNAVAILABLE_MESSAGE`].
    #[error("{}", DIRECTION_UNAVAILABLE_MESSAGE)]
    Paused,
    /// Rolling-24h-volume quota exhausted for this direction, from a
    /// live, read-only on-chain check made right here at request time —
    /// see [`DIRECTION_UNAVAILABLE_MESSAGE`]. This specific check never
    /// itself sets any pause flag; it only reports the exhaustion up
    /// front instead of letting the deposit be accepted and fail later
    /// at on-chain release time. Separately, this service's own
    /// background tick (`crate::quota`) may ALSO have already engaged
    /// this direction's local pause once it observed the same
    /// exhaustion — in which case a caller may see [`ApiError::Paused`]
    /// instead on a later request, which (unlike the on-chain window's
    /// own automatic reset) never clears itself; see
    /// docs/09-runbook.md's 2026-08-22 update for the full workflow.
    /// Either way the end-user copy is identical.
    #[error("{}", DIRECTION_UNAVAILABLE_MESSAGE)]
    QuotaExhausted,
    /// The requested route exists as a name but is not open on this
    /// deployment — `crate::routes::RouteGate` refused it.
    ///
    /// Deliberately a DISTINCT variant from [`ApiError::Paused`]: a paused
    /// direction is temporarily closed machinery that an operator reopens,
    /// whereas this is a route whose settlement machinery does not exist in
    /// this build. Collapsing them would tell a user "check back later" about
    /// something no amount of waiting changes, and would tell an operator
    /// "unpause it" about something no unpause can open.
    ///
    /// The message is `crate::routes::RouteGateError::UNAVAILABLE_MESSAGE` —
    /// cause-agnostic, and it never reveals WHICH of the three gates
    /// refused (that detail is logged server-side and exposed only to
    /// operators), so a probing client cannot map out the deployment's
    /// configuration from error text.
    #[error("{}", crate::routes::RouteGateError::UNAVAILABLE_MESSAGE)]
    RouteDisabled,
    /// The destination wallet — or the declared source wallet — is still
    /// inside its rolling 24-hour window (`ledger::wallet_window`): the
    /// request was refused before any capacity was reserved, and the
    /// caller should read [`RouteWalletEligibilityView`] to show the
    /// user when each leg reopens. `429`, with the reasons and the reopen
    /// instant in the body ([`ErrorBody`]).
    #[error(
        "wallet used within the last 24 hours ({}); retry after {retry_after}",
        blocked_reasons.join(", ")
    )]
    WalletLimited {
        blocked_reasons: Vec<String>,
        retry_after: i64,
    },
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("could not read live chain state: {0}")]
    Upstream(String),
}

impl From<crate::routes::RouteGateError> for ApiError {
    fn from(e: crate::routes::RouteGateError) -> ApiError {
        match e {
            crate::routes::RouteGateError::Disabled { .. } => ApiError::RouteDisabled,
            crate::routes::RouteGateError::Ledger(inner) => ApiError::Ledger(inner),
        }
    }
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::InsufficientLiquidity { .. }
            | ApiError::Paused
            | ApiError::QuotaExhausted
            | ApiError::RouteDisabled => StatusCode::CONFLICT,
            ApiError::WalletLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            ApiError::Ledger(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::Upstream(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Default page size for every cursor-paginated list endpoint, when
/// `?limit=` is omitted.
const DEFAULT_PAGE_LIMIT: u32 = 50;
/// Hard ceiling on page size — a client asking for more is not a 400
/// (it is a legitimate, if greedy, request), so the limit is silently
/// clamped down to this value rather than the request being rejected.
const MAX_PAGE_LIMIT: u32 = 200;

/// Everything the HTTP layer needs; implemented once against the real
/// ledger/chain ([`BridgeApi`]) and mockable for tests.
pub trait ApiSource: Send + Sync + 'static {
    fn status(&self) -> BoxFut<'_, Result<BridgeStatus, ApiError>>;
    /// See [`ChainsView`]. Deliberately a NEW endpoint rather than extra
    /// fields on [`BridgeStatus`]: `GET /status` is consumed by the
    /// deployed UI and by operator tooling, and leaving its shape entirely
    /// untouched is worth more than saving a round trip.
    fn chains(&self) -> BoxFut<'_, Result<ChainsView, ApiError>>;
    fn limits(&self) -> BoxFut<'_, Result<TransferLimits, ApiError>>;
    fn reserve(&self) -> BoxFut<'_, Result<ReserveAvailability, ApiError>>;
    fn create_goldcoin_deposit_transfer(
        &self,
        input: CreateTransferInput,
    ) -> BoxFut<'_, Result<CreateTransferOutput, ApiError>>;
    fn get_transfer(&self, id: i64) -> BoxFut<'_, Result<Option<TransferView>, ApiError>>;
    fn quote(&self, input: QuoteInput) -> BoxFut<'_, Result<QuoteOutput, ApiError>>;
    fn health(&self) -> BoxFut<'_, Result<PublicHealth, ApiError>>;
    fn stats(&self) -> BoxFut<'_, Result<BridgeStats, ApiError>>;
    fn reserves_history(
        &self,
        direction: Option<ReserveDirection>,
        cursor: Option<i64>,
        limit: u32,
    ) -> BoxFut<'_, Result<Page<ReserveHistoryEntry>, ApiError>>;
    fn explorer_events(
        &self,
        direction: Option<Direction>,
        state: Option<RequestState>,
        cursor: Option<i64>,
        limit: u32,
    ) -> BoxFut<'_, Result<Page<ExplorerEvent>, ApiError>>;
    /// Wallet-scoped "my activity" view — every transfer where `address`
    /// is the caller's own address on that route, whichever column
    /// carries it (see [`Ledger::transfers_page`] for the full
    /// direction/column table). `GET /transfers/:id` remains the id-based
    /// lookup; this is the address-based one a UI needs before it knows
    /// any request ids at all.
    ///
    /// The filter is chain-tagged, so a Solana pubkey is only ever
    /// matched against the two Solana-addressed directions and an EVM
    /// address only against the two Robinhood-addressed ones.
    fn list_transfers(
        &self,
        address: Option<TransferAddressFilter>,
        state: Option<RequestState>,
        cursor: Option<i64>,
        limit: u32,
    ) -> BoxFut<'_, Result<Page<TransferView>, ApiError>>;
    /// See [`RecipientEligibility`]. `address` is the raw user-entered
    /// Goldcoin destination address string; `wallet`, when given, is the
    /// connected Solana wallet's pubkey bytes (already base58-decoded by
    /// the query parser — never the raw string), checked against the
    /// source-wallet limit alongside the recipient limit.
    fn sol_to_glc_recipient_eligibility(
        &self,
        address: String,
        wallet: Option<[u8; 32]>,
    ) -> BoxFut<'_, Result<RecipientEligibility, ApiError>>;
    /// The `RhnToGlc` twin of
    /// [`ApiSource::sol_to_glc_recipient_eligibility`]. `address` is the
    /// raw user-entered Goldcoin destination string; `wallet`, when
    /// given, is the connected Robinhood wallet's 20 address bytes
    /// (already `0x`-hex-decoded and EIP-55-checked by the query parser —
    /// never the raw string), checked against the Robinhood source-wallet
    /// limit alongside the route-global recipient limit.
    fn rhn_to_glc_recipient_eligibility(
        &self,
        address: String,
        wallet: Option<[u8; 20]>,
    ) -> BoxFut<'_, Result<RecipientEligibility, ApiError>>;
    /// See [`RouteWalletEligibilityView`] — the route-generic wallet
    /// pre-check for every route. `source`/`destination` are the raw
    /// user-entered addresses (at least one present, enforced by the
    /// query parser); each is validated as its chain's address type by
    /// the implementation.
    fn route_wallet_eligibility(
        &self,
        route: crate::routes::Route,
        source: Option<String>,
        destination: Option<String>,
    ) -> BoxFut<'_, Result<RouteWalletEligibilityView, ApiError>>;
    /// See [`RobinhoodReserveView`]. Independent of [`ApiSource::reserve`]
    /// in every sense: a separate endpoint, separate figures, and no
    /// arithmetic between the two.
    fn robinhood_reserve(&self) -> BoxFut<'_, Result<RobinhoodReserveView, ApiError>>;
    /// See [`RobinhoodLimitsView`]. Independent of [`ApiSource::limits`],
    /// which reports the Solana program's own configuration.
    fn robinhood_limits(&self) -> BoxFut<'_, Result<RobinhoodLimitsView, ApiError>>;
}

/// The concrete [`ApiSource`]: a fresh [`Ledger`] connection per call
/// (same concurrency model as [`crate::ops::collector::OpsCollector`] —
/// SQLite's own `BEGIN IMMEDIATE` transactions are the real safety
/// boundary, not a single shared in-process handle) plus a live chain
/// read for the handful of fields ([`BridgeStatus`]/[`TransferLimits`])
/// that only the on-chain `BridgeConfig` actually knows.
pub struct BridgeApi<SR: SolanaRpc> {
    db_path: PathBuf,
    solana_rpc: SR,
    vault_address: String,
    /// The root 2-of-3 vault (unmodified signer set/threshold) — used
    /// ONLY to derive a fresh request-specific deposit vault per new
    /// `GlcToSol` request (`goldcoin::derivation::derive_request_vault`,
    /// Step 1). Never itself the destination of a new request's deposit
    /// instructions; never used to sign anything here.
    root_vault: crate::goldcoin::vault::MultisigVault,
    goldcoin_network: crate::goldcoin::address::Network,
    reservation_ttl_secs: i64,
    goldcoin_confirmation_depth: i64,
    goldcoin_indexer_status: Arc<IndexerStatus>,
    solana_indexer_status: Arc<IndexerStatus>,
    /// The route admission gate. Consulted on every route-bearing request;
    /// never cached into a per-request boolean.
    route_gate: Arc<crate::routes::RouteGate>,
    /// One fee rate per executable route (`[fees]`), resolved at config
    /// load. Every price this API quotes or charges comes from here, BY
    /// ROUTE — there is no global rate left in this file, and no
    /// per-chain one either. A route with no entry is an error, never a
    /// borrowed number.
    route_fees: crate::fees::RouteFees,
    /// The Robinhood indexer's health, when this deployment runs one.
    /// `RobinhoodHealth::unconfigured()` otherwise, which reports
    /// `configured: false` forever — so a reader gets the same shape
    /// either way and never has to distinguish an absent object from an
    /// unhealthy one.
    robinhood_health: Arc<crate::robinhood::health::RobinhoodHealth>,
    /// Live contract reads for the two public Robinhood endpoints.
    /// `None` when no `[robinhood.settlement]` section names a contract —
    /// which is every production deployment today, and is reported as
    /// `"not_configured"` rather than as zeroes. Read-only: this holds no
    /// signer and cannot open a route (see
    /// [`crate::robinhood::public`]).
    robinhood_contract: Option<Arc<dyn crate::robinhood::public::RobinhoodContractSource>>,
    /// The source-side gross floor this instance admits against.
    ///
    /// Always [`crate::min_transfer::SOURCE_MINIMUM_CANONICAL`] in
    /// production: [`BridgeApi::new`] is the only constructor and sets it
    /// unconditionally, no config key reaches it, and the single method
    /// that can change it is named for the tests it exists for. A field
    /// rather than a direct read of the constant so a test whose subject
    /// is something else entirely — pagination, fee rounding, reserve
    /// accounting — can keep using the tiny fixture amounts it was
    /// written with instead of being rewritten around a policy it is not
    /// testing.
    source_minimum: crate::amount_conversion::CanonicalAtomic,
}

impl<SR: SolanaRpc> BridgeApi<SR> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db_path: PathBuf,
        solana_rpc: SR,
        vault_address: String,
        root_vault: crate::goldcoin::vault::MultisigVault,
        goldcoin_network: crate::goldcoin::address::Network,
        reservation_ttl_secs: i64,
        goldcoin_confirmation_depth: i64,
        goldcoin_indexer_status: Arc<IndexerStatus>,
        solana_indexer_status: Arc<IndexerStatus>,
        route_gate: Arc<crate::routes::RouteGate>,
        // A REQUIRED parameter, not a builder step like `with_robinhood`
        // below: an API that can serve a quote before it knows what to
        // charge is an API that will serve a wrong one. Making it
        // positional means no construction path can forget it.
        route_fees: crate::fees::RouteFees,
    ) -> Self {
        BridgeApi {
            db_path,
            solana_rpc,
            vault_address,
            root_vault,
            goldcoin_network,
            reservation_ttl_secs,
            goldcoin_confirmation_depth,
            goldcoin_indexer_status,
            solana_indexer_status,
            route_gate,
            route_fees,
            // Deliberately defaulted rather than added to `new`'s
            // parameter list: a deployment without Robinhood — every one
            // today — constructs this API with the identical call it
            // always made, and the Robinhood endpoints answer
            // "not configured" from these defaults. Attaching the real
            // sources is one explicit builder call in the daemon.
            robinhood_health: crate::robinhood::health::RobinhoodHealth::unconfigured(),
            robinhood_contract: None,
            // The policy, applied by construction. Not read from config,
            // not defaulted from a chain, and not optional: every
            // production `BridgeApi` in existence admits against exactly
            // 100 GLC because this line has no alternative branch.
            source_minimum: crate::min_transfer::SOURCE_MINIMUM_CANONICAL,
        }
    }

    /// Lowers the source-side floor. **Tests only.**
    ///
    /// Deliberately awkward to reach and impossible to reach by accident:
    /// it is not a config key, not an environment variable, and not a
    /// field on any deserialized type, so no deployment can lower the
    /// floor however its TOML is written. The only way to a floor below
    /// [`crate::min_transfer::SOURCE_MINIMUM_CANONICAL`] is a caller that
    /// typed this method's name.
    ///
    /// It exists because most of this file's tests are about something
    /// else — cursor pagination, fee rounding, reserve capacity, route
    /// gating — and were written against fixtures of a few hundred
    /// thousand atomic units seeded from reserves of ten million. Scaling
    /// all of that by four orders of magnitude to satisfy a rule none of
    /// those tests is exercising would rewrite every pinned figure in the
    /// suite, which is how a real regression gets rewritten into
    /// agreement with a bug. The policy itself is proved on the DEFAULT
    /// path instead, by the tests named in
    /// [`crate::min_transfer`]'s module docs and by
    /// `api::tests`' own default-path cases.
    #[doc(hidden)]
    pub fn with_source_minimum_for_tests(
        mut self,
        minimum: crate::amount_conversion::CanonicalAtomic,
    ) -> Self {
        self.source_minimum = minimum;
        self
    }

    /// Every executable route's configured fee, rendered for display.
    ///
    /// One place, so `GET /stats` and any future surface report the same
    /// table in the same order with the same formatting.
    fn route_fee_views(&self) -> Vec<RouteFeeView> {
        self.route_fees
            .iter()
            .map(|(route, fee_bps)| RouteFeeView {
                route: route.as_str().to_string(),
                fee_bps,
                fee_percent_display: crate::chain_policy::human::format_percent(fee_bps),
            })
            .collect()
    }

    /// One route's rate for a DISPLAY surface, where a missing entry must
    /// render as `0` rather than fail a whole status page.
    ///
    /// Never used to price anything: pricing calls
    /// [`crate::fees::RouteFees::fee_bps`], which fails closed. A status
    /// endpoint that 500s because one route is unpriced would hide the
    /// three that are fine, and the misconfiguration is already reported
    /// by `glc-admin fees-show` and refused at startup.
    fn display_fee_bps(&self, route: crate::routes::Route) -> u64 {
        self.route_fees.get(route).unwrap_or(0)
    }

    /// Attaches this deployment's Robinhood read sources.
    ///
    /// Purely additive and purely READ: `health` is the same snapshot
    /// object the indexer tick already updates, and `contract` performs
    /// `eth_call`s only. Neither can enable a route — that stays with
    /// [`crate::routes::RouteGate`]'s three independent gates, which
    /// nothing here consults or mutates.
    pub fn with_robinhood(
        mut self,
        health: Arc<crate::robinhood::health::RobinhoodHealth>,
        contract: Option<Arc<dyn crate::robinhood::public::RobinhoodContractSource>>,
    ) -> Self {
        self.robinhood_health = health;
        self.robinhood_contract = contract;
        self
    }

    /// Resolves a caller-supplied route string (or the `GlcToSol` default)
    /// and runs it through the gate. Returns the settlement [`Direction`]
    /// only when the route both passed the gate AND has settlement
    /// machinery.
    ///
    /// The `as_direction()` step is the last of four independent checks
    /// (config, ledger, adapter, then this) and is the one that cannot be
    /// misconfigured: a Robinhood route has no `Direction`, so even a
    /// deployment that had somehow opened all three gates would still be
    /// unable to produce the value the settlement path requires.
    fn resolve_route(
        &self,
        ledger: &Ledger,
        raw: Option<&str>,
    ) -> Result<(crate::routes::Route, Direction), ApiError> {
        let route: crate::routes::Route = match raw {
            None => crate::routes::Route::GlcToSol,
            Some(s) => s.parse().map_err(ApiError::BadRequest)?,
        };
        self.route_gate.ensure_enabled(ledger, route)?;
        let direction = route.as_direction().ok_or(ApiError::RouteDisabled)?;
        Ok((route, direction))
    }

    fn open_ledger(&self) -> Result<Ledger, ApiError> {
        Ok(Ledger::open(&self.db_path)?)
    }

    /// Shared `BridgeRequest -> TransferView` projection, used by both
    /// [`ApiSource::get_transfer`] and [`ApiSource::list_transfers`] so
    /// the two never drift apart in what they expose.
    fn to_transfer_view(
        &self,
        ledger: &Ledger,
        request: crate::ledger::BridgeRequest,
    ) -> Result<TransferView, ApiError> {
        let destination_txid = ledger
            .get_destination_txid(request.id)?
            .map(|bytes| glc_hex::encode(&bytes));
        // A Goldcoin-funded request has a confirmation depth a user can
        // watch; a contract-sourced one reaches finality by a rule that is
        // not a UTXO confirmation count and is reported elsewhere.
        let required_source_confirmations = if request.direction.source_is_goldcoin() {
            Some(self.goldcoin_confirmation_depth)
        } else {
            None
        };
        let refund = self.refund_view(ledger, &request)?;
        Ok(TransferView {
            id: request.id,
            direction: request.direction.as_str().to_string(),
            state: request.state.as_str().to_string(),
            gross_amount_atomic: AtomicU64(request.gross_amount_atomic),
            fee_bps: request.fee_bps,
            fee_amount_atomic: AtomicU64(request.fee_amount_atomic),
            net_amount_atomic: AtomicU64(request.net_amount_atomic),
            created_at: request.created_at,
            source_txid: request.source_txid.map(|t| glc_hex::encode(&t)),
            source_confirmations: request.source_confirmations,
            required_source_confirmations,
            destination_txid,
            failure_reason: request.failure_reason,
            refund,
        })
    }

    /// The refund-row projection for one request, or `None` when the
    /// request is not in the refund lifecycle at all.
    ///
    /// Gated on `is_refund_lifecycle()` rather than on an unconditional
    /// lookup because this runs once per row of `GET /transfers` too, and
    /// the gate is exact rather than an optimisation: a refund row and
    /// the request's move out of `ManualReview` are written in one
    /// transaction, so "request is in a refund state" and "a refund row
    /// exists" are the same condition.
    fn refund_view(
        &self,
        ledger: &Ledger,
        request: &crate::ledger::BridgeRequest,
    ) -> Result<Option<RefundView>, ApiError> {
        if !request.state.is_refund_lifecycle() {
            return Ok(None);
        }
        Ok(match request.direction {
            // Both Goldcoin-SOURCED directions refund on the Goldcoin
            // side, from the same `goldcoin_refunds` table and the same
            // lifecycle — see `crate::goldcoin::refund`.
            Direction::GlcToSol | Direction::GlcToRhn => ledger
                .get_goldcoin_refund(request.id)?
                .map(|row| RefundView {
                    state: row.state.as_str().to_string(),
                    // Goldcoin's native atomic unit IS the canonical
                    // accounting unit (both 8 decimals,
                    // `amount_conversion::GOLDCOIN_DECIMALS`), so these two
                    // columns need no conversion to sit alongside the rest of
                    // this DTO.
                    observed_amount_atomic: AtomicU64(row.observed_amount_atomic),
                    refund_amount_atomic: AtomicU64(row.refund_amount_atomic),
                    fee_charged_atomic: AtomicU64(0),
                    refund_txid: row.txid.map(|t| glc_hex::encode(&t)),
                    broadcast_at: row.broadcast_at,
                    refunded_at: row.refunded_at,
                }),
            // `solana_refunds.amount_solana_atomic` is denominated in the
            // reserve MINT's own decimals, not the canonical unit, and
            // narrowing it here would need a live mint read — a chain
            // dependency this endpoint has never had and must not grow,
            // since it would make a refunded transfer's page fail
            // whenever the RPC is down.
            //
            // It is not needed. `solana::refund::dry_run_refund` refuses
            // the refund unless `CanonicalAtomic(gross_amount_atomic)
            // .to_solana(mint_decimals) == obligation.amount`, and
            // `Ledger::begin_solana_refund` re-checks `stored gross ==
            // verified.gross_canonical_atomic` inside its own writing
            // transaction. So for any request that HAS a `solana_refunds`
            // row, `gross_amount_atomic` is the chain-verified canonical
            // spelling of the very amount being refunded — an enforced
            // equality, not an inference from the quote, and it is read
            // only once the refund row proves those checks passed.
            //
            // A `SolToGlc` deposit also cannot diverge from its expected
            // gross the way #2477's Goldcoin deposit did: the request is
            // FOLDED from the on-chain obligation, so the observed amount
            // is what created the request rather than something compared
            // against it afterwards.
            // A Robinhood-sourced deposit refunds on the ROBINHOOD side,
            // by returning the depositor's exact principal from the
            // custody contract — a different chain, a different table and
            // a different unit from either refund above
            // (`crate::robinhood::refund`).
            //
            // Every field below is read from the refund OPERATION ROW,
            // and none is invented for the sake of filling this DTO:
            //
            // * `state` is the operation's own lifecycle
            //   (`RobinhoodTxState`), which is finer-grained than the
            //   request's `RefundPending`/`RefundBroadcast`/`Refunded` —
            //   exactly the reason this field exists for the other two
            //   directions.
            // * `observed_amount_atomic`/`refund_amount_atomic` are the
            //   SAME figure, and legitimately so: `robinhood::refund`
            //   builds the authorization from the obligation's own
            //   on-chain `amount`, the contract compares it exactly and
            //   reverts on any difference, and there are no partial
            //   refunds. `amount_robinhood` is that word, narrowed back
            //   through the one conversion that enforces exactness. This
            //   is not the request's expected gross re-labelled — a
            //   Robinhood deposit is FOLDED from the observation, so
            //   there is no #2477-style expected/observed divergence to
            //   begin with, and the value used here is the contract's.
            // * `fee_charged_atomic` is `0` because a refunded request
            //   never settles and the fee accrues at settlement only
            //   (docs/20-bridge-fee.md) — the same assertion the other
            //   two directions make.
            //
            // A row whose `amount_robinhood` is absent or does not narrow
            // exactly reports `None` rather than a guessed amount: the
            // schema requires the column on a refund, so its absence is a
            // contradiction to surface, not to paper over.
            //
            // `RhnToSol` refunds through the identical path: the source
            // deposit is the same kind of obligation in the same
            // contract, and the route byte the contract binds is read
            // from that obligation, never chosen here.
            Direction::RhnToGlc | Direction::RhnToSol => ledger
                .get_robinhood_tx_for(crate::ledger::RobinhoodTxKind::Refund, request.id)?
                .and_then(|row| {
                    let principal = row.amount_robinhood.and_then(|word| {
                        crate::amount_conversion::robinhood::RobinhoodAtomic::try_from_u256(
                            crate::evm::EvmU256::from_be_bytes(word),
                        )
                        .ok()?
                        .to_canonical()
                        .ok()
                    })?;
                    Some(RefundView {
                        state: row.state.as_str().to_string(),
                        observed_amount_atomic: AtomicU64(principal.0),
                        refund_amount_atomic: AtomicU64(principal.0),
                        fee_charged_atomic: AtomicU64(0),
                        // The EVM transaction hash, hex-encoded with the
                        // same encoder `source_txid` uses.
                        refund_txid: row.tx_hash.map(|h| glc_hex::encode(&h)),
                        broadcast_at: row.first_broadcast_at,
                        refunded_at: row.finalized_at,
                    })
                }),
            // `SolToRhn` refunds exactly as `SolToGlc` does — the source
            // deposit is the same `WithdrawalObligation`, returned by the
            // same `refund_withdraw` to the same derived depositor ATA.
            Direction::SolToGlc | Direction::SolToRhn => {
                ledger.get_solana_refund(request.id)?.map(|row| RefundView {
                    state: row.state.as_str().to_string(),
                    observed_amount_atomic: AtomicU64(request.gross_amount_atomic),
                    refund_amount_atomic: AtomicU64(request.gross_amount_atomic),
                    fee_charged_atomic: AtomicU64(0),
                    refund_txid: row.refund_signature,
                    broadcast_at: row.broadcast_at,
                    refunded_at: row.confirmed_at,
                })
            }
        })
    }

    /// This deployment's Robinhood indexer liveness, reduced to the four
    /// non-sensitive facts a public caller may see. An unconfigured
    /// deployment holds `RobinhoodHealth::unconfigured()`, whose snapshot
    /// is all-default forever, so this needs no separate absent case.
    fn robinhood_indexer_view(&self) -> RobinhoodIndexerView {
        let snapshot = self.robinhood_health.snapshot();
        RobinhoodIndexerView {
            configured: snapshot.configured,
            connected: snapshot.connected,
            lag_blocks: snapshot.lag_blocks,
            last_success_at: snapshot.last_success_unix,
            halted: snapshot.halt.is_some(),
        }
    }

    /// One contract read, or the reason there is none. `None` for
    /// `robinhood_contract` is `NotConfigured` — a permanent answer for
    /// this process — while a configured-but-failed read is
    /// `Unavailable`; the two are never collapsed, because only one of
    /// them is worth retrying.
    async fn robinhood_contract_status(&self) -> crate::robinhood::public::RobinhoodContractStatus {
        match &self.robinhood_contract {
            None => crate::robinhood::public::RobinhoodContractStatus::NotConfigured,
            Some(source) => source.state().await,
        }
    }

    /// The contract half of [`RobinhoodReserveView`].
    async fn robinhood_onchain_view(&self, now: i64) -> RobinhoodOnchainView {
        let status = self.robinhood_contract_status().await;
        let Some(state) = status.state() else {
            return RobinhoodOnchainView {
                availability: status.as_str().to_string(),
                encumbered_reserve_atomic: None,
                protected_min_reserve_atomic: None,
                deposits_paused: None,
                payouts_paused: None,
                inbound_window: None,
                outbound_window: None,
                window_seconds: None,
            };
        };
        // The contract accounts its windows in unix seconds, and `now` is
        // the same clock every other `as_of` on this API uses. Negative
        // is impossible in practice and clamps to 0 rather than wrapping.
        let now_u64 = now.max(0) as u64;
        RobinhoodOnchainView {
            availability: status.as_str().to_string(),
            encumbered_reserve_atomic: u256_decimal(state.encumbered_reserve),
            protected_min_reserve_atomic: u256_decimal(state.limits.protected_min_reserve),
            deposits_paused: Some(state.deposits_paused),
            payouts_paused: Some(state.payouts_paused),
            inbound_window: robinhood_window_view(
                state.limits.inbound_rolling_limit,
                state.inbound_window,
                now_u64,
            ),
            outbound_window: robinhood_window_view(
                state.limits.outbound_rolling_limit,
                state.outbound_window,
                now_u64,
            ),
            window_seconds: Some(state.window_seconds),
        }
    }

    async fn fetch_bridge_config(&self) -> Result<accounts::BridgeConfigSnapshot, ApiError> {
        let account = self
            .solana_rpc
            .get_account(&accounts::bridge_config_pda())
            .await
            .map_err(|e| ApiError::Upstream(e.to_string()))?
            .ok_or_else(|| ApiError::Upstream("bridge_config account does not exist yet".into()))?;
        accounts::decode_bridge_config(&account.data).map_err(|e| ApiError::Upstream(e.to_string()))
    }

    /// The size `SolToGlc`'s public availability is evaluated at — see
    /// [`AdmissionProbe`] and [`route_availability`]'s "The size the
    /// decision is asked at".
    ///
    /// Takes the already-fetched `bridge_config` so a listing pays for
    /// one extra chain read (the reserve mint's live `decimals`, which
    /// the widening to canonical needs) and not two. `None` — logged —
    /// when the mint cannot be read, the widening is inexact, or the
    /// route is unpriced; the caller then fails closed with
    /// [`AVAILABILITY_REASON_PROBE_UNAVAILABLE`] rather than falling
    /// back to the one-unit form, because "we do not know what a normal
    /// transfer is" is not a reason to advertise the route.
    ///
    /// A `per_transfer_limit` so small it nets to nothing is probed at
    /// one atomic unit: the program would refuse every deposit anyway,
    /// and the weakest form is the only one left to ask.
    async fn sol_to_glc_probe(
        &self,
        config: &accounts::BridgeConfigSnapshot,
    ) -> Option<AdmissionProbe> {
        let route = crate::routes::Route::SolToGlc;
        let decimals = match accounts::fetch_reserve_mint_decimals(
            &self.solana_rpc,
            &config.reserve_token_mint,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "reserve mint decimals unreadable; SolToGlc availability fails closed (no probe)"
                );
                return None;
            }
        };
        let gross = match crate::amount_conversion::SolanaAtomic(config.per_transfer_limit)
            .to_canonical(decimals)
        {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    per_transfer_limit = config.per_transfer_limit,
                    "per_transfer_limit does not widen to canonical; SolToGlc availability fails closed"
                );
                return None;
            }
        };
        let fee_bps = match self.route_fees.fee_bps(route) {
            Ok(bps) => bps,
            Err(e) => {
                tracing::warn!(error = %e, "SolToGlc is unpriced; availability fails closed");
                return None;
            }
        };
        let net = match crate::amount_conversion::compute_fee_at_bps(gross, fee_bps) {
            Ok(fb) => fb.net.0,
            Err(e) => {
                tracing::warn!(error = %e, "SolToGlc probe fee computation failed; fails closed");
                return None;
            }
        };
        Some(AdmissionProbe {
            gross_canonical: gross.0,
            fee_bps,
            net_destination_atomic: i64::try_from(net).ok()?.max(1),
        })
    }

    /// [`SolanaProgramPause`] and [`RouteProbes`] together, from ONE
    /// `bridge_config` read — what every route listing needs. Fail-closed
    /// on both axes when the config is unreadable: every Solana leg is
    /// treated as paused ([`SolanaProgramPause::UNKNOWN`]) so `GET
    /// /chains` keeps serving the Goldcoin<->Robinhood routes — which the
    /// program cannot affect — while never advertising a Solana route it
    /// cannot vouch for, and `SolToGlc` additionally has no probe.
    async fn route_listing_inputs(&self) -> (SolanaProgramPause, RouteProbes) {
        match self.fetch_bridge_config().await {
            Ok(config) => (
                SolanaProgramPause::from_config(&config),
                RouteProbes {
                    sol_to_glc: self.sol_to_glc_probe(&config).await,
                },
            ),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "bridge_config unreadable; reporting every Solana route as unavailable (fail-closed)"
                );
                (SolanaProgramPause::UNKNOWN, RouteProbes::default())
            }
        }
    }

    /// `SolToGlc`'s [`route_availability`] verdict for the status
    /// surfaces. Synchronous on purpose: the probe (the one chain read
    /// involved) is resolved by the caller first, so no `&Ledger` is
    /// ever held across an await.
    fn sol_to_glc_verdict(
        &self,
        ledger: &Ledger,
        onchain: SolanaProgramPause,
        probes: RouteProbes,
    ) -> RouteAvailability {
        let route = crate::routes::Route::SolToGlc;
        route_availability(
            ledger,
            onchain,
            probes,
            route,
            self.route_gate.is_enabled(ledger, route),
        )
    }

    /// Live rolling-24h-volume headroom remaining for one direction's
    /// window (`0` = release/`GlcToSol`, `1` = deposit/`SolToGlc` — see
    /// [`accounts::rolling_volume_window_pda`]), as of right now. A
    /// read-only chain read plus [`accounts::rolling_volume_remaining`]'s
    /// pure projection — never mutates anything, never itself a pause.
    async fn fetch_rolling_volume_remaining(
        &self,
        direction_byte: u8,
        config: &accounts::BridgeConfigSnapshot,
    ) -> Result<u64, ApiError> {
        let account = self
            .solana_rpc
            .get_account(&accounts::rolling_volume_window_pda(direction_byte))
            .await
            .map_err(|e| ApiError::Upstream(e.to_string()))?
            .ok_or_else(|| {
                ApiError::Upstream("rolling_volume_window account does not exist yet".into())
            })?;
        let window = accounts::decode_rolling_volume_window(&account.data)
            .map_err(|e| ApiError::Upstream(e.to_string()))?;
        Ok(accounts::rolling_volume_remaining(
            config.rolling_volume_limit,
            config.rolling_window_seconds,
            window,
            now_unix(),
        ))
    }

    /// The reserve mint's live `decimals`.
    ///
    /// Two Solana `get_account` round trips — the bridge config, then the
    /// mint it names — so every caller must be a route that actually has a
    /// Solana leg. Both call sites reach for this from inside a
    /// `Direction` arm that needs it, never above the match: a route with
    /// no Solana leg must not inherit Solana's availability.
    async fn fetch_solana_reserve_decimals(&self) -> Result<u8, ApiError> {
        let config = self.fetch_bridge_config().await?;
        accounts::fetch_reserve_mint_decimals(&self.solana_rpc, &config.reserve_token_mint)
            .await
            .map_err(|e| ApiError::Upstream(e.to_string()))
    }
}

impl<SR: SolanaRpc + Send + Sync + 'static> ApiSource for BridgeApi<SR> {
    fn status(&self) -> BoxFut<'_, Result<BridgeStatus, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let config = self.fetch_bridge_config().await?;
            let goldcoin_paused = ledger.is_paused(ReserveDirection::GoldcoinReserve)?;
            let solana_paused = ledger.is_paused(ReserveDirection::SolanaReserve)?;
            // Both admission axes, ANDed: either one being closed means
            // a new obligation would park, so either one must show here
            // as "not accepting new transfers". Read-only — neither this
            // endpoint nor any other read path ever evaluates (and so
            // never moves) the automatic gate.
            //
            // This is the GOLDCOIN DESTINATION's admission state, which
            // governs `SolToGlc` and `RhnToGlc` alike (both fold against
            // this one `reserve_ledger` row). Computed once and reported
            // under both names — the historical `sol_to_glc_*` spelling
            // and the destination-neutral one — so the two can never
            // disagree.
            let goldcoin_destination_admission_open = !ledger
                .is_admission_closed(ReserveDirection::GoldcoinReserve)?
                && !ledger.is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)?;
            // `sol_to_glc_admission_open` is NO LONGER the same value as
            // the destination-neutral one above, and must not be
            // collapsed back into it. Since schema v25 each inbound
            // route also carries its OWN admission gate, so `SolToGlc`
            // can be closed while `RhnToGlc` stays open out of the same
            // reserve, and vice versa.
            //
            // The reserve-wide field keeps its exact historical meaning
            // (it answers for the RESERVE, which is what its name says);
            // this one narrows it with the `SolToGlc` route gate,
            // because it feeds `sol_to_glc_available` and a UI must not
            // be told a route is open that `GET /chains` reports closed.
            // That divergence — one endpoint's availability disagreeing
            // with another's — is the exact failure class `RouteView`'s
            // `enabled`/`available` split exists to close, and it would
            // be reintroduced here by omission.
            let sol_to_glc_admission_open = goldcoin_destination_admission_open
                && !ledger.route_admission_closed(crate::routes::Route::SolToGlc)?;
            let glc_to_sol_rolling_volume_remaining =
                self.fetch_rolling_volume_remaining(0, &config).await?;
            let sol_to_glc_rolling_volume_remaining =
                self.fetch_rolling_volume_remaining(1, &config).await?;
            let glc_to_sol_quota_exhausted =
                glc_to_sol_rolling_volume_remaining < config.min_transfer_amount;
            let sol_to_glc_quota_exhausted =
                sol_to_glc_rolling_volume_remaining < config.min_transfer_amount;
            // The program's own flags, ANDed in beside the local layer —
            // `config` is already in hand, so this costs no extra read.
            let onchain = SolanaProgramPause::from_config(&config);
            let glc_to_sol_available = !solana_paused
                && !onchain.blocks(Direction::GlcToSol)
                && !glc_to_sol_quota_exhausted
                && ledger.available_capacity(ReserveDirection::SolanaReserve)? > 0;
            // `SolToGlc` through the SAME verdict `GET /chains` reports —
            // the shared evaluator, probed at the program's
            // `per_transfer_limit` — and then the program's rolling
            // window on top, which is this endpoint's own gate. Every
            // clause the old boolean chain spelled out (pause, both
            // admission axes, capacity > 0) is a gate inside that
            // evaluator; what it adds is the buffer rule at a REAL size,
            // so this field can no longer read `true` while every
            // normal deposit parks `liquidity_buffer_low_at_fold`.
            let probes = RouteProbes {
                sol_to_glc: self.sol_to_glc_probe(&config).await,
            };
            let sol_to_glc_verdict = self.sol_to_glc_verdict(&ledger, onchain, probes);
            let sol_to_glc_available = sol_to_glc_verdict.available && !sol_to_glc_quota_exhausted;
            let sol_to_glc_availability_reason =
                sol_to_glc_verdict.availability_reason.clone().or_else(|| {
                    sol_to_glc_quota_exhausted
                        .then(|| AVAILABILITY_REASON_QUOTA_EXHAUSTED.to_string())
                });
            Ok(BridgeStatus {
                goldcoin_paused,
                solana_paused,
                vault_address: self.vault_address.clone(),
                next_solana_obligation_index: config.obligation_count,
                glc_to_sol_available,
                sol_to_glc_available,
                glc_to_sol_quota_exhausted,
                sol_to_glc_quota_exhausted,
                glc_to_sol_rolling_volume_remaining: AtomicU64(glc_to_sol_rolling_volume_remaining),
                sol_to_glc_rolling_volume_remaining: AtomicU64(sol_to_glc_rolling_volume_remaining),
                sol_to_glc_admission_open,
                goldcoin_destination_admission_open,
                sol_to_glc_availability_reason,
                sol_to_glc_capacity: sol_to_glc_verdict.capacity,
            })
        })
    }

    fn chains(&self) -> BoxFut<'_, Result<ChainsView, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let chains = crate::routes::Chain::ALL
                .iter()
                .filter(|c| self.route_gate.registry().contains(**c))
                .map(|c| ChainView {
                    id: c.as_str().to_string(),
                    display_name: c.display_name().to_string(),
                })
                .collect();
            let (onchain, probes) = self.route_listing_inputs().await;
            let routes = crate::routes::Route::ALL
                .iter()
                .map(|r| RouteView::build(&self.route_gate, &ledger, onchain, probes, *r))
                .collect();
            Ok(ChainsView {
                chains,
                routes,
                as_of: now_unix(),
            })
        })
    }

    fn limits(&self) -> BoxFut<'_, Result<TransferLimits, ApiError>> {
        Box::pin(async move {
            let config = self.fetch_bridge_config().await?;
            Ok(TransferLimits {
                min_transfer_amount: AtomicU64(config.min_transfer_amount),
                per_transfer_limit: AtomicU64(config.per_transfer_limit),
                // The SOLANA-bound route's rate, beside the Solana
                // program's own limits. Never a global one.
                bridge_fee_bps: self.display_fee_bps(crate::routes::Route::GlcToSol),
            })
        })
    }

    fn health(&self) -> BoxFut<'_, Result<PublicHealth, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            // Every direction, not just the two legacy ones: a Goldcoin
            // deposit against a `GlcToRhn` request can be parked in
            // `ManualReview` by the same amount-mismatch and
            // late-deposit-no-capacity paths that park a `GlcToSol` one,
            // and a backlog an operator cannot see is a backlog nobody
            // works.
            let manual_review_backlog: u64 = Direction::ALL
                .iter()
                .map(|&d| {
                    ledger
                        .requests_by_state(d, RequestState::ManualReview)
                        .map(|r| r.len() as u64)
                        .unwrap_or(0)
                })
                .sum();
            let goldcoin_indexer_halted = self.goldcoin_indexer_status.is_halted();
            let post_finality_reorg_events = ledger.post_finality_reorg_event_count()?;
            Ok(PublicHealth {
                healthy: !goldcoin_indexer_halted && post_finality_reorg_events == 0,
                goldcoin_indexer_halted,
                manual_review_backlog,
                post_finality_reorg_events,
            })
        })
    }

    fn stats(&self) -> BoxFut<'_, Result<BridgeStats, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let config = self.fetch_bridge_config().await?;
            let goldcoin_paused = ledger.is_paused(ReserveDirection::GoldcoinReserve)?;
            let solana_paused = ledger.is_paused(ReserveDirection::SolanaReserve)?;
            let glc_to_sol_rolling_volume_remaining =
                self.fetch_rolling_volume_remaining(0, &config).await?;
            let sol_to_glc_rolling_volume_remaining =
                self.fetch_rolling_volume_remaining(1, &config).await?;
            let glc_to_sol_quota_exhausted =
                glc_to_sol_rolling_volume_remaining < config.min_transfer_amount;
            let sol_to_glc_quota_exhausted =
                sol_to_glc_rolling_volume_remaining < config.min_transfer_amount;
            let onchain = SolanaProgramPause::from_config(&config);
            let glc_to_sol_available = !solana_paused
                && !onchain.blocks(Direction::GlcToSol)
                && !glc_to_sol_quota_exhausted
                && ledger.available_capacity(ReserveDirection::SolanaReserve)? > 0;
            // Same verdict as `GET /status` and `GET /chains` — see
            // `status()` for why the boolean chain this replaced could
            // advertise a route every normal deposit would park on.
            let probes = RouteProbes {
                sol_to_glc: self.sol_to_glc_probe(&config).await,
            };
            let sol_to_glc_verdict = self.sol_to_glc_verdict(&ledger, onchain, probes);
            let sol_to_glc_available = sol_to_glc_verdict.available && !sol_to_glc_quota_exhausted;
            let sol_to_glc_availability_reason =
                sol_to_glc_verdict.availability_reason.clone().or_else(|| {
                    sol_to_glc_quota_exhausted
                        .then(|| AVAILABILITY_REASON_QUOTA_EXHAUSTED.to_string())
                });

            let glc_to_sol = direction_stats(ledger.request_state_counts(Direction::GlcToSol)?);
            let sol_to_glc = direction_stats(ledger.request_state_counts(Direction::SolToGlc)?);

            let goldcoin_reserve = ReserveStats {
                paused: goldcoin_paused,
                available_capacity: AtomicI64(
                    ledger.available_capacity(ReserveDirection::GoldcoinReserve)?,
                ),
                settled_volume_atomic: AtomicU64(
                    ledger.settled_liquidity(ReserveDirection::GoldcoinReserve)?,
                ),
                accrued_fees_atomic: AtomicU64(
                    ledger.accrued_fees(ReserveDirection::GoldcoinReserve)?,
                ),
            };
            let solana_reserve = ReserveStats {
                paused: solana_paused,
                available_capacity: AtomicI64(
                    ledger.available_capacity(ReserveDirection::SolanaReserve)?,
                ),
                settled_volume_atomic: AtomicU64(
                    ledger.settled_liquidity(ReserveDirection::SolanaReserve)?,
                ),
                accrued_fees_atomic: AtomicU64(
                    ledger.accrued_fees(ReserveDirection::SolanaReserve)?,
                ),
            };

            let now = now_unix();

            // The SAME projection `glc-admin robinhood-status` and
            // `glc-admin robinhood-reserve` print, and the same one
            // `GET /robinhood/reserve` serves — `reserve_report`, not a
            // second reading of `reserve_ledger` that could drift from it.
            //
            // `None` means no `[reserve.robinhood]` section and therefore
            // no `reserve_ledger` row. That is why this is fetched through
            // a function that returns `Option` rather than through the
            // `ledger.is_paused(...)?` / `ledger.available_capacity(...)?`
            // calls the two reserves above use: for this direction those
            // raise `ReserveNotInitialized`, and a `?` on any of them here
            // would fail the WHOLE of `GET /stats` on every deployment
            // that has not configured a Robinhood reserve — which is all
            // of them today.
            let robinhood_report = crate::robinhood::admin::reserve_report(&ledger, now)?;
            let robinhood_reserve = RobinhoodReserveStats {
                ledger_availability: match &robinhood_report {
                    Some(_) => crate::robinhood::public::AVAILABILITY_AVAILABLE,
                    None => crate::robinhood::public::AVAILABILITY_NOT_CONFIGURED,
                }
                .to_string(),
                paused: robinhood_report.as_ref().map(|r| r.paused),
                available_capacity: robinhood_report
                    .as_ref()
                    .map(|r| AtomicI64(r.available_capacity_atomic)),
                // `reserve_report` does not carry the settled-volume
                // counter, so it is read here from the identical
                // accessor `goldcoin_reserve`/`solana_reserve` use — and
                // only inside the `Some` arm, where the row is known to
                // exist and the read therefore cannot raise
                // `ReserveNotInitialized`.
                settled_volume_atomic: match &robinhood_report {
                    Some(_) => Some(AtomicU64(
                        ledger.settled_liquidity(ReserveDirection::RobinhoodReserve)?,
                    )),
                    None => None,
                },
                accrued_fees_atomic: robinhood_report
                    .as_ref()
                    .map(|r| AtomicU64(r.accrued_fees_atomic)),
            };

            Ok(BridgeStats {
                goldcoin_paused,
                solana_paused,
                glc_to_sol_available,
                sol_to_glc_available,
                sol_to_glc_availability_reason,
                glc_to_sol_quota_exhausted,
                sol_to_glc_quota_exhausted,
                glc_to_sol_rolling_volume_remaining: AtomicU64(glc_to_sol_rolling_volume_remaining),
                sol_to_glc_rolling_volume_remaining: AtomicU64(sol_to_glc_rolling_volume_remaining),
                bridge_fee_bps: self.display_fee_bps(crate::routes::Route::GlcToSol),
                route_fees: self.route_fee_views(),
                glc_to_sol,
                sol_to_glc,
                goldcoin_reserve,
                solana_reserve,
                robinhood_reserve,
                goldcoin_indexer_halted: self.goldcoin_indexer_status.is_halted(),
                goldcoin_indexer_seconds_since_tick: self
                    .goldcoin_indexer_status
                    .seconds_since_tick(now),
                solana_indexer_seconds_since_tick: self
                    .solana_indexer_status
                    .seconds_since_tick(now),
                post_finality_reorg_events: ledger.post_finality_reorg_event_count()?,
                as_of: now,
            })
        })
    }

    fn reserves_history(
        &self,
        direction: Option<ReserveDirection>,
        cursor: Option<i64>,
        limit: u32,
    ) -> BoxFut<'_, Result<Page<ReserveHistoryEntry>, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let rows = ledger.reconciliation_findings_page(direction, cursor, limit)?;
            let next_cursor = if rows.len() as u32 == limit {
                rows.last().map(|r| r.id.to_string())
            } else {
                None
            };
            let items = rows
                .into_iter()
                .map(|r| ReserveHistoryEntry {
                    id: r.id,
                    direction: r.direction.as_str().to_string(),
                    detected_at: r.detected_at,
                    expected_atomic: AtomicI64(r.expected),
                    observed_atomic: AtomicI64(r.observed),
                    delta_atomic: AtomicI64(r.delta),
                    classification: r.classification,
                    auto_paused: r.auto_paused,
                })
                .collect();
            Ok(Page {
                items,
                next_cursor,
                as_of: now_unix(),
            })
        })
    }

    fn explorer_events(
        &self,
        direction: Option<Direction>,
        state: Option<RequestState>,
        cursor: Option<i64>,
        limit: u32,
    ) -> BoxFut<'_, Result<Page<ExplorerEvent>, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let rows = ledger.explorer_events_page(direction, state, cursor, limit)?;
            let next_cursor = if rows.len() as u32 == limit {
                rows.last().map(|r| r.id.to_string())
            } else {
                None
            };
            let items = rows
                .into_iter()
                .map(|r| ExplorerEvent {
                    id: r.id,
                    request_id: r.request_id,
                    direction: r.direction.as_str().to_string(),
                    from_state: r.from_state.map(|s| s.as_str().to_string()),
                    to_state: r.to_state.as_str().to_string(),
                    at: r.at,
                    reason: r.reason,
                })
                .collect();
            Ok(Page {
                items,
                next_cursor,
                as_of: now_unix(),
            })
        })
    }

    fn reserve(&self) -> BoxFut<'_, Result<ReserveAvailability, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            Ok(ReserveAvailability {
                goldcoin_available_capacity: AtomicI64(
                    ledger.available_capacity(ReserveDirection::GoldcoinReserve)?,
                ),
                solana_available_capacity: AtomicI64(
                    ledger.available_capacity(ReserveDirection::SolanaReserve)?,
                ),
            })
        })
    }

    fn create_goldcoin_deposit_transfer(
        &self,
        input: CreateTransferInput,
    ) -> BoxFut<'_, Result<CreateTransferOutput, ApiError>> {
        Box::pin(async move {
            // ROUTE GATE FIRST — before amount validation, before the fee
            // computation, before any chain read, and long before any
            // ledger write or deposit-address derivation. A refused route
            // must leave nothing behind, so nothing may happen before this.
            let gate_ledger = self.open_ledger()?;
            let (route, direction) = self.resolve_route(&gate_ledger, input.route.as_deref())?;
            drop(gate_ledger);
            if !direction.source_is_goldcoin() {
                // `SolToGlc`/`RhnToGlc` are created by the depositor's own
                // on-chain transaction on the SOURCE chain, not through
                // this endpoint (see the module docs) — so naming one here
                // is a client error, not a disabled route.
                return Err(ApiError::BadRequest(format!(
                    "route {} is not created through this endpoint",
                    route.as_str()
                )));
            }
            // `amount_atomic` is the `AtomicU64` newtype on the wire (atomic
            // amounts serialize as decimal strings); unwrapped once here
            // because the fee computation below works in plain canonical
            // units.
            let amount_atomic = input.amount_atomic.0;
            if amount_atomic == 0 {
                return Err(ApiError::BadRequest("amount_atomic must be > 0".into()));
            }
            // The source-side minimum, against the GROSS the caller is
            // about to send — before any fee, which is what the policy is
            // a statement about (`crate::min_transfer`). Refused here, on
            // the one route family this service originates, so nothing
            // moves at all: the alternative is a deposit this service
            // priced and a destination chain then refuses to deliver.
            crate::min_transfer::enforce_source_minimum_at(
                route,
                amount_conversion::CanonicalAtomic(amount_atomic),
                self.source_minimum,
            )
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
            // The recipient is parsed AS THE DESTINATION CHAIN'S OWN
            // address type, chosen by the direction the gate already
            // resolved. There is no common representation and no fallback:
            // a Solana pubkey offered on a `GlcToRhn` request fails to
            // parse as an EVM address and the request is refused, rather
            // than being stored as bytes that some later payout would try
            // to interpret.
            let recipient_bytes: Vec<u8> = match direction {
                Direction::GlcToSol => input
                    .recipient
                    .parse::<Pubkey>()
                    .map_err(|e| ApiError::BadRequest(format!("invalid recipient: {e}")))?
                    .to_bytes()
                    .to_vec(),
                Direction::GlcToRhn => {
                    let address = input
                        .recipient
                        .parse::<crate::evm::address::EvmAddress>()
                        .map_err(|e| ApiError::BadRequest(format!("invalid recipient: {e}")))?;
                    // The zero address is a valid EVM address and the
                    // EVM's burn sink; `EvmAddress::ZERO`'s own docs
                    // record why it is never treated as "no address".
                    // Accepting it here would reserve real reserve
                    // capacity against a payout that destroys the value.
                    if address.is_zero() {
                        return Err(ApiError::BadRequest(
                            "invalid recipient: the zero address is the EVM burn sink, not a \
                             payout destination"
                                .into(),
                        ));
                    }
                    address.to_bytes().to_vec()
                }
                // Unreachable behind `source_is_goldcoin()` above, but
                // written as a refusal rather than a panic: this match is
                // exhaustive over `Direction`, so a seventh one is a
                // compile error here, and an unexpected observed-source
                // one is a 400 rather than a downed process.
                Direction::SolToGlc
                | Direction::RhnToGlc
                | Direction::SolToRhn
                | Direction::RhnToSol => {
                    return Err(ApiError::BadRequest(format!(
                        "route {} is not created through this endpoint",
                        route.as_str()
                    )))
                }
            };
            // `input.amount_atomic` is the caller-declared GROSS amount,
            // canonical units (Goldcoin-native) — the fee/net breakdown is
            // computed authoritatively HERE, server-side, at THIS ROUTE'S
            // configured rate; nothing about it is accepted from the caller
            // (docs/20-bridge-fee.md: "never trust gross, fee or net
            // calculations supplied by the UI"). `CreateTransferInput` has
            // no fee/net field for exactly this reason — there is nothing
            // for a client to submit that could bypass or alter the fee.
            //
            // The rate is resolved BY ROUTE, from `[fees]`. It used to be
            // the compiled-in global constant, which meant a `GlcToRhn`
            // transfer was priced at the Solana rate — the exact leak
            // `crate::fees` exists to close. A route with no configured
            // rate fails closed here rather than borrowing another's.
            let fee_bps = self.route_fees.fee_bps(route).map_err(|e| {
                ApiError::Upstream(format!("no fee is configured for this route: {e}"))
            })?;
            let fee_breakdown = amount_conversion::compute_fee_at_bps(
                amount_conversion::CanonicalAtomic(amount_atomic),
                fee_bps,
            )
            .map_err(|e| ApiError::BadRequest(format!("invalid amount: {e}")))?;
            // `net_destination_atomic` is what the DESTINATION reserve
            // must actually release, in that reserve's own accounting
            // unit — the figure `Ledger::create_request` reserves capacity
            // against. The two Goldcoin-sourced directions differ here and
            // nowhere else in this function.
            let net_destination_atomic = match direction {
                Direction::GlcToSol => {
                    let config = self.fetch_bridge_config().await?;
                    let solana_decimals = accounts::fetch_reserve_mint_decimals(
                        &self.solana_rpc,
                        &config.reserve_token_mint,
                    )
                    .await
                    .map_err(|e| ApiError::Upstream(e.to_string()))?;
                    let net_destination =
                        fee_breakdown.net.to_solana(solana_decimals).map_err(|e| {
                            ApiError::BadRequest(format!(
                                "amount {} cannot be represented exactly after the bridge fee \
                                 at the reserve mint's {solana_decimals}-decimal precision: {e}",
                                amount_atomic
                            ))
                        })?;
                    // Proactive rolling-24h-volume check, GlcToSol =
                    // release = direction byte 0 (see
                    // `accounts::rolling_volume_window_pda` docs). Without
                    // this, a deposit could be accepted here (off-chain
                    // capacity reserved, Goldcoin funds requested from the
                    // user) only to fail later at actual on-chain
                    // `release_from_reserve` time when the same quota is
                    // checked for real — this check can never be MORE
                    // permissive than that real check (same limit, same
                    // window, same read), only catches the rejection
                    // earlier, before the user has sent anything.
                    //
                    // It is deliberately NOT applied to `GlcToRhn`: this
                    // quota is a Solana PROGRAM's rolling window, read
                    // from a Solana PDA, and it bounds the Solana
                    // reserve's releases. A Robinhood payout draws down a
                    // different reserve and is bounded by the custody
                    // contract's own `inboundWindow`, which
                    // `robinhood::preflight` reads from the contract.
                    // Applying the Solana window to a Robinhood payout
                    // would be a limit that neither chain enforces.
                    let glc_to_sol_remaining =
                        self.fetch_rolling_volume_remaining(0, &config).await?;
                    if net_destination.0 > glc_to_sol_remaining {
                        return Err(ApiError::QuotaExhausted);
                    }
                    net_destination.0
                }
                Direction::GlcToRhn => {
                    // The Robinhood reserve is accounted in CANONICAL
                    // 8-decimal units like every other reserve row
                    // (`ReserveDirection`'s own docs on why 18 decimals
                    // never reach an `INTEGER` column), so the net
                    // entitlement needs no conversion to be reserved.
                    //
                    // The 18-decimal widening is still exercised — and
                    // discarded — purely to prove the amount is
                    // deliverable at the destination's real precision
                    // before any capacity is held. `quote` runs the same
                    // check for the same reason; a create that skipped it
                    // could reserve capacity for a payout
                    // `Settler::authorize_payout` would then refuse.
                    fee_breakdown.net.to_robinhood().map_err(|e| {
                        ApiError::BadRequest(format!(
                            "amount {amount_atomic} cannot be represented exactly after the \
                             bridge fee at Robinhood's precision: {e}"
                        ))
                    })?;
                    fee_breakdown.net.0
                }
                Direction::SolToGlc
                | Direction::RhnToGlc
                | Direction::SolToRhn
                | Direction::RhnToSol => {
                    return Err(ApiError::BadRequest(format!(
                        "route {} is not created through this endpoint",
                        route.as_str()
                    )))
                }
            };
            let amounts = crate::ledger::RequestAmounts {
                gross_atomic: fee_breakdown.gross.0,
                fee_bps: fee_breakdown.fee_bps,
                fee_atomic: fee_breakdown.fee.0,
                net_atomic: fee_breakdown.net.0,
                net_destination_atomic,
            };
            // The declared funding wallet, canonicalized to the exact
            // spelling the indexer records for a deposit from it
            // (`goldcoin::address::canonical_standard_address`), so the
            // window it consumes here is the window that deposit will be
            // checked against. Malformed is a 400, never a silently
            // ignored declaration.
            let source_wallet: Option<Vec<u8>> = match input.source_address.as_deref() {
                None => None,
                Some(raw) if raw.trim().is_empty() => None,
                Some(raw) => Some(
                    crate::goldcoin::address::canonical_standard_address(
                        raw.trim(),
                        self.goldcoin_network,
                    )
                    .map_err(|e| ApiError::BadRequest(format!("invalid source_address: {e}")))?
                    .into_bytes(),
                ),
            };
            let mut ledger = self.open_ledger()?;
            let now = now_unix();
            // Created AS the resolved direction, in one INSERT. There is
            // no path here that creates a `GlcToSol` row and adjusts it
            // afterwards: `Ledger::create_request_from` writes `direction`
            // on insert and nothing in this service ever updates that
            // column, so the route a request is born with is the route it
            // dies with. Both rolling-24h wallet windows are checked
            // inside that same insert transaction.
            let outcome = ledger.create_request_from(
                direction,
                amounts,
                &recipient_bytes,
                None,
                source_wallet.as_deref(),
                self.reservation_ttl_secs,
                now,
            )?;
            match outcome {
                CreateRequestOutcome::Reserved { request_id } => {
                    // Unique per-request deposit address (Step 1's pure
                    // derivation + Step 2's ledger support) — replaces
                    // the shared static vault address + OP_RETURN
                    // binding for every NEW request from here on. The
                    // legacy static-vault/OP_RETURN path keeps working
                    // for requests that already exist; it is simply
                    // never used again for a request created through
                    // this endpoint.
                    let derived_vault = crate::goldcoin::derivation::derive_request_vault(
                        &self.root_vault,
                        request_id,
                        self.goldcoin_network,
                    )
                    .map_err(|e| ApiError::Upstream(e.to_string()))?;
                    let mut ledger_for_address = self.open_ledger()?;
                    // Binds the derived script to THIS request id, under
                    // the partial unique index on
                    // `deposit_script_pubkey_hex`. Since the row already
                    // carries its direction and its recipient, that one
                    // write is what makes the route durable on the
                    // Goldcoin side: the address an indexer later resolves
                    // leads back to exactly one row, carrying exactly one
                    // route and one intended recipient.
                    ledger_for_address.set_goldcoin_deposit_address(
                        request_id,
                        derived_vault.address(),
                        &derived_vault.script_pubkey_hex(),
                        &derived_vault.redeem_script_hex(),
                    )?;
                    Ok(CreateTransferOutput {
                        request_id,
                        deposit_address: derived_vault.address().to_string(),
                    })
                }
                CreateRequestOutcome::InsufficientLiquidity { available_capacity } => {
                    Err(ApiError::InsufficientLiquidity {
                        available: available_capacity,
                    })
                }
                CreateRequestOutcome::Paused => Err(ApiError::Paused),
                CreateRequestOutcome::WalletLimited { eligibility } => {
                    let (_, retry_after) = eligibility
                        .blocker()
                        .expect("WalletLimited is only returned when a window blocks");
                    Err(ApiError::WalletLimited {
                        blocked_reasons: eligibility
                            .blocked_reasons()
                            .into_iter()
                            .map(str::to_string)
                            .collect(),
                        retry_after,
                    })
                }
            }
        })
    }

    fn get_transfer(&self, id: i64) -> BoxFut<'_, Result<Option<TransferView>, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let Some(request) = ledger.get_request(id)? else {
                return Ok(None);
            };
            Ok(Some(self.to_transfer_view(&ledger, request)?))
        })
    }

    fn list_transfers(
        &self,
        address: Option<TransferAddressFilter>,
        state: Option<RequestState>,
        cursor: Option<i64>,
        limit: u32,
    ) -> BoxFut<'_, Result<Page<TransferView>, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let requests = ledger.transfers_page(address, state, cursor, limit)?;
            let next_cursor = if requests.len() as u32 == limit {
                requests.last().map(|r| r.id.to_string())
            } else {
                None
            };
            let mut items = Vec::with_capacity(requests.len());
            for request in requests {
                items.push(self.to_transfer_view(&ledger, request)?);
            }
            Ok(Page {
                items,
                next_cursor,
                as_of: now_unix(),
            })
        })
    }

    fn sol_to_glc_recipient_eligibility(
        &self,
        address: String,
        wallet: Option<[u8; 32]>,
    ) -> BoxFut<'_, Result<RecipientEligibility, ApiError>> {
        Box::pin(async move {
            // Trimmed exactly as the UI trims before building the
            // on-chain instruction: the ledger's rate limit matches on
            // the raw bytes the obligation carried, so the bytes checked
            // here must be the bytes a deposit for this input would
            // actually carry.
            let address = address.trim().to_string();
            // Same acceptance rule as the payout path itself
            // (`goldcoin::payout_recovery`/`signing::goldcoin_vault`
            // decode a recipient with `decode_p2pkh` on this network) —
            // an address that could never be paid out gets a 400 here,
            // not a misleading eligibility verdict.
            crate::goldcoin::address::decode_p2pkh(&address, self.goldcoin_network)
                .map_err(|e| ApiError::BadRequest(format!("invalid Goldcoin address: {e}")))?;
            let ledger = self.open_ledger()?;
            let now = now_unix();
            // Checked wallet-first (see `RecipientEligibility::blocked_reason`'s
            // doc comment) — independent of the recipient check either
            // way, so this only decides which single reason is reported
            // when both would block.
            let wallet_retry_after = match wallet {
                Some(requester) => {
                    ledger.sol_to_glc_source_wallet_rate_limited_until(&requester, now)?
                }
                None => None,
            };
            let recipient_retry_after =
                ledger.goldcoin_recipient_rate_limited_until(address.as_bytes(), now)?;
            Ok(RecipientEligibility::from_windows(
                "SolToGlc",
                address,
                wallet.map(|w| Pubkey::new_from_array(w).to_string()),
                wallet_retry_after,
                recipient_retry_after,
                now,
            ))
        })
    }

    fn rhn_to_glc_recipient_eligibility(
        &self,
        address: String,
        wallet: Option<[u8; 20]>,
    ) -> BoxFut<'_, Result<RecipientEligibility, ApiError>> {
        Box::pin(async move {
            // Trimmed and validated exactly as the SolToGlc leg is, and
            // for the same reason: the ledger's limit matches on the raw
            // destination bytes, so the bytes checked here must be the
            // bytes a real deposit for this input would carry. The
            // acceptance rule is `decode_p2pkh` on this network — the
            // same rule `robinhood::fold::validate_goldcoin_destination`
            // applies at fold time and the payout builder applies at
            // settlement, so an address that could never be paid out gets
            // a 400 here rather than a misleading eligibility verdict.
            let address = address.trim().to_string();
            crate::goldcoin::address::decode_p2pkh(&address, self.goldcoin_network)
                .map_err(|e| ApiError::BadRequest(format!("invalid Goldcoin address: {e}")))?;
            let ledger = self.open_ledger()?;
            let now = now_unix();
            // The SAME two ledger views the enforcing folds consult —
            // `Ledger::rhn_source_wallet_rate_limit_blocker_created_at`
            // and `Ledger::recipient_rate_limit_blocker_created_at` — not
            // a second implementation of either window. Note which is
            // which: the wallet leg is `RhnToGlc`-scoped (a Robinhood
            // wallet's window is its own and is never pooled with a
            // Solana wallet's), while the recipient leg is route-global,
            // so a recent SolToGlc payout to this address reports as
            // blocking here too.
            let wallet_retry_after = match wallet {
                Some(depositor) => {
                    ledger.rhn_to_glc_source_wallet_rate_limited_until(&depositor, now)?
                }
                None => None,
            };
            let recipient_retry_after =
                ledger.goldcoin_recipient_rate_limited_until(address.as_bytes(), now)?;
            Ok(RecipientEligibility::from_windows(
                "RhnToGlc",
                address,
                wallet.map(|w| crate::evm::address::EvmAddress::from_bytes(w).to_string()),
                wallet_retry_after,
                recipient_retry_after,
                now,
            ))
        })
    }

    fn route_wallet_eligibility(
        &self,
        route: crate::routes::Route,
        source: Option<String>,
        destination: Option<String>,
    ) -> BoxFut<'_, Result<RouteWalletEligibilityView, ApiError>> {
        Box::pin(async move {
            // Every route has a direction since Phase H; kept as a
            // refusal rather than an unwrap so a route without settlement
            // machinery could never be asked about bytes it never keys.
            let direction = route.as_direction().ok_or(ApiError::RouteDisabled)?;
            let source = source
                .map(|raw| {
                    parse_route_wallet(route, WalletRole::Source, &raw, self.goldcoin_network)
                })
                .transpose()?;
            let destination = destination
                .map(|raw| {
                    parse_route_wallet(route, WalletRole::Destination, &raw, self.goldcoin_network)
                })
                .transpose()?;
            let ledger = self.open_ledger()?;
            let now = now_unix();
            // THE query admission enforces with, read-only.
            let eligibility = ledger.route_wallet_eligibility(
                direction,
                source.as_ref().map(|(_, bytes)| bytes.as_slice()),
                destination.as_ref().map(|(_, bytes)| bytes.as_slice()),
                now,
            )?;
            Ok(RouteWalletEligibilityView::from_eligibility(
                route,
                source.map(|(address, _)| address),
                destination.map(|(address, _)| address),
                eligibility,
                now,
            ))
        })
    }

    fn robinhood_reserve(&self) -> BoxFut<'_, Result<RobinhoodReserveView, ApiError>> {
        Box::pin(async move {
            let now = now_unix();
            let ledger = self.open_ledger()?;
            // The SAME projection `glc-admin robinhood-reserve` prints,
            // not a second implementation of it. `None` means no
            // `[reserve.robinhood]` section and therefore no
            // `reserve_ledger` row — the fail-closed answer is "this
            // reserve does not exist", never "this reserve is empty".
            let report = crate::robinhood::admin::reserve_report(&ledger, now)?;
            // Every route with a Robinhood leg — the four the custody
            // contract models.
            let (onchain, probes) = self.route_listing_inputs().await;
            let routes = crate::routes::Route::ALL
                .iter()
                .filter(|r| r.contract_route_id().is_some())
                .map(|r| RouteView::build(&self.route_gate, &ledger, onchain, probes, *r))
                .collect();
            let onchain = self.robinhood_onchain_view(now).await;
            Ok(RobinhoodReserveView {
                ledger_availability: match &report {
                    Some(_) => crate::robinhood::public::AVAILABILITY_AVAILABLE.to_string(),
                    None => crate::robinhood::public::AVAILABILITY_NOT_CONFIGURED.to_string(),
                },
                balance_atomic: report.as_ref().map(|r| AtomicU64(r.balance_atomic)),
                protected_minimum_atomic: report
                    .as_ref()
                    .map(|r| AtomicU64(r.protected_minimum_atomic)),
                reserved_liquidity_atomic: report
                    .as_ref()
                    .map(|r| AtomicU64(r.reserved_liquidity_atomic)),
                pending_obligations_atomic: report
                    .as_ref()
                    .map(|r| AtomicU64(r.pending_obligations_atomic)),
                available_capacity_atomic: report
                    .as_ref()
                    .map(|r| AtomicI64(r.available_capacity_atomic)),
                accrued_fees_atomic: report.as_ref().map(|r| AtomicU64(r.accrued_fees_atomic)),
                paused: report.as_ref().map(|r| r.paused),
                onchain,
                routes,
                indexer: self.robinhood_indexer_view(),
                as_of: now,
            })
        })
    }

    fn robinhood_limits(&self) -> BoxFut<'_, Result<RobinhoodLimitsView, ApiError>> {
        Box::pin(async move {
            let now = now_unix();
            let status = self.robinhood_contract_status().await;
            // Every limit is `Some` only inside this one match arm.
            // There is no `unwrap_or`, no default and no fallback to the
            // Solana `BridgeConfig` anywhere below: an unread contract
            // yields nulls.
            let state = status.state();
            let words = state.map(|s| s.limits);
            Ok(RobinhoodLimitsView {
                availability: status.as_str().to_string(),
                inbound_min_atomic: words.and_then(|l| u256_decimal(l.inbound_min)),
                inbound_max_atomic: words.and_then(|l| u256_decimal(l.inbound_max)),
                inbound_rolling_limit_atomic: words
                    .and_then(|l| u256_decimal(l.inbound_rolling_limit)),
                outbound_min_atomic: words.and_then(|l| u256_decimal(l.outbound_min)),
                outbound_max_atomic: words.and_then(|l| u256_decimal(l.outbound_max)),
                outbound_rolling_limit_atomic: words
                    .and_then(|l| u256_decimal(l.outbound_rolling_limit)),
                protected_min_reserve_atomic: words
                    .and_then(|l| u256_decimal(l.protected_min_reserve)),
                rolling_window_seconds: state.map(|s| s.window_seconds),
                // The SAME projection `GET /robinhood/reserve` publishes,
                // through the same `robinhood_window_view` — there is one
                // implementation of "what does `_consumeWindow` leave",
                // so the two endpoints cannot answer it differently. Note
                // `now as u64`: `now_unix` saturates to 0 before the
                // epoch, and a negative clock would make every bucket
                // look current rather than expired, which is the
                // conservative direction.
                rhn_to_glc_rolling_window: state.and_then(|s| {
                    robinhood_window_view(
                        s.limits.inbound_rolling_limit,
                        s.inbound_window,
                        now.max(0) as u64,
                    )
                }),
                glc_to_rhn_rolling_window: state.and_then(|s| {
                    robinhood_window_view(
                        s.limits.outbound_rolling_limit,
                        s.outbound_window,
                        now.max(0) as u64,
                    )
                }),
                // Known without any chain read: the contract stores no
                // fee, so these are entirely this service's configured
                // rates — and they are the ROBINHOOD routes' own, not
                // whatever `GET /limits` reports for Solana.
                bridge_fee_bps: self.display_fee_bps(crate::routes::Route::RhnToGlc),
                glc_to_rhn_fee_bps: self.display_fee_bps(crate::routes::Route::GlcToRhn),
                rhn_to_glc_fee_bps: self.display_fee_bps(crate::routes::Route::RhnToGlc),
                as_of: now,
            })
        })
    }

    fn quote(&self, input: QuoteInput) -> BoxFut<'_, Result<QuoteOutput, ApiError>> {
        Box::pin(async move {
            // Quoting a route is quoting a promise about it, so the same
            // gate applies here as on creation. `QuoteInput::direction`
            // keeps its name for wire compatibility but is now parsed as a
            // `Route`, so `"GlcToRhn"` is a recognised name that is refused
            // (409) rather than an unrecognised one (400) — a UI can tell
            // "not open" from "you sent nonsense".
            let gate_ledger = self.open_ledger()?;
            let (route, direction) =
                self.resolve_route(&gate_ledger, Some(input.direction.as_str()))?;
            drop(gate_ledger);
            // `gross_amount` is the `AtomicU64` newtype on the wire; see the
            // note in `create_goldcoin_deposit_transfer`.
            let gross_amount = input.gross_amount.0;
            if gross_amount == 0 {
                return Err(ApiError::BadRequest("gross_amount must be > 0".into()));
            }
            // A quote is a promise about a transfer, so it answers for an
            // amount this bridge would actually accept and for no other.
            // Quoting a sub-minimum amount and then refusing to create it
            // would publish a price for something that cannot be bought —
            // and on a chain-sourced route, where the user acts on the
            // quote by making an irreversible deposit, it would be worse
            // than misleading.
            crate::min_transfer::enforce_source_minimum_at(
                route,
                amount_conversion::CanonicalAtomic(gross_amount),
                self.source_minimum,
            )
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
            let goldcoin_decimals = amount_conversion::GOLDCOIN_DECIMALS as u8;
            // Robinhood GLC's precision is a compile-time constant, not a
            // live read: an 18-decimal token is what makes the separate
            // `RobinhoodAtomic` unit necessary at all, so a different
            // value would not mean "quote differently", it would mean
            // this is not the asset this code models. It is asserted
            // against the deployed token at preflight
            // (`amount_conversion::robinhood::ensure_robinhood_decimals`),
            // which is the one place that assumption is checked.
            let robinhood_decimals = amount_conversion::robinhood::ROBINHOOD_DECIMALS as u8;
            // The reserve mint's live precision is read ONLY by the two
            // routes that have a Solana leg. Reading it unconditionally
            // made every quote's availability depend on Solana RPC: a
            // Solana outage answered `RhnToGlc` — a route with no Solana
            // leg, whose two decimals are both compile-time constants —
            // with a 5xx, which reads to a user as "the Robinhood route is
            // down" when it is healthy. Scoped exactly as
            // `create_goldcoin_deposit_transfer` already scopes the same
            // read, and for the same reason.
            //
            // `Option` rather than a sentinel value: the deliverability
            // check below needs the real figure on the `GlcToSol` arm and
            // must not be able to run against an invented one.
            let (
                solana_decimals,
                source_decimals,
                destination_decimals,
                source_asset,
                destination_asset,
            ) = match direction {
                Direction::GlcToSol => {
                    let solana = self.fetch_solana_reserve_decimals().await?;
                    (
                        Some(solana),
                        goldcoin_decimals,
                        solana,
                        "GLC (Goldcoin)",
                        "GLC (Solana)",
                    )
                }
                Direction::SolToGlc => {
                    let solana = self.fetch_solana_reserve_decimals().await?;
                    (
                        Some(solana),
                        solana,
                        goldcoin_decimals,
                        "GLC (Solana)",
                        "GLC (Goldcoin)",
                    )
                }
                Direction::GlcToRhn => (
                    None,
                    goldcoin_decimals,
                    robinhood_decimals,
                    "GLC (Goldcoin)",
                    "GLC (Robinhood)",
                ),
                Direction::RhnToGlc => (
                    None,
                    robinhood_decimals,
                    goldcoin_decimals,
                    "GLC (Robinhood)",
                    "GLC (Goldcoin)",
                ),
                Direction::SolToRhn => {
                    let solana = self.fetch_solana_reserve_decimals().await?;
                    (
                        Some(solana),
                        solana,
                        robinhood_decimals,
                        "GLC (Solana)",
                        "GLC (Robinhood)",
                    )
                }
                Direction::RhnToSol => {
                    let solana = self.fetch_solana_reserve_decimals().await?;
                    (
                        Some(solana),
                        robinhood_decimals,
                        solana,
                        "GLC (Robinhood)",
                        "GLC (Solana)",
                    )
                }
            };
            // THIS ROUTE'S rate — the same lookup, from the same table,
            // that `create_goldcoin_deposit_transfer` prices with, so a
            // quote and the request it becomes can never disagree. Before
            // `crate::fees` this used the global constant and every
            // Robinhood quote was wrong by exactly the difference between
            // the two rates.
            let fee_bps = self.route_fees.fee_bps(route).map_err(|e| {
                ApiError::Upstream(format!("no fee is configured for this route: {e}"))
            })?;
            let fee_breakdown = amount_conversion::compute_fee_at_bps(
                amount_conversion::CanonicalAtomic(gross_amount),
                fee_bps,
            )
            .map_err(|e| ApiError::BadRequest(format!("invalid amount: {e}")))?;
            // Confirms the net entitlement is actually deliverable at the
            // destination chain's real precision — a quote must never
            // promise an amount a real transfer would then reject
            // (docs/20-bridge-fee.md).
            match (direction, solana_decimals) {
                (Direction::GlcToSol, Some(solana_decimals)) => {
                    fee_breakdown.net.to_solana(solana_decimals).map_err(|e| {
                        ApiError::BadRequest(format!(
                            "amount {} cannot be represented exactly after the bridge fee at \
                             the reserve mint's {solana_decimals}-decimal precision: {e}",
                            gross_amount
                        ))
                    })?;
                }
                // Unreachable: the match above resolves `Some` on exactly
                // this arm. Written as a refusal rather than an `expect` so
                // that if the two ever drift apart, a `GlcToSol` quote
                // fails loudly instead of skipping its deliverability check.
                // `RhnToSol` is the second route whose DESTINATION is the
                // Solana reserve mint, and it narrows exactly as
                // `GlcToSol` does: the net entitlement must be a whole
                // multiple of the canonical-to-mint scale, or the deposit
                // would fold to `ManualReview` as undeliverable and have
                // to be refunded on Robinhood. A quote must say so first.
                (Direction::RhnToSol, Some(solana_decimals)) => {
                    fee_breakdown.net.to_solana(solana_decimals).map_err(|e| {
                        ApiError::BadRequest(format!(
                            "amount {} cannot be represented exactly after the bridge fee at \
                             the reserve mint's {solana_decimals}-decimal precision: {e}",
                            gross_amount
                        ))
                    })?;
                }
                (Direction::GlcToSol | Direction::RhnToSol, None) => {
                    return Err(ApiError::Upstream(
                        "reserve mint decimals were not read for a Solana-legged quote".into(),
                    ))
                }
                (Direction::GlcToRhn | Direction::SolToRhn, _) => {
                    // Widening canonical -> Robinhood is exact for every
                    // representable canonical amount (the conversion
                    // module proves this at the `u64::MAX` boundary), but
                    // it is still checked rather than assumed: the proof
                    // rests on two decimals constants, and a change to
                    // either must surface as a refused quote rather than
                    // a promise the transfer would then break.
                    fee_breakdown.net.to_robinhood().map_err(|e| {
                        ApiError::BadRequest(format!(
                            "amount {gross_amount} cannot be represented exactly after the \
                             bridge fee at Robinhood's {robinhood_decimals}-decimal \
                             precision: {e}"
                        ))
                    })?;
                }
                // Both settle on Goldcoin, whose native atomic unit IS the
                // canonical accounting unit (both 8 decimals) — always exact.
                (Direction::SolToGlc | Direction::RhnToGlc, _) => {}
            }
            Ok(QuoteOutput {
                direction: input.direction,
                gross_amount: AtomicU64(fee_breakdown.gross.0),
                gross_display_amount: format_atomic_as_decimal_string(
                    fee_breakdown.gross.0,
                    goldcoin_decimals,
                ),
                fee_bps: fee_breakdown.fee_bps,
                fee_amount: AtomicU64(fee_breakdown.fee.0),
                fee_display_amount: format_atomic_as_decimal_string(
                    fee_breakdown.fee.0,
                    goldcoin_decimals,
                ),
                net_amount: AtomicU64(fee_breakdown.net.0),
                net_display_amount: format_atomic_as_decimal_string(
                    fee_breakdown.net.0,
                    goldcoin_decimals,
                ),
                source_decimals,
                destination_decimals,
                source_asset: source_asset.to_string(),
                destination_asset: destination_asset.to_string(),
            })
        })
    }
}

/// Renders an atomic amount as a fixed-point decimal string via checked
/// integer arithmetic only — never a float (docs/20-bridge-fee.md).
/// A `uint256` as a plain decimal string, or `None` if it does not fit a
/// `u128`.
///
/// `None` is not a formatting limitation dressed up as an error: a
/// Robinhood GLC amount above `u128::MAX` is 10^20 whole tokens and
/// cannot arise from this contract, so a word that large means the read
/// did not return what this service thinks it did. Reporting the field as
/// unknown is the honest answer; truncating it to 128 bits would publish
/// a different number than the chain holds.
fn u256_decimal(word: crate::evm::EvmU256) -> Option<String> {
    word.try_to_u128().ok().map(|v| v.to_string())
}

/// One rolling window projected for `now`, using the contract's own
/// [`crate::robinhood::calls::RollingWindow`] arithmetic rather than a
/// second implementation of it — so a bucket that has rolled over reports
/// the full limit remaining, exactly as `_consumeWindow` would on its
/// next write.
///
/// `None` only when a figure exceeds `u128` (see [`u256_decimal`]); a
/// partially-rendered window is never produced.
fn robinhood_window_view(
    limit: crate::evm::EvmU256,
    window: crate::robinhood::calls::RollingWindow,
    now: u64,
) -> Option<RobinhoodWindowView> {
    let is_current = window.is_current(now);
    // A stale bucket's recorded total is no longer charged against
    // anything, so the honest "used" figure for the bucket `now` falls in
    // is zero — the same conclusion `RollingWindow::remaining` reaches.
    let used = if is_current {
        window.total
    } else {
        crate::evm::EvmU256::from_u64(0)
    };
    Some(RobinhoodWindowView {
        limit_atomic: u256_decimal(limit)?,
        used_atomic: u256_decimal(used)?,
        remaining_atomic: u256_decimal(window.remaining(limit, now))?,
        resets_at: window.resets_at(),
        is_current,
    })
}

fn format_atomic_as_decimal_string(atomic: u64, decimals: u8) -> String {
    let scale = 10u64.pow(u32::from(decimals));
    let whole = atomic / scale;
    let frac = atomic % scale;
    format!("{whole}.{frac:0width$}", width = decimals as usize)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Reduces a `(RequestState, count)` breakdown (`Ledger::
/// request_state_counts`) into [`DirectionStats`]'s summary shape.
fn direction_stats(counts: Vec<(RequestState, i64)>) -> DirectionStats {
    let mut total_requests = 0i64;
    let mut in_progress_requests = 0i64;
    let mut settled_requests = 0i64;
    let mut manual_review_requests = 0i64;
    for (state, count) in counts {
        total_requests += count;
        if state.is_active() {
            in_progress_requests += count;
        }
        match state {
            RequestState::Settled => settled_requests += count,
            RequestState::ManualReview => manual_review_requests += count,
            _ => {}
        }
    }
    DirectionStats {
        total_requests,
        in_progress_requests,
        settled_requests,
        manual_review_requests,
    }
}

/// Parses a raw HTTP query string (`a=1&b=2`) into a lookup map. No
/// percent-decoding: every query parameter this API accepts is a simple
/// enum name or a small non-negative integer, none of which ever need it
/// — a value that did would fail the corresponding typed parse below
/// (e.g. `direction`, `state`) or be rejected as a malformed cursor/limit,
/// rather than being silently misinterpreted.
fn parse_query_string(query: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(q) = query else {
        return map;
    };
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");
        if !key.is_empty() {
            map.insert(key.to_string(), value.to_string());
        }
    }
    map
}

/// Shared `?cursor=`/`?limit=` parsing for every paginated list endpoint.
/// An absent or empty value falls back to the default; a present but
/// non-numeric value is a 400 (malformed pagination); a `limit` above
/// [`MAX_PAGE_LIMIT`] is silently clamped down to it, never rejected —
/// see [`MAX_PAGE_LIMIT`]'s own docs for why.
fn parse_page_params(
    q: &std::collections::HashMap<String, String>,
) -> Result<(Option<i64>, u32), ApiError> {
    let cursor = match q.get("cursor").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(
            s.parse::<i64>()
                .map_err(|_| ApiError::BadRequest("cursor must be an integer".into()))?,
        ),
    };
    let limit = match q.get("limit").map(String::as_str) {
        Some("") | None => DEFAULT_PAGE_LIMIT,
        Some(s) => {
            let n: u32 = s
                .parse()
                .map_err(|_| ApiError::BadRequest("limit must be a positive integer".into()))?;
            if n == 0 {
                return Err(ApiError::BadRequest("limit must be >= 1".into()));
            }
            n.min(MAX_PAGE_LIMIT)
        }
    };
    Ok((cursor, limit))
}

/// `?direction=` for `GET /reserves/history` — the reserve-level axis
/// (`ReserveDirection`), matching `glc-admin`'s own `goldcoin`/`solana`
/// CLI convention rather than the internal `GoldcoinReserve`/
/// `SolanaReserve` enum spelling.
fn parse_reserves_history_query(
    query: Option<&str>,
) -> Result<(Option<ReserveDirection>, Option<i64>, u32), ApiError> {
    let q = parse_query_string(query);
    let direction = match q.get("direction").map(String::as_str) {
        Some("") | None => None,
        Some("goldcoin") => Some(ReserveDirection::GoldcoinReserve),
        Some("solana") => Some(ReserveDirection::SolanaReserve),
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "unknown direction {other:?} (expected goldcoin|solana)"
            )))
        }
    };
    let (cursor, limit) = parse_page_params(&q)?;
    Ok((direction, cursor, limit))
}

/// `(direction, to_state, cursor, limit)` — parsed `/explorer/events`
/// query parameters.
type ExplorerEventsQuery = (Option<Direction>, Option<RequestState>, Option<i64>, u32);

/// `(address, state, cursor, limit)` — parsed `GET /transfers` query
/// parameters.
type ListTransfersQuery = (
    Option<TransferAddressFilter>,
    Option<RequestState>,
    Option<i64>,
    u32,
);

/// `?address=`/`?state=` for `GET /transfers` — `address` is a base58
/// Solana pubkey (same spelling `POST /transfers`'s `recipient` field
/// already accepts), `state` a `RequestState` filter.
/// `(address, wallet)` for `GET /recipients/sol-to-glc/eligibility` —
/// `address` required, passed through raw for
/// [`ApiSource::sol_to_glc_recipient_eligibility`] to validate as a
/// Goldcoin address; `wallet` optional, a base58 Solana pubkey (same
/// spelling `GET /transfers`'s `?address=` already accepts) parsed here
/// so the trait boundary carries real bytes, never an unvalidated string.
/// `parse_query_string`'s no-percent-decoding rule holds here too: both a
/// base58check address and a base58 pubkey are purely alphanumeric, so a
/// value that would need decoding is not valid and fails validation,
/// never gets misread.
fn parse_recipient_eligibility_query(
    query: Option<&str>,
) -> Result<(String, Option<[u8; 32]>), ApiError> {
    let q = parse_query_string(query);
    let address = required_eligibility_address(&q)?;
    let wallet = match q.get("wallet").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(
            s.parse::<Pubkey>()
                .map_err(|e| ApiError::BadRequest(format!("invalid wallet: {e}")))?
                .to_bytes(),
        ),
    };
    Ok((address, wallet))
}

/// The `?address=` half both eligibility endpoints share: required,
/// non-empty, and passed through RAW for the handler to validate as a
/// Goldcoin address. Shared so "address is required" cannot come to mean
/// two different things on two routes.
fn required_eligibility_address(
    q: &std::collections::HashMap<String, String>,
) -> Result<String, ApiError> {
    match q.get("address").map(String::as_str) {
        Some("") | None => Err(ApiError::BadRequest(
            "address query parameter is required".into(),
        )),
        Some(s) => Ok(s.to_string()),
    }
}

/// `(address, wallet)` for `GET /recipients/rhn-to-glc/eligibility` — the
/// exact shape [`parse_recipient_eligibility_query`] parses, with the
/// wallet read as a `0x`-prefixed EVM address instead of a base58 Solana
/// pubkey.
///
/// The wallet is parsed by [`crate::evm::address::EvmAddress`]'s own
/// `FromStr` — the same strict parser `GET /transfers`'s `?address=`
/// already uses: exactly 40 hex digits after `0x`, `0X` refused, and the
/// EIP-55 checksum verified whenever the digits mix case. A malformed
/// wallet is a 400, never a zero-padded or truncated blob that would then
/// be asked about someone else's rate-limit window. `parse_query_string`'s
/// no-percent-decoding rule holds here as it does for the Solana leg:
/// `0x` + hex is purely alphanumeric.
fn parse_rhn_recipient_eligibility_query(
    query: Option<&str>,
) -> Result<(String, Option<[u8; 20]>), ApiError> {
    let q = parse_query_string(query);
    let address = required_eligibility_address(&q)?;
    let wallet = match q.get("wallet").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(
            s.parse::<crate::evm::address::EvmAddress>()
                .map_err(|e| ApiError::BadRequest(format!("invalid wallet: {e}")))?
                .to_bytes(),
        ),
    };
    Ok((address, wallet))
}

/// `(route, source, destination)` for `GET /routes/{route}/eligibility`
/// — the route from the path (`Route::as_str` spelling, the same one
/// `POST /transfers`'s `route` field takes), `?source=`/`?destination=`
/// each optional, raw, and passed through for
/// [`ApiSource::route_wallet_eligibility`] to validate as the right
/// chain's address type; at least one must be present, since an answer
/// about no wallet at all is not an answer. `parse_query_string`'s
/// no-percent-decoding rule holds: every address form here is purely
/// alphanumeric.
#[allow(clippy::type_complexity)]
fn parse_route_wallet_eligibility_query(
    path: &str,
    query: Option<&str>,
) -> Result<(crate::routes::Route, Option<String>, Option<String>), ApiError> {
    let route_id = path
        .strip_prefix("/routes/")
        .and_then(|rest| rest.strip_suffix("/eligibility"))
        .ok_or_else(|| ApiError::BadRequest("not found".into()))?;
    let route: crate::routes::Route = route_id.parse().map_err(ApiError::BadRequest)?;
    let q = parse_query_string(query);
    let leg = |name: &str| match q.get(name).map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(s.to_string()),
    };
    let source = leg("source");
    let destination = leg("destination");
    if source.is_none() && destination.is_none() {
        return Err(ApiError::BadRequest(
            "at least one of the source and destination query parameters is required".into(),
        ));
    }
    Ok((route, source, destination))
}

/// `?address=` for `GET /transfers` — a caller's own address on EITHER
/// chain a route can name them on.
///
/// # The discriminator, and why it cannot be ambiguous
///
/// A `0x` prefix means an EVM address and nothing else; anything else is
/// parsed as a base58 Solana pubkey exactly as before. The two grammars
/// cannot overlap: `0` is not in the base58 alphabet, so no valid Solana
/// pubkey has ever started with `0`, let alone `0x`. Dispatching on the
/// prefix therefore cannot reinterpret an input that used to parse as a
/// pubkey — the existing filter is bit-for-bit unchanged for every string
/// that previously reached it.
///
/// # Both halves are strict
///
/// `0x`-prefixed input is parsed by [`crate::evm::address::EvmAddress`]'s
/// own `FromStr`, which requires exactly 40 hex digits, refuses `0X`, and
/// verifies the EIP-55 checksum whenever the digits mix case. A malformed
/// `0x` address is a 400, never a silent fallthrough to the Solana parser
/// and never a zero-padded or truncated blob: the whole point of a typed
/// address here is that the next thing anyone does with a matching row is
/// show a user their money.
fn parse_transfer_address_filter(raw: &str) -> Result<TransferAddressFilter, ApiError> {
    if raw.starts_with("0x") {
        return raw
            .parse::<crate::evm::address::EvmAddress>()
            .map(|a| TransferAddressFilter::Evm(a.to_bytes()))
            .map_err(|e| ApiError::BadRequest(format!("invalid address: {e}")));
    }
    raw.parse::<Pubkey>()
        .map(|p| TransferAddressFilter::Solana(p.to_bytes()))
        .map_err(|e| ApiError::BadRequest(format!("invalid address: {e}")))
}

fn parse_list_transfers_query(query: Option<&str>) -> Result<ListTransfersQuery, ApiError> {
    let q = parse_query_string(query);
    let address = match q.get("address").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(parse_transfer_address_filter(s)?),
    };
    let state = match q.get("state").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(s.parse::<RequestState>().map_err(ApiError::BadRequest)?),
    };
    let (cursor, limit) = parse_page_params(&q)?;
    Ok((address, state, cursor, limit))
}

/// `?direction=`/`?state=` for `GET /explorer/events` — the transfer-level
/// axis (`Direction`) and an optional `RequestState` filter, both parsed
/// via their own `FromStr` (the same `"GlcToSol"`/`"SolToGlc"` spelling
/// `POST /quote` already accepts as input).
fn parse_explorer_events_query(query: Option<&str>) -> Result<ExplorerEventsQuery, ApiError> {
    let q = parse_query_string(query);
    let direction = match q.get("direction").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(s.parse::<Direction>().map_err(ApiError::BadRequest)?),
    };
    let state = match q.get("state").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(s.parse::<RequestState>().map_err(ApiError::BadRequest)?),
    };
    let (cursor, limit) = parse_page_params(&q)?;
    Ok((direction, state, cursor, limit))
}

/// Every error response's body. `error` is always present; the two
/// optional fields are set only for [`ApiError::WalletLimited`], so a
/// client can act on WHICH window blocked and when it reopens without
/// parsing the message.
#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocked_reasons: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after: Option<i64>,
}

impl ErrorBody {
    fn message(error: impl Into<String>) -> Self {
        ErrorBody {
            error: error.into(),
            blocked_reasons: None,
            retry_after: None,
        }
    }
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(bytes)))
        .expect("well-formed response")
}

fn error_response(err: ApiError) -> Response<Full<Bytes>> {
    let status = err.status();
    let body = match &err {
        ApiError::WalletLimited {
            blocked_reasons,
            retry_after,
        } => ErrorBody {
            error: err.to_string(),
            blocked_reasons: Some(blocked_reasons.clone()),
            retry_after: Some(*retry_after),
        },
        _ => ErrorBody::message(err.to_string()),
    };
    json_response(status, &body)
}

async fn handle<S: ApiSource>(
    req: Request<hyper::body::Incoming>,
    source: Arc<S>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let response = match (&method, path.as_str()) {
        (&Method::GET, "/status") => match source.status().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/chains") => match source.chains().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/limits") => match source.limits().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/reserve") => match source.reserve().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        // The two Robinhood read endpoints. Deliberately their own paths
        // rather than fields grafted onto `/reserve` and `/limits`: those
        // two keep their exact existing response shape, so no client that
        // has never heard of Robinhood sees any change at all. Both are
        // GET-only and read-only — there is no Robinhood write surface on
        // this listener, and serving these does not consult, let alone
        // open, any route gate.
        (&Method::GET, "/robinhood/reserve") => match source.robinhood_reserve().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/robinhood/limits") => match source.robinhood_limits().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/health") => match source.health().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/stats") => match source.stats().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/reserves/history") => {
            match parse_reserves_history_query(req.uri().query()) {
                Ok((direction, cursor, limit)) => {
                    match source.reserves_history(direction, cursor, limit).await {
                        Ok(v) => json_response(StatusCode::OK, &v),
                        Err(e) => error_response(e),
                    }
                }
                Err(e) => error_response(e),
            }
        }
        (&Method::GET, "/explorer/events") => {
            match parse_explorer_events_query(req.uri().query()) {
                Ok((direction, state, cursor, limit)) => {
                    match source
                        .explorer_events(direction, state, cursor, limit)
                        .await
                    {
                        Ok(v) => json_response(StatusCode::OK, &v),
                        Err(e) => error_response(e),
                    }
                }
                Err(e) => error_response(e),
            }
        }
        (&Method::GET, "/transfers") => match parse_list_transfers_query(req.uri().query()) {
            Ok((address, state, cursor, limit)) => {
                match source.list_transfers(address, state, cursor, limit).await {
                    Ok(v) => json_response(StatusCode::OK, &v),
                    Err(e) => error_response(e),
                }
            }
            Err(e) => error_response(e),
        },
        (&Method::POST, "/transfers") => {
            let body = match req.into_body().collect().await {
                Ok(b) => b.to_bytes(),
                Err(_) => {
                    return Ok(json_response(
                        StatusCode::BAD_REQUEST,
                        &ErrorBody::message("could not read request body"),
                    ))
                }
            };
            match serde_json::from_slice::<CreateTransferInput>(&body) {
                Ok(input) => match source.create_goldcoin_deposit_transfer(input).await {
                    Ok(v) => json_response(StatusCode::CREATED, &v),
                    Err(e) => error_response(e),
                },
                Err(e) => json_response(
                    StatusCode::BAD_REQUEST,
                    &ErrorBody::message(format!("malformed request body: {e}")),
                ),
            }
        }
        (&Method::POST, "/quote") => {
            let body = match req.into_body().collect().await {
                Ok(b) => b.to_bytes(),
                Err(_) => {
                    return Ok(json_response(
                        StatusCode::BAD_REQUEST,
                        &ErrorBody::message("could not read request body"),
                    ))
                }
            };
            match serde_json::from_slice::<QuoteInput>(&body) {
                Ok(input) => match source.quote(input).await {
                    Ok(v) => json_response(StatusCode::OK, &v),
                    Err(e) => error_response(e),
                },
                Err(e) => json_response(
                    StatusCode::BAD_REQUEST,
                    &ErrorBody::message(format!("malformed request body: {e}")),
                ),
            }
        }
        (&Method::GET, "/recipients/sol-to-glc/eligibility") => {
            match parse_recipient_eligibility_query(req.uri().query()) {
                Ok((address, wallet)) => {
                    match source
                        .sol_to_glc_recipient_eligibility(address, wallet)
                        .await
                    {
                        Ok(v) => json_response(StatusCode::OK, &v),
                        Err(e) => error_response(e),
                    }
                }
                Err(e) => error_response(e),
            }
        }
        (&Method::GET, "/recipients/rhn-to-glc/eligibility") => {
            match parse_rhn_recipient_eligibility_query(req.uri().query()) {
                Ok((address, wallet)) => {
                    match source
                        .rhn_to_glc_recipient_eligibility(address, wallet)
                        .await
                    {
                        Ok(v) => json_response(StatusCode::OK, &v),
                        Err(e) => error_response(e),
                    }
                }
                Err(e) => error_response(e),
            }
        }
        (&Method::GET, p) if p.starts_with("/routes/") && p.ends_with("/eligibility") => {
            match parse_route_wallet_eligibility_query(p, req.uri().query()) {
                Ok((route, source_wallet, destination_wallet)) => {
                    match source
                        .route_wallet_eligibility(route, source_wallet, destination_wallet)
                        .await
                    {
                        Ok(v) => json_response(StatusCode::OK, &v),
                        Err(e) => error_response(e),
                    }
                }
                Err(e) => error_response(e),
            }
        }
        (&Method::GET, p) if p.starts_with("/transfers/") => {
            let id_str = &p["/transfers/".len()..];
            match id_str.parse::<i64>() {
                Ok(id) => match source.get_transfer(id).await {
                    Ok(Some(v)) => json_response(StatusCode::OK, &v),
                    Ok(None) => json_response(
                        StatusCode::NOT_FOUND,
                        &ErrorBody::message(format!("no transfer with id {id}")),
                    ),
                    Err(e) => error_response(e),
                },
                Err(_) => json_response(
                    StatusCode::BAD_REQUEST,
                    &ErrorBody::message("transfer id must be an integer"),
                ),
            }
        }
        _ => json_response(StatusCode::NOT_FOUND, &ErrorBody::message("not found")),
    };
    Ok(response)
}

/// Serves the bridge API until `shutdown` fires. No authentication and no
/// TLS termination here (same posture as [`crate::ops::health::serve`]) —
/// run this behind a reverse proxy that provides both if it is ever
/// reachable from outside a trusted network.
pub async fn serve<S: ApiSource>(
    addr: SocketAddr,
    source: Arc<S>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "bridge API listening");
    serve_on(listener, source, shutdown).await
}

/// [`serve`] over a listener the CALLER already bound — see
/// [`crate::admin_api::serve_on`] for why that ordering matters.
pub async fn serve_on<S: ApiSource>(
    listener: tokio::net::TcpListener,
    source: Arc<S>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("bridge API: shutdown signal received, exiting");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "bridge API: accept failed");
                        continue;
                    }
                };
                let source = Arc::clone(&source);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| handle(req, Arc::clone(&source)));
                    if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                        tracing::debug!(%peer, error = %e, "bridge API connection ended");
                    }
                });
            }
        }
    }
}

pub mod atomic;

#[cfg(test)]
mod tests;
