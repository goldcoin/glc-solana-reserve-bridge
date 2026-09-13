//! The single source of truth for "would this reserve admit new demand
//! right now, and if not, which gate refused".
//!
//! # Why this module exists
//!
//! Before it, the admission AND was written out three times — once in
//! [`Ledger::fold_sol_deposit`], once in
//! [`Ledger::fold_robinhood_deposit`], and (partially, and for one
//! route only) in the public API's `sol_to_glc_admission_open`. The
//! production incident that produced this module was the gap between the
//! second and the third: `GET /chains` reported `RhnToGlc` as
//! `enabled: true` — which is a statement about
//! [`crate::routes::RouteGate`], and was correct — while
//! `reserve_ledger.admission_closed` was set on `GoldcoinReserve`, so
//! every newly observed Robinhood deposit folded straight into
//! `ManualReview` with `admission_closed_at_fold`. A user could, and
//! did, make an irreversible on-chain deposit through a UI that had been
//! told the route was available.
//!
//! An `RhnToGlc` deposit is made directly to the custody contract; there
//! is no `POST /transfers` preflight in front of it and there cannot be
//! one. The only defence is that the availability signal the UI reads is
//! computed from the SAME state the fold will later gate on. So the
//! evaluation lives here, once, and every caller — both folds and the
//! public API — routes through it.
//!
//! # What is in scope, and what deliberately is not
//!
//! In scope: the DIRECTION-WIDE gates, i.e. everything that depends only
//! on the destination reserve's own state — plus, since v25, exactly one
//! ROUTE-scoped gate, for the reason the next section gives.
//!
//! - `route_admission_closed` (the route's own `route_admission` row —
//!   ROUTE-scoped, not reserve-scoped),
//! - `paused` (operator pause, and the quota auto-pause `crate::quota`
//!   engages),
//! - `admission_closed` (the operator-only reserve-wide admission
//!   switch),
//! - `liquidity_admission_closed` (the automatic confirmed-liquidity
//!   gate's hysteresis state),
//! - the confirmed-liquidity admission safety buffer's per-request
//!   arithmetic,
//! - the mature-UTXO pool floor (`utxo_pool_min_available_count`),
//! - the plain capacity check.
//!
//! Not in scope: anything keyed on an identity rather than on the
//! reserve. The two rolling-24h limits are per-recipient and per-source-
//! wallet, so a route-level answer cannot evaluate them at all — they are
//! passed IN as [`InboundRateLimits`] by the fold, which knows the
//! addresses, and are reported to a UI by
//! `GET /recipients/{sol,rhn}-to-glc/eligibility` instead. Keeping them
//! as an input rather than moving them here is what lets one ranking
//! function serve both callers without either inventing a limit the
//! other does not apply.
//!
//! # Why ONE route-scoped gate lives in a reserve-scoped evaluator
//!
//! `route_admission_closed` is not a property of the destination
//! reserve, so by the rule above it does not belong here. It is here
//! anyway, and deliberately, because the alternative is worse in
//! exactly the way this module exists to prevent.
//!
//! `SolToGlc` and `RhnToGlc` both settle out of `GoldcoinReserve`, so
//! before v25 the only admission control either had was shared between
//! them and closing one closed both. The route-level gate exists to
//! separate them. Evaluating it OUTSIDE this module would mean writing
//! the same AND in three places again —
//! [`Ledger::fold_sol_deposit`], [`Ledger::fold_robinhood_deposit`] and
//! the public API's per-route `available` — which is precisely the
//! duplication whose drift caused the production incident described
//! above. A UI reading an `available` that omitted this gate would
//! offer an `RhnToGlc` transfer that the fold then parked, and an
//! `RhnToGlc` deposit is irreversible with no preflight in front of it.
//!
//! So the gate is evaluated here, once, ranked FIRST (it is the most
//! specific operator statement available), and every caller gets it for
//! free. The reserve-scoped fields keep their meaning untouched: this
//! struct now carries "the gates a newly observed deposit on THIS ROUTE
//! must pass", of which all but one are reserve-wide.
//!
//! The two are ANDed and neither can clear the other. Reopening a
//! reserve-wide pause does not open a route whose own gate is closed,
//! and opening a route gate does not admit anything while the reserve
//! is paused.
//!
//! Also not in scope: the route ENABLEMENT gate
//! ([`crate::routes::RouteGate`] — a different axis with a different
//! settable set), the Robinhood custody contract's own
//! `routeEnabled`/pause flags, the destination's deliverability, and the
//! Solana program's on-chain rolling-volume window. Those are separate
//! gates with separate owners; see [`InboundAdmissionGates::read`]'s docs
//! for what a caller still has to check for itself.

use rusqlite::Connection;

use super::{Direction, Ledger, LedgerError, ReserveDirection};

/// The per-identity rolling-24h wallet windows (`ledger::wallet_window`),
/// supplied by a caller that knows the source wallet and the recipient —
/// `source_wallet_rate_limited` is the source wallet's window,
/// `recipient_rate_limited` the destination wallet's.
///
/// [`Default`] is "neither limit applies", which is the only honest
/// answer a route-level caller can give: it has no address to ask about.
/// A route-level `available` therefore says nothing about whether a
/// PARTICULAR user is inside a cooldown window, exactly as it says
/// nothing about whether their destination address parses — and the
/// eligibility endpoints exist to answer that half.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InboundRateLimits {
    pub source_wallet_rate_limited: bool,
    pub recipient_rate_limited: bool,
}

impl From<super::RouteWalletEligibility> for InboundRateLimits {
    fn from(eligibility: super::RouteWalletEligibility) -> Self {
        InboundRateLimits {
            source_wallet_rate_limited: eligibility.source_retry_after.is_some(),
            recipient_rate_limited: eligibility.destination_retry_after.is_some(),
        }
    }
}

/// Which admission gate refused, ranked most specific first.
///
/// The variant order IS the ranking [`InboundAdmissionGates::blocker`]
/// applies, and it reproduces the `else if` chain both folds used to
/// spell out separately — so the same situation still produces the same
/// `manual_review_note` on either route, and now cannot stop doing so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundAdmissionBlocker {
    /// An operator has closed admission on THIS ROUTE
    /// (`glc-admin route-admission-close`), independently of the
    /// reserve-wide switches below. Never automatic.
    ///
    /// Ranked first because it is the narrowest true statement: it names
    /// the one route an operator actually closed, where
    /// [`Self::AdmissionClosed`] and [`Self::ReservePaused`] would both
    /// report a reserve-wide condition that may not be present at all.
    ///
    /// Ranking it first changes nothing for any pre-v25 ledger: the v25
    /// seed leaves every route's gate OPEN, so this variant cannot fire
    /// until an operator closes one.
    RouteAdmissionClosed,
    /// An operator has closed admission on this RESERVE
    /// (`glc-admin close-admission`), which closes every route drawing
    /// on it. Never automatic.
    AdmissionClosed,
    /// The reserve's own local pause.
    ReservePaused,
    SourceWalletRateLimited,
    RecipientRateLimited,
    /// The mature, unreserved vault UTXO pool is at or below
    /// `utxo_pool_min_available_count`.
    UtxoLiquidityLow,
    /// Confirmed unreserved headroom is inside the admission safety
    /// buffer, either direction-wide (the hysteresis gate is closed) or
    /// for this particular amount.
    LiquidityBufferLow,
    /// The accounting figure itself is exhausted.
    InsufficientCapacity,
}

impl InboundAdmissionBlocker {
    /// A stable operator-facing identifier for this gate, for the admin
    /// API and CLI.
    ///
    /// Deliberately NOT the `manual_review_note` below, and never
    /// interchangeable with it: that string is a durable column value on
    /// a `bridge_requests` row that resume/refund allowlists match
    /// against exactly, so it must never be reworded for display
    /// reasons. This one names the live gate and is free to read well.
    pub fn as_str(self) -> &'static str {
        match self {
            InboundAdmissionBlocker::RouteAdmissionClosed => "route_admission_closed",
            InboundAdmissionBlocker::AdmissionClosed => "reserve_admission_closed",
            InboundAdmissionBlocker::ReservePaused => "reserve_paused",
            InboundAdmissionBlocker::SourceWalletRateLimited => {
                Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT
            }
            InboundAdmissionBlocker::RecipientRateLimited => {
                Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT
            }
            InboundAdmissionBlocker::UtxoLiquidityLow => "utxo_liquidity_low",
            InboundAdmissionBlocker::LiquidityBufferLow => "liquidity_buffer_low",
            InboundAdmissionBlocker::InsufficientCapacity => "insufficient_capacity",
        }
    }

    /// The `bridge_requests.manual_review_note` a fold records for this
    /// blocker — read from [`Ledger`]'s reason constants, never
    /// re-spelled, so the note strings and the ranking that chooses
    /// between them live next to each other.
    pub fn manual_review_note(self) -> &'static str {
        match self {
            InboundAdmissionBlocker::RouteAdmissionClosed => {
                Ledger::MANUAL_REVIEW_REASON_ROUTE_ADMISSION_CLOSED
            }
            InboundAdmissionBlocker::AdmissionClosed => {
                Ledger::MANUAL_REVIEW_REASON_ADMISSION_CLOSED
            }
            InboundAdmissionBlocker::ReservePaused => Ledger::MANUAL_REVIEW_REASON_PAUSED,
            InboundAdmissionBlocker::SourceWalletRateLimited => {
                Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT
            }
            InboundAdmissionBlocker::RecipientRateLimited => {
                Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT
            }
            InboundAdmissionBlocker::UtxoLiquidityLow => {
                Ledger::MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW
            }
            InboundAdmissionBlocker::LiquidityBufferLow => {
                Ledger::MANUAL_REVIEW_REASON_LIQUIDITY_BUFFER_LOW
            }
            InboundAdmissionBlocker::InsufficientCapacity => {
                Ledger::MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY
            }
        }
    }
}

/// One reserve's admission-relevant state, read at a single instant.
///
/// A plain data snapshot with no connection inside it: the fold reads it
/// from within its own write transaction (so the state a decision was
/// made against and the decision itself commit or roll back together),
/// while the API reads it from a read-only connection. Both then call
/// the same [`InboundAdmissionGates::blocker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboundAdmissionGates {
    /// The ROUTE's own admission gate (`route_admission.admission_closed`
    /// — v25), the only field here that is not a property of the
    /// reserve. See the module docs' "Why ONE route-scoped gate lives in
    /// a reserve-scoped evaluator".
    ///
    /// Always `false` for a route with no route-level gate
    /// ([`crate::routes::Route::is_admission_settable`]), and `false` on
    /// a pre-v25 ledger — both of which mean "nobody has closed this
    /// route", which is the pre-existing behaviour.
    pub route_admission_closed: bool,
    pub paused: bool,
    pub admission_closed: bool,
    /// The confirmed-liquidity gate's CURRENT hysteresis state. The fold
    /// passes the value it just re-evaluated and persisted; a read-only
    /// caller passes the persisted one. Never re-derived here — the
    /// hysteresis rule lives in
    /// [`Ledger::next_liquidity_admission_closed`] and must stay there.
    pub liquidity_admission_closed: bool,
    /// `total_reserve_balance - protected_minimum - reserved_liquidity`.
    pub confirmed_headroom_atomic: i64,
    /// The admission safety buffer's close threshold; `0` disables it.
    pub admission_buffer_atomic: i64,
    /// `0` disables the mature-UTXO-pool floor.
    pub min_available_utxo_count: i64,
    /// The live mature, unreserved UTXO count. Always `0` when
    /// `min_available_utxo_count` is `0` (the query is skipped, since
    /// the floor short-circuits) and for every reserve other than
    /// `GoldcoinReserve`, whose vault pool it is.
    pub available_utxo_count: i64,
}

impl InboundAdmissionGates {
    /// Reads every gate a newly observed deposit on `direction` must
    /// pass: that route's own admission row, and every direction-wide
    /// gate on the reserve it settles out of.
    ///
    /// Takes the settlement [`Direction`] rather than a
    /// [`ReserveDirection`] so the route and the reserve cannot
    /// disagree — the reserve is derived here, from
    /// [`Direction::destination_reserve`], rather than supplied
    /// alongside a route that a caller might mismatch it with.
    ///
    /// `liquidity_admission_closed` is an INPUT rather than a read
    /// because the two callers legitimately want different instants of
    /// it: [`Ledger::fold_sol_deposit`] and
    /// [`Ledger::fold_robinhood_deposit`] re-evaluate the hysteresis
    /// against current headroom and persist the transition inside their
    /// own transaction, then pass that fresh value; a read-only caller
    /// must never move the gate and passes the persisted column. Making
    /// it a parameter is what keeps this function incapable of writing.
    ///
    /// # This is necessary, never sufficient
    ///
    /// A caller still owns every gate that is not read here: the route
    /// ENABLEMENT gate ([`crate::routes::RouteGate`] — a different axis
    /// from the route ADMISSION gate this does read), the destination's
    /// deliverability, the two rolling-24h limits (via
    /// [`InboundRateLimits`]), and — for anything touching the Robinhood
    /// custody contract — the contract's own
    /// `routeEnabled`/`depositsPaused`/`payoutsPaused`/`signerEpoch`,
    /// which this service does not control and cannot cache.
    pub(crate) fn read(
        conn: &Connection,
        direction: Direction,
        liquidity_admission_closed: bool,
    ) -> Result<Self, LedgerError> {
        let reserve = direction.destination_reserve();
        // The route's own gate, read from the SAME connection (and so,
        // for a fold, from inside the same write transaction) as every
        // reserve figure below — so the state the decision was made
        // against and the decision itself commit or roll back together.
        let route_admission_closed =
            Ledger::route_admission_closed_in(conn, crate::routes::Route::from(direction))?;
        let (
            paused,
            admission_closed,
            min_available_utxo_count,
            balance,
            protected_minimum,
            reserved,
        ): (i64, i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT paused, admission_closed, utxo_pool_min_available_count,
                        total_reserve_balance, protected_minimum, reserved_liquidity
                 FROM reserve_ledger WHERE direction = ?1",
                [reserve],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => LedgerError::ReserveNotInitialized(reserve),
                other => LedgerError::Sqlite(other),
            })?;
        let (admission_buffer_atomic, _reopen_atomic, _persisted_closed) =
            Ledger::read_liquidity_admission_row(conn, reserve)?;
        // The mature-pool floor is a property of the GOLDCOIN vault, so
        // it is asked about only for the reserve that vault backs —
        // exactly the short-circuit
        // `Ledger::check_utxo_liquidity_for_admission` already applies,
        // rather than a second opinion about which reserves own a UTXO
        // pool. Skipped again when the floor is disabled, since
        // `min_available_utxo_count == 0` short-circuits the check
        // itself and the count would be read only to be ignored.
        let available_utxo_count =
            if reserve == ReserveDirection::GoldcoinReserve && min_available_utxo_count > 0 {
                Ledger::count_available_vault_utxos(conn)?
            } else {
                0
            };
        Ok(InboundAdmissionGates {
            route_admission_closed,
            paused: paused != 0,
            admission_closed: admission_closed != 0,
            liquidity_admission_closed,
            confirmed_headroom_atomic: balance - protected_minimum - reserved,
            admission_buffer_atomic,
            min_available_utxo_count: if reserve == ReserveDirection::GoldcoinReserve {
                min_available_utxo_count
            } else {
                0
            },
            available_utxo_count,
        })
    }

    /// The read-only form: reads the persisted hysteresis state rather
    /// than evaluating it, so it can never move the gate.
    pub(crate) fn read_persisted(
        conn: &Connection,
        direction: Direction,
    ) -> Result<Self, LedgerError> {
        let reserve = direction.destination_reserve();
        let (_buffer, _reopen, persisted_closed) =
            Ledger::read_liquidity_admission_row(conn, reserve).map_err(|e| match e {
                LedgerError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                    LedgerError::ReserveNotInitialized(reserve)
                }
                other => other,
            })?;
        Self::read(conn, direction, persisted_closed)
    }

    /// Whether the mature-UTXO pool floor is satisfied.
    fn utxo_liquidity_ok(&self) -> bool {
        self.min_available_utxo_count == 0
            || self.available_utxo_count > self.min_available_utxo_count
    }

    /// Whether admitting `net_destination_atomic` would still leave the
    /// confirmed-liquidity admission safety buffer intact — the full
    /// required formula
    ///
    /// ```text
    /// balance >= protected_minimum + reserved_liquidity
    ///            + net_destination_atomic + buffer
    /// ```
    ///
    /// rearranged around the already-computed headroom.
    fn liquidity_buffer_ok(&self, net_destination_atomic: i64) -> bool {
        self.admission_buffer_atomic <= 0
            || self.confirmed_headroom_atomic - net_destination_atomic
                >= self.admission_buffer_atomic
    }

    /// **The admission decision.** `None` means every gate is open and
    /// this amount would be admitted; `Some(blocker)` names the
    /// highest-ranked gate that refused.
    ///
    /// The ranking is the variant order of
    /// [`InboundAdmissionBlocker`], which reproduces verbatim the
    /// `else if` chain both folds previously spelled out for themselves.
    pub fn blocker(
        &self,
        net_destination_atomic: i64,
        limits: InboundRateLimits,
    ) -> Option<InboundAdmissionBlocker> {
        if self.route_admission_closed {
            Some(InboundAdmissionBlocker::RouteAdmissionClosed)
        } else if self.admission_closed {
            Some(InboundAdmissionBlocker::AdmissionClosed)
        } else if self.paused {
            Some(InboundAdmissionBlocker::ReservePaused)
        } else if limits.source_wallet_rate_limited {
            Some(InboundAdmissionBlocker::SourceWalletRateLimited)
        } else if limits.recipient_rate_limited {
            Some(InboundAdmissionBlocker::RecipientRateLimited)
        } else if !self.utxo_liquidity_ok() {
            Some(InboundAdmissionBlocker::UtxoLiquidityLow)
        } else if self.liquidity_admission_closed
            || !self.liquidity_buffer_ok(net_destination_atomic)
        {
            Some(InboundAdmissionBlocker::LiquidityBufferLow)
        } else if net_destination_atomic > self.confirmed_headroom_atomic {
            Some(InboundAdmissionBlocker::InsufficientCapacity)
        } else {
            None
        }
    }

    /// The ROUTE-level question: is there any amount at all this route
    /// would admit right now — its own admission gate open AND its
    /// destination reserve willing?
    ///
    /// Defined as [`Self::blocker`] at the smallest amount that can
    /// exist — one atomic unit — and with no rate limits, because a
    /// route-level caller has no address to evaluate them against. That
    /// is not an approximation of the amount-dependent gates, it is
    /// their exact weakest form: `net <= headroom` at `net = 1` is
    /// `headroom > 0`, and `headroom - net >= buffer` at `net = 1` is
    /// `headroom > buffer`. Deriving it by CALLING the real decision,
    /// rather than by re-stating those two inequalities, is what makes
    /// this incapable of drifting away from what a fold will do.
    ///
    /// So `None` here means "a minimum-sized deposit would be admitted",
    /// never "this specific deposit would be" — a large enough one can
    /// still be held back by the buffer or by capacity, and is then
    /// parked and refundable exactly as before. A public verdict for a
    /// route with a known normal transfer size must therefore use
    /// [`Self::route_blocker_at`] instead; see its docs for the incident
    /// that made the difference matter.
    pub fn route_blocker(&self) -> Option<InboundAdmissionBlocker> {
        self.route_blocker_at(1)
    }

    /// The ROUTE-level question for a deposit of a KNOWN size: would a
    /// transfer whose net destination amount is `probe_net_destination_
    /// atomic` be admitted right now, with no wallet to evaluate the
    /// rate limits against?
    ///
    /// This is what a public "is this route open" answer must be
    /// computed from when the route has a normal transfer size. The
    /// one-atomic-unit form ([`Self::route_blocker`]) is exact for the
    /// question it asks, but that question — "would ANY amount be
    /// admitted" — is not the one a depositor is asking. With a safety
    /// buffer configured, `headroom > buffer` holds while `headroom -
    /// net < buffer` for every real transfer, and a route advertised on
    /// the former parks every deposit it attracts on the latter
    /// (`liquidity_buffer_low_at_fold`). The 2026-09-12 production
    /// incident was exactly that: `SolToGlc` reported `available: true`
    /// with 280,252 GLC of headroom against a 250,000 GLC buffer while
    /// every 47,000 GLC deposit folded straight into `ManualReview`.
    ///
    /// Still the real decision — [`Self::blocker`] with the probe amount
    /// and no rate limits — never a restatement of its inequalities, so
    /// this cannot drift from what a fold of that size will do. The
    /// probe is the caller's statement of "a normal transfer" (for
    /// `SolToGlc`, the Solana program's own `per_transfer_limit` net of
    /// the route fee); a probe of `1` is the weakest form and equals
    /// [`Self::route_blocker`].
    pub fn route_blocker_at(
        &self,
        probe_net_destination_atomic: i64,
    ) -> Option<InboundAdmissionBlocker> {
        self.blocker(probe_net_destination_atomic, InboundRateLimits::default())
    }

    /// The largest NET destination amount this route would admit right
    /// now with no wallet limits in play — `0` when no amount at all
    /// would be (any amount-independent gate closed, or headroom already
    /// inside the buffer).
    ///
    /// Derived from the same inequalities [`Self::blocker`] applies, and
    /// pinned to it by test (`blocker(max) == None` and `blocker(max + 1)
    /// != None` whenever `max > 0`) rather than trusted: the buffer rule
    /// `headroom - net >= buffer` gives `net <= headroom - buffer`, and
    /// the capacity rule `net <= headroom` is weaker whenever a buffer is
    /// configured, so the buffer bounds it when present and headroom
    /// does otherwise. Reported to callers so a UI can render "up to N"
    /// instead of discovering N by parking a deposit.
    pub fn max_admissible_net_destination_atomic(&self) -> i64 {
        // Any gate that does not depend on the amount closes the route
        // for every amount — asked of the real decision at the weakest
        // probe, so the list of such gates is never restated here.
        if self.route_blocker().is_some() {
            return 0;
        }
        let bound = if self.admission_buffer_atomic > 0 {
            self.confirmed_headroom_atomic - self.admission_buffer_atomic
        } else {
            self.confirmed_headroom_atomic
        };
        bound.max(0)
    }
}

#[cfg(test)]
mod tests;
