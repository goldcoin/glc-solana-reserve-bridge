//! The reserve ledger: reservation/capacity accounting and the
//! bridge-request state machine (docs/04-state-machines.md,
//! docs/05-reserve-accounting.md). Owns every mutation of
//! `bridge_requests`/`reserve_ledger` — chain-observation modules
//! (`goldcoin::indexer`, `solana::indexer`) call into this module rather
//! than touching SQL directly, so the accounting invariant is enforced in
//! exactly one place.
//!
//! # Concurrency and crash safety
//!
//! SQLite serializes writers DB-wide; every mutating operation here runs
//! inside a single `BEGIN IMMEDIATE` transaction that either fully commits
//! or fully rolls back, which is what makes "reservation and settlement
//! bookkeeping" race-free per docs/05-reserve-accounting.md without a
//! separate row-lock primitive — SQLite's write lock IS the lock. A crash
//! mid-operation leaves the last COMMITted state on disk (WAL mode); there
//! is no partial-write state to recover from, and every observation-
//! processing entry point below is additionally idempotent (checked via a
//! UNIQUE constraint or an explicit already-processed check) so replaying
//! the same chain event after a restart is always safe (constraint 5).

mod admission;
pub mod rapid_burst;
mod robinhood;
pub mod robinhood_tx;
mod schema;
mod types;
pub mod wallet_window;

pub use admission::{InboundAdmissionBlocker, InboundAdmissionGates, InboundRateLimits};
pub use rapid_burst::{RapidBurstMatch, RapidBurstPolicy, RapidBurstRule};
pub use robinhood::{
    RobinhoodDepositObservation, RobinhoodFinality, RobinhoodHalt, RobinhoodHaltReason,
    RobinhoodObservationConflict, RobinhoodObservationOutcome, RobinhoodObservationRow,
    RobinhoodObservationSummary, RobinhoodRangeApplied,
};
pub use robinhood_tx::{
    BeginTxOutcome, NewRobinhoodTx, RobinhoodAuthSignature, RobinhoodPayoutEvidence, RobinhoodTx,
    RobinhoodTxKind, RobinhoodTxState,
};
pub use types::{
    AdminAuditEntry, AdminAuditFilter, AdminAuditOutcome, AdminAuditRow, BridgeRequest,
    CustodyTransition, CustodyTransitionKind, CustodyTransitionState, Direction,
    ManualReviewDisposition, OperatorDecision, RebalanceKind, RebalanceRequest, RebalanceState,
    RequestAmounts, RequestState, ReserveDirection, SolanaRefund, SolanaRefundState, SourceChain,
    TransferAddressFilter, LEGACY_SOLANA_SOURCE_CONTRACT,
};
pub use wallet_window::{RouteWalletEligibility, WalletRole, WalletWindowScope};

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

/// Row counts of everything Robinhood-side. See [`Ledger::robinhood_activity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RobinhoodActivity {
    pub deposit_observations: u64,
    pub transactions: u64,
    /// `bridge_requests` on any of the four Robinhood routes.
    pub requests: u64,
    /// `rebalance_requests` against the Robinhood reserve.
    pub rebalances: u64,
}

impl RobinhoodActivity {
    pub fn is_empty(&self) -> bool {
        self.deposit_observations == 0
            && self.transactions == 0
            && self.requests == 0
            && self.rebalances == 0
    }
}

impl std::fmt::Display for RobinhoodActivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} deposit observation(s), {} outbound operation(s), {} bridge request(s), {} \
             rebalance request(s)",
            self.deposit_observations, self.transactions, self.requests, self.rebalances
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The ledger database was written by a NEWER binary than this one
    /// (`schema_version` is ahead of `CURRENT_SCHEMA_VERSION`). Refused
    /// outright rather than silently stamping this binary's older version
    /// over it — see `schema::open_and_migrate`'s forward-compatibility
    /// guard and docs/09-runbook.md "Schema rollback".
    #[error(
        "ledger database schema version {found} is NEWER than this binary supports \
         ({supported}) — this binary is older than the one that last wrote this database. \
         Refusing to open it. Deploy the newer binary again, or restore a pre-upgrade backup \
         with scripts/restore-ledger.sh; never run an older binary against a newer ledger."
    )]
    SchemaTooNew { found: i64, supported: i64 },
    /// A schema migration refused to commit because one of its OWN
    /// post-conditions did not hold (a short row copy, a `foreign_key_check`
    /// violation, a failed `integrity_check`). The migration's transaction
    /// is rolled back before this is returned, so the database is left
    /// exactly as the migration found it and the same binary can simply be
    /// run again once the cause is understood — see
    /// `schema::apply_v21`/docs/09-runbook.md "Schema rollback".
    #[error("ledger schema migration refused to commit: {0}")]
    SchemaMigrationFailed(String),
    /// One Robinhood outbound operation is not in the state a caller
    /// expected. Reported rather than asserted because the expected state
    /// is a fact about a durable row, and a mismatch after a restart is a
    /// real, recoverable condition rather than a programming error.
    #[error("Robinhood transaction {id} is in state {actual:?}, expected {expected:?}")]
    RobinhoodTxWrongState {
        id: i64,
        expected: crate::ledger::robinhood_tx::RobinhoodTxState,
        actual: crate::ledger::robinhood_tx::RobinhoodTxState,
    },
    /// A Robinhood outbound operation was asked to do something that
    /// would contradict what is already durably recorded about it —
    /// a second authorization, a second set of signed bytes, a nonce
    /// belonging to a different submitter.
    #[error("Robinhood transaction {id} refused: {detail}")]
    RobinhoodTxInvalid { id: i64, detail: String },
    #[error("reserve {0:?} has not been initialized")]
    ReserveNotInitialized(ReserveDirection),
    /// An operator asked to write a `bridge_routes` flag for a route that
    /// is not theirs to switch — see
    /// [`crate::routes::Route::is_operator_settable`]. A validated
    /// refusal, never a storage failure.
    #[error(
        "route {route} is not operator-settable in the ledger's bridge_routes state: {detail}"
    )]
    RouteNotOperatorSettable {
        route: &'static str,
        detail: &'static str,
    },
    /// `bridge_routes` has no row for a route the v24 migration seeds one
    /// for, so this ledger has not run that migration (or something
    /// deleted the row). Refused rather than inserted: route state is not
    /// written into a schema this binary has not established.
    #[error(
        "the ledger has no bridge_routes row for {0} — schema migration v24 has not been \
         applied to this database. Start the daemon (or any binary of this version) against it \
         once to migrate, then retry."
    )]
    RouteStateNotInitialized(&'static str),
    /// A route-level admission write was attempted for a route that has
    /// no route-level admission gate — see
    /// [`crate::routes::Route::is_admission_settable`]. A validated
    /// refusal, never a storage failure, so an audited caller records it
    /// and rolls back rather than treating it as a crash.
    #[error("route {route} has no route-level admission gate: {detail}")]
    RouteAdmissionNotSettable {
        route: &'static str,
        detail: &'static str,
    },
    /// `route_admission` has no row for a route the v25 migration seeds
    /// one for, so this ledger has not run that migration (or something
    /// deleted the row). Refused rather than inserted, for the same
    /// reason [`LedgerError::RouteStateNotInitialized`] is: route state
    /// is not written into a schema this binary has not established.
    #[error(
        "the ledger has no route_admission row for {0} — schema migration v25 has not been \
         applied to this database. Start the daemon (or any binary of this version) against it \
         once to migrate, then retry."
    )]
    RouteAdmissionStateNotInitialized(&'static str),
    #[error("bridge request {0} not found")]
    RequestNotFound(i64),
    #[error(
        "accounting invariant violated for {direction:?}: balance {balance} < protected_minimum \
         {protected_minimum} + reserved_liquidity {reserved_liquidity}"
    )]
    InvariantViolated {
        direction: ReserveDirection,
        balance: i64,
        protected_minimum: i64,
        reserved_liquidity: i64,
    },
    #[error("requested {requested} vault UTXOs to reserve, but only {available} of them are still Available (a concurrent reservation won)")]
    VaultUtxoUnavailable { requested: usize, available: usize },
    #[error("a Goldcoin payout already exists for request {0}")]
    PayoutAlreadyExists(i64),
    #[error("no Goldcoin payout record exists for request {0}")]
    PayoutNotFound(i64),
    #[error("vault UTXO {}:{vout} reserved for request {request_id}'s payout is no longer reserved exactly as it was left — needs operator investigation before recovery can proceed", crate::goldcoin::hex::encode(txid))]
    VaultUtxoReservationDrifted {
        request_id: i64,
        txid: [u8; 32],
        vout: u32,
    },
    #[error(
        "cannot finalize request {0}: on-chain completion has not been submitted/confirmed yet"
    )]
    CompletionNotSubmitted(i64),
    #[error("invalid rebalance request: {0}")]
    InvalidRebalanceRequest(String),
    #[error("rebalance request {0} not found")]
    RebalanceNotFound(i64),
    #[error("rebalance request {id} is in state {actual:?}, expected {expected:?}")]
    RebalanceWrongState {
        id: i64,
        expected: RebalanceState,
        actual: RebalanceState,
    },
    #[error("invalid custody transition: {0}")]
    InvalidCustodyTransition(String),
    #[error("custody transition {0} not found")]
    CustodyTransitionNotFound(i64),
    #[error("custody transition {id} is in state {actual:?}, expected {expected:?}")]
    CustodyTransitionWrongState {
        id: i64,
        expected: CustodyTransitionState,
        actual: CustodyTransitionState,
    },
    #[error(
        "custody transition {id} requires {direction:?} to be paused before execution can be recorded"
    )]
    CustodyTransitionRequiresPause {
        id: i64,
        direction: ReserveDirection,
    },
    /// A request whose SOURCE leg is not a Goldcoin L1 deposit was
    /// offered to a Goldcoin-deposit-pipeline function. Distinct from
    /// [`LedgerError::NotAGlcToSolRequest`], which is narrower: that one
    /// guards the Solana-settlement-specific paths, this one guards the
    /// deposit intake shared by `GlcToSol` and `GlcToRhn`.
    #[error(
        "request {id} is {actual_direction:?}, whose source leg is not a Goldcoin deposit — \
         there is no Goldcoin deposit address to assign"
    )]
    NotAGoldcoinSourcedRequest {
        id: i64,
        actual_direction: Direction,
    },
    /// A Solana-settlement-specific path — today only the Goldcoin
    /// refund builder — was offered a request of another direction.
    #[error("request {id} is {actual_direction:?}, not GlcToSol — this path settles the Solana leg only")]
    NotAGlcToSolRequest {
        id: i64,
        actual_direction: Direction,
    },
    /// The request already has a DIFFERENT deposit address assigned.
    /// Never silently overwritten — a request-specific deposit address,
    /// once assigned, may already have been shown to a user or received
    /// funds; changing it out from under either would be a real
    /// accounting hazard, not just a cosmetic inconsistency. Calling
    /// this again with the SAME address is fine (idempotent).
    #[error("request {id} already has deposit address {existing}, cannot reassign to {attempted}")]
    DepositAddressAlreadySet {
        id: i64,
        existing: String,
        attempted: String,
    },
    #[error(
        "no vault UTXO {}:{vout} is known to this ledger",
        crate::goldcoin::hex::encode(txid)
    )]
    VaultUtxoNotFound { txid: [u8; 32], vout: u32 },
    #[error(
        "vault UTXO {}:{vout} is not splittable — state is {state}, not Available",
        crate::goldcoin::hex::encode(txid)
    )]
    VaultUtxoNotSplittable {
        txid: [u8; 32],
        vout: u32,
        state: String,
    },
    #[error(
        "vault UTXO {}:{vout} has already been split",
        crate::goldcoin::hex::encode(txid)
    )]
    VaultUtxoAlreadySplit { txid: [u8; 32], vout: u32 },
    #[error("vault UTXO split #{0} not found")]
    VaultUtxoSplitNotFound(i64),
    #[error(
        "vault UTXO split #{id} is in state {state} — the requested transition does not apply"
    )]
    VaultUtxoSplitNotRecoverable { id: i64, state: String },
    /// [`Ledger::resume_manual_review_sol_to_glc`] was called for a
    /// request that isn't `SolToGlc` — only that direction can land in
    /// `ManualReview` via [`Ledger::fold_sol_deposit`]'s admission/
    /// capacity gate, which is the only thing this command resumes.
    #[error("request {id} is {actual_direction:?}, not SolToGlc — this command only resumes a SolToGlc request parked by fold_sol_deposit")]
    NotASolToGlcRequest {
        id: i64,
        actual_direction: Direction,
    },
    /// [`Ledger::resume_manual_review_rhn_to_glc`] was called for a
    /// request that isn't `RhnToGlc` — the exact counterpart of
    /// [`LedgerError::NotASolToGlcRequest`], naming the other fold path.
    #[error("request {id} is {actual_direction:?}, not RhnToGlc — this command only resumes an RhnToGlc request parked by fold_robinhood_deposit")]
    NotARhnToGlcRequest {
        id: i64,
        actual_direction: Direction,
    },
    /// [`Ledger::resume_manual_review_sol_to_glc`] refuses: the request is
    /// not in a state this command can safely act on (wrong state, an
    /// unrecognized/non-fold `manual_review_note`, a Goldcoin payout or
    /// destination transaction already exists, or the source deposit was
    /// never finalized). Deliberately one variant with a human-readable
    /// detail, mirroring `signing::goldcoin_vault::SigningError::
    /// PayoutNotRecoverable`'s shape — every case here is "no, and here is
    /// exactly why," not a distinct recovery path per cause.
    #[error("request {id} cannot be resumed from ManualReview: {detail}")]
    ManualReviewNotRecoverable { id: i64, detail: String },
    /// [`Ledger::begin_solana_refund`] refuses: one of the refund
    /// eligibility conditions does not hold (wrong direction/state, a
    /// non-whitelisted `manual_review_note`, settlement evidence exists,
    /// the request ever advanced past `ManualReview`, a cross-check
    /// against the on-chain-verified values failed, or the SolanaReserve
    /// capacity/invariant check failed). One variant with a
    /// human-readable detail, same shape and rationale as
    /// [`LedgerError::ManualReviewNotRecoverable`]. Fail-closed: any
    /// ambiguity is a refusal, never a broadened eligibility.
    #[error("request {id} is not eligible for a Solana refund: {detail}")]
    RefundNotEligible { id: i64, detail: String },
    /// A `solana_refunds` row exists for this request, so the request is
    /// permanently ineligible for `resume-manual-review` (any surface:
    /// CLI, admin API, daemon auto-resume) and for any Goldcoin payout —
    /// regardless of what `bridge_requests.state` says (defense in depth
    /// against out-of-band state edits). Returned by
    /// [`Ledger::resume_manual_review_sol_to_glc`] and
    /// [`Ledger::record_goldcoin_payout_built`].
    #[error(
        "request {id} has a refund lifecycle (refund state {refund_state}) — a refunded request \
         can never be resumed or paid out; inspect it with glc-admin refund-list / \
         refund-manual-review"
    )]
    RefundLifecycleExists { id: i64, refund_state: String },
    #[error("no Solana refund record exists for request {0}")]
    RefundNotFound(i64),
    #[error("refund for request {id} is in state {actual}, expected {expected}")]
    RefundWrongState {
        id: i64,
        expected: &'static str,
        actual: String,
    },
    /// A Goldcoin refund lifecycle already exists for this request (or for
    /// its source outpoint). Fail-closed and one-way: once a refund row
    /// exists the request can never be resumed, settled, or refunded a
    /// second time, whatever state that row is in.
    #[error(
        "request {id} already has a Goldcoin refund in state {refund_state}; a request may only \
         ever be refunded once"
    )]
    GoldcoinRefundExists { id: i64, refund_state: String },
    #[error("no Goldcoin refund record exists for request {0}")]
    GoldcoinRefundNotFound(i64),
    #[error("Goldcoin refund for request {id} is in state {actual}, expected {expected}")]
    GoldcoinRefundWrongState {
        id: i64,
        expected: &'static str,
        actual: String,
    },
    /// The request is not eligible for a Goldcoin refund. Carries the
    /// operator-facing reason verbatim — the FIRST refusal encountered,
    /// which is exactly what an execution would hit.
    #[error("request {id} is not refundable on the Goldcoin side: {detail}")]
    GlcRefundNotEligible { id: i64, detail: String },
    /// [`Ledger::resume_manual_review_sol_to_glc`] refuses (no override,
    /// no mutation — the request is left exactly as it was in
    /// `ManualReview`): the mature Goldcoin UTXO pool is still at or below
    /// `utxo_pool_min_available_count`, the same count-based admission
    /// gate [`Ledger::fold_sol_deposit`] applies to a brand-new
    /// obligation, applied here to something already accepted so a resume
    /// can never re-admit demand the mature pool still can't safely
    /// support (docs/09-runbook.md's "UTXO liquidity" section). Retrying
    /// this exact same call once `available_utxo_count` recovers succeeds
    /// normally — this is a transient, self-clearing refusal, not a
    /// terminal one.
    #[error(
        "cannot resume request {request_id}: mature Goldcoin UTXO pool ({available_utxo_count} \
         available) is still at or below the configured floor ({min_available_count}) — \
         utxo_liquidity_low"
    )]
    UtxoLiquidityLow {
        request_id: i64,
        available_utxo_count: i64,
        min_available_count: i64,
    },
    /// [`Ledger::check_utxo_liquidity_for_admission`] refuses (no
    /// override, no mutation): the mature Goldcoin UTXO pool is still at
    /// or below `utxo_pool_min_available_count`, the same count-based
    /// admission gate [`Ledger::fold_sol_deposit`] applies to a brand-new
    /// obligation — reopening admission onto a pool this thin would
    /// immediately re-admit exactly the demand backpressure exists to
    /// hold back. Always includes `own_unconfirmed_change_atomic` so an
    /// operator can see, in the same error, whether the "missing"
    /// liquidity is already known and en route to maturing rather than
    /// genuinely gone. Never produced for `SolanaReserve`, which has no
    /// UTXO-pool concept — Solana admission is completely unaffected.
    #[error(
        "cannot open admission for {direction:?}: mature Goldcoin UTXO pool \
         ({available_utxo_count} available) is still at or below the configured floor \
         ({min_available_count}) — utxo_liquidity_low ({own_unconfirmed_change_atomic} atomic \
         units are known to be this service's own unconfirmed payout change, not yet spendable)"
    )]
    UtxoLiquidityLowForAdmission {
        direction: ReserveDirection,
        available_utxo_count: i64,
        min_available_count: i64,
        own_unconfirmed_change_atomic: u64,
    },
    /// [`Ledger::resume_manual_review_sol_to_glc`] refuses (no override,
    /// no mutation): reserving this request's capacity now would leave
    /// confirmed unreserved Goldcoin headroom below the configured
    /// admission safety buffer (docs/09-runbook.md's "Confirmed-liquidity
    /// admission safety buffer" section). Exactly the same reasoning as
    /// [`LedgerError::UtxoLiquidityLow`] one variant up: a resume
    /// re-admits real demand onto the reserve precisely as a fresh fold
    /// would, so it must never bypass a floor a fresh fold would have
    /// been held back by. Transient and self-clearing — retrying the same
    /// call once headroom recovers succeeds normally, and the refund path
    /// (`glc-admin refund-manual-review`) remains available for a deposit
    /// that will genuinely never be paid out.
    ///
    /// `headroom` is CONFIRMED headroom only (`total_reserve_balance -
    /// protected_minimum - reserved_liquidity`); still-immature payout
    /// change is deliberately not counted (see
    /// [`Ledger::confirmed_admission_headroom`]).
    #[error(
        "cannot resume request {request_id}: reserving {net_destination_atomic} would leave \
         confirmed unreserved Goldcoin headroom ({headroom}) below the admission safety buffer \
         ({buffer_atomic}) — liquidity_buffer_low"
    )]
    AdmissionLiquidityBufferLow {
        request_id: i64,
        headroom: i64,
        net_destination_atomic: i64,
        buffer_atomic: i64,
    },
    /// [`Ledger::check_liquidity_buffer_for_admission`] refuses (no
    /// override, no mutation): confirmed unreserved Goldcoin headroom has
    /// not yet recovered to the reopen threshold, so the automatic
    /// confirmed-liquidity gate is still closed. Reopening operator
    /// admission on top of a still-closed automatic gate would be
    /// ineffective AND misleading — every new fold would still park —
    /// so the operator command refuses and says why instead.
    ///
    /// Includes `own_unconfirmed_change_atomic` for the same reason
    /// [`LedgerError::UtxoLiquidityLowForAdmission`] does: an operator can
    /// see in one message whether the headroom shortfall is already
    /// explained by this service's own maturing change (recovery in
    /// flight) rather than genuinely absent. That figure is reported
    /// ONLY — it is never added to `headroom`, which stays confirmed-only
    /// by design.
    #[error(
        "cannot open admission for {direction:?}: confirmed unreserved Goldcoin headroom \
         ({headroom}) has not recovered to the reopen threshold ({reopen_atomic}) — \
         liquidity_admission_closed ({own_unconfirmed_change_atomic} atomic units are known to \
         be this service's own unconfirmed payout change, not yet spendable and deliberately \
         not counted as headroom)"
    )]
    LiquidityAdmissionClosedForAdmission {
        direction: ReserveDirection,
        headroom: i64,
        reopen_atomic: i64,
        own_unconfirmed_change_atomic: u64,
    },
    /// [`Ledger::set_admission_liquidity_thresholds`] refuses a threshold
    /// pair that cannot express hysteresis: the reopen threshold must be
    /// greater than or equal to the close threshold, or the gate could
    /// close and reopen on the same headroom — the exact flapping the
    /// buffer exists to prevent.
    #[error(
        "invalid admission liquidity thresholds for {direction:?}: reopen ({reopen_atomic}) \
         must be >= buffer ({buffer_atomic})"
    )]
    InvalidAdmissionThresholds {
        direction: ReserveDirection,
        buffer_atomic: u64,
        reopen_atomic: u64,
    },
    /// [`Ledger::set_rapid_burst_policy`] refuses a policy that cannot
    /// be evaluated (non-positive window, a zero maximum, or a negative
    /// minimum review hold) — the config layer validates the same
    /// bounds, so this is a second line, not the first.
    #[error("invalid rapid-burst policy: {0}")]
    InvalidRapidBurstPolicy(String),
    #[error(
        "no unmatched Goldcoin deposit {}:{vout} is known to this ledger",
        crate::goldcoin::hex::encode(txid)
    )]
    UnmatchedDepositNotFound { txid: [u8; 32], vout: u32 },
    /// [`Ledger::reconcile_unmatched_goldcoin_deposit`] refuses: no
    /// `Broadcast` `vault_utxo_splits` transaction with this txid exists,
    /// or this exact `(vout, amount_atomic)` is not one of its expected
    /// outputs. No override — reconciling anything else would mean
    /// marking a genuinely unexplained deposit as explained.
    #[error(
        "unmatched Goldcoin deposit {}:{vout} does not exactly match any known vault split output",
        crate::goldcoin::hex::encode(txid)
    )]
    UnmatchedDepositNotAKnownSplitOutput { txid: [u8; 32], vout: u32 },
    /// A resume refuses (no override, no mutation): the request's
    /// `role` wallet on `chain` still backs ANOTHER qualifying request
    /// created inside the rolling 24-hour window (`ledger::wallet_window`)
    /// — checked unconditionally on every resume attempt, regardless of
    /// the request's original `manual_review_note`, so a manual operator
    /// resume can never bypass a window. Only a STRICT PREDECESSOR (an
    /// earlier row, by `(created_at, id)`) sharing the wallet can ever
    /// be the blocker named here — a later sibling can never block an
    /// earlier one, which is what keeps oldest-first draining true for a
    /// busy wallet. `retry_after` is the unix second at which the
    /// blocking request ages out of the window; retrying this exact call
    /// at or after that time succeeds normally, the same self-clearing
    /// shape as `UtxoLiquidityLow`. One variant for both roles and all
    /// three chains, carrying which so the message can never name the
    /// wrong chain's wallet.
    #[error(
        "cannot resume request {request_id}: {} wallet {} on {} already backs a bridge request \
         inside the rolling 24-hour window, retry after {retry_after} — {}",
        role.as_str(),
        crate::ledger::wallet_window::render_wallet(*chain, wallet),
        chain.as_str(),
        role.limit_reason()
    )]
    WalletWindowActive {
        request_id: i64,
        role: WalletRole,
        chain: crate::routes::Chain,
        wallet: Vec<u8>,
        retry_after: i64,
    },
    /// Two `DepositCreated` events claim one durable Robinhood identity
    /// (`source_chain` + `source_contract` + `source_obligation_index`)
    /// and disagree about what happened. Never reconciled automatically:
    /// the scan range's transaction is rolled back, so the cursor does
    /// not advance and nothing is half-written, and the indexer halts for
    /// a human (`crate::robinhood::indexer`). Boxed to keep `LedgerError`
    /// small — this is the only variant carrying a multi-field payload.
    #[error(
        "conflicting Robinhood deposit observations for obligation {}: field `{}` was recorded \
         as {} but the chain now reports {} — refusing to overwrite an observation",
        .0.obligation_index, .0.field, .0.stored, .0.observed
    )]
    RobinhoodObservationConflict(Box<RobinhoodObservationConflict>),
    /// A rollback would have orphaned a block holding an observation
    /// already promoted to `Final`. Refused at the last possible moment —
    /// the caller is expected to have detected this with
    /// [`Ledger::robinhood_final_observations_above`] and halted before
    /// getting here.
    #[error(
        "Robinhood reorg to block {fork_block} would orphan {finalized_above} observation(s) \
         already recorded as final — this is a post-finality reorg, not a routine one, and is \
         never rolled back automatically"
    )]
    RobinhoodPostFinalityReorg {
        fork_block: u64,
        finalized_above: i64,
    },
    /// A value that must round-trip through SQLite's signed 64-bit
    /// integer did not fit. Only reachable from a malformed or hostile
    /// RPC response that the decoder should already have refused —
    /// storing a wrapped or negative number instead is never an option.
    #[error("Robinhood {field} value {value} does not fit the ledger's signed 64-bit column")]
    RobinhoodValueOutOfRange { field: &'static str, value: String },
    /// A `robinhood_indexer_state` row holds a halt reason this binary
    /// does not know. Refused rather than treated as "not halted".
    #[error("Robinhood indexer state is malformed: {0}")]
    RobinhoodStateMalformed(String),
}

pub struct Ledger {
    conn: Connection,
}

/// A write transaction for one Ledger mutation. Standalone (the only
/// case before the admin control plane existed) this is EXACTLY the old
/// `BEGIN IMMEDIATE` transaction — same statement, same write-lock
/// acquisition, same rollback-on-drop. When an admin-action scope is
/// already open on the connection ([`Ledger::begin_admin_action`]) it is
/// a SAVEPOINT instead, so the mutation nests inside the scope and
/// commits or rolls back atomically WITH its audit row rather than
/// failing on a nested `BEGIN`.
enum WriteTx<'conn> {
    Transaction(rusqlite::Transaction<'conn>),
    Savepoint(rusqlite::Savepoint<'conn>),
}

impl<'conn> WriteTx<'conn> {
    fn commit(self) -> rusqlite::Result<()> {
        match self {
            WriteTx::Transaction(tx) => tx.commit(),
            WriteTx::Savepoint(sp) => sp.commit(),
        }
    }

    fn rollback(self) -> rusqlite::Result<()> {
        match self {
            WriteTx::Transaction(tx) => tx.rollback(),
            WriteTx::Savepoint(mut sp) => sp.rollback(),
        }
    }
}

impl std::ops::Deref for WriteTx<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self {
            WriteTx::Transaction(tx) => tx,
            WriteTx::Savepoint(sp) => sp,
        }
    }
}

/// Begins a [`WriteTx`] on `conn` — a free function over the connection
/// (not a `Ledger` method) so call sites keep the same field-level borrow
/// shape as the `self.conn.transaction_with_behavior(...)` calls it
/// replaced.
fn write_tx(conn: &mut Connection) -> Result<WriteTx<'_>, LedgerError> {
    if conn.is_autocommit() {
        Ok(WriteTx::Transaction(conn.transaction_with_behavior(
            rusqlite::TransactionBehavior::Immediate,
        )?))
    } else {
        Ok(WriteTx::Savepoint(conn.savepoint()?))
    }
}

/// `(from_state, to_state, at, reason)` — one row of a request's audit
/// trail, per [`Ledger::state_log`].
pub type StateLogEntry = (Option<RequestState>, RequestState, i64, Option<String>);

/// `(from_state, to_state, at, reason, actor)` — one row of a rebalance
/// request's audit trail, per [`Ledger::rebalance_state_log`].
pub type RebalanceStateLogEntry = (
    Option<RebalanceState>,
    RebalanceState,
    i64,
    Option<String>,
    String,
);

/// `(from_state, to_state, at, reason, actor)` — one row of a custody
/// transition's audit trail, per [`Ledger::custody_transition_state_log`].
pub type CustodyTransitionStateLogEntry = (
    Option<CustodyTransitionState>,
    CustodyTransitionState,
    i64,
    Option<String>,
    String,
);

/// Outcome of [`Ledger::create_request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateRequestOutcome {
    /// Capacity reserved; request created in `AwaitingDeposit`.
    Reserved { request_id: i64 },
    /// Never accept a transfer that cannot be fulfilled (docs/05): no row
    /// is created, no capacity is touched.
    InsufficientLiquidity { available_capacity: i64 },
    /// The destination reserve (or the bridge globally) is paused.
    Paused,
    /// The destination wallet — or the source wallet the caller declared
    /// — is still inside its rolling 24-hour window
    /// (`ledger::wallet_window`): no row is created, no capacity is
    /// touched. Carries both legs' reopen instants so the caller can
    /// tell the user everything they must wait for.
    WalletLimited { eligibility: RouteWalletEligibility },
}

/// One `bridge_routes` row, as recorded — never resolved against a
/// default. See [`Ledger::route_ledger_rows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteLedgerRow {
    pub route: crate::routes::Route,
    pub enabled: bool,
    /// Operator context for a disabled route, last-write-wins. The
    /// authoritative history is `admin_audit_log`.
    pub disabled_reason: Option<String>,
    /// Unix seconds, written by SQL's own clock so the ledger keeps one
    /// clock rather than gaining a second.
    pub updated_at: i64,
}

/// Everything `bridge_routes` currently holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteLedgerState {
    /// One row per recognised route, in `Route::ALL` order.
    pub rows: Vec<RouteLedgerRow>,
    /// `route_id` values this build does not model. Reported rather than
    /// dropped: an unrecognised row is a hand-written one or a downgrade.
    pub unknown_route_ids: Vec<String>,
}

impl RouteLedgerState {
    /// The recorded row for one route, if the table holds one.
    pub fn row(&self, route: crate::routes::Route) -> Option<&RouteLedgerRow> {
        self.rows.iter().find(|r| r.route == route)
    }
}

/// One `route_admission` row, as recorded — never resolved against a
/// default. See [`Ledger::route_admission_rows`].
///
/// Deliberately a separate type from [`RouteLedgerRow`], for the same
/// reason the tables are separate: `bridge_routes.enabled` answers "is
/// this route switched on in this deployment"
/// ([`crate::routes::RouteGate`]) and this answers "would this route
/// accept a newly observed inbound deposit right now"
/// ([`InboundAdmissionGates`]). A route must pass both, they have
/// different settable sets, and one struct carrying both fields would
/// invite reading either as the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteAdmissionRow {
    pub route: crate::routes::Route,
    /// `true` means this route parks newly observed inbound deposits
    /// into `ManualReview` even while the reserve itself would admit
    /// them. Never automatic — only an operator ever sets this, and
    /// nothing ever clears it on its own.
    pub admission_closed: bool,
    /// Operator context for a closed route, last-write-wins. The
    /// authoritative history is `admin_audit_log`.
    pub admission_closed_reason: Option<String>,
    /// Unix seconds, written by SQL's own clock so the ledger keeps one
    /// clock rather than gaining a second.
    pub updated_at: i64,
}

/// Everything `route_admission` currently holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteAdmissionState {
    /// One row per recognised route, in `Route::ADMISSION_SETTABLE`
    /// order.
    pub rows: Vec<RouteAdmissionRow>,
    /// `route_id` values this build does not model, or models but does
    /// not consider admission-settable. Reported rather than dropped:
    /// the table's own CHECK makes such a row impossible for this
    /// binary to write, so one that exists is hand-written or a
    /// downgrade artefact and is worth an operator's attention.
    pub unknown_route_ids: Vec<String>,
}

impl RouteAdmissionState {
    /// The recorded row for one route, if the table holds one.
    pub fn row(&self, route: crate::routes::Route) -> Option<&RouteAdmissionRow> {
        self.rows.iter().find(|r| r.route == route)
    }
}

/// See [`Ledger::get_goldcoin_payout`]. `state` is the raw `goldcoin_payouts.state`
/// text value (`'Built'|'Signed'|'Broadcast'|'Confirmed'|'Completed'`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldcoinPayoutSnapshot {
    pub payout_atomic: u64,
    pub txid: Option<[u8; 32]>,
    pub state: String,
    pub confirmations: i64,
    pub mined_height: Option<i64>,
    pub onchain_completion_signature: Option<[u8; 64]>,
    /// When `onchain_completion_signature` was last (re-)submitted — what
    /// the orchestrator's completion-confirmation tick uses to decide
    /// that a still-unobserved submission is old enough to have
    /// demonstrably expired and must be re-sent.
    pub onchain_completion_submitted_at: Option<i64>,
}

/// See [`Ledger::get_goldcoin_payout_full`] — every persisted fact about
/// an existing payout that [`crate::goldcoin::payout_recovery`] needs to
/// independently reconstruct and re-verify its plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldcoinPayoutFull {
    pub commitment_hash: [u8; 32],
    pub payout_atomic: u64,
    /// Sum of `change_outputs` — kept for every existing consumer that
    /// only needs the total (e.g. `pending_destination_settlement_amount`'s
    /// SQL, unchanged since this migration).
    pub change_atomic: u64,
    /// The deterministic change FAN-OUT itself, in construction order
    /// (`goldcoin::coin::finalize_fanout`) — reconstructed from
    /// `goldcoin_payout_change_outputs` when present, or synthesized as a
    /// single legacy output equal to `change_atomic` for a payout built
    /// before this column existed (never backfilled; see
    /// `schema::apply_v12`). Empty exactly when `change_atomic == 0`.
    pub change_outputs: Vec<u64>,
    pub fee_atomic: u64,
    pub dest_p2pkh_hash: [u8; 20],
    pub unsigned_tx_hex: Option<String>,
    pub signed_tx_hex: Option<String>,
    /// Raw `goldcoin_payouts.state` text value
    /// (`'Built'|'Signed'|'Broadcast'|'Confirmed'|'Completed'`).
    pub state: String,
}

/// The Goldcoin vault's UTXO-pool health, distinguishing what a naive
/// "reserve balance dropped" reading cannot (docs/09-runbook.md's "UTXO
/// liquidity" section): (A) actual reserve loss is neither of these
/// figures — it is whatever a `reconcile` call classifies as an
/// unexplained residual drop; (B) `own_unconfirmed_change_atomic`/
/// `unconfirmed_change_utxo_count` is reserve value KNOWN to be
/// temporarily locked in this service's own broadcast-but-immature payout
/// change, not missing; (C) `mature_available_atomic`/
/// `available_utxo_count` is the real, currently spendable liquidity coin
/// selection can actually draw from right now. See [`Ledger::
/// utxo_pool_health`].
/// One 0-conf-policy candidate with the chain-budget figures — see
/// [`Ledger::zero_conf_change_vault_utxos_with_depth`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZeroConfChangeCandidate {
    pub utxo: crate::goldcoin::coin::VaultUtxo,
    /// Recorded at broadcast; an upper bound on this output's unconfirmed
    /// own-payout ancestor count (never shrinks before a confirmation).
    pub unconfirmed_ancestor_depth: u32,
    pub confirmations: i64,
}

/// The confirmed-liquidity admission gate's state after one evaluation
/// — see [`Ledger::evaluate_liquidity_admission_gate`] and
/// docs/09-runbook.md's "Confirmed-liquidity admission safety buffer".
///
/// Carries the inputs alongside the verdict deliberately: every operator
/// surface that reports "admission is closed on liquidity" can then also
/// say by how much and against which threshold, from the one evaluation
/// that actually decided it, rather than re-reading the row and risking a
/// figure that no longer matches the verdict shown next to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiquidityAdmissionGate {
    pub direction: ReserveDirection,
    /// `true` when NEW SolToGlc obligations are being held back by
    /// confirmed-liquidity backpressure. Independent of, and never
    /// merged with, the operator-only `admission_closed` flag.
    pub closed: bool,
    /// Whether THIS evaluation changed the state (as opposed to
    /// confirming it). The held band between the two thresholds means
    /// most evaluations are expected to report `false` here.
    pub transitioned: bool,
    /// Confirmed unreserved headroom at evaluation time — immature payout
    /// change deliberately excluded (see
    /// [`Ledger::confirmed_admission_headroom`]).
    pub headroom: i64,
    pub buffer_atomic: i64,
    pub reopen_atomic: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UtxoPoolHealth {
    pub mature_available_atomic: u64,
    pub own_unconfirmed_change_atomic: u64,
    pub available_utxo_count: u32,
    pub unconfirmed_change_utxo_count: u32,
    /// Authoritative payout change below the confirmed threshold and not
    /// on a parent-validation hold — the 0-conf-spendability policy's
    /// candidate pool (depth cap applied at selection, not here). Shown
    /// SEPARATELY from `mature_available_atomic` so an operator never
    /// mistakes it for confirmed reserve liquidity.
    pub zero_conf_change_candidate_atomic: u64,
    pub zero_conf_change_candidate_count: u32,
    /// Change outputs currently excluded because their parent payout is
    /// not known/accepted by the configured node (see
    /// `Ledger::set_zero_conf_hold`). Nonzero deserves operator
    /// attention: a parent payout may have been evicted or conflicted.
    pub zero_conf_change_held_count: u32,
}

/// A single `vault_utxos` row's live state, as needed by
/// [`crate::goldcoin::split`]'s independent re-derivation — see
/// [`Ledger::get_vault_utxo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultUtxoRow {
    pub amount_atomic: u64,
    pub script_pubkey_hex: String,
    /// Raw `vault_utxos.state` text value
    /// (`'Available'|'Reserved'|'Spent'|'Unconfirmed'`).
    pub state: String,
}

/// One not-yet-`Broadcast` `vault_utxo_splits` row, as returned by
/// [`Ledger::pending_vault_utxo_splits`] — just enough to locate the full
/// snapshot ([`Ledger::get_vault_utxo_split`]) and dispatch the right
/// resume path per `state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingVaultUtxoSplit {
    pub id: i64,
    pub source_txid: [u8; 32],
    pub source_vout: u32,
    /// `'Built'` or `'Signed'`.
    pub state: String,
}

/// One `Broadcast` `vault_utxo_splits` row, as returned by
/// [`Ledger::broadcast_vault_utxo_splits`] — what lifecycle maintenance
/// needs to confirm, re-broadcast, or abandon it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnconfirmedBroadcastSplit {
    pub id: i64,
    pub txid: [u8; 32],
    /// Always present for a `Broadcast` row (`record_vault_utxo_split_
    /// signed` sets it before `Broadcast` is reachable) — `Option` only
    /// because the column is nullable in earlier states.
    pub signed_tx_hex: Option<String>,
}

/// See [`Ledger::get_vault_utxo_split`] — everything an operator or a
/// re-run of `split-vault-utxo` needs to know about a previously attempted
/// split of a given source outpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultUtxoSplitSnapshot {
    pub id: i64,
    pub source_amount_atomic: u64,
    pub chunk_count: i64,
    pub chunk_target_atomic: u64,
    pub fee_atomic: u64,
    pub unsigned_tx_hex: String,
    pub signed_tx_hex: Option<String>,
    pub txid: Option<[u8; 32]>,
    /// Raw `vault_utxo_splits.state` text value
    /// (`'Built'|'Signed'|'Broadcast'`).
    pub state: String,
}

/// See [`Ledger::get_broadcast_vault_utxo_split`] — the already-persisted
/// figures needed to reproduce a `Broadcast` split's exact output list
/// (`crate::goldcoin::split::matches_expected_split_output`) purely from
/// its broadcast `txid`, without touching `unsigned_tx_hex` (this crate's
/// `Transaction` type has no deserializer, deliberately — see
/// `goldcoin::tx` module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BroadcastVaultUtxoSplit {
    pub source_amount_atomic: u64,
    pub fee_atomic: u64,
    pub chunk_count: i64,
}

/// Outcome of [`Ledger::reconcile_unmatched_goldcoin_deposit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileUnmatchedDepositOutcome {
    Reconciled,
    /// Already reconciled by a prior call — a safe, non-mutating no-op.
    AlreadyReconciled,
}

/// Outcome of [`Ledger::record_glc_deposit_observed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlcObservationOutcome {
    Recorded,
    /// Already recorded for this exact request+txid+vout (restart replay).
    AlreadyRecorded,
    /// No `AwaitingDeposit` request exists with this id/direction — the
    /// vault payment is real but unmatched. Callers should log it to
    /// [`Ledger::record_unmatched_goldcoin_deposit`] for audit rather than
    /// discard it (never silently ignore a real vault payment).
    NoMatchingRequest,
    /// Observed amount does not equal the request's reserved amount — the
    /// deposit is recorded but routed to `ManualReview` rather than
    /// silently accepted (constraint 6/10: never let an observed amount
    /// override what capacity was actually reserved for).
    AmountMismatch {
        expected: u64,
        observed: u64,
    },
    /// The deposit arrived after this request's reservation had already
    /// `Expired`, but capacity was still available: a fresh reservation was
    /// auto-recreated on the same request and the deposit was recorded
    /// against it, continuing the flow normally (docs/04-state-machines.md
    /// "Open design item: late deposits after expiry").
    LateDepositRecreated,
    /// The deposit arrived after this request's reservation had already
    /// `Expired`, and capacity is no longer available to re-reserve. The
    /// deposit is real and irreversible, so this is routed to
    /// `ManualReview` for a compensating action rather than dropped.
    LateDepositNoCapacity,
    /// The deposit is real and matched its request, but the wallet that
    /// funded it — or the request's destination wallet — still backs
    /// ANOTHER request created inside the rolling 24-hour window
    /// (`ledger::wallet_window`). Recorded, with its outpoint and amount
    /// witness, and parked in `ManualReview` under `reason`
    /// (`wallet_source_24h_limit` / `wallet_destination_24h_limit`),
    /// refundable — never advanced to `Confirming`, so no payout can
    /// follow. `retry_after` is when the blocking window reopens.
    WalletLimited {
        reason: &'static str,
        retry_after: i64,
    },
    /// The deposit is real and matched its request, but it tripped the
    /// configured rapid-burst rule (`ledger::rapid_burst`, schema v30).
    /// Recorded, with its outpoint and amount witness, and parked in
    /// `ManualReview` as a RAPID-BURST HOLD: never advanced to
    /// `Confirming`, never auto-resumed, never auto-refunded; only an
    /// explicit operator `process`/`refund` decision — normally not
    /// before `review_after` — ends it.
    RapidBurstHeld {
        rule: RapidBurstRule,
        review_after: i64,
    },
}

/// Outcome of [`Ledger::approve_rebalance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebalanceApprovalOutcome {
    /// This approval was recorded but the threshold has not been reached
    /// yet.
    Recorded { approvals: u32, required: u32 },
    /// This approval was the one that reached `required_approvals`; the
    /// request has moved to `RebalanceState::Approved`.
    ThresholdReached,
}

/// Outcome of [`Ledger::approve_custody_transition`]. Structurally
/// identical to [`RebalanceApprovalOutcome`]; kept as a distinct type so
/// each state machine's approval outcome is self-describing at call
/// sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyApprovalOutcome {
    /// This approval was recorded but the threshold has not been reached
    /// yet.
    Recorded { approvals: u32, required: u32 },
    /// This approval was the one that reached `required_approvals`; the
    /// transition has moved to `CustodyTransitionState::Approved`.
    ThresholdReached,
}

/// Outcome of [`Ledger::fold_sol_deposit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolFoldOutcome {
    /// Capacity was available; a request was created directly in
    /// `SourceFinalized` (Solana finality is a single instant at the
    /// commitment level, unlike Goldcoin's confirmation-depth ramp — see
    /// module docs on the asymmetry).
    FoldedFinalized { request_id: i64 },
    /// Already folded for this obligation index (restart replay).
    AlreadyFolded { request_id: i64 },
    /// No pre-existing reservation is possible for this direction (the
    /// on-chain `deposit_to_reserve` instruction has no reservation-
    /// correlation parameter — see module docs) and capacity was NOT
    /// available at fold time. The deposit is real and irreversible on
    /// Solana; it is recorded in `ManualReview`, never dropped.
    FoldedManualReview { request_id: i64 },
}

/// Outcome of [`Ledger::resume_manual_review_sol_to_glc`] and
/// [`Ledger::resume_manual_review_rhn_to_glc`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeManualReviewOutcome {
    /// The request moved `ManualReview -> SourceFinalized` and its
    /// capacity was reserved.
    Resumed,
    /// The request was already past `ManualReview` (a prior call to this
    /// same command already resumed it) — a safe, non-mutating no-op,
    /// safe to call again.
    AlreadyResumed { state: RequestState },
}

/// Outcome of [`Ledger::dry_run_resume_manual_review`] — what an
/// execution against the CURRENT live state would do, determined by
/// actually running it and rolling back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeDryRunOutcome {
    /// Every check passes: an execute would re-admit the request to
    /// `SourceFinalized` and reserve its capacity.
    WouldResume,
    /// Already past `ManualReview` via a prior resume — an execute would
    /// be a safe no-op.
    AlreadyResumed { state: RequestState },
    /// An execute would refuse, with this operator-facing reason (the
    /// first refusal it would hit, verbatim).
    WouldRefuse { reason: String },
}

/// The durable state of a Goldcoin-side refund
/// ([`Ledger::begin_goldcoin_refund`]). Distinct from the REQUEST's
/// state: the request walks `ManualReview -> RefundPending ->
/// RefundBroadcast -> Refunded` while this row records which concrete
/// transaction artifact exists, which is what crash recovery keys off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoldcoinRefundState {
    /// Inputs reserved and the unsigned transaction persisted. Nothing
    /// has been signed; no value can have moved.
    Built,
    /// A fully signed transaction exists. From here a replacement is
    /// NEVER built: the same bytes are re-broadcast instead, because a
    /// signed transaction may already have reached a mempool.
    Signed,
    /// Handed to the node and accepted (or already known to it). Only
    /// confirmations advance from here.
    Broadcast,
    /// Terminal: confirmed to the configured depth.
    Refunded,
}

impl GoldcoinRefundState {
    pub fn as_str(self) -> &'static str {
        match self {
            GoldcoinRefundState::Built => "Built",
            GoldcoinRefundState::Signed => "Signed",
            GoldcoinRefundState::Broadcast => "Broadcast",
            GoldcoinRefundState::Refunded => "Refunded",
        }
    }
}

impl rusqlite::types::FromSql for GoldcoinRefundState {
    fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        match v.as_str()? {
            "Built" => Ok(GoldcoinRefundState::Built),
            "Signed" => Ok(GoldcoinRefundState::Signed),
            "Broadcast" => Ok(GoldcoinRefundState::Broadcast),
            "Refunded" => Ok(GoldcoinRefundState::Refunded),
            other => Err(rusqlite::types::FromSqlError::Other(
                format!("unknown goldcoin refund state {other:?}").into(),
            )),
        }
    }
}

impl rusqlite::ToSql for GoldcoinRefundState {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
    }
}

/// A persisted Goldcoin refund row, as the CLI listing and the crash
/// recovery path read it.
#[derive(Debug, Clone)]
pub struct GoldcoinRefundRow {
    pub request_id: i64,
    pub source_txid: [u8; 32],
    pub source_vout: u32,
    pub observed_amount_atomic: u64,
    pub source_input_txid: [u8; 32],
    pub source_input_vout: u32,
    pub refund_dest_p2pkh_hash: [u8; 20],
    pub refund_dest_address: String,
    pub refund_amount_atomic: u64,
    pub fee_atomic: u64,
    pub unsigned_tx_hex: Option<String>,
    pub signed_tx_hex: Option<String>,
    pub txid: Option<[u8; 32]>,
    pub confirmations: i64,
    pub state: GoldcoinRefundState,
    pub manual_review_reason: String,
    pub note: String,
    pub created_by: String,
    pub built_at: i64,
    pub signed_at: Option<i64>,
    pub broadcast_at: Option<i64>,
    pub refunded_at: Option<i64>,
    pub reservation_released: bool,
}

/// The `bridge_requests` columns both Goldcoin-refund eligibility paths
/// read. Aliased so the read-only check and the transactional one cannot
/// drift on which columns they consider.
type GlcRefundEligibilityRow = (
    Direction,
    RequestState,
    Option<String>,
    Option<Vec<u8>>,
    Option<u32>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
);

/// Read-only, per-condition result of the DATABASE-side Goldcoin refund
/// eligibility checks ([`Ledger::glc_refund_db_checks`]). One field per
/// check so the dry run can print each individually as PASS/FAIL rather
/// than stopping at the first failure — an operator needs the whole
/// picture, not just the first thing that went wrong.
///
/// These are the DATABASE half only. The chain half (the independent
/// Goldcoin source trace and the independent Solana no-release check)
/// lives in `crate::goldcoin::refund`, and BOTH must pass.
#[derive(Debug, Clone)]
pub struct GlcRefundDbChecks {
    pub request_found: bool,
    /// The request's direction, as stored. Reported so the operator
    /// surface can name which route-specific proof was applied instead of
    /// inferring it.
    pub direction: Option<Direction>,
    /// The GATE: only a request whose SOURCE leg is a Goldcoin L1 deposit
    /// has a Goldcoin principal to return at all
    /// ([`Direction::source_is_goldcoin`]). A `SolToGlc`/`RhnToGlc`
    /// request is refunded on its own source chain, not here.
    pub direction_is_goldcoin_sourced: bool,
    /// Whether the SOLANA-shaped settlement proof
    /// (`no_destination_txid` + `no_settlement_claim`, plus the on-chain
    /// `DepositClaim` witness in `goldcoin::refund`) is the one that
    /// governs this request. True for `GlcToSol` and nothing else.
    pub direction_is_glc_to_sol: bool,
    pub state_is_manual_review: bool,
    pub reason_is_refundable: bool,
    pub has_source_outpoint: bool,
    /// No `goldcoin_payouts` row: for a GlcToSol request this would be
    /// anomalous entirely, and it is checked rather than assumed.
    pub no_goldcoin_payout: bool,
    /// No Solana-side settlement recorded in the database. The
    /// authoritative check is the on-chain one in `goldcoin::refund`;
    /// this is the cheap DB-side half of the same question.
    pub no_destination_txid: bool,
    pub no_settlement_claim: bool,
    /// The ROBINHOOD-shaped proof, and the one that governs `GlcToRhn`:
    /// no durable Robinhood payout state names this request.
    ///
    /// Not a restatement of the Solana columns above — a Robinhood payout
    /// writes neither of them, which is exactly why it needs its own
    /// proof. See [`RobinhoodPayoutEvidence`] for what is examined and
    /// why the mere existence of an operation row is disqualifying.
    ///
    /// Evaluated for both Goldcoin-sourced directions: for `GlcToRhn` it
    /// IS the proof, for `GlcToSol` it is a corruption tripwire that can
    /// only ever fire on a contradiction.
    pub no_robinhood_payout_started: bool,
    /// Each durable fact behind a `false` above, so the operator surface
    /// can list every one rather than only the first.
    pub robinhood_payout_evidence: Vec<RobinhoodPayoutEvidence>,
    pub no_existing_refund: bool,
    /// The DURABLE amount witness (`bridge_requests.observed_amount_atomic`,
    /// schema v20): what the indexer independently decoded when it parked
    /// this request, written in the same transaction as the source
    /// outpoint. `None` means the request predates the witness — a
    /// LEGACY row, handled by an explicitly reported reduced-assurance
    /// mode, never by parsing the note.
    ///
    /// Deliberately NOT `vault_utxos`: that table is `listunspent`-derived
    /// spendable inventory for addresses the NODE's wallet owns, and this
    /// service imports no per-request derived P2SH into the node, so a
    /// request-specific deposit can never appear there. Requiring it was
    /// unsatisfiable by construction, not merely unmet.
    pub durable_observed_amount_atomic: Option<u64>,
    /// `bridge_requests.deposit_script_pubkey_hex` as stored, if any.
    /// Compared against the INDEPENDENTLY DERIVED script as a consistency
    /// witness — never used as the authority for where money goes.
    pub stored_deposit_script_pubkey_hex: Option<String>,
    /// First refusal encountered, verbatim. `None` when every check
    /// passed.
    pub refusal: Option<String>,
}

impl GlcRefundDbChecks {
    pub fn all_passed(&self) -> bool {
        self.refusal.is_none()
    }
}

/// Read-only, per-condition result of the DATABASE-side refund
/// eligibility checks ([`Ledger::solana_refund_db_checks`]) — one field
/// per check so the CLI dry run can print every check individually
/// instead of only the first failure. [`Ledger::begin_solana_refund`]
/// enforces the same conditions through the same shared evaluation (one
/// implementation — the printable view and the enforced gate cannot
/// drift), plus the chain-side cross-checks and the capacity check.
#[derive(Debug, Clone)]
pub struct SolanaRefundDbChecks {
    pub direction: Direction,
    pub direction_ok: bool,
    pub state: RequestState,
    pub state_is_manual_review: bool,
    pub manual_review_reason: Option<String>,
    pub reason_whitelisted: bool,
    pub source_finalized: bool,
    pub has_obligation_index: bool,
    pub has_requester: bool,
    pub no_destination_txid: bool,
    pub not_settled: bool,
    pub no_goldcoin_payout: bool,
    /// No `bridge_request_state_log` row ever moved this request INTO any
    /// state at or past `SourceFinalized` — the per-request PROOF that no
    /// Goldcoin-side `reserved_liquidity`/`pending_obligations` increment
    /// was ever applied (fold-time parks skip the increment; the only
    /// path that applies it afterwards, resume, logs exactly such a
    /// transition), so a refund has nothing to release and must not
    /// subtract blindly.
    pub never_advanced_past_manual_review: bool,
    /// `Some(detail)` when the row is HELD (schema v30) and no `refund`
    /// operator decision has been recorded — [`Ledger::refund_hold_blocker_in`].
    /// `None` for an unheld row, or a held row whose decision is `refund`.
    pub hold_blocker: Option<String>,
    /// `Some` if a `solana_refunds` row already exists (its state) — the
    /// caller then resumes THAT lifecycle rather than beginning a new
    /// one.
    pub existing_refund: Option<SolanaRefundState>,
}

impl SolanaRefundDbChecks {
    /// The first failing precondition for BEGINNING a new refund
    /// lifecycle, as an operator-readable detail — `None` when every
    /// database-side check passes. An existing refund row is reported as
    /// a failure here because `begin` must not run then; the caller
    /// handles that case by resuming the existing lifecycle instead.
    pub fn first_failure_for_begin(&self) -> Option<String> {
        if let Some(state) = self.existing_refund {
            return Some(format!(
                "a refund lifecycle already exists (state {})",
                state.as_str()
            ));
        }
        if !self.direction_ok {
            return Some(format!(
                "direction is {:?}, not SolToGlc/SolToRhn — only a Solana-side deposit can be \
                 refunded on Solana",
                self.direction
            ));
        }
        if !self.state_is_manual_review {
            return Some(format!("state is {:?}, not ManualReview", self.state));
        }
        if !self.reason_whitelisted {
            return Some(format!(
                "manual_review_note {:?} is not a whitelisted fold-time refund reason \
                 (allowed: {:?}{})",
                self.manual_review_reason,
                Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS,
                if self.direction == Direction::SolToRhn {
                    "; for SolToRhn also route_disabled_at_fold and undeliverable destination"
                } else {
                    ""
                }
            ));
        }
        if !self.source_finalized {
            return Some("source deposit is not finalized".to_string());
        }
        if !self.has_obligation_index {
            return Some("no source_obligation_index recorded".to_string());
        }
        if !self.has_requester {
            return Some("no requester (original sender) recorded".to_string());
        }
        if !self.no_destination_txid {
            return Some("a destination transaction already exists".to_string());
        }
        if !self.not_settled {
            return Some("the request has a settled_at timestamp".to_string());
        }
        if !self.no_goldcoin_payout {
            return Some(
                "a destination payout row (Goldcoin or Robinhood) already exists".to_string(),
            );
        }
        if !self.never_advanced_past_manual_review {
            return Some(
                "the state log shows this request once advanced to SourceFinalized or beyond — \
                 it may hold (or have held) reserved liquidity and is not a pure fold-time park"
                    .to_string(),
            );
        }
        if let Some(detail) = &self.hold_blocker {
            return Some(detail.clone());
        }
        None
    }
}

/// Result of the SolanaReserve capacity check for a refund
/// ([`Ledger::solana_refund_capacity`]): the refund amount must fit above
/// `protected_minimum` AND above the liquidity already reserved for
/// GlcToSol releases AND above every other still-open refund — strictly
/// stricter than the on-chain `enforce_protected_minimum` backstop, which
/// only knows the floor.
#[derive(Debug, Clone, Copy)]
pub struct SolanaRefundCapacityCheck {
    pub amount_solana_atomic: u64,
    pub total_reserve_balance: i64,
    pub protected_minimum: i64,
    pub reserved_liquidity: i64,
    /// Sum of every other `solana_refunds` row still `Pending`/`Broadcast`
    /// — already committed outflows the cached balance does not yet
    /// reflect.
    pub other_open_refunds_atomic: i64,
    pub ok: bool,
}

/// The chain-verified inputs [`Ledger::begin_solana_refund`] records —
/// every field was read/derived by `solana::refund` from FRESH
/// `finalized`-commitment chain state (the on-chain
/// `WithdrawalObligation`, `BridgeConfig`, and the canonical ATA
/// derivation), never accepted from operator input. The ledger
/// additionally cross-checks `obligation_index`/`requester`/
/// `gross_canonical_atomic` byte-for-byte against its own stored request
/// row and refuses on any disagreement — a tampered database row and a
/// tampered caller both fail closed.
#[derive(Debug, Clone, Copy)]
pub struct VerifiedRefundInputs {
    pub obligation_index: u64,
    /// Exact gross deposited amount in the reserve mint's native atomic
    /// units — the on-chain `WithdrawalObligation.amount`, verified equal
    /// to the stored canonical gross narrowed to the live mint decimals.
    pub amount_solana_atomic: u64,
    /// The stored `bridge_requests.gross_amount_atomic` the caller
    /// re-verified `amount_solana_atomic` against (canonical 8-decimal
    /// units).
    pub gross_canonical_atomic: u64,
    pub requester: [u8; 32],
    /// ATA(requester, reserve mint, reserve token program) — derived,
    /// never supplied.
    pub destination_token_account: [u8; 32],
    pub reserve_mint: [u8; 32],
    pub token_program: [u8; 32],
}

impl Ledger {
    /// How long a connection waits for a contended write lock before
    /// giving up with `SQLITE_BUSY`.
    ///
    /// The ledger file is opened by more than one process at a time in
    /// normal operation — the daemon's loops, the admin API, and any
    /// `glc-admin` invocation an operator runs — and the journal is WAL
    /// (`schema::open_and_migrate`), which allows concurrent readers but
    /// still serializes writers. With no busy timeout at all, SQLite's
    /// default, a writer that arrives while another holds the lock fails
    /// IMMEDIATELY rather than waiting: `LedgerError::Sqlite` ->
    /// `AdminError::Ledger` -> a 500 for an operator whose only mistake
    /// was running a command while a tick was committing.
    ///
    /// Bounded deliberately. Long enough to absorb the millisecond-scale
    /// overlap of two short transactions, short enough that a genuinely
    /// stuck writer still surfaces as an error an operator can see rather
    /// than as a request that hangs. It is NOT a retry loop: SQLite
    /// retries the lock acquisition, the transaction body runs once, and
    /// a real conflict still fails.
    const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Self::BUSY_TIMEOUT)?;
        schema::open_and_migrate(&conn)?;
        Ok(Ledger { conn })
    }

    /// Direct connection access, for tests in OTHER modules that need to
    /// set up or perturb raw rows (e.g. making the database disagree with
    /// the chain on purpose). Test-only: not compiled into the binary.
    #[cfg(test)]
    pub(crate) fn conn_for_tests(&self) -> &rusqlite::Connection {
        &self.conn
    }

    pub fn open_in_memory() -> Result<Self, LedgerError> {
        let conn = Connection::open_in_memory()?;
        // Same setting as [`Ledger::open`], so an in-memory ledger and a
        // file-backed one behave identically under contention rather than
        // differing in a way only a test would ever notice.
        conn.busy_timeout(Self::BUSY_TIMEOUT)?;
        schema::open_and_migrate(&conn)?;
        Ok(Ledger { conn })
    }

    /// Raw connection access, TEST BUILDS ONLY.
    ///
    /// Deliberately `#[cfg(test)]` and `pub(crate)`: it does not exist in a
    /// production binary at all. The `Ledger` API is intentionally a set of
    /// specific, invariant-preserving operations rather than a SQL escape
    /// hatch — every mutation goes through a method that keeps the reserve
    /// bookkeeping consistent, and a general-purpose accessor would let a
    /// caller sidestep all of that.
    ///
    /// Its one use is to let route-gate tests stand up the Phase-2
    /// `bridge_routes` table that Phase 1 deliberately does not create, so
    /// the "table present" branches of [`Ledger::route_enabled`] are
    /// exercised before the migration that will produce them ships.
    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    // ------------------------------------------------------------ reserve setup --

    /// Initializes (or re-parameterizes) a reserve's threshold configuration.
    /// Idempotent — safe to call at every startup with the current config.
    /// Does not touch `reserved_liquidity`/`pending_obligations`, which are
    /// derived from live `bridge_requests`, not configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn configure_reserve(
        &mut self,
        direction: ReserveDirection,
        initial_balance: u64,
        protected_minimum: u64,
        target_reserve: u64,
        warning_reserve: u64,
        critical_reserve: u64,
        now: i64,
    ) -> Result<(), LedgerError> {
        assert!(
            critical_reserve > protected_minimum,
            "critical_reserve must exceed protected_minimum (docs/05-reserve-accounting.md)"
        );
        self.conn.execute(
            "INSERT INTO reserve_ledger
                (direction, total_reserve_balance, balance_refreshed_at, protected_minimum,
                 target_reserve, warning_reserve, critical_reserve)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(direction) DO UPDATE SET
                protected_minimum = excluded.protected_minimum,
                target_reserve = excluded.target_reserve,
                warning_reserve = excluded.warning_reserve,
                critical_reserve = excluded.critical_reserve",
            rusqlite::params![
                direction,
                initial_balance as i64,
                now,
                protected_minimum as i64,
                target_reserve as i64,
                warning_reserve as i64,
                critical_reserve as i64,
            ],
        )?;
        Ok(())
    }

    /// Every Robinhood-side row this ledger holds, counted. All zero means
    /// this ledger has never accounted a Robinhood operation — the one
    /// state in which a reserve baseline of "nothing reserved, nothing
    /// pending" is true (`robinhood::reserve_init`).
    pub fn robinhood_activity(&self) -> Result<RobinhoodActivity, LedgerError> {
        let count = |sql: &str| -> Result<u64, LedgerError> {
            Ok(self
                .conn
                .query_row(sql, [], |r| r.get::<_, i64>(0))
                .map(|n| n.max(0) as u64)?)
        };
        Ok(RobinhoodActivity {
            deposit_observations: count("SELECT COUNT(*) FROM robinhood_deposit_observations")?,
            transactions: count("SELECT COUNT(*) FROM robinhood_transactions")?,
            requests: count(
                "SELECT COUNT(*) FROM bridge_requests
                 WHERE direction IN ('GlcToRhn','RhnToGlc','SolToRhn','RhnToSol')",
            )?,
            rebalances: count(
                "SELECT COUNT(*) FROM rebalance_requests WHERE direction = 'RobinhoodReserve'",
            )?,
        })
    }

    /// Updates the cached live-chain balance (called by reconciliation after
    /// a real chain read — never guessed, never left stale silently: callers
    /// must pass an actually-observed balance).
    pub fn refresh_reserve_balance(
        &mut self,
        direction: ReserveDirection,
        observed_balance: u64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE reserve_ledger SET total_reserve_balance = ?1, balance_refreshed_at = ?2
             WHERE direction = ?3",
            rusqlite::params![observed_balance as i64, now, direction],
        )?;
        if n == 0 {
            return Err(LedgerError::ReserveNotInitialized(direction));
        }
        Ok(())
    }

    pub fn set_paused(
        &mut self,
        direction: ReserveDirection,
        paused: bool,
        reason: Option<&str>,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE reserve_ledger SET paused = ?1, pause_reason = ?2 WHERE direction = ?3",
            rusqlite::params![paused as i64, reason, direction],
        )?;
        if n == 0 {
            return Err(LedgerError::ReserveNotInitialized(direction));
        }
        Ok(())
    }

    pub fn is_paused(&self, direction: ReserveDirection) -> Result<bool, LedgerError> {
        let paused: i64 = self
            .conn
            .query_row(
                "SELECT paused FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                // Same actionable error `set_paused` reports for a
                // missing row — an operator on a fresh database needs
                // "configure the reserve", not a storage error.
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })?;
        Ok(paused != 0)
    }

    /// The last note recorded alongside a [`Ledger::set_paused`] call —
    /// last-write-wins display context for an operator dashboard; the
    /// full history lives in `admin_audit_log`.
    pub fn pause_reason(&self, direction: ReserveDirection) -> Result<Option<String>, LedgerError> {
        let reason: Option<String> = self.conn.query_row(
            "SELECT pause_reason FROM reserve_ledger WHERE direction = ?1",
            [direction],
            |r| r.get(0),
        )?;
        Ok(reason)
    }

    /// The last note recorded alongside a [`Ledger::set_admission`] call
    /// — same last-write-wins caveat as [`Ledger::pause_reason`].
    pub fn admission_reason(
        &self,
        direction: ReserveDirection,
    ) -> Result<Option<String>, LedgerError> {
        let reason: Option<String> = self.conn.query_row(
            "SELECT admission_reason FROM reserve_ledger WHERE direction = ?1",
            [direction],
            |r| r.get(0),
        )?;
        Ok(reason)
    }

    /// Closes or opens admission of NEW obligations for `direction` — a
    /// separate axis from [`Ledger::set_paused`] (docs/09-runbook.md's
    /// "Admission control (Solana->Goldcoin)" section). Nothing in this
    /// crate ever calls this automatically: unlike `paused` (which
    /// reconciliation/the rolling-volume quota can set on a breach),
    /// `admission_closed` changes ONLY via an explicit operator call
    /// (`glc-admin close-admission`/`open-admission`) — there is no
    /// automatic reopen, and nothing auto-closes it either.
    ///
    /// The confirmed-liquidity admission buffer
    /// ([`Ledger::evaluate_liquidity_admission_gate`]) IS automatic, but
    /// it is a strictly separate axis on its own column: it never reads
    /// or writes `admission_closed`, so an automatic reopen can never
    /// undo a deliberate operator closure, and an operator reopen can
    /// never override thin confirmed liquidity. A new obligation needs
    /// BOTH open.
    pub fn set_admission(
        &mut self,
        direction: ReserveDirection,
        closed: bool,
        reason: Option<&str>,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE reserve_ledger SET admission_closed = ?1, admission_reason = ?2 WHERE direction = ?3",
            rusqlite::params![closed as i64, reason, direction],
        )?;
        if n == 0 {
            return Err(LedgerError::ReserveNotInitialized(direction));
        }
        Ok(())
    }

    pub fn is_admission_closed(&self, direction: ReserveDirection) -> Result<bool, LedgerError> {
        let closed: i64 = self
            .conn
            .query_row(
                "SELECT admission_closed FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })?;
        Ok(closed != 0)
    }

    /// Persisted per-route enable state — the LEDGER leg of
    /// [`crate::routes::RouteGate`]'s three-place AND.
    ///
    /// # Why this still tolerates a missing table
    ///
    /// The table now EXISTS: schema **v24** creates it and seeds one row
    /// per [`crate::routes::Route`] (`schema::apply_v24`). The migration
    /// was deferred through Phase 1 — a version bump would have locked the
    /// then-deployed daemon out of any ledger this code touched — and
    /// landed once that constraint was gone; docs/30-robinhood-network-
    /// phase1.md carries the original design and the numbering history
    /// (v18 -> v19 -> v22 -> v24, each earlier number taken by other work).
    ///
    /// The missing-table fallback is NOT dead code and must not be
    /// removed. It is what keeps this read fail-closed for a route with no
    /// opinion recorded anywhere, and it is the reason the v24 migration
    /// is a behavioural no-op rather than a behaviour change: the rows it
    /// seeds carry exactly the values this fallback already produced.
    ///
    /// So this read stays correct in all three worlds:
    ///
    /// | state | result |
    /// |---|---|
    /// | table absent (today, and every production ledger) | `default_enabled` |
    /// | table present, no row for this route | `default_enabled` |
    /// | table present, row present | that row's `enabled` flag |
    ///
    /// `default_enabled` is [`crate::routes::Route::default_enabled`]:
    /// `true` for the two legacy routes, `false` for everything else. So a
    /// ledger from before v24 resolves the legacy routes to enabled
    /// (behaviour unchanged) and any new route to disabled (fail closed),
    /// and a ledger that has run v24 resolves both to the same answers
    /// from real rows — which is what makes opening a Robinhood route a
    /// deliberate WRITE ([`Ledger::set_route_enabled`]) rather than a
    /// side effect of upgrading.
    ///
    /// The table's existence is probed via `sqlite_master` rather than by
    /// catching a "no such table" error string — an error-message match
    /// would silently start failing open if rusqlite ever reworded it.
    pub fn route_enabled(
        &self,
        route_id: &str,
        default_enabled: bool,
    ) -> Result<bool, LedgerError> {
        let table_exists: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'bridge_routes')",
            [],
            |r| r.get::<_, i64>(0).map(|v| v != 0),
        )?;
        if !table_exists {
            return Ok(default_enabled);
        }
        let enabled: Option<i64> = self
            .conn
            .query_row(
                "SELECT enabled FROM bridge_routes WHERE route_id = ?1",
                [route_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match enabled {
            Some(flag) => flag != 0,
            None => default_enabled,
        })
    }

    /// Every row of `bridge_routes`, exactly as recorded — the read side
    /// of [`Ledger::set_route_enabled`], and the one behind `glc-admin
    /// robinhood-routes`.
    ///
    /// # Why this exists separately from [`Ledger::route_enabled`]
    ///
    /// `route_enabled` answers the ADMISSION question and therefore
    /// resolves an absent table or row to [`crate::routes::Route::
    /// default_enabled`] — it must, because the gate has to return a
    /// verdict. That resolution is exactly wrong for an operator display:
    /// "disabled" and "no row was ever written" are different facts with
    /// different remedies (write the flag, versus run the v24 migration),
    /// and collapsing them is how an operator ends up re-running a command
    /// that cannot work. So this returns `None` for an absent table and
    /// reports the rows it actually found, resolving nothing.
    ///
    /// Read-only. It takes no lock beyond the read, writes nothing, and
    /// is safe against a ledger the daemon is using.
    ///
    /// `unknown_route_ids` carries any `route_id` in the table that this
    /// build does not model. Such a row is never silently dropped: a
    /// route_id nothing recognises is either a hand-written row or a
    /// downgrade, and both are worth an operator's attention.
    pub fn route_ledger_rows(&self) -> Result<Option<RouteLedgerState>, LedgerError> {
        // Probed via `sqlite_master` rather than by catching a "no such
        // table" message, for the same reason `route_enabled` does it:
        // an error-string match would start failing the day rusqlite
        // rewords it.
        let table_exists: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'bridge_routes')",
            [],
            |r| r.get::<_, i64>(0).map(|v| v != 0),
        )?;
        if !table_exists {
            return Ok(None);
        }

        let mut stmt = self.conn.prepare(
            "SELECT route_id, enabled, disabled_reason, updated_at
               FROM bridge_routes ORDER BY route_id",
        )?;
        let raw = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? != 0,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut state = RouteLedgerState::default();
        for (route_id, enabled, disabled_reason, updated_at) in raw {
            match route_id.parse::<crate::routes::Route>() {
                Ok(route) => state.rows.push(RouteLedgerRow {
                    route,
                    enabled,
                    disabled_reason,
                    updated_at,
                }),
                Err(_) => state.unknown_route_ids.push(route_id),
            }
        }
        // Registry order, not lexicographic: an operator reads these
        // beside `Route::ALL` everywhere else.
        state.rows.sort_by_key(|row| {
            crate::routes::Route::ALL
                .iter()
                .position(|r| *r == row.route)
                .unwrap_or(usize::MAX)
        });
        Ok(Some(state))
    }

    /// Writes one route's persisted `enabled` flag — the ONLY supported
    /// way to open (or re-close) a route in ledger state, and the write
    /// side of [`Ledger::route_enabled`].
    ///
    /// # What this is not
    ///
    /// It is not permission to move value, and it does not weaken or
    /// bypass anything. It sets exactly ONE of
    /// [`crate::routes::RouteGate`]'s three gates; the config gate, the
    /// adapter-capability gate, the Robinhood contract's own
    /// `routeEnabled`/`depositsPaused`/`payoutsPaused`, preflight, the
    /// signer quorum, the reserve invariants and the pause all still
    /// stand in front of every transfer. A route whose other gates are
    /// shut stays shut after this returns `Ok(())`.
    ///
    /// # Which routes it accepts
    ///
    /// Only [`crate::routes::Route::is_operator_settable`] routes:
    /// `GlcToRhn` and `RhnToGlc`. The two legacy routes are refused
    /// because their control is the pause/admission machinery and must
    /// not gain a second, divergent spelling here; `SolToRhn`/`RhnToSol`
    /// are refused because no `Direction` exists for them, so an
    /// `enabled = 1` row would be a claim nothing else could honour. Both
    /// refusals are [`LedgerError::RouteNotOperatorSettable`] — a
    /// validated refusal, not a storage error, so an audited caller
    /// records it and rolls back rather than treating it as a crash.
    ///
    /// # Why a missing row is an error rather than an insert
    ///
    /// The v24 migration seeds a row for every route, so a missing row
    /// means this ledger has not run it. Inserting one here would paper
    /// over that and write route state into a database whose schema this
    /// binary has not established; the refusal
    /// ([`LedgerError::RouteStateNotInitialized`]) names the actual
    /// remedy instead.
    ///
    /// `reason` is operator context for the disabled state, stored
    /// last-write-wins for display; the authoritative history is
    /// `admin_audit_log`, written by
    /// `crate::admin_api::audited_set_route_enabled`.
    pub fn set_route_enabled(
        &mut self,
        route: crate::routes::Route,
        enabled: bool,
        reason: Option<&str>,
    ) -> Result<(), LedgerError> {
        if !route.is_operator_settable() {
            return Err(LedgerError::RouteNotOperatorSettable {
                route: route.as_str(),
                detail: if route.is_legacy() {
                    "this route's controls are the local pause and admission control \
                     (glc-admin pause / close-admission), never a bridge_routes write"
                } else {
                    "this route has no settlement machinery (Route::as_direction is None) \
                     and can never be executed, so it cannot be enabled"
                },
            });
        }
        let n = self.conn.execute(
            "UPDATE bridge_routes
                SET enabled = ?1,
                    -- Cleared on enable: a stale reason next to an open
                    -- route reads as an explanation of a state that is no
                    -- longer true.
                    disabled_reason = CASE WHEN ?1 = 1 THEN NULL ELSE ?2 END,
                    -- The clock in SQL, matching the v24 seed, so the
                    -- ledger keeps taking its timestamps from one place
                    -- rather than gaining a second clock of its own.
                    updated_at = CAST(strftime('%s', 'now') AS INTEGER)
              WHERE route_id = ?3",
            rusqlite::params![enabled as i64, reason, route.as_str()],
        )?;
        if n == 0 {
            return Err(LedgerError::RouteStateNotInitialized(route.as_str()));
        }
        Ok(())
    }

    // ------------------------------------------- route-level admission --

    /// Whether `route`'s OWN admission gate is closed — the route-scoped
    /// companion to [`Ledger::is_admission_closed`]'s reserve-wide flag.
    ///
    /// # Absence resolves to OPEN, inverting this module's usual rule
    ///
    /// A missing `route_admission` table, and a missing row, both answer
    /// `false`. Everywhere else in this service an absent opinion fails
    /// CLOSED; here it must not, and the reasoning is recorded in full
    /// on `schema::apply_v25`. In short: absence of this table IS the
    /// pre-v25 state, in which no route-level admission gate existed at
    /// all, so resolving absence to "closed" would make the migration
    /// itself an outage for `SolToGlc`.
    ///
    /// This is safe because the gate can only ever SUBTRACT from what
    /// the reserve-wide gates already allow. It is ANDed with them by
    /// [`InboundAdmissionGates::blocker`], never consulted alone, so its
    /// absence can never admit a deposit the reserve would have refused.
    /// A route that is not [`crate::routes::Route::is_admission_settable`]
    /// has no row by construction (the table's CHECK forbids one) and so
    /// always answers `false`, which is the only correct answer: those
    /// routes are governed entirely by their own destination reserve.
    ///
    /// A genuine STORAGE failure is still an `Err` and is never
    /// flattened into `false` — `api::route_availability` renders that
    /// as unavailable, exactly as it already did for every other ledger
    /// read failure.
    pub fn route_admission_closed(&self, route: crate::routes::Route) -> Result<bool, LedgerError> {
        // Probed via `sqlite_master` rather than by catching a "no such
        // table" message, for the same reason `route_enabled` does it:
        // an error-string match would start failing the day rusqlite
        // rewords it.
        let table_exists: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = \
             'route_admission')",
            [],
            |r| r.get::<_, i64>(0).map(|v| v != 0),
        )?;
        if !table_exists {
            return Ok(false);
        }
        Self::route_admission_closed_in(&self.conn, route)
    }

    /// [`Ledger::route_admission_closed`]'s body, against a caller-held
    /// connection or transaction.
    ///
    /// Taking a `&Connection` rather than `&self` is what lets both folds
    /// read this INSIDE their existing write transaction, atomically with
    /// the admission decision they are making — a separate read could be
    /// overtaken between the check and the write. Same reasoning as
    /// [`Ledger::read_liquidity_admission_row`].
    ///
    /// Assumes the table exists: the folds run against a ledger this
    /// binary has already migrated, so a missing table there is a real
    /// storage failure rather than the benign pre-v25 absence
    /// `route_admission_closed` resolves for read-only callers.
    pub(crate) fn route_admission_closed_in(
        conn: &Connection,
        route: crate::routes::Route,
    ) -> Result<bool, LedgerError> {
        let closed: Option<i64> = conn
            .query_row(
                "SELECT admission_closed FROM route_admission WHERE route_id = ?1",
                [route.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(closed.is_some_and(|flag| flag != 0))
    }

    /// Every row of `route_admission`, exactly as recorded — the read
    /// side of [`Ledger::set_route_admission`], and the one behind
    /// `glc-admin route-admission-show`.
    ///
    /// Resolves nothing, for the same reason
    /// [`Ledger::route_ledger_rows`] resolves nothing: "open" and "no row
    /// was ever written" are different facts with different remedies
    /// (write the flag, versus run the v25 migration), and collapsing
    /// them is how an operator ends up re-running a command that cannot
    /// work. `None` means the table does not exist.
    ///
    /// Read-only. It takes no lock beyond the read, writes nothing, and
    /// is safe against a ledger the daemon is using.
    pub fn route_admission_rows(&self) -> Result<Option<RouteAdmissionState>, LedgerError> {
        let table_exists: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = \
             'route_admission')",
            [],
            |r| r.get::<_, i64>(0).map(|v| v != 0),
        )?;
        if !table_exists {
            return Ok(None);
        }

        let mut stmt = self.conn.prepare(
            "SELECT route_id, admission_closed, admission_closed_reason, updated_at
               FROM route_admission ORDER BY route_id",
        )?;
        let raw = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? != 0,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut state = RouteAdmissionState::default();
        for (route_id, admission_closed, admission_closed_reason, updated_at) in raw {
            match route_id.parse::<crate::routes::Route>() {
                // A parseable route that is nonetheless not
                // admission-settable is still reported as unknown: the
                // table's CHECK makes such a row impossible for this
                // binary to write, so one that exists did not come from
                // here and must not be rendered as ordinary state.
                Ok(route) if route.is_admission_settable() => state.rows.push(RouteAdmissionRow {
                    route,
                    admission_closed,
                    admission_closed_reason,
                    updated_at,
                }),
                _ => state.unknown_route_ids.push(route_id),
            }
        }
        // Registry order, not lexicographic: an operator reads these
        // beside `Route::ADMISSION_SETTABLE` everywhere else.
        state.rows.sort_by_key(|row| {
            crate::routes::Route::ADMISSION_SETTABLE
                .iter()
                .position(|r| *r == row.route)
                .unwrap_or(usize::MAX)
        });
        Ok(Some(state))
    }

    /// Writes one route's persisted admission flag — the ONLY supported
    /// way to close or open a route-level admission gate, and the write
    /// side of [`Ledger::route_admission_closed`].
    ///
    /// # What this is and is not
    ///
    /// It is a NARROWING of the existing inbound admission machinery, not
    /// a new mechanism beside it. The flag it writes is read by the same
    /// [`InboundAdmissionGates`] evaluator both folds and `GET /chains`
    /// already gate on, it parks deposits into the same `ManualReview`
    /// with the same recoverable and refundable treatment as
    /// `admission_closed_at_fold`, and it is written only through
    /// `crate::admin_api::audited_set_route_admission`.
    ///
    /// It does NOT weaken anything. The reserve-wide `paused` and
    /// `admission_closed` still close every route drawing on that
    /// reserve, regardless of what any row here says: the two are ANDed,
    /// and opening a route gate can never reopen a paused reserve. Nor
    /// does it touch enablement — [`crate::routes::RouteGate`]'s three
    /// gates are a separate axis and a disabled route stays disabled.
    ///
    /// # Which routes it accepts
    ///
    /// Only [`crate::routes::Route::is_admission_settable`] routes:
    /// `SolToGlc` and `RhnToGlc`, the two whose destination reserve is
    /// Goldcoin. Everything else is
    /// [`LedgerError::RouteAdmissionNotSettable`] — a validated refusal,
    /// not a storage error, so an audited caller records it and rolls
    /// back rather than treating it as a crash. The `route_admission`
    /// table's own CHECK refuses the same set independently, so this is
    /// the second of two guards rather than the only one.
    ///
    /// # Why a missing row is an error rather than an insert
    ///
    /// The v25 migration seeds a row for both settable routes, so a
    /// missing row means this ledger has not run it. Inserting one here
    /// would paper over that and write route state into a schema this
    /// binary has not established; the refusal
    /// ([`LedgerError::RouteAdmissionStateNotInitialized`]) names the
    /// actual remedy instead. Same reasoning as
    /// [`Ledger::set_route_enabled`].
    ///
    /// `reason` is operator context for the closed state, stored
    /// last-write-wins for display; the authoritative history is
    /// `admin_audit_log`.
    pub fn set_route_admission(
        &mut self,
        route: crate::routes::Route,
        closed: bool,
        reason: Option<&str>,
    ) -> Result<(), LedgerError> {
        if !route.is_admission_settable() {
            return Err(LedgerError::RouteAdmissionNotSettable {
                route: route.as_str(),
                detail: match route.as_direction() {
                    // Goldcoin is this route's SOURCE, so it draws on the
                    // Solana or Robinhood reserve and an
                    // inbound-to-Goldcoin admission flag would gate a
                    // reserve it has nothing to do with. Its controls are
                    // that reserve's own pause and, for GlcToRhn, the
                    // route enablement gate.
                    Some(_) => {
                        "route-level admission exists only for the two INBOUND-TO-GOLDCOIN routes \
                         (SolToGlc, RhnToGlc); this route's destination reserve is not Goldcoin, \
                         so its control is that reserve's own pause"
                    }
                    // No settlement machinery at all, so there is no
                    // admission to open or close — the same structural
                    // refusal `set_route_enabled` gives these two.
                    None => {
                        "this route has no settlement machinery (Route::as_direction is None) and \
                         can never be executed, so it has no admission to open or close"
                    }
                },
            });
        }
        let n = self.conn.execute(
            "UPDATE route_admission
                SET admission_closed = ?1,
                    -- Cleared on open: a stale reason next to an open
                    -- route reads as an explanation of a state that is no
                    -- longer true. Same discipline as
                    -- `set_route_enabled`'s `disabled_reason`.
                    admission_closed_reason = CASE WHEN ?1 = 1 THEN ?2 ELSE NULL END,
                    -- The clock in SQL, matching the v25 seed, so the
                    -- ledger keeps taking its timestamps from one place
                    -- rather than gaining a second clock of its own.
                    updated_at = CAST(strftime('%s', 'now') AS INTEGER)
              WHERE route_id = ?3",
            rusqlite::params![closed as i64, reason, route.as_str()],
        )?;
        if n == 0 {
            return Err(LedgerError::RouteAdmissionStateNotInitialized(
                route.as_str(),
            ));
        }
        Ok(())
    }

    /// Configures GoldcoinReserve's UTXO-liquidity admission backpressure
    /// (docs/09-runbook.md's "UTXO liquidity" section):
    /// `min_available_count` is the number of mature, unreserved vault
    /// UTXOs that must remain after admitting one more SolToGlc obligation
    /// — `Ledger::fold_sol_deposit` parks (never drops) a new obligation to
    /// `ManualReview` with reason `utxo_liquidity_low_at_fold` whenever the
    /// live count would fall to or below this floor, exactly the same
    /// fail-closed shape as its existing `paused`/`admission_closed`
    /// checks. `warning_count` (>= `min_available_count`) is purely
    /// observational — surfaced via `Ledger::utxo_pool_health` for
    /// operator visibility before backpressure actually engages; never
    /// itself gates admission. Defaults to `(0, 0)` (no backpressure, no
    /// warning) on every reserve until explicitly configured — idempotent,
    /// safe to call at every startup with the current config, matching
    /// `configure_reserve`.
    pub fn set_utxo_pool_thresholds(
        &mut self,
        direction: ReserveDirection,
        min_available_count: u32,
        warning_count: u32,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE reserve_ledger SET utxo_pool_min_available_count = ?1, utxo_pool_warning_count = ?2
             WHERE direction = ?3",
            rusqlite::params![min_available_count, warning_count, direction],
        )?;
        if n == 0 {
            return Err(LedgerError::ReserveNotInitialized(direction));
        }
        Ok(())
    }

    /// The `(min_available_count, warning_count)` pair last set by
    /// [`Ledger::set_utxo_pool_thresholds`] — `(0, 0)` (no backpressure, no
    /// warning) until explicitly configured. Read by
    /// [`crate::ops::reserve_health`] so an operator can see how close
    /// `utxo_pool_health().available_utxo_count` is to engaging
    /// backpressure, without duplicating the threshold values.
    pub fn utxo_pool_thresholds(
        &self,
        direction: ReserveDirection,
    ) -> Result<(u32, u32), LedgerError> {
        let (min_available_count, warning_count): (u32, u32) = self.conn.query_row(
            "SELECT utxo_pool_min_available_count, utxo_pool_warning_count
             FROM reserve_ledger WHERE direction = ?1",
            [direction],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((min_available_count, warning_count))
    }

    /// `total_reserve_balance - protected_minimum - reserved_liquidity`
    /// (docs/05-reserve-accounting.md). Not clamped at zero deliberately:
    /// a negative value is itself diagnostic (see [`Ledger::check_invariant`]).
    ///
    /// Deliberately does NOT subtract `accrued_fees_atomic`
    /// (docs/20-bridge-fee.md): `reserved_liquidity`/`pending_obligations`/
    /// `settled_liquidity_total` already track NET customer entitlements
    /// only (never gross), so fee revenue was never counted as a customer
    /// obligation in the first place — there is nothing to double-subtract.
    /// A separate subtraction here would incorrectly shrink capacity by
    /// the fee amount twice.
    pub fn available_capacity(&self, direction: ReserveDirection) -> Result<i64, LedgerError> {
        let (balance, protected_minimum, reserved) = self.reserve_row(direction)?;
        Ok(balance - protected_minimum - reserved)
    }

    fn reserve_row(&self, direction: ReserveDirection) -> Result<(i64, i64, i64), LedgerError> {
        self.conn
            .query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
                 FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })
    }

    /// Asserts `available reserves >= all releases that can currently become
    /// payable` — i.e. `total_reserve_balance >= protected_minimum +
    /// reserved_liquidity`. Called defensively by tests after every mutating
    /// operation and by reconciliation; a violation here means the ledger's
    /// own bookkeeping has diverged from what it promised, which must never
    /// happen by construction and is treated as a hard error, not a
    /// warning.
    pub fn check_invariant(&self, direction: ReserveDirection) -> Result<(), LedgerError> {
        let (balance, protected_minimum, reserved) = self.reserve_row(direction)?;
        if balance < protected_minimum + reserved {
            return Err(LedgerError::InvariantViolated {
                direction,
                balance,
                protected_minimum,
                reserved_liquidity: reserved,
            });
        }
        Ok(())
    }

    /// The same count-based admission gate [`Ledger::fold_sol_deposit`]
    /// applies to a brand-new obligation, applied here to reopening
    /// admission direction-wide: refuses (no override) if the mature
    /// Goldcoin UTXO pool is still at or below `utxo_pool_min_available_count`,
    /// so admission is never reopened onto a pool this thin — that would
    /// immediately re-admit exactly the demand backpressure exists to hold
    /// back. Does NOT replace [`Ledger::check_invariant`] — callers must
    /// still check that separately; this check is purely additive and
    /// never weakens the hard reserve invariant. Always `Ok(())` for
    /// `SolanaReserve`, which has no UTXO-pool concept — Solana admission
    /// behavior is completely unaffected by this check.
    pub fn check_utxo_liquidity_for_admission(
        &self,
        direction: ReserveDirection,
        now: i64,
    ) -> Result<(), LedgerError> {
        if direction != ReserveDirection::GoldcoinReserve {
            return Ok(());
        }
        let (min_available_count, _warning_count) = self.utxo_pool_thresholds(direction)?;
        let pool = self.utxo_pool_health(now)?;
        let available_utxo_count = pool.available_utxo_count as i64;
        let min_available_count = min_available_count as i64;
        // `== 0` means backpressure is disabled — identical short-circuit
        // to `fold_sol_deposit`'s own.
        let utxo_liquidity_ok =
            min_available_count == 0 || available_utxo_count > min_available_count;
        if !utxo_liquidity_ok {
            return Err(LedgerError::UtxoLiquidityLowForAdmission {
                direction,
                available_utxo_count,
                min_available_count,
                own_unconfirmed_change_atomic: pool.own_unconfirmed_change_atomic,
            });
        }
        Ok(())
    }

    // ------------------------------- confirmed-liquidity admission buffer --

    /// The pure hysteresis decision behind the confirmed-liquidity
    /// admission gate (docs/09-runbook.md's "Confirmed-liquidity admission
    /// safety buffer" section) — the SINGLE place the open/closed rule is
    /// expressed. Every caller (the fold path, the resume path, the
    /// orchestrator's per-tick evaluation, the operator reopen check)
    /// routes through this one function, so no two of them can ever drift
    /// onto slightly different arithmetic.
    ///
    /// Given the CURRENT state and the CURRENT confirmed unreserved
    /// headroom:
    ///
    /// - `buffer_atomic == 0` disables the feature entirely — always open,
    ///   the same short-circuit shape `utxo_pool_min_available_count == 0`
    ///   already uses, so an unconfigured ledger behaves exactly as it did
    ///   before the buffer existed.
    /// - **open -> closed** as soon as `headroom < buffer_atomic`.
    /// - **closed -> open** only once `headroom >= reopen_atomic`.
    ///
    /// Between the two thresholds the state is HELD, whichever it is. That
    /// held band is the whole point: with a 250 000 / 350 000 GLC pair, a
    /// headroom oscillating anywhere inside that range produces no state
    /// change at all, so the gate cannot flap even under continuous
    /// deposit/payout churn. A single-threshold design would toggle on
    /// every crossing of one number.
    ///
    /// Deliberately takes the current state as an ARGUMENT rather than
    /// reading it: hysteresis is genuinely stateful (at a headroom inside
    /// the band the correct answer depends on which side the reserve
    /// arrived from), and making that dependency explicit in the signature
    /// is what keeps the function pure and directly unit-testable.
    pub fn next_liquidity_admission_closed(
        currently_closed: bool,
        headroom: i64,
        buffer_atomic: i64,
        reopen_atomic: i64,
    ) -> bool {
        if buffer_atomic <= 0 {
            return false;
        }
        if currently_closed {
            headroom < reopen_atomic
        } else {
            headroom < buffer_atomic
        }
    }

    /// Confirmed unreserved headroom for `direction`: `total_reserve_
    /// balance - protected_minimum - reserved_liquidity`, i.e. exactly
    /// [`Ledger::available_capacity`] — delegated to, never re-derived, so
    /// the admission buffer and the pre-existing capacity check can never
    /// disagree about what "headroom" means.
    ///
    /// # Why this is already confirmed-only
    ///
    /// `total_reserve_balance` is a MATURE-only figure by construction:
    /// `sync_vault_utxos`/`Orchestrator::tick_goldcoin_reconciliation`
    /// filter by `vault_min_confirmations` before it is ever computed, and
    /// both [`Ledger::immature_vault_utxo_total`] and
    /// [`Ledger::own_unconfirmed_change_atomic`] document that they are
    /// observational only and never added to it. So still-immature payout
    /// change — this service's own, already-known, en-route-to-maturity
    /// change included — contributes nothing here, which is precisely the
    /// admission policy required: value that cannot be spent yet must not
    /// be counted as room to accept new demand. (Reconciliation's hard
    /// solvency invariant DOES add `own_unconfirmed_change_atomic`, on
    /// purpose and separately — that check asks "is anything actually
    /// missing", a different question from "may we take on more".)
    pub fn confirmed_admission_headroom(
        &self,
        direction: ReserveDirection,
    ) -> Result<i64, LedgerError> {
        self.available_capacity(direction)
    }

    /// Configures the confirmed-liquidity admission buffer for `direction`
    /// — the close threshold and the (higher) reopen threshold, in the
    /// reserve's own atomic units. Called once at daemon startup from
    /// `goldcoin.admission_safety_buffer_atomic`/
    /// `goldcoin.admission_reopen_headroom_atomic`, exactly mirroring how
    /// [`Ledger::set_utxo_pool_thresholds`] is wired.
    ///
    /// `buffer_atomic == 0` disables the feature (and `reopen_atomic` is
    /// then irrelevant). Refuses `reopen_atomic < buffer_atomic`: a reopen
    /// mark below the close mark cannot express hysteresis at all and
    /// would let the gate close and immediately reopen on one unchanged
    /// headroom — the flapping this whole mechanism exists to prevent.
    /// Equal thresholds are permitted (degenerate but coherent: no held
    /// band, still no oscillation on a single reading).
    ///
    /// Does NOT itself open or close anything — it only sets the policy;
    /// the state transition happens on the next
    /// [`Ledger::evaluate_liquidity_admission_gate`] or fold.
    pub fn set_admission_liquidity_thresholds(
        &mut self,
        direction: ReserveDirection,
        buffer_atomic: u64,
        reopen_atomic: u64,
    ) -> Result<(), LedgerError> {
        if reopen_atomic < buffer_atomic {
            return Err(LedgerError::InvalidAdmissionThresholds {
                direction,
                buffer_atomic,
                reopen_atomic,
            });
        }
        let n = self.conn.execute(
            "UPDATE reserve_ledger SET admission_buffer_atomic = ?1, admission_reopen_atomic = ?2
             WHERE direction = ?3",
            rusqlite::params![buffer_atomic as i64, reopen_atomic as i64, direction],
        )?;
        if n == 0 {
            return Err(LedgerError::ReserveNotInitialized(direction));
        }
        Ok(())
    }

    /// The `(buffer_atomic, reopen_atomic)` pair last set by
    /// [`Ledger::set_admission_liquidity_thresholds`] — `(0, 0)` (feature
    /// disabled) until explicitly configured.
    pub fn admission_liquidity_thresholds(
        &self,
        direction: ReserveDirection,
    ) -> Result<(i64, i64), LedgerError> {
        self.conn
            .query_row(
                "SELECT admission_buffer_atomic, admission_reopen_atomic
                 FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })
    }

    /// The AUTOMATIC confirmed-liquidity gate's persisted state — strictly
    /// separate from [`Ledger::is_admission_closed`], which remains the
    /// operator-only switch. A new SolToGlc obligation is admitted only
    /// when BOTH are open (plus every pre-existing gate), and neither one
    /// can ever clear the other.
    /// The read-only admission snapshot for `direction` — every
    /// direction-wide gate a fold will apply to a newly observed
    /// deposit, read WITHOUT evaluating or moving the confirmed-liquidity
    /// hysteresis (it reports the persisted state, exactly as
    /// `GET /status` has always done).
    ///
    /// This is what the public API's per-route `available` is computed
    /// from (at a stated normal transfer size for `SolToGlc` —
    /// `InboundAdmissionGates::route_blocker_at` — and at one atomic
    /// unit for the other routes). It is the same
    /// [`crate::ledger::admission::InboundAdmissionGates`] both folds
    /// read from inside their own write transaction, so an availability
    /// answer and the fold that follows it cannot disagree about the
    /// rules — only, at worst, about the instant, which no read-then-act
    /// API can avoid and which the fold's own re-check inside its
    /// transaction is what makes safe.
    pub fn inbound_admission_gates(
        &self,
        direction: Direction,
    ) -> Result<InboundAdmissionGates, LedgerError> {
        InboundAdmissionGates::read_persisted(&self.conn, direction)
    }

    /// `None` when this ROUTE would currently admit a new deposit of the
    /// smallest representable size — its own admission gate open AND its
    /// destination reserve willing; otherwise the highest-ranked gate
    /// refusing it. See [`InboundAdmissionGates::route_blocker`].
    ///
    /// Keyed by settlement [`Direction`] rather than by
    /// [`ReserveDirection`] since v25: two directions can share one
    /// destination reserve (`SolToGlc` and `RhnToGlc` both settle out of
    /// `GoldcoinReserve`) and now have independent route-level gates, so
    /// a reserve alone is no longer enough to answer the question.
    pub fn route_admission_blocker(
        &self,
        direction: Direction,
    ) -> Result<Option<InboundAdmissionBlocker>, LedgerError> {
        Ok(self.inbound_admission_gates(direction)?.route_blocker())
    }

    pub fn is_liquidity_admission_closed(
        &self,
        direction: ReserveDirection,
    ) -> Result<bool, LedgerError> {
        let closed: i64 = self
            .conn
            .query_row(
                "SELECT liquidity_admission_closed FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })?;
        Ok(closed != 0)
    }

    /// Reads the gate's inputs and current state from one `reserve_ledger`
    /// row, inside whatever transaction/connection the caller already
    /// holds. Returns `(buffer_atomic, reopen_atomic, currently_closed)`.
    ///
    /// Taking a `&Connection` rather than `&self` is what lets
    /// [`Ledger::fold_sol_deposit`] and
    /// [`Ledger::resume_manual_review_sol_to_glc`] evaluate the gate
    /// INSIDE their existing write transaction, atomically with the
    /// admission decision they are making — a separate read could be
    /// overtaken between the check and the write.
    /// The live count of MATURE, UNRESERVED vault UTXOs — the same
    /// candidate pool `available_vault_utxos` offers coin selection,
    /// counted rather than fetched in full.
    ///
    /// The ONE definition of that predicate. It had three copies:
    /// [`Ledger::utxo_pool_health`], [`Ledger::fold_sol_deposit`], and
    /// [`Ledger::fold_robinhood_deposit`] — and the third had already
    /// drifted, excluding only unfinalized `GlcToSol` deposits where the
    /// other two excluded every Goldcoin-sourced direction
    /// ([`Direction::SOURCE_IS_GOLDCOIN_SQL_IN`], i.e. `GlcToRhn` too).
    /// A UTXO still backing an unfinalized `GlcToRhn` deposit was
    /// therefore counted as available headroom by the Robinhood fold and
    /// as unavailable by everything else. Collapsing the three onto this
    /// function fixes that in the safe direction (strictly fewer UTXOs
    /// counted) and makes the divergence unrepresentable.
    /// `total_reserve_balance - protected_minimum - reserved_liquidity`
    /// for `direction`, read through an arbitrary connection (a write
    /// transaction, typically) rather than `&self`.
    ///
    /// The same figure [`Ledger::available_capacity`] and
    /// [`Ledger::confirmed_admission_headroom`] report; this overload
    /// exists only because the folds need it from INSIDE their own
    /// transaction.
    fn reserve_headroom(
        conn: &rusqlite::Connection,
        direction: ReserveDirection,
    ) -> Result<i64, LedgerError> {
        let (balance, protected_minimum, reserved): (i64, i64, i64) = conn
            .query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
                 FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })?;
        Ok(balance - protected_minimum - reserved)
    }

    fn count_available_vault_utxos(conn: &rusqlite::Connection) -> Result<i64, LedgerError> {
        Ok(conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM vault_utxos v
             WHERE v.state = 'Available'
               AND {deposit_excl}
               AND {claim_excl}",
                claim_excl = live_split_claim_exclusion("v"),
                deposit_excl = unfinalized_goldcoin_deposit_exclusion("v")
            ),
            [],
            |r| r.get(0),
        )?)
    }

    fn read_liquidity_admission_row(
        conn: &rusqlite::Connection,
        direction: ReserveDirection,
    ) -> Result<(i64, i64, bool), LedgerError> {
        let (buffer, reopen, closed): (i64, i64, i64) = conn.query_row(
            "SELECT admission_buffer_atomic, admission_reopen_atomic, liquidity_admission_closed
             FROM reserve_ledger WHERE direction = ?1",
            [direction],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok((buffer, reopen, closed != 0))
    }

    /// Persists a gate transition, stamping `liquidity_admission_closed_at`
    /// only when the state actually CHANGES — so the timestamp answers
    /// "when did the gate last flip", not "when was it last looked at".
    /// A no-op when the state is unchanged.
    fn write_liquidity_admission_state(
        conn: &rusqlite::Connection,
        direction: ReserveDirection,
        was_closed: bool,
        now_closed: bool,
        now: i64,
    ) -> Result<(), LedgerError> {
        if was_closed == now_closed {
            return Ok(());
        }
        conn.execute(
            "UPDATE reserve_ledger SET liquidity_admission_closed = ?1,
                liquidity_admission_closed_at = ?2 WHERE direction = ?3",
            rusqlite::params![now_closed as i64, now, direction],
        )?;
        Ok(())
    }

    /// Evaluates the confirmed-liquidity gate against current headroom and
    /// persists any resulting transition. Idempotent and safe to call as
    /// often as desired — calling it never admits, parks, cancels or
    /// otherwise touches a single request; it only moves the direction-wide
    /// gate.
    ///
    /// Called once per orchestrator tick (right after the pre-admission
    /// reconciliation pass, so it sees the freshest balance) purely so the
    /// state an operator/`/status` reads stays truthful even during a long
    /// stretch with no folds at all — in particular so a RECOVERY to the
    /// reopen threshold is noticed without waiting for the next deposit.
    /// The authoritative evaluation for an actual admission decision
    /// happens inside [`Ledger::fold_sol_deposit`]'s own transaction; this
    /// one can never be the thing that admits something.
    ///
    /// Always a no-op for any direction other than `GoldcoinReserve`
    /// and whenever `admission_buffer_atomic` is `0`.
    ///
    /// `GoldcoinReserve` is not a synonym for `SolToGlc`: this gate
    /// governs admission of every inbound-to-Goldcoin deposit, which
    /// since Phase F means `RhnToGlc` as well — both folds read this one
    /// row (see [`crate::ledger::admission`]).
    pub fn evaluate_liquidity_admission_gate(
        &mut self,
        direction: ReserveDirection,
        now: i64,
    ) -> Result<LiquidityAdmissionGate, LedgerError> {
        let (buffer_atomic, reopen_atomic, was_closed) =
            Self::read_liquidity_admission_row(&self.conn, direction).map_err(|e| match e {
                LedgerError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => other,
            })?;
        let headroom = self.confirmed_admission_headroom(direction)?;
        let now_closed = if direction == ReserveDirection::GoldcoinReserve {
            Self::next_liquidity_admission_closed(
                was_closed,
                headroom,
                buffer_atomic,
                reopen_atomic,
            )
        } else {
            false
        };
        Self::write_liquidity_admission_state(&self.conn, direction, was_closed, now_closed, now)?;
        Ok(LiquidityAdmissionGate {
            direction,
            closed: now_closed,
            transitioned: was_closed != now_closed,
            headroom,
            buffer_atomic,
            reopen_atomic,
        })
    }

    /// The confirmed-liquidity twin of
    /// [`Ledger::check_utxo_liquidity_for_admission`], applied to an
    /// OPERATOR reopening admission direction-wide (`glc-admin
    /// open-admission`): refuses (no override) while the automatic gate is
    /// still closed, i.e. while confirmed unreserved headroom has not
    /// recovered to `admission_reopen_atomic`.
    ///
    /// Without this, `open-admission` would appear to succeed while every
    /// new fold still parked in `ManualReview` — the operator flag would
    /// be cleared and nothing would visibly change, with no explanation.
    /// Evaluates the gate first (so a reserve that has ALREADY recovered
    /// reopens and this check passes), then reports. Purely additive:
    /// never weakens the hard reserve invariant or the UTXO-count floor,
    /// both of which `open-admission` still checks separately. Always
    /// `Ok(())` for `SolanaReserve` and whenever the buffer is disabled.
    pub fn check_liquidity_buffer_for_admission(
        &mut self,
        direction: ReserveDirection,
        now: i64,
    ) -> Result<(), LedgerError> {
        if direction != ReserveDirection::GoldcoinReserve {
            return Ok(());
        }
        let gate = self.evaluate_liquidity_admission_gate(direction, now)?;
        if gate.closed {
            return Err(LedgerError::LiquidityAdmissionClosedForAdmission {
                direction,
                headroom: gate.headroom,
                reopen_atomic: gate.reopen_atomic,
                own_unconfirmed_change_atomic: self.own_unconfirmed_change_atomic(now)?,
            });
        }
        Ok(())
    }

    // -------------------------------------------------------------- reservation --

    /// [`Self::create_request_from`] with no declared source wallet — the
    /// original signature, kept for the callers that never knew one.
    /// The DESTINATION wallet window is still enforced: it needs no
    /// declaration, since the recipient is what the request is for.
    pub fn create_request(
        &mut self,
        direction: Direction,
        amounts: RequestAmounts,
        recipient: &[u8],
        requester: Option<[u8; 32]>,
        reservation_ttl_secs: i64,
        now: i64,
    ) -> Result<CreateRequestOutcome, LedgerError> {
        self.create_request_from(
            direction,
            amounts,
            recipient,
            requester,
            None,
            reservation_ttl_secs,
            now,
        )
    }

    /// Never accept a transfer that cannot be fulfilled: capacity check and
    /// reservation write are one atomic transaction. `amounts` must already
    /// be a fully computed, internally consistent gross/fee/net breakdown
    /// (`amount_conversion::compute_fee`, converted to the destination's
    /// native unit) — the ledger never computes a fee or a conversion
    /// itself (docs/20-bridge-fee.md). The capacity check compares
    /// `amounts.net_destination_atomic` (what the destination reserve must
    /// actually release) against available capacity, NOT the gross amount
    /// the user declared.
    ///
    /// # The wallet windows, checked here and again at observation
    ///
    /// Both rolling-24h wallet windows (`ledger::wallet_window`) are
    /// evaluated inside this same write transaction, BEFORE any row is
    /// written: the destination (`recipient`) always, and the source
    /// wallet when the caller declared one (`source_wallet` — a
    /// `POST /transfers` caller's own Goldcoin address, which is stored on
    /// the row so the window is consumed from admission, not from the
    /// moment the deposit lands). A blocked window refuses with
    /// [`CreateRequestOutcome::WalletLimited`] and leaves no trace: no
    /// row, no reserved liquidity, no derived address. Nothing is
    /// on-chain yet, so refusing is the right shape here — unlike a fold,
    /// which must record a deposit that already happened.
    ///
    /// A declared source is a CLAIM, not evidence. The wallet that really
    /// funds the deposit is traced from the deposit transaction's own
    /// inputs by `goldcoin::indexer` and re-checked — against every other
    /// request in the window — by [`Self::record_glc_deposit_observed`],
    /// which is the enforcing check for the source leg; a caller that
    /// declares nothing, or declares a different wallet, gains nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn create_request_from(
        &mut self,
        direction: Direction,
        amounts: RequestAmounts,
        recipient: &[u8],
        requester: Option<[u8; 32]>,
        source_wallet: Option<&[u8]>,
        reservation_ttl_secs: i64,
        now: i64,
    ) -> Result<CreateRequestOutcome, LedgerError> {
        let reserve = direction.destination_reserve();
        let tx = write_tx(&mut self.conn)?;

        let paused: i64 = tx.query_row(
            "SELECT paused FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| r.get(0),
        )?;
        if paused != 0 {
            tx.rollback()?;
            return Ok(CreateRequestOutcome::Paused);
        }

        // An empty declaration is no declaration: the column CHECK
        // forbids an empty blob, and "declared nothing" is the honest
        // reading of it.
        let source_wallet = source_wallet.filter(|w| !w.is_empty());
        let eligibility = Self::route_wallet_eligibility_in(
            &tx,
            direction,
            source_wallet,
            Some(recipient),
            now,
            WalletWindowScope::NewRequest,
        )?;
        if !eligibility.is_eligible() {
            tx.rollback()?;
            return Ok(CreateRequestOutcome::WalletLimited { eligibility });
        }

        let (balance, protected_minimum, reserved): (i64, i64, i64) = tx.query_row(
            "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
             FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let available = balance - protected_minimum - reserved;
        if (amounts.net_destination_atomic as i64) > available {
            tx.rollback()?;
            return Ok(CreateRequestOutcome::InsufficientLiquidity {
                available_capacity: available,
            });
        }

        tx.execute(
            // `source_chain` is named explicitly rather than defaulted:
            // the v21 column is `NOT NULL` with no default precisely so
            // that a source identity can never be omitted by accident.
            // A `GlcToSol` request's source leg is Goldcoin from the
            // moment it is created — before any deposit is seen — and
            // Goldcoin has no contract identity at all (its source is an
            // outpoint), so `source_contract` stays NULL, which the
            // table's CHECKs require for this chain.
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 reserved_at, reservation_expires_at, source_confirmations, source_chain,
                 source_wallet)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11, 0, ?12, ?13)",
            rusqlite::params![
                direction,
                RequestState::AwaitingDeposit,
                amounts.gross_atomic as i64,
                amounts.fee_bps as i64,
                amounts.fee_atomic as i64,
                amounts.net_atomic as i64,
                amounts.net_destination_atomic as i64,
                recipient,
                requester.map(|r| r.to_vec()),
                now,
                now + reservation_ttl_secs,
                SourceChain::Goldcoin,
                source_wallet,
            ],
        )?;
        let request_id = tx.last_insert_rowid();
        log_transition(
            &tx,
            request_id,
            None,
            RequestState::LiquidityReserved,
            now,
            None,
            "system",
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::LiquidityReserved),
            RequestState::AwaitingDeposit,
            now,
            None,
            "system",
        )?;
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1 WHERE direction = ?2",
            rusqlite::params![amounts.net_destination_atomic as i64, reserve],
        )?;
        tx.commit()?;
        Ok(CreateRequestOutcome::Reserved { request_id })
    }

    /// Sweeps `AwaitingDeposit`/`LiquidityReserved` requests past their
    /// `reservation_expires_at`, releasing their reserved capacity. Returns
    /// the number expired. Idempotent — a request already past `Expired`
    /// is never matched again by the `WHERE` clause.
    pub fn expire_reservations(&mut self, now: i64) -> Result<u32, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let mut stmt = tx.prepare(
            "SELECT id, direction, net_destination_atomic FROM bridge_requests
             WHERE state = 'AwaitingDeposit' AND reservation_expires_at IS NOT NULL
               AND reservation_expires_at <= ?1",
        )?;
        let rows: Vec<(i64, Direction, i64)> = stmt
            .query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        let mut count = 0u32;
        for (id, direction, amount) in rows {
            tx.execute(
                "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
                rusqlite::params![RequestState::Expired, id],
            )?;
            log_transition(
                &tx,
                id,
                Some(RequestState::AwaitingDeposit),
                RequestState::Expired,
                now,
                Some("reservation_ttl_elapsed"),
                "system",
            )?;
            tx.execute(
                "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity - ?1 WHERE direction = ?2",
                rusqlite::params![amount, direction.destination_reserve()],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }

    /// Operator/user cancellation before a deposit is observed. Same
    /// capacity-release effect as expiry, distinct reason.
    pub fn cancel_request(&mut self, id: i64, now: i64, note: &str) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, amount, state): (Direction, i64, RequestState) = tx
            .query_row(
                "SELECT direction, net_destination_atomic, state FROM bridge_requests WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(LedgerError::RequestNotFound(id))?;
        assert!(
            matches!(
                state,
                RequestState::LiquidityReserved | RequestState::AwaitingDeposit
            ),
            "cancel_request called on a request past reservation ({state:?}); caller bug"
        );
        tx.execute(
            "UPDATE bridge_requests SET state = ?1, manual_review_note = ?2 WHERE id = ?3",
            rusqlite::params![RequestState::Cancelled, note, id],
        )?;
        log_transition(
            &tx,
            id,
            Some(state),
            RequestState::Cancelled,
            now,
            Some(note),
            "operator",
        )?;
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity - ?1 WHERE direction = ?2",
            rusqlite::params![amount, direction.destination_reserve()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------ Goldcoin leg --

    /// Looks up a request by id, for the Goldcoin indexer's OP_RETURN-
    /// encoded-id correlation (docs/01-reuse-inventory.md notes this
    /// replaces recipient-only matching to remove FIFO ambiguity).
    pub fn get_request(&self, id: i64) -> Result<Option<BridgeRequest>, LedgerError> {
        self.conn
            .query_row(SELECT_REQUEST, [id], row_to_request)
            .optional()
            .map_err(LedgerError::from)
    }

    // --------------------------------------------- unique deposit addresses --
    //
    // Schema/ledger support for the OP_RETURN-replacement redesign.
    // `request_id` doubles as the derivation index (`goldcoin::
    // derivation`'s own docs) — nothing here derives an address itself;
    // callers compute it via `goldcoin::derivation::derive_request_vault`
    // and pass the result in. The indexer (`goldcoin::indexer`), the API
    // (`api::BridgeApi::create_goldcoin_deposit_transfer`), and the SolToGlc
    // payout path (`signing::goldcoin_vault::rederive_plan`) all read
    // these columns now.

    /// Assigns a freshly-derived Goldcoin deposit address to a
    /// GOLDCOIN-SOURCED request. Idempotent on an exact repeat (same
    /// address); fails closed — never silently overwrites — if the
    /// request already has a DIFFERENT address, or if its direction has
    /// no Goldcoin deposit step at all
    /// ([`Direction::source_is_goldcoin`]). The database-level
    /// partial unique index on `deposit_script_pubkey_hex`
    /// (`ux_bridge_requests_deposit_script`) is the actual, race-safe
    /// guarantee that no two requests are ever assigned the same
    /// deposit script — this method's own pre-check is a friendlier
    /// error message for the ordinary case, not the safety boundary
    /// itself.
    ///
    /// # Why the direction is checked but never CHOSEN here
    ///
    /// The row's direction is read, not written: a request is created
    /// as `GlcToSol` or as `GlcToRhn` by
    /// [`Ledger::create_request`] and is never mutated into the other
    /// afterwards. This method's contribution to that binding is the
    /// deposit script, which is derived from the request id alone
    /// (`goldcoin::derivation::derive_request_vault`) and is unique per
    /// request by the index above — so the script an operator or an
    /// indexer resolves leads back to exactly one row, carrying exactly
    /// one direction, for the life of the request.
    pub fn set_goldcoin_deposit_address(
        &mut self,
        request_id: i64,
        address: &str,
        script_pubkey_hex: &str,
        redeem_script_hex: &str,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(Direction, Option<String>)> = tx
            .query_row(
                "SELECT direction, deposit_address FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((direction, existing_address)) = row else {
            tx.rollback()?;
            return Err(LedgerError::RequestNotFound(request_id));
        };
        if !direction.source_is_goldcoin() {
            tx.rollback()?;
            return Err(LedgerError::NotAGoldcoinSourcedRequest {
                id: request_id,
                actual_direction: direction,
            });
        }
        if let Some(existing) = existing_address {
            tx.rollback()?;
            if existing == address {
                return Ok(());
            }
            return Err(LedgerError::DepositAddressAlreadySet {
                id: request_id,
                existing,
                attempted: address.to_string(),
            });
        }
        tx.execute(
            "UPDATE bridge_requests
             SET deposit_address = ?1, deposit_script_pubkey_hex = ?2, deposit_redeem_script_hex = ?3
             WHERE id = ?4",
            rusqlite::params![address, script_pubkey_hex, redeem_script_hex, request_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Resolves a live on-chain P2SH scriptPubKey to the
    /// Goldcoin-sourced request it was assigned to, if any — the
    /// indexer's address-based match step. `script_pubkey_hex` must be
    /// compared byte-for-byte as produced by
    /// [`crate::goldcoin::vault::MultisigVault::script_pubkey_hex`] —
    /// this does no normalization (matches this codebase's existing
    /// exact-match convention for the legacy `vault_script_hex`
    /// comparison in `goldcoin::deposit::vault_output_candidates`).
    ///
    /// Returns the request's DIRECTION alongside its id. The caller
    /// never has to infer it, and — because a script is unique to one
    /// request by `ux_bridge_requests_deposit_script` — the direction
    /// returned is the one that request was CREATED with. An address
    /// therefore witnesses its own route: a deposit paid to a
    /// `GlcToRhn` request's script can only ever resolve to that
    /// `GlcToRhn` row.
    pub fn find_goldcoin_deposit_request_by_script(
        &self,
        script_pubkey_hex: &str,
    ) -> Result<Option<(i64, Direction)>, LedgerError> {
        self.conn
            .query_row(
                &format!(
                    "SELECT id, direction FROM bridge_requests
                     WHERE direction IN {sources} AND deposit_script_pubkey_hex = ?1",
                    sources = Direction::SOURCE_IS_GOLDCOIN_SQL_IN
                ),
                [script_pubkey_hex],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Every deposit scriptPubKey ever assigned to a Goldcoin-sourced
    /// request, regardless of that request's current state — an indexer
    /// widening its watch-list needs the full historical set, not just
    /// currently-open requests, since a settled request's UTXO can still
    /// sit unswept at its derived address.
    pub fn all_goldcoin_deposit_script_pubkeys(&self) -> Result<Vec<String>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT deposit_script_pubkey_hex FROM bridge_requests
             WHERE direction IN {sources} AND deposit_script_pubkey_hex IS NOT NULL",
            sources = Direction::SOURCE_IS_GOLDCOIN_SQL_IN
        ))?;
        let rows: Result<Vec<String>, _> = stmt.query_map([], |r| r.get(0))?.collect();
        Ok(rows?)
    }

    /// Every deposit ADDRESS ever assigned to a Goldcoin-sourced request
    /// — same full-historical-set discipline as
    /// [`Ledger::all_goldcoin_deposit_script_pubkeys`], but returning the
    /// human-readable address `listunspent` actually accepts
    /// (`Orchestrator::watched_goldcoin_addresses`), not the scriptPubKey
    /// used for indexer-side matching.
    ///
    /// Covering BOTH Goldcoin-sourced directions is what makes a
    /// `GlcToRhn` deposit visible to the node at all: an address absent
    /// from this list is an address `list_unspent` is never asked about,
    /// so its UTXO would never reach `vault_utxos` and the reserve
    /// reconciliation would later read the real payment as an
    /// unexplained balance change.
    pub fn all_goldcoin_deposit_addresses(&self) -> Result<Vec<String>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT deposit_address FROM bridge_requests
             WHERE direction IN {sources} AND deposit_address IS NOT NULL",
            sources = Direction::SOURCE_IS_GOLDCOIN_SQL_IN
        ))?;
        let rows: Result<Vec<String>, _> = stmt.query_map([], |r| r.get(0))?.collect();
        Ok(rows?)
    }

    /// Raw `bridge_requests.destination_txid` bytes — a 64-byte Solana
    /// transaction signature for a `GlcToSol` release
    /// ([`Ledger::record_release_submitted`]) or a 32-byte Goldcoin txid
    /// for a `SolToGlc` payout ([`Ledger::record_goldcoin_payout_broadcast`]);
    /// length depends on direction, so this returns the raw bytes rather
    /// than a fixed-size array.
    pub fn get_destination_txid(&self, request_id: i64) -> Result<Option<Vec<u8>>, LedgerError> {
        Ok(self
            .conn
            .query_row(
                "SELECT destination_txid FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Records that a candidate Goldcoin deposit binds to `request_id`.
    /// Idempotent on `(source_txid, source_vout)` — calling this twice with
    /// the same observation after a restart returns `AlreadyRecorded`
    /// rather than erroring or double-counting.
    ///
    /// If `request_id` is already `Expired` (deposit arrived after the
    /// reservation TTL elapsed), this implements
    /// docs/04-state-machines.md's late-deposit auto-recreate: capacity is
    /// re-checked and, if available, re-reserved on the same request before
    /// continuing the flow normally (see [`GlcObservationOutcome::LateDepositRecreated`]);
    /// otherwise the request is routed to `ManualReview`
    /// ([`GlcObservationOutcome::LateDepositNoCapacity`]) rather than
    /// treated as an uncorrelated payment.
    ///
    /// [`Self::record_glc_deposit_observed_from`] with no traced funding
    /// wallets — for callers that have none (the deposit's inputs were
    /// not traceable, or a test). The destination window is still
    /// re-checked; the source window cannot be, and is not.
    #[allow(clippy::too_many_arguments)]
    pub fn record_glc_deposit_observed(
        &mut self,
        request_id: i64,
        txid: [u8; 32],
        vout: u32,
        observed_amount: u64,
        block_height: i64,
        block_hash: [u8; 32],
        now: i64,
    ) -> Result<GlcObservationOutcome, LedgerError> {
        self.record_glc_deposit_observed_from(
            request_id,
            txid,
            vout,
            observed_amount,
            block_height,
            block_hash,
            &[],
            now,
        )
    }

    /// [`Self::record_glc_deposit_observed`], with the wallets the deposit
    /// transaction was ACTUALLY funded from.
    ///
    /// # The wallet windows, enforced where the source is finally known
    ///
    /// A Goldcoin-sourced request is admitted (`create_request`) before
    /// any deposit exists, so the only source identity available then is
    /// whatever the caller declared. This is where the real one arrives:
    /// `source_wallets` is every distinct wallet the deposit's inputs
    /// were spent from, in input order, as traced by `goldcoin::indexer`
    /// from the prevouts (the address text of a standard P2PKH/P2SH
    /// script, the raw script bytes otherwise). Empty when nothing was
    /// traceable (a coinbase-funded deposit; a node response with no
    /// inputs).
    ///
    /// Both windows are evaluated inside this write transaction against
    /// every OTHER request in the window
    /// ([`WalletWindowScope::ExcludingRequest`]): EVERY traced input
    /// wallet for the source leg (a deposit combining an input from a
    /// wallet still inside its window is that wallet's second attempt),
    /// and the request's own `recipient` for the destination leg — again,
    /// because a sibling to the same destination can have been admitted
    /// while this request sat `Expired` and is only now being funded
    /// late. A blocked window parks the request in `ManualReview`
    /// ([`GlcObservationOutcome::WalletLimited`]) with the deposit's
    /// outpoint and amount witness recorded exactly as an amount mismatch
    /// records them, so it is refundable, and it is never advanced to
    /// `Confirming`; the reservation stays on the book until the refund
    /// releases it, as for every other Goldcoin-sourced park.
    ///
    /// The first traced wallet is recorded as the row's `source_wallet`
    /// (over any declared one — chain evidence outranks a claim), so
    /// from here on this request consumes the window of the wallet that
    /// really funded it.
    #[allow(clippy::too_many_arguments)]
    pub fn record_glc_deposit_observed_from(
        &mut self,
        request_id: i64,
        txid: [u8; 32],
        vout: u32,
        observed_amount: u64,
        block_height: i64,
        block_hash: [u8; 32],
        source_wallets: &[Vec<u8>],
        now: i64,
    ) -> Result<GlcObservationOutcome, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let mut recreated_from_expired = false;
        #[allow(clippy::type_complexity)]
        let row: Option<(Direction, RequestState, i64, i64, Option<Vec<u8>>, Vec<u8>)> = tx
            .query_row(
                "SELECT direction, state, gross_amount_atomic, net_destination_atomic, source_txid,
                        recipient
                 FROM bridge_requests WHERE id = ?1",
                [request_id],
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
            .optional()?;
        let Some((
            direction,
            mut state,
            reserved_amount,
            net_destination_atomic,
            existing_txid,
            recipient,
        )) = row
        else {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::NoMatchingRequest);
        };

        if (state == RequestState::DepositObserved || state == RequestState::Confirming)
            && existing_txid.as_deref() == Some(txid.as_slice())
        {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::AlreadyRecorded);
        }
        // A deposit may only bind to a request whose SOURCE leg is a
        // Goldcoin L1 payment. `GlcToSol` and `GlcToRhn` both are, and
        // are treated identically from here on — the direction decides
        // which reserve is drawn down and which settlement engine later
        // picks the request up, never how the deposit itself is
        // observed, counted or protected. `SolToGlc`/`RhnToGlc` have no
        // Goldcoin deposit step and fall out here rather than being
        // funded by a payment that was never meant for them.
        if !direction.source_is_goldcoin() {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::NoMatchingRequest);
        }

        // Late deposit: the reservation TTL elapsed before this deposit was
        // observed, but the Goldcoin payment is real and irreversible
        // (docs/04-state-machines.md "Open design item: late deposits after
        // expiry"). Never fold this into the uncorrelated-payment path
        // below (`NoMatchingRequest`) — the OP_RETURN binding already
        // resolved this to a specific request, so the request's own
        // capacity, not "any capacity", is what must be re-checked.
        if state == RequestState::Expired {
            let reserve = direction.destination_reserve();
            let (balance, protected_minimum, reserved_liquidity): (i64, i64, i64) = tx.query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
                 FROM reserve_ledger WHERE direction = ?1",
                [reserve],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let available = balance - protected_minimum - reserved_liquidity;
            if net_destination_atomic > available {
                tx.execute(
                    "UPDATE bridge_requests SET state = ?1, source_txid = ?2, source_vout = ?3,
                        source_block_height = ?4, source_block_hash = ?5, manual_review_note = ?6
                     WHERE id = ?7",
                    rusqlite::params![
                        RequestState::ManualReview,
                        txid.as_slice(),
                        vout,
                        block_height,
                        block_hash.as_slice(),
                        "late_deposit_no_capacity",
                        request_id,
                    ],
                )?;
                log_transition(
                    &tx,
                    request_id,
                    Some(RequestState::Expired),
                    RequestState::ManualReview,
                    now,
                    Some("late_deposit_no_capacity"),
                    "system",
                )?;
                tx.commit()?;
                return Ok(GlcObservationOutcome::LateDepositNoCapacity);
            }

            tx.execute(
                "UPDATE bridge_requests
                 SET state = ?1, reserved_at = ?2, reservation_expires_at = NULL
                 WHERE id = ?3",
                rusqlite::params![RequestState::AwaitingDeposit, now, request_id],
            )?;
            log_transition(
                &tx,
                request_id,
                Some(RequestState::Expired),
                RequestState::LiquidityReserved,
                now,
                Some("late_deposit_recreated"),
                "system",
            )?;
            log_transition(
                &tx,
                request_id,
                Some(RequestState::LiquidityReserved),
                RequestState::AwaitingDeposit,
                now,
                None,
                "system",
            )?;
            tx.execute(
                "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1 WHERE direction = ?2",
                rusqlite::params![net_destination_atomic, reserve],
            )?;
            state = RequestState::AwaitingDeposit;
            recreated_from_expired = true;
        }

        if state != RequestState::AwaitingDeposit {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::NoMatchingRequest);
        }

        if observed_amount != reserved_amount as u64 {
            // `observed_amount_atomic` (schema v20) is written HERE, in
            // the same statement and the same transaction as the source
            // outpoint and the ManualReview transition. That coupling is
            // the point: a crash can leave both or neither, never an
            // outpoint whose amount witness is missing.
            //
            // The value is what the INDEXER independently decoded from
            // the transaction output. It is never taken from the note
            // below (that string is an operator-readable message, not
            // evidence) and never from operator input. A later refund
            // requires a fresh RPC read to equal it exactly — two
            // independent observations of one fact.
            tx.execute(
                "UPDATE bridge_requests SET state = ?1, source_txid = ?2, source_vout = ?3,
                    source_block_height = ?4, source_block_hash = ?5, manual_review_note = ?6,
                    observed_amount_atomic = ?7
                 WHERE id = ?8",
                rusqlite::params![
                    RequestState::ManualReview,
                    txid.as_slice(),
                    vout,
                    block_height,
                    block_hash.as_slice(),
                    format!("deposit_amount_mismatch: expected {reserved_amount} observed {observed_amount}"),
                    observed_amount,
                    request_id,
                ],
            )?;
            log_transition(
                &tx,
                request_id,
                Some(RequestState::AwaitingDeposit),
                RequestState::ManualReview,
                now,
                Some("deposit_amount_mismatch"),
                "system",
            )?;
            tx.commit()?;
            return Ok(GlcObservationOutcome::AmountMismatch {
                expected: reserved_amount as u64,
                observed: observed_amount,
            });
        }

        // The wallet windows, against the wallets that really funded this
        // deposit (see the function docs). Source first, matching every
        // fold's ranking; the newest blocker across every traced input
        // decides the reopen time.
        let traced: Vec<&[u8]> = {
            let mut seen: Vec<&[u8]> = Vec::new();
            for wallet in source_wallets {
                if !wallet.is_empty() && !seen.contains(&wallet.as_slice()) {
                    seen.push(wallet.as_slice());
                }
            }
            seen
        };
        let mut source_retry_after: Option<i64> = None;
        for wallet in &traced {
            if let Some(retry_after) = Self::wallet_window_retry_after_in(
                &tx,
                direction.source_chain(),
                WalletRole::Source,
                wallet,
                now,
                WalletWindowScope::ExcludingRequest(request_id),
            )? {
                source_retry_after =
                    Some(source_retry_after.map_or(retry_after, |t: i64| t.max(retry_after)));
            }
        }
        let destination_retry_after = Self::wallet_window_retry_after_in(
            &tx,
            direction.destination_chain(),
            WalletRole::Destination,
            &recipient,
            now,
            WalletWindowScope::ExcludingRequest(request_id),
        )?;
        let eligibility = RouteWalletEligibility {
            source_retry_after,
            destination_retry_after,
        };
        let primary_source_wallet: Option<&[u8]> = traced.first().copied();
        // The rapid-burst rule (`ledger::rapid_burst`, schema v30),
        // ranked above the wallet windows: a deposit matching both is
        // held (non-self-clearing) rather than parked for 24 hours.
        // Every traced funding wallet is asked; the recipient once.
        let mut burst: Option<RapidBurstMatch> = None;
        let burst_sources: Vec<Option<&[u8]>> = if traced.is_empty() {
            vec![None]
        } else {
            traced.iter().map(|w| Some(*w)).collect()
        };
        for wallet in burst_sources {
            burst = Self::rapid_burst_verdict_in(
                &tx,
                direction,
                wallet,
                Some(&recipient),
                now,
                WalletWindowScope::ExcludingRequest(request_id),
            )?;
            if burst.is_some() {
                break;
            }
        }
        if let Some(matched) = burst {
            let reason = Self::MANUAL_REVIEW_REASON_RAPID_BURST_HOLD;
            tx.execute(
                "UPDATE bridge_requests SET state = ?1, source_txid = ?2, source_vout = ?3,
                    source_block_height = ?4, source_block_hash = ?5, manual_review_note = ?6,
                    observed_amount_atomic = ?7,
                    source_wallet = COALESCE(?8, source_wallet)
                 WHERE id = ?9",
                rusqlite::params![
                    RequestState::ManualReview,
                    txid.as_slice(),
                    vout,
                    block_height,
                    block_hash.as_slice(),
                    reason,
                    observed_amount,
                    primary_source_wallet,
                    request_id,
                ],
            )?;
            log_transition(
                &tx,
                request_id,
                Some(RequestState::AwaitingDeposit),
                RequestState::ManualReview,
                now,
                Some(reason),
                "system",
            )?;
            Self::mark_rapid_burst_hold_in(&tx, request_id, &matched, now)?;
            let review_after: i64 = tx.query_row(
                "SELECT review_after FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )?;
            tx.commit()?;
            return Ok(GlcObservationOutcome::RapidBurstHeld {
                rule: matched.rule,
                review_after,
            });
        }
        if let Some((role, retry_after)) = eligibility.blocker() {
            let reason = role.limit_reason();
            // Same durable evidence an amount-mismatch park records, in
            // the same statement: the outpoint, the block, and the
            // independently decoded amount witness — so a refund can be
            // built from chain-verified facts, never from this note.
            tx.execute(
                "UPDATE bridge_requests SET state = ?1, source_txid = ?2, source_vout = ?3,
                    source_block_height = ?4, source_block_hash = ?5, manual_review_note = ?6,
                    observed_amount_atomic = ?7,
                    source_wallet = COALESCE(?8, source_wallet)
                 WHERE id = ?9",
                rusqlite::params![
                    RequestState::ManualReview,
                    txid.as_slice(),
                    vout,
                    block_height,
                    block_hash.as_slice(),
                    reason,
                    observed_amount,
                    primary_source_wallet,
                    request_id,
                ],
            )?;
            log_transition(
                &tx,
                request_id,
                Some(RequestState::AwaitingDeposit),
                RequestState::ManualReview,
                now,
                Some(reason),
                "system",
            )?;
            tx.commit()?;
            return Ok(GlcObservationOutcome::WalletLimited {
                reason,
                retry_after,
            });
        }

        tx.execute(
            "UPDATE bridge_requests SET state = ?1, source_txid = ?2, source_vout = ?3,
                source_block_height = ?4, source_block_hash = ?5, source_confirmations = 1,
                source_wallet = COALESCE(?6, source_wallet)
             WHERE id = ?7",
            rusqlite::params![
                RequestState::DepositObserved,
                txid.as_slice(),
                vout,
                block_height,
                block_hash.as_slice(),
                primary_source_wallet,
                request_id,
            ],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::AwaitingDeposit),
            RequestState::DepositObserved,
            now,
            None,
            "system",
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::DepositObserved),
            RequestState::Confirming,
            now,
            None,
            "system",
        )?;
        tx.execute(
            "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![RequestState::Confirming, request_id],
        )?;
        tx.commit()?;
        if recreated_from_expired {
            Ok(GlcObservationOutcome::LateDepositRecreated)
        } else {
            Ok(GlcObservationOutcome::Recorded)
        }
    }

    /// Creates `unmatched_goldcoin_deposits` if it doesn't exist yet (fresh
    /// database) and ensures the `reconciled_at` column is present
    /// (idempotent `ALTER`, same `column_exists`-guarded discipline as
    /// `schema::apply_v9` — this table lives outside the versioned
    /// schema-migration system, created ad hoc on first use, so it needs
    /// its own idempotent-columnar handling rather than a numbered
    /// migration). Never drops or recreates anything already there.
    fn ensure_unmatched_goldcoin_deposits_table(conn: &Connection) -> Result<(), LedgerError> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS unmatched_goldcoin_deposits (
                id INTEGER PRIMARY KEY, txid BLOB NOT NULL, vout INTEGER NOT NULL,
                amount_atomic INTEGER NOT NULL, block_height INTEGER NOT NULL,
                reason TEXT NOT NULL, discovered_at INTEGER NOT NULL,
                reconciled_at INTEGER, reconciliation_note TEXT,
                UNIQUE(txid, vout)
             )",
            [],
        )?;
        if !schema::column_exists(conn, "unmatched_goldcoin_deposits", "reconciled_at")? {
            conn.execute(
                "ALTER TABLE unmatched_goldcoin_deposits ADD COLUMN reconciled_at INTEGER",
                [],
            )?;
        }
        if !schema::column_exists(conn, "unmatched_goldcoin_deposits", "reconciliation_note")? {
            conn.execute(
                "ALTER TABLE unmatched_goldcoin_deposits ADD COLUMN reconciliation_note TEXT",
                [],
            )?;
        }
        Ok(())
    }

    /// A real vault payment that could not be matched to any pending
    /// request — recorded for audit rather than dropped (constraint: never
    /// silently ignore a real chain observation).
    pub fn record_unmatched_goldcoin_deposit(
        &mut self,
        txid: [u8; 32],
        vout: u32,
        amount_atomic: u64,
        block_height: i64,
        reason: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        Self::ensure_unmatched_goldcoin_deposits_table(&self.conn)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO unmatched_goldcoin_deposits
                (txid, vout, amount_atomic, block_height, reason, discovered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                txid.as_slice(),
                vout,
                amount_atomic as i64,
                block_height,
                reason,
                now
            ],
        )?;
        Ok(())
    }

    /// Marks a previously-recorded unmatched deposit `reconciled_at = now`
    /// — never deletes the row, so the audit history stays intact
    /// (docs/09-runbook.md's "Vault UTXO splitting" section). Refuses (no
    /// override) unless `(txid, vout, amount_atomic)` exactly matches an
    /// expected output of a `Broadcast` `vault_utxo_splits` transaction —
    /// the same [`crate::goldcoin::split::matches_expected_split_output`]
    /// check `goldcoin::indexer` uses to recognize a split output live, so
    /// this can retroactively reconcile a row recorded before that
    /// recognition existed. Idempotent: reconciling an already-reconciled
    /// row again is a safe no-op reporting so, not a second write.
    pub fn reconcile_unmatched_goldcoin_deposit(
        &mut self,
        txid: [u8; 32],
        vout: u32,
        note: &str,
        now: i64,
    ) -> Result<ReconcileUnmatchedDepositOutcome, LedgerError> {
        Self::ensure_unmatched_goldcoin_deposits_table(&self.conn)?;
        let tx = write_tx(&mut self.conn)?;

        let row: Option<(i64, Option<i64>)> = tx
            .query_row(
                "SELECT amount_atomic, reconciled_at FROM unmatched_goldcoin_deposits
                 WHERE txid = ?1 AND vout = ?2",
                rusqlite::params![txid.as_slice(), vout],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((amount_atomic, reconciled_at)) = row else {
            tx.rollback()?;
            return Err(LedgerError::UnmatchedDepositNotFound { txid, vout });
        };
        if reconciled_at.is_some() {
            tx.rollback()?;
            return Ok(ReconcileUnmatchedDepositOutcome::AlreadyReconciled);
        }

        let split: Option<(i64, i64, i64)> = tx
            .query_row(
                "SELECT source_amount_atomic, fee_atomic, chunk_count
                 FROM vault_utxo_splits WHERE txid = ?1 AND state IN ('Broadcast','Confirmed')",
                [txid.as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((source_amount_atomic, fee_atomic, chunk_count)) = split else {
            tx.rollback()?;
            return Err(LedgerError::UnmatchedDepositNotAKnownSplitOutput { txid, vout });
        };
        let matches = crate::goldcoin::split::matches_expected_split_output(
            source_amount_atomic as u64,
            fee_atomic as u64,
            chunk_count as u64,
            vout,
            amount_atomic as u64,
        );
        if !matches {
            tx.rollback()?;
            return Err(LedgerError::UnmatchedDepositNotAKnownSplitOutput { txid, vout });
        }

        tx.execute(
            "UPDATE unmatched_goldcoin_deposits SET reconciled_at = ?1, reconciliation_note = ?2
             WHERE txid = ?3 AND vout = ?4",
            rusqlite::params![now, note, txid.as_slice(), vout],
        )?;
        tx.commit()?;
        Ok(ReconcileUnmatchedDepositOutcome::Reconciled)
    }

    /// The already-persisted figures a `Broadcast` `vault_utxo_splits`
    /// transaction's output list was deterministically built from — what
    /// [`crate::goldcoin::split::matches_expected_split_output`] needs to
    /// reproduce that exact output list independently, from `txid` alone.
    pub fn get_broadcast_vault_utxo_split(
        &self,
        split_txid: [u8; 32],
    ) -> Result<Option<BroadcastVaultUtxoSplit>, LedgerError> {
        self.conn
            .query_row(
                "SELECT source_amount_atomic, fee_atomic, chunk_count
                 FROM vault_utxo_splits WHERE txid = ?1 AND state IN ('Broadcast','Confirmed')",
                [split_txid.as_slice()],
                |r| {
                    Ok(BroadcastVaultUtxoSplit {
                        source_amount_atomic: r.get::<_, i64>(0)? as u64,
                        fee_atomic: r.get::<_, i64>(1)? as u64,
                        chunk_count: r.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Updates confirmation depth for a `Confirming` request; a no-op if
    /// the depth hasn't increased (idempotent under repeated ticks).
    pub fn update_glc_confirmations(
        &mut self,
        request_id: i64,
        confirmations: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE bridge_requests SET source_confirmations = ?1
             WHERE id = ?2 AND state = 'Confirming' AND source_confirmations < ?1",
            rusqlite::params![confirmations, request_id],
        )?;
        Ok(())
    }

    /// `Confirming -> SourceFinalized`: the source deposit is now treated as
    /// an irreversible fact. Moves the amount into `pending_obligations`
    /// (docs/05: committed exposure that can no longer safely expire).
    /// Idempotent: a no-op if already `SourceFinalized`.
    pub fn mark_glc_source_finalized(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(Direction, RequestState, i64)> = tx
            .query_row(
                "SELECT direction, state, net_destination_atomic FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((direction, state, amount)) = row else {
            tx.rollback()?;
            return Err(LedgerError::RequestNotFound(request_id));
        };
        if state == RequestState::SourceFinalized {
            tx.rollback()?;
            return Ok(());
        }
        assert_eq!(
            state,
            RequestState::Confirming,
            "mark_glc_source_finalized on unexpected state"
        );
        tx.execute(
            "UPDATE bridge_requests SET state = ?1, source_finalized_at = ?2 WHERE id = ?3",
            rusqlite::params![RequestState::SourceFinalized, now, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::SourceFinalized,
            now,
            None,
            "system",
        )?;
        tx.execute(
            "UPDATE reserve_ledger SET pending_obligations = pending_obligations + ?1 WHERE direction = ?2",
            rusqlite::params![amount, direction.destination_reserve()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Confirming -> ManualReview`: the vault output backing this
    /// GlcToSol deposit was found already spent when re-checked at
    /// confirmation depth — anomalous, not a routine reorg (that path is
    /// `find_fork_point`/rollback, which returns the request to
    /// `AwaitingDeposit`, not here). This happens when a concurrent
    /// SolToGlc payout's coin selection picks the same vault UTXO before
    /// this GlcToSol deposit reaches `SourceFinalized`; prevention lives in
    /// `available_vault_utxos` (excluding UTXOs still backing a
    /// non-finalized GlcToSol deposit), but this is the required fail-
    /// closed backstop for any case that slips past it (e.g. a UTXO spent
    /// by something outside this service's own payout path). Never
    /// silently left in `Confirming` forever — the previous behavior was
    /// to warn and continue, permanently stranding the request and its
    /// reservation with no operator-visible terminal state. No
    /// `reserve_ledger` accounting changes here: the request was never
    /// `SourceFinalized`, so `pending_obligations` was never incremented
    /// for it — same as every other pre-finalization ManualReview
    /// transition in this module. Idempotent: a no-op if already
    /// `ManualReview`.
    pub fn mark_glc_deposit_spent_before_finalized(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<RequestState> = tx
            .query_row(
                "SELECT state FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = row else {
            tx.rollback()?;
            return Err(LedgerError::RequestNotFound(request_id));
        };
        if state == RequestState::ManualReview {
            tx.rollback()?;
            return Ok(());
        }
        assert_eq!(
            state,
            RequestState::Confirming,
            "mark_glc_deposit_spent_before_finalized on unexpected state"
        );
        tx.execute(
            "UPDATE bridge_requests SET state = ?1, manual_review_note = ?2 WHERE id = ?3",
            rusqlite::params![
                RequestState::ManualReview,
                "deposit_spent_before_finalized",
                request_id
            ],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::ManualReview,
            now,
            Some("deposit_spent_before_finalized"),
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Pre-finality reorg: the block carrying the deposit was orphaned.
    /// Releases the source-txid claim and returns the request to
    /// `AwaitingDeposit` so a future re-observation (same or different
    /// qualifying transaction) can bind cleanly — a documented
    /// simplification of docs/04-state-machines.md's "retry via Confirming
    /// if the tx still exists" vs "AwaitingDeposit if gone" distinction:
    /// this always retries via `AwaitingDeposit`, which is safe (the next
    /// indexer tick re-discovers the deposit if it is still valid, in
    /// whichever block it ends up mined in) at the cost of one extra
    /// confirmation cycle in the same-block-different-branch case.
    /// Reserved liquidity is NOT released (the reservation is still live,
    /// just waiting for a fresh confirmation) — only the source binding is
    /// cleared. Never callable once `SourceFinalized` (irreversible by
    /// policy; see docs/10-threat-model.md's post-finality-reorg section).
    pub fn mark_glc_reorged(&mut self, request_id: i64, now: i64) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let state: RequestState = tx
            .query_row(
                "SELECT state FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(LedgerError::RequestNotFound(request_id))?;
        assert!(
            matches!(
                state,
                RequestState::DepositObserved | RequestState::Confirming
            ),
            "mark_glc_reorged called post-finality or pre-observation ({state:?}) — caller bug; \
             post-finality reorg must never auto-revert (docs/10-threat-model.md)"
        );
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::Reorged,
            now,
            Some("block_orphaned"),
            "system",
        )?;
        tx.execute(
            "UPDATE bridge_requests SET state = ?1, source_txid = NULL, source_vout = NULL,
                source_block_height = NULL, source_block_hash = NULL, source_confirmations = 0
             WHERE id = ?2",
            rusqlite::params![RequestState::AwaitingDeposit, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::Reorged),
            RequestState::AwaitingDeposit,
            now,
            None,
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    // -------------------------------------------------------------- Solana leg --

    /// `bridge_requests.manual_review_note` values [`Ledger::fold_sol_deposit`]
    /// can produce for a `SolToGlc` request — the exact, exhaustive set
    /// [`Ledger::resume_manual_review_sol_to_glc`]'s allowlist recognizes as
    /// structurally recoverable (every one of them means "capacity/gating
    /// was the only problem," never a data-integrity or fraud concern).
    /// Shared here so the two functions can never drift apart on the exact
    /// string values.
    const MANUAL_REVIEW_REASON_ADMISSION_CLOSED: &str = "admission_closed_at_fold";
    /// An operator closed THIS ROUTE's own admission gate
    /// (`glc-admin route-admission-close`, schema v25's
    /// `route_admission` table) while the reserve itself would still
    /// have admitted the deposit.
    ///
    /// Distinct from [`Self::MANUAL_REVIEW_REASON_ADMISSION_CLOSED`] on
    /// purpose, and the distinction is the point of the whole
    /// route-scoped axis: an operator must be able to tell "I closed
    /// inbound-to-Goldcoin entirely" apart from "I closed only
    /// `RhnToGlc` and left `SolToGlc` running", because the remedies are
    /// different commands against different state. Collapsing them would
    /// reintroduce exactly the ambiguity v25 exists to remove.
    ///
    /// Recoverable and refundable on the same terms as every other
    /// fold-time park — see [`Self::RECOVERABLE_MANUAL_REVIEW_REASONS`]
    /// and [`Self::REFUNDABLE_MANUAL_REVIEW_REASONS`]. Deliberately NOT
    /// auto-resumable ([`Self::is_auto_resumable_manual_review_reason`]):
    /// an operator closed this on purpose, and a background pass must
    /// not undo that — the same treatment `admission_closed_at_fold` and
    /// `reserve_paused_at_fold` get.
    pub(crate) const MANUAL_REVIEW_REASON_ROUTE_ADMISSION_CLOSED: &str =
        "route_admission_closed_at_fold";
    const MANUAL_REVIEW_REASON_PAUSED: &str = "reserve_paused_at_fold";
    const MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY: &str = "insufficient_capacity_at_fold";
    /// A finalized Robinhood deposit arrived while its route was closed
    /// by [`crate::routes::RouteGate`].
    ///
    /// Deliberately NOT a refusal to fold. The deposit already happened
    /// and is irreversible; declining to record it would leave real money
    /// in the custody contract with no ledger row, no reserve accounting
    /// and no refund path. See `crate::robinhood::fold`'s module docs.
    pub(crate) const MANUAL_REVIEW_REASON_ROUTE_DISABLED: &str = "route_disabled_at_fold";
    /// Distinct from `MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY`
    /// (accounting-figure exhaustion): this means the mature, unreserved
    /// vault UTXO POOL itself would run dangerously thin — the exact
    /// production incident this reason exists to prevent (many payouts,
    /// each consuming a mature UTXO and creating immature change, draining
    /// the pool faster than 6-confirmation maturity replenished it) — see
    /// `Ledger::set_utxo_pool_thresholds`/`Ledger::utxo_pool_health`.
    /// `pub(crate)`, not private: `Orchestrator`'s automatic-recovery phase
    /// (`tick_auto_resume_utxo_liquidity_backlog`) filters
    /// `ManualReview`-parked requests down to exactly this reason, and must
    /// read it from here rather than a duplicated string literal, so the
    /// two can never drift apart.
    pub(crate) const MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW: &str = "utxo_liquidity_low_at_fold";
    /// Distinct from BOTH capacity reasons above: the accounting figure
    /// is not exhausted (`insufficient_capacity`) and the mature UTXO
    /// pool is not thin (`utxo_liquidity_low`) — confirmed unreserved
    /// headroom has simply fallen into (or would be pushed into) the
    /// admission safety buffer that sits ON TOP of `protected_minimum`
    /// (docs/09-runbook.md's "Confirmed-liquidity admission safety
    /// buffer"). Kept as its own reason precisely so an operator can tell
    /// "we are near the hard floor" apart from "we are at it": this one
    /// fires while the reserve is still perfectly solvent and every
    /// already-accepted obligation is still processing normally.
    ///
    /// Auto-resumable, but — unlike the other self-clearing reasons —
    /// only while the direction-wide confirmed-liquidity gate is OPEN.
    /// See [`Ledger::is_auto_resumable_manual_review_reason`], which is
    /// where that condition lives, and the note there on why gating on
    /// the gate USES the hysteresis rather than defeating it: the gate
    /// reopens only on a genuine recovery to `admission_reopen_atomic`,
    /// never on a single reading that ticks back over the close
    /// threshold. `pub(crate)` for the same reason as
    /// `MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW`.
    pub(crate) const MANUAL_REVIEW_REASON_LIQUIDITY_BUFFER_LOW: &str =
        "liquidity_buffer_low_at_fold";
    /// The SOURCE wallet of this request already backed another bridge
    /// request created inside the rolling [`Self::WALLET_WINDOW_SECS`]
    /// window on its chain — the wallet uniqueness rule
    /// (`ledger::wallet_window`), applied on every route. Written by every
    /// fold and by the Goldcoin deposit observation when the source
    /// deposit is already on-chain; a not-yet-funded `POST /transfers`
    /// is refused with the same reason instead of being recorded.
    ///
    /// Self-clearing: the blocking request ages out of the window 24
    /// hours after its `created_at`, after which the parked request is
    /// resumable (and, on the Goldcoin-bound routes, auto-resumed) or
    /// refundable. `pub(crate)` for the same reason as
    /// `MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW`: `Orchestrator`'s
    /// automatic-recovery phase filters on exactly this reason and must
    /// read it from here, never a duplicated string literal.
    pub(crate) const MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT: &str = "wallet_source_24h_limit";
    /// The DESTINATION wallet of this request already backed another
    /// bridge request created inside the rolling
    /// [`Self::WALLET_WINDOW_SECS`] window on its chain — the destination
    /// half of the same rule, keyed on `recipient`, enforced ALONGSIDE the
    /// source half (never replacing it): one closes the bypass where a
    /// wallet spreads deposits across many destinations, the other the
    /// bypass where many wallets feed one destination. Same lifecycle as
    /// [`Self::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT`].
    pub(crate) const MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT: &str =
        "wallet_destination_24h_limit";
    /// The pre-generalization spelling of
    /// [`Self::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT`], as it
    /// stands on every `SolToGlc`/`RhnToGlc` row parked before the rule
    /// was generalized. Never written any more; still RECOGNIZED by every
    /// list below, so an existing park keeps its exits — the exact
    /// failure the 2026-09-02 `liquidity_buffer_low_at_fold` incident
    /// recorded on `RECOVERABLE_MANUAL_REVIEW_REASONS` is a reason that
    /// exists on rows but not in a list.
    pub(crate) const LEGACY_MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED: &str =
        "recipient_rate_limited";
    /// The pre-generalization spelling of
    /// [`Self::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT`]. Never
    /// written any more; recognized everywhere, for the same reason.
    pub(crate) const LEGACY_MANUAL_REVIEW_REASON_SOURCE_WALLET_RATE_LIMITED: &str =
        "source_wallet_rate_limited";
    /// The deposit matched the configured rapid-burst rule at fold time
    /// (`ledger::rapid_burst`, schema v30). Written together with
    /// `manual_review_disposition = rapid_burst_hold` and the hold
    /// columns, on a deposit that is custodied and finalized. NEVER
    /// auto-resumed and NEVER auto-refunded: the row leaves
    /// `ManualReview` only through an explicit operator `process` or
    /// `refund` decision, normally not before `review_after`.
    pub const MANUAL_REVIEW_REASON_RAPID_BURST_HOLD: &'static str = "rapid_burst_hold";
    /// Whether `note` is one of the wallet-window park reasons, in either
    /// spelling — the one predicate every list and every filter below
    /// asks, so the legacy spellings cannot be dropped from one of them
    /// by accident.
    pub fn is_wallet_window_manual_review_reason(note: &str) -> bool {
        matches!(
            note,
            Self::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT
                | Self::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT
                | Self::LEGACY_MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED
                | Self::LEGACY_MANUAL_REVIEW_REASON_SOURCE_WALLET_RATE_LIMITED
        )
    }
    /// The `manual_review_note` values a ManualReview request may carry
    /// and still be recoverable into normal Goldcoin settlement — the
    /// same seven fold-time reasons
    /// [`Ledger::resume_manual_review_sol_to_glc`] accepts, which reads
    /// this constant through [`Ledger::is_recoverable_manual_review_reason`]
    /// rather than carrying its own `matches!` arm list. That is the
    /// point: the enforcing path and every discovery surface now consult
    /// ONE list, so a reason added for one cannot be missed by the other.
    ///
    /// It could before. `liquidity_buffer_low_at_fold` was added to the
    /// resume path's inline `matches!` (and to
    /// [`Self::REFUNDABLE_MANUAL_REVIEW_REASONS`]) when the admission
    /// safety buffer landed on 2026-09-02, but not here — so
    /// `manual-review-settle-list`, which filters on this constant,
    /// reported "no ManualReview requests are currently recoverable"
    /// while `manual-review-settle --request-id N` on those very requests
    /// answered WOULD RE-ADMIT. Three production SolToGlc requests were
    /// invisible to the listing for two days. The single-source refactor
    /// and the both-directions drift guard
    /// (`ledger::tests::resume_acceptance_matches_the_recoverable_reason_list`)
    /// exist so that specific failure cannot recur: the old guard only
    /// checked that every LISTED reason is accepted, never that every
    /// ACCEPTED reason is listed, which is the direction that broke.
    pub const RECOVERABLE_MANUAL_REVIEW_REASONS: [&'static str; 11] = [
        Self::MANUAL_REVIEW_REASON_ADMISSION_CLOSED,
        // The route-scoped twin of the reserve-wide reason above, and
        // recoverable for exactly the same reason: gating was the only
        // problem. Listed here from the day the gate shipped, rather
        // than left for a later patch — the 2026-09-02
        // `liquidity_buffer_low_at_fold` incident recorded above is what
        // happens when a new fold reason reaches production before this
        // list does, and a park with no exit is strictly worse when the
        // parking was an operator's own deliberate act.
        Self::MANUAL_REVIEW_REASON_ROUTE_ADMISSION_CLOSED,
        Self::MANUAL_REVIEW_REASON_PAUSED,
        Self::MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY,
        Self::MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW,
        // A park held back by the confirmed-liquidity admission safety
        // buffer is as transient and self-clearing as the UTXO-count one
        // above: the identical resume succeeds once headroom recovers,
        // and the resume path re-checks the buffer arithmetic for itself
        // on every attempt, so listing it here grants no bypass.
        Self::MANUAL_REVIEW_REASON_LIQUIDITY_BUFFER_LOW,
        // The wallet uniqueness windows — self-clearing 24-hour holds —
        // in both the current spelling and the one every park written
        // before the rule was generalized still carries.
        Self::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT,
        Self::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT,
        Self::LEGACY_MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED,
        Self::LEGACY_MANUAL_REVIEW_REASON_SOURCE_WALLET_RATE_LIMITED,
        // A rapid-burst hold (schema v30) is recoverable ONLY through the
        // explicit `process` decision, which clears the hold marker in
        // the same transaction as the resume. Listing it here is what
        // lets that resume succeed; the hold marker — refused first by
        // every resume entry point and skipped outright by the automatic
        // pass (`is_auto_resumable_manual_review_reason` never returns
        // true for it) — is what keeps everything else out.
        Self::MANUAL_REVIEW_REASON_RAPID_BURST_HOLD,
    ];

    /// The single canonical answer to "is this `manual_review_note` a
    /// recoverable fold-time reason?", read from
    /// [`Self::RECOVERABLE_MANUAL_REVIEW_REASONS`].
    ///
    /// Used by [`Ledger::resume_manual_review_sol_to_glc`] itself — the
    /// enforcing path — and by every read-only discovery surface
    /// (`solana::manual_review_settle::list_candidates`). A `None` note
    /// is never recoverable: an unknown or absent reason is excluded,
    /// never broadened.
    ///
    /// This is a REASON predicate only, not an eligibility verdict.
    /// Nothing here says the request can actually be re-admitted right
    /// now — that answer comes only from trialling the real resume
    /// ([`Ledger::dry_run_resume_manual_review`]), which re-checks state,
    /// the refund lifecycle, both rolling-24h windows, the mature-UTXO
    /// floor, the safety buffer and the reserve invariant against live
    /// state. Discovery must never re-derive any of those for itself.
    pub fn is_recoverable_manual_review_reason(note: Option<&str>) -> bool {
        note.is_some_and(|r| Self::RECOVERABLE_MANUAL_REVIEW_REASONS.contains(&r))
    }

    /// Which of the recoverable reasons the daemon's UNATTENDED
    /// auto-resume pass (`Orchestrator::tick_auto_resume_utxo_liquidity_
    /// backlog`) may even consider — a strict subset of
    /// [`Self::RECOVERABLE_MANUAL_REVIEW_REASONS`], because "an operator
    /// may recover this" and "a background pass may recover this
    /// unwatched" are different questions.
    ///
    /// Self-clearing, always considered:
    /// `utxo_liquidity_low_at_fold` (the mature pool refills), and the
    /// wallet-window parks `wallet_source_24h_limit`/
    /// `wallet_destination_24h_limit` — plus their legacy spellings
    /// `recipient_rate_limited`/`source_wallet_rate_limited` — (the
    /// rolling 24-hour windows age out).
    ///
    /// Self-clearing, considered only while `liquidity_admission_open`:
    /// `liquidity_buffer_low_at_fold`. The condition that parked it —
    /// confirmed unreserved headroom inside the admission safety buffer —
    /// clears on its own exactly like the other three, so leaving it out
    /// entirely meant a request parked by the buffer sat in `ManualReview`
    /// until a human noticed, even after the reserve had fully recovered.
    /// Gating on the direction-wide gate is what makes retrying it safe:
    /// that gate is the hysteresis (closed below `admission_buffer_
    /// atomic`, reopening only at `admission_reopen_atomic`, held in
    /// between), so consulting it means recovery waits for a GENUINE
    /// recovery to the reopen threshold rather than firing the instant
    /// headroom ticks back over the close line. The buffer's own
    /// per-request arithmetic is still re-checked inside
    /// [`Ledger::resume_manual_review_sol_to_glc`] on every attempt, so
    /// no invariant, floor or buffer is weakened by this: the gate only
    /// decides whether it is worth ASKING.
    ///
    /// Never auto-resumed, at all: `admission_closed_at_fold` and
    /// `reserve_paused_at_fold` (an operator closed something
    /// deliberately, and a background pass must not undo that), and
    /// `insufficient_capacity_at_fold` (the accounting reserve is
    /// genuinely exhausted; a human should look at why).
    pub(crate) fn is_auto_resumable_manual_review_reason(
        note: Option<&str>,
        liquidity_admission_open: bool,
    ) -> bool {
        match note {
            Some(Self::MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW) => true,
            Some(Self::MANUAL_REVIEW_REASON_LIQUIDITY_BUFFER_LOW) => liquidity_admission_open,
            Some(reason) => Self::is_wallet_window_manual_review_reason(reason),
            None => false,
        }
    }

    /// The `bridge_requests.state` values that do NOT consume a
    /// wallet window — the single shared exclude-list behind every
    /// (chain, role) limiter on every route, in SQL `IN` form.
    ///
    /// An EXCLUDE-list, never an include-list: a state added in future
    /// therefore defaults to COUNTING against the window (the safe
    /// direction) rather than being silently ignored. Every entry is a
    /// terminal state that never produced, and now never will produce, a
    /// real payout. `Failed`/`DestinationSubmissionFailed`/
    /// `InsufficientReserveAtSettlement` are defined but never set
    /// anywhere in this codebase today, and `Cancelled`/`Expired`/
    /// `Reorged` are structurally unreachable for an inbound-to-Goldcoin
    /// route — all six are listed anyway, defensively, since they clearly
    /// represent "no payout resulted."
    ///
    /// Note which states are deliberately ABSENT: the refund lifecycle
    /// (`RefundPending`/`RefundBroadcast`/`Refunded`) is NOT excluded, so
    /// a refunded deposit still consumes its recipient's and its source
    /// wallet's window for the full 24 hours. That is the long-standing
    /// `SolToGlc` behaviour and it is preserved verbatim for `RhnToGlc`:
    /// a refund means the service declined to complete the transfer, not
    /// that the deposit never happened, and letting a refund reset the
    /// window would hand an abuser a free retry on demand. `ManualReview`
    /// is likewise absent — a parked row blocks the next arrival, which
    /// is what makes the queue drain oldest-first.
    ///
    /// Lives here as ONE literal because it is interpolated into every
    /// spelling of the one window query
    /// (`Ledger::wallet_window_blocker_created_at`). Hand-typed copies of
    /// a security predicate is exactly the drift this codebase keeps
    /// designing out.
    pub(crate) const RATE_LIMIT_EXCLUDED_STATES_SQL_IN: &'static str =
        "('Failed', 'DestinationSubmissionFailed', 'InsufficientReserveAtSettlement', \
          'Cancelled', 'Expired', 'Reorged')";

    // The window every wallet limit shares now lives with the rule
    // itself: `Self::WALLET_WINDOW_SECS` (`ledger::wallet_window`),
    // alongside the one query behind every fold, every `POST /transfers`,
    // every Goldcoin deposit observation, every resume and every
    // read-only eligibility view.

    /// Folds an observed Solana `WithdrawalObligation` (a `deposit_to_reserve`
    /// execution, seen at `finalized` commitment) into the ledger. See the
    /// module docs and [`SolFoldOutcome`] for why this direction has no
    /// pre-existing-reservation match and instead reserves/commits capacity
    /// retroactively, and why it folds directly to `SourceFinalized` (Solana
    /// finality is a single instant, unlike Goldcoin's depth ramp).
    /// Idempotent on `source_obligation_index`. `amounts` must already be a
    /// fully computed gross/fee/net breakdown for this obligation's raw
    /// on-chain amount (docs/20-bridge-fee.md — see [`Ledger::create_request`]'s
    /// matching doc comment). The capacity check compares
    /// `amounts.net_destination_atomic` (Goldcoin-native — the destination
    /// for this direction) against available `GoldcoinReserve` capacity,
    /// NOT the raw gross Solana amount that was deposited.
    ///
    /// # Admission gates, all of which must be clear
    ///
    /// `paused`, the operator-only `admission_closed`, the
    /// `utxo_pool_min_available_count` mature-pool floor, both rolling
    /// 24-hour rate limits (recipient and source wallet), the plain
    /// capacity check — and, added 2026-09-02, the confirmed-liquidity
    /// admission safety buffer (docs/09-runbook.md): the direction-wide
    /// hysteresis gate AND the per-request requirement that
    ///
    /// ```text
    /// balance >= protected_minimum + reserved_liquidity
    ///            + net_destination_atomic + admission_safety_buffer
    /// ```
    ///
    /// Every gate parks rather than drops (the Solana-side deposit is
    /// already real and irreversible), each with its own
    /// `manual_review_note` so the cause is never ambiguous.
    pub fn fold_sol_deposit(
        &mut self,
        obligation_index: u64,
        amounts: RequestAmounts,
        requester: [u8; 32],
        recipient_glc_address: &[u8],
        refusal: Option<&str>,
        now: i64,
    ) -> Result<SolFoldOutcome, LedgerError> {
        let tx = write_tx(&mut self.conn)?;

        // CHAIN-scoped, not index-only and not contract-scoped either —
        // exactly matching `ux_bridge_requests_solana_obligation`, the
        // guard this pre-check exists to answer for (schema v21).
        //
        // Chain-scoped, because an unqualified match would report a
        // FOREIGN chain's obligation N as "already folded" and silently
        // drop a real, irreversible deposit — the collision v21 closes.
        //
        // But NOT additionally contract-scoped, because rows migrated from
        // a pre-v21 database carry `LEGACY_SOLANA_SOURCE_CONTRACT` rather
        // than a known program id, and an index one of them holds may well
        // belong to the program running today. Matching on the contract
        // too would classify such a re-observation as a brand-new deposit
        // and pay it out twice. Scoping to the chain keeps the exact
        // pre-v21 promise — one Solana obligation index, one request,
        // ever — and returns a clean `AlreadyFolded` instead of letting
        // the insert below trip a raw constraint error.
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM bridge_requests
                 WHERE source_chain = ?1 AND source_obligation_index = ?2",
                rusqlite::params![SourceChain::Solana, obligation_index as i64],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            tx.rollback()?;
            return Ok(SolFoldOutcome::AlreadyFolded { request_id: id });
        }

        let reserve = ReserveDirection::GoldcoinReserve;
        let available = Self::reserve_headroom(&tx, reserve)?;
        // Confirmed-liquidity admission gate (docs/09-runbook.md's
        // "Confirmed-liquidity admission safety buffer"). Evaluated HERE,
        // inside the same write transaction as the admission decision it
        // governs, so the hysteresis state a decision was made against and
        // the decision itself commit or roll back together — a separate
        // read could be overtaken by a concurrent fold between the check
        // and the write.
        //
        // `available` is already the CONFIRMED unreserved headroom this
        // gate is specified in terms of: `total_reserve_balance` is a
        // mature-only figure by construction, so this service's own
        // still-immature payout change contributes nothing to it (see
        // `Ledger::confirmed_admission_headroom`). Value that cannot be
        // spent yet must never read as room to accept new demand.
        let (buffer_atomic, reopen_atomic, was_liquidity_closed) =
            Self::read_liquidity_admission_row(&tx, reserve)?;
        let liquidity_admission_closed = Self::next_liquidity_admission_closed(
            was_liquidity_closed,
            available,
            buffer_atomic,
            reopen_atomic,
        );
        Self::write_liquidity_admission_state(
            &tx,
            reserve,
            was_liquidity_closed,
            liquidity_admission_closed,
            now,
        )?;
        // Both rolling-24h wallet windows (`ledger::wallet_window`).
        // Evaluated here, where the recipient bytes and the on-chain
        // `requester` are known, and handed to the shared admission
        // decision as `InboundRateLimits` — the decision itself never
        // re-derives a window (see `crate::ledger::admission`'s module
        // docs on why the per-identity limits are an input rather than
        // a gate it owns).
        //
        // The destination leg: "a Goldcoin L1 address may receive at
        // most one bridge payout in a rolling 24-hour window, from any
        // inbound route". The source leg: "a Solana wallet may fund at
        // most one bridge attempt in that window, on any Solana-sourced
        // route" — independent of, and enforced ALONGSIDE, the
        // destination rule (never replacing it), closing the bypass
        // where one wallet spreads deposits across many recipients.
        // `requester` is decoded straight from the on-chain
        // `WithdrawalObligation` account by `solana::indexer`
        // (`WithdrawalObligationSnapshot.requester`, itself
        // `record.requester = ctx.accounts.user.key()` set by the program
        // from the deposit's own `Signer`), never a client-supplied
        // string. Any row created inside the window counts UNLESS its
        // state is on the terminal never-paid exclude-list — see the
        // module docs for the exact query, which the resume re-check and
        // the API's read-only eligibility view share.
        let eligibility = Self::route_wallet_eligibility_in(
            &tx,
            Direction::SolToGlc,
            Some(requester.as_slice()),
            Some(recipient_glc_address),
            now,
            WalletWindowScope::NewRequest,
        )?;
        let recipient_rate_limited = eligibility.destination_retry_after.is_some();
        let source_wallet_rate_limited = eligibility.source_retry_after.is_some();

        // THE admission decision, taken by the one shared evaluator
        // (`crate::ledger::admission`) that `fold_robinhood_deposit` and
        // the public API's per-route `available` also call. Admission is
        // a separate axis from `paused` (docs/09-runbook.md's "Admission
        // control (Solana->Goldcoin)"): EITHER gate blocks a new
        // obligation from being admitted, and the ranking that decides
        // which one an operator sees lives with the gates rather than
        // here.
        //
        // The gate snapshot is read from inside THIS transaction, using
        // the hysteresis state just re-evaluated and persisted above, so
        // the state the decision was made against and the decision
        // itself commit or roll back together.
        let gates = crate::ledger::admission::InboundAdmissionGates::read(
            &tx,
            Direction::SolToGlc,
            liquidity_admission_closed,
        )?;
        let blocker = gates.blocker(
            amounts.net_destination_atomic as i64,
            crate::ledger::admission::InboundRateLimits {
                source_wallet_rate_limited,
                recipient_rate_limited,
            },
        );
        // The rapid-burst rule (`ledger::rapid_burst`, schema v30),
        // evaluated on the same identities the wallet windows use — the
        // on-chain `requester` and the recipient — against this
        // transaction's view of recent history. It outranks every other
        // reason: a burst-held deposit is custodied and classified for an
        // explicit operator decision, whatever else the reserve would
        // have said about it.
        let burst = Self::rapid_burst_verdict_in(
            &tx,
            Direction::SolToGlc,
            Some(requester.as_slice()),
            Some(recipient_glc_address),
            now,
            WalletWindowScope::NewRequest,
        )?;
        // An explicit refusal outranks a capacity blocker, matching
        // `fold_sol_deposit_to_robinhood`'s ranking: a deposit that may
        // not be paid out AT ALL is not usefully described as one the
        // reserve is currently too small for, and the two have different
        // remedies.
        let capacity_ok = blocker.is_none() && refusal.is_none() && burst.is_none();
        let manual_review_reason = if burst.is_some() {
            Self::MANUAL_REVIEW_REASON_RAPID_BURST_HOLD
        } else {
            refusal.unwrap_or_else(|| {
                blocker
                    .map(|b| b.manual_review_note())
                    .unwrap_or(Self::MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY)
            })
        };

        tx.execute(
            // The obligation index is only half an identity: it is local
            // to the contract that issued it, so the chain and that
            // contract's own address are recorded alongside it in the same
            // statement. `ux_bridge_requests_obligation_source` is keyed
            // on all three (schema v21).
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 reserved_at, source_obligation_index, source_confirmations, source_finalized_at,
                 manual_review_note, source_chain, source_contract, source_wallet)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11, 1, ?10, ?12, ?13, ?14,
                     ?9)",
            rusqlite::params![
                Direction::SolToGlc,
                if capacity_ok {
                    RequestState::SourceFinalized
                } else {
                    RequestState::ManualReview
                },
                amounts.gross_atomic as i64,
                amounts.fee_bps as i64,
                amounts.fee_atomic as i64,
                amounts.net_atomic as i64,
                amounts.net_destination_atomic as i64,
                recipient_glc_address,
                requester.as_slice(),
                now,
                obligation_index as i64,
                if capacity_ok {
                    None
                } else {
                    Some(manual_review_reason)
                },
                SourceChain::Solana,
                &glc_reserve_bridge_shared::PROGRAM_ID_BYTES[..],
            ],
        )?;
        let request_id = tx.last_insert_rowid();
        log_transition(
            &tx,
            request_id,
            None,
            if capacity_ok {
                RequestState::SourceFinalized
            } else {
                RequestState::ManualReview
            },
            now,
            Some("retroactive_fold_sol_deposit"),
            "system",
        )?;
        if let Some(matched) = &burst {
            Self::mark_rapid_burst_hold_in(&tx, request_id, matched, now)?;
        }

        if capacity_ok {
            tx.execute(
                "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1,
                    pending_obligations = pending_obligations + ?1 WHERE direction = ?2",
                rusqlite::params![amounts.net_destination_atomic as i64, reserve],
            )?;
        }
        tx.commit()?;

        Ok(if capacity_ok {
            SolFoldOutcome::FoldedFinalized { request_id }
        } else {
            SolFoldOutcome::FoldedManualReview { request_id }
        })
    }

    /// Folds an observed Solana `WithdrawalObligation` whose destination
    /// payload is a Robinhood (EVM) address into a `SolToRhn` request —
    /// the Solana-sourced twin of [`Self::fold_robinhood_deposit`] and
    /// the Robinhood-bound twin of [`Self::fold_sol_deposit`].
    ///
    /// # What is the same as `fold_sol_deposit`
    ///
    /// The replay guard (chain-scoped on the obligation index, for the
    /// legacy-sentinel reason that function records), the row shape, the
    /// retroactive reservation against the DESTINATION reserve, the
    /// direct-to-`SourceFinalized` fold (Solana finality is a single
    /// instant), and the shared admission evaluator
    /// ([`crate::ledger::admission::InboundAdmissionGates`]) — now keyed
    /// `Direction::SolToRhn`, so it reads the `RobinhoodReserve` row's
    /// gates and this route's own `route_admission` row.
    ///
    /// # What is different, and why
    ///
    /// - **The destination reserve is `RobinhoodReserve`**, accounted in
    ///   canonical units, so `amounts.net_destination_atomic` must equal
    ///   `amounts.net_atomic` (asserted). The caller has already proven
    ///   the net widens exactly to Robinhood's 18 decimals
    ///   (`CanonicalAtomic::to_robinhood`), which it always does.
    /// - **The route ENABLEMENT gate applies at fold time**
    ///   (`route_open`), exactly as it does for a Robinhood deposit: a
    ///   deposit that arrives while the route is closed is recorded and
    ///   parked with `route_disabled_at_fold`, never dropped and never
    ///   paid. `SolToGlc` has no such gate because it is a legacy route
    ///   that is enabled by construction.
    /// - **The rolling-24h wallet windows are keyed for THIS route**: the
    ///   Solana `requester` on the source leg, the EVM recipient on the
    ///   destination leg (`ledger::wallet_window`) — the same rule
    ///   `fold_sol_deposit` applies, on this route's own chains. The
    ///   custody contract's per-transfer and rolling outbound limits
    ///   still apply on-chain, independently. The mature-UTXO floor does
    ///   not apply (no Goldcoin vault is involved) — the shared evaluator
    ///   already skips it for any reserve other than `GoldcoinReserve`.
    ///
    /// Every gate parks rather than drops, each with its own
    /// `manual_review_note`, and a park takes NO reserve capacity.
    ///
    /// `recipient_evm` is `None` when the destination payload is not a
    /// usable EVM address; the raw payload is then stored as the
    /// recipient (the evidence a refund decision rests on) and the row is
    /// parked with `refusal`, exactly as `fold_robinhood_deposit` treats
    /// an undeliverable Goldcoin destination.
    #[allow(clippy::too_many_arguments)]
    pub fn fold_sol_deposit_to_robinhood(
        &mut self,
        obligation_index: u64,
        amounts: RequestAmounts,
        requester: [u8; 32],
        recipient_evm: Option<[u8; 20]>,
        raw_destination: &[u8],
        route_open: bool,
        refusal: Option<&str>,
        now: i64,
    ) -> Result<SolFoldOutcome, LedgerError> {
        assert_eq!(
            amounts.net_destination_atomic, amounts.net_atomic,
            "a SolToRhn request reserves against the canonical-unit RobinhoodReserve row"
        );
        let tx = write_tx(&mut self.conn)?;

        // Same chain-scoped pre-check as `fold_sol_deposit`, for the same
        // reason: one Solana obligation index, one request, ever —
        // whichever chain the depositor bound it for.
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM bridge_requests
                 WHERE source_chain = ?1 AND source_obligation_index = ?2",
                rusqlite::params![SourceChain::Solana, obligation_index as i64],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            tx.rollback()?;
            return Ok(SolFoldOutcome::AlreadyFolded { request_id: id });
        }

        let direction = Direction::SolToRhn;
        let reserve = direction.destination_reserve();
        let available = Self::reserve_headroom(&tx, reserve)?;
        let (buffer_atomic, reopen_atomic, was_liquidity_closed) =
            Self::read_liquidity_admission_row(&tx, reserve)?;
        let liquidity_admission_closed = Self::next_liquidity_admission_closed(
            was_liquidity_closed,
            available,
            buffer_atomic,
            reopen_atomic,
        );
        Self::write_liquidity_admission_state(
            &tx,
            reserve,
            was_liquidity_closed,
            liquidity_admission_closed,
            now,
        )?;

        // Both rolling-24h wallet windows (`ledger::wallet_window`), the
        // SAME rule `fold_sol_deposit` applies, keyed for this route: the
        // Solana `requester` on the source leg (a window spanning every
        // Solana-sourced route), the EVM recipient on the destination
        // leg (spanning every Robinhood-bound route). An undeliverable
        // destination has no wallet to ask about and is parked for that
        // reason regardless.
        let eligibility = Self::route_wallet_eligibility_in(
            &tx,
            direction,
            Some(requester.as_slice()),
            recipient_evm.as_ref().map(|a| &a[..]),
            now,
            WalletWindowScope::NewRequest,
        )?;
        let gates = crate::ledger::admission::InboundAdmissionGates::read(
            &tx,
            direction,
            liquidity_admission_closed,
        )?;
        let reserve_blocker = gates.blocker(
            amounts.net_destination_atomic as i64,
            crate::ledger::admission::InboundRateLimits {
                source_wallet_rate_limited: eligibility.source_retry_after.is_some(),
                recipient_rate_limited: eligibility.destination_retry_after.is_some(),
            },
        );
        // The rapid-burst rule (`ledger::rapid_burst`), ranked above
        // everything else — see `fold_sol_deposit`.
        let burst = Self::rapid_burst_verdict_in(
            &tx,
            direction,
            Some(requester.as_slice()),
            recipient_evm.as_ref().map(|a| &a[..]),
            now,
            WalletWindowScope::NewRequest,
        )?;
        let payable = route_open
            && refusal.is_none()
            && recipient_evm.is_some()
            && reserve_blocker.is_none()
            && burst.is_none();
        // Ranked exactly as `fold_robinhood_deposit` ranks them: the
        // burst hold, the explicit refusal, then deliverability, then
        // the route's ENABLEMENT gate (a different axis from admission),
        // then the shared reserve-side ranking verbatim.
        let note: Option<&str> = if payable {
            None
        } else if burst.is_some() {
            Some(Self::MANUAL_REVIEW_REASON_RAPID_BURST_HOLD)
        } else if let Some(explicit) = refusal {
            Some(explicit)
        } else if recipient_evm.is_none() {
            Some("undeliverable destination")
        } else if !route_open {
            Some(Self::MANUAL_REVIEW_REASON_ROUTE_DISABLED)
        } else {
            Some(
                reserve_blocker
                    .map(|b| b.manual_review_note())
                    .unwrap_or(Self::MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY),
            )
        };
        let state = if payable {
            RequestState::SourceFinalized
        } else {
            RequestState::ManualReview
        };

        tx.execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 reserved_at, source_obligation_index, source_confirmations, source_finalized_at,
                 manual_review_note, source_chain, source_contract, source_wallet)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11, 1, ?10, ?12, ?13, ?14,
                     ?9)",
            rusqlite::params![
                direction,
                state,
                amounts.gross_atomic as i64,
                amounts.fee_bps as i64,
                amounts.fee_atomic as i64,
                amounts.net_atomic as i64,
                amounts.net_destination_atomic as i64,
                // The 20 raw address bytes, exactly as `GlcToRhn` stores
                // its recipient — `Settler::authorize_payout` parses
                // this column as an `EvmAddress` for both — or, for an
                // undeliverable destination, the payload as deposited.
                match recipient_evm.as_ref() {
                    Some(address) => &address[..],
                    None => raw_destination,
                },
                requester.as_slice(),
                now,
                obligation_index as i64,
                note,
                SourceChain::Solana,
                &glc_reserve_bridge_shared::PROGRAM_ID_BYTES[..],
            ],
        )?;
        let request_id = tx.last_insert_rowid();
        log_transition(
            &tx,
            request_id,
            None,
            state,
            now,
            Some("retroactive_fold_sol_deposit_to_robinhood"),
            "system",
        )?;
        if let Some(matched) = &burst {
            Self::mark_rapid_burst_hold_in(&tx, request_id, matched, now)?;
        }

        if payable {
            tx.execute(
                "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1,
                    pending_obligations = pending_obligations + ?1 WHERE direction = ?2",
                rusqlite::params![amounts.net_destination_atomic as i64, reserve],
            )?;
        }
        tx.commit()?;

        Ok(if payable {
            SolFoldOutcome::FoldedFinalized { request_id }
        } else {
            SolFoldOutcome::FoldedManualReview { request_id }
        })
    }

    /// Resumes a `SolToRhn`/`RhnToSol` request parked in `ManualReview`
    /// for one of the fold-time reasons on
    /// [`Self::RECOVERABLE_MANUAL_REVIEW_REASONS`] — the cross-route
    /// twin of [`Self::resume_manual_review_inbound`].
    ///
    /// # What is the same
    ///
    /// The shape of every check, in the same order, for the same
    /// reasons: the direction, the durable refund marker (checked against
    /// the ROW so an out-of-band `state` edit cannot re-open a refunded
    /// request), the `ManualReview` state with the same idempotent
    /// already-resumed answer, the recoverable-reason list, a finalized
    /// source, no destination payout begun, and the reservation applied
    /// exactly once and only when the destination reserve's shared
    /// admission evaluator says the amount fits right now — the SAME
    /// [`crate::ledger::admission::InboundAdmissionGates`] the fold
    /// gated on, so a resume can never admit what a fresh fold would
    /// park.
    ///
    /// # What is different
    ///
    /// No mature-UTXO floor: neither route pays out of the Goldcoin
    /// vault, and the shared evaluator already omits the floor for any
    /// other reserve. (The two wallet windows ARE re-checked, exactly as
    /// the inbound resume re-checks them — the rule spans every route.)
    /// The route
    /// ENABLEMENT gate is deliberately NOT consulted, exactly as the
    /// Goldcoin-bound resume ignores `paused`/`admission_closed`: this
    /// resumes something already accepted and admits nothing new — and a
    /// `route_disabled_at_fold` park is not on the recoverable list, so
    /// a deposit made while the route was closed is refunded rather than
    /// resumed, the same posture `RhnToGlc` takes.
    pub fn resume_manual_review_cross_route(
        &mut self,
        expected_direction: Direction,
        request_id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<ResumeManualReviewOutcome, LedgerError> {
        debug_assert!(
            matches!(
                expected_direction,
                Direction::SolToRhn | Direction::RhnToSol
            ),
            "resume_manual_review_cross_route is only meaningful for a Solana<->Robinhood route"
        );
        let tx = write_tx(&mut self.conn)?;
        Self::refuse_if_auto_resume_held(&tx, request_id)?;

        #[allow(clippy::type_complexity)]
        let row: Option<(
            Direction,
            RequestState,
            Option<String>,
            i64,
            Option<i64>,
            Option<Vec<u8>>,
            Vec<u8>,
            i64,
            Option<Vec<u8>>,
        )> = tx
            .query_row(
                "SELECT direction, state, manual_review_note, net_destination_atomic,
                        source_finalized_at, destination_txid, recipient, created_at,
                        source_wallet
                 FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            direction,
            state,
            manual_review_note,
            net_destination_atomic,
            source_finalized_at,
            destination_txid,
            recipient,
            candidate_created_at,
            source_wallet,
        )) = row
        else {
            tx.rollback()?;
            return Err(LedgerError::RequestNotFound(request_id));
        };
        if direction != expected_direction {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!(
                    "request is {}, not {}",
                    direction.as_str(),
                    expected_direction.as_str()
                ),
            });
        }
        // Fails closed on a missing source wallet, exactly as the
        // Goldcoin-bound resume does and for the same reason.
        let Some(source_wallet) = source_wallet.filter(|w: &Vec<u8>| !w.is_empty()) else {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!(
                    "{} request has no source wallet recorded, so its source-wallet window \
                     cannot be checked",
                    direction.as_str()
                ),
            });
        };

        // The durable refund marker, per source chain.
        let refund_state: Option<String> = if direction.source_is_robinhood() {
            tx.query_row(
                "SELECT state FROM robinhood_transactions
                 WHERE request_id = ?1 AND kind = 'Refund'",
                [request_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        } else {
            tx.query_row(
                "SELECT state FROM solana_refunds WHERE request_id = ?1",
                [request_id],
                |r| r.get::<_, SolanaRefundState>(0),
            )
            .optional()?
            .map(|s| s.as_str().to_string())
        };
        if let Some(refund_state) = refund_state {
            tx.rollback()?;
            return Err(LedgerError::RefundLifecycleExists {
                id: request_id,
                refund_state,
            });
        }

        if state != RequestState::ManualReview {
            let previously_resumed: bool = tx
                .query_row(
                    "SELECT 1 FROM bridge_request_state_log
                     WHERE request_id = ?1 AND from_state = ?2 AND to_state = ?3 LIMIT 1",
                    rusqlite::params![
                        request_id,
                        RequestState::ManualReview,
                        RequestState::SourceFinalized
                    ],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            tx.rollback()?;
            if previously_resumed {
                return Ok(ResumeManualReviewOutcome::AlreadyResumed { state });
            }
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!("state is {state:?}, not ManualReview"),
            });
        }
        if !Self::is_recoverable_manual_review_reason(manual_review_note.as_deref()) {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!(
                    "manual_review_note {manual_review_note:?} is not a known recoverable reason"
                ),
            });
        }
        if source_finalized_at.is_none() {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "source deposit is not finalized".to_string(),
            });
        }
        // No destination payout may have begun, on either destination
        // chain: a Solana release signature, or a Robinhood payout
        // operation row of any state.
        let robinhood_payout_begun: bool = tx
            .query_row(
                "SELECT 1 FROM robinhood_transactions WHERE request_id = ?1 AND kind = 'Payout'",
                [request_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if destination_txid.is_some() || robinhood_payout_begun {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "a destination payout already exists".to_string(),
            });
        }
        if net_destination_atomic <= 0 {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "the request carries no destination amount".to_string(),
            });
        }

        // Both wallet windows, unconditionally and strict-predecessor-
        // only — the identical re-check the Goldcoin-bound resume makes,
        // through the identical function, so an operator can no more
        // resume a cross-route request past a live window than an
        // inbound one.
        if let Err(window) = Self::resume_wallet_windows(
            &tx,
            direction,
            request_id,
            candidate_created_at,
            &source_wallet,
            &recipient,
            now,
        ) {
            tx.rollback()?;
            return Err(window);
        }

        // The destination reserve's shared admission decision, evaluated
        // exactly as the fold evaluated it — hysteresis re-derived and
        // persisted inside this same transaction — with the wallet windows
        // already settled above. The route-level admission and
        // reserve-wide switches are NOT consulted, per the docs.
        let reserve = direction.destination_reserve();
        let available = Self::reserve_headroom(&tx, reserve)?;
        let (buffer_atomic, reopen_atomic, was_liquidity_closed) =
            Self::read_liquidity_admission_row(&tx, reserve)?;
        let liquidity_admission_closed = Self::next_liquidity_admission_closed(
            was_liquidity_closed,
            available,
            buffer_atomic,
            reopen_atomic,
        );
        Self::write_liquidity_admission_state(
            &tx,
            reserve,
            was_liquidity_closed,
            liquidity_admission_closed,
            now,
        )?;
        // The two gates a resume MUST re-apply, spelled with the same
        // arithmetic the shared evaluator uses (and the Goldcoin-bound
        // resume spells out): the reserve invariant after this
        // reservation, and the confirmed-liquidity safety buffer. Judged
        // on the buffer arithmetic only, never on the persisted
        // hysteresis state, for the reason the Goldcoin-bound resume
        // records: a resume of an already-received deposit is judged on
        // whether the reserve can actually carry it right now.
        if net_destination_atomic > available {
            let (balance, protected_minimum, reserved): (i64, i64, i64) = tx.query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
                 FROM reserve_ledger WHERE direction = ?1",
                [reserve],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            tx.rollback()?;
            return Err(LedgerError::InvariantViolated {
                direction: reserve,
                balance,
                protected_minimum,
                reserved_liquidity: reserved + net_destination_atomic,
            });
        }
        if buffer_atomic > 0 && available - net_destination_atomic < buffer_atomic {
            tx.rollback()?;
            return Err(LedgerError::AdmissionLiquidityBufferLow {
                request_id,
                headroom: available,
                net_destination_atomic,
                buffer_atomic,
            });
        }

        tx.execute(
            "UPDATE bridge_requests SET state = 'SourceFinalized', reserved_at = ?2
             WHERE id = ?1",
            rusqlite::params![request_id, now],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::ManualReview),
            RequestState::SourceFinalized,
            now,
            Some(note),
            actor,
        )?;
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1,
                pending_obligations = pending_obligations + ?1 WHERE direction = ?2",
            rusqlite::params![net_destination_atomic, reserve],
        )?;
        tx.commit()?;
        Ok(ResumeManualReviewOutcome::Resumed)
    }

    /// The unconditional, strict-predecessor-only wallet-window re-check
    /// every resume path makes (see the caller's comment for why only a
    /// predecessor may block). Source first. `Err(WalletWindowActive)`
    /// when either window still applies; the caller rolls back.
    fn resume_wallet_windows(
        tx: &Connection,
        direction: Direction,
        request_id: i64,
        candidate_created_at: i64,
        source_wallet: &[u8],
        recipient: &[u8],
        now: i64,
    ) -> Result<(), LedgerError> {
        let scope = WalletWindowScope::StrictPredecessorOf {
            created_at: candidate_created_at,
            id: request_id,
        };
        for (role, chain, wallet) in [
            (WalletRole::Source, direction.source_chain(), source_wallet),
            (
                WalletRole::Destination,
                direction.destination_chain(),
                recipient,
            ),
        ] {
            if let Some(retry_after) =
                Self::wallet_window_retry_after_in(tx, chain, role, wallet, now, scope)?
            {
                return Err(LedgerError::WalletWindowActive {
                    request_id,
                    role,
                    chain,
                    wallet: wallet.to_vec(),
                    retry_after,
                });
            }
        }
        Ok(())
    }

    /// STRICTLY READ-ONLY trial of [`Self::resume_manual_review_sol_to_glc`]:
    /// runs that exact function inside an outer admin scope and then rolls
    /// the whole scope back, so nothing whatsoever persists — no state
    /// change, no `reserved_liquidity`/`pending_obligations` movement, no
    /// state-log row, no audit row.
    ///
    /// Deliberately a TRIAL rather than a re-implementation of the guard
    /// list. Every eligibility, rate-limit, UTXO-liquidity and reserve
    /// check therefore comes from the one function that actually enforces
    /// them, evaluated against the same live state under the same write
    /// lock — the dry run and the real thing cannot drift apart, because
    /// they are the same code. A parallel "checks preview" would be
    /// exactly the drift hazard this avoids.
    ///
    /// Reports the FIRST refusal, which is precisely what an execution
    /// would hit. Supplementary context for an operator (current
    /// capacity, mature UTXO count, rate-limit windows) is available from
    /// the existing read-only accessors and is gathered by the caller.
    ///
    /// Takes `&mut self` because it briefly holds SQLite's write lock;
    /// that is an implementation detail of the rollback, not a mutation.
    pub fn dry_run_resume_manual_review(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<ResumeDryRunOutcome, LedgerError> {
        self.begin_admin_action()?;
        // The note/actor never reach storage: the enclosing scope is
        // rolled back unconditionally below, on every path.
        let attempted =
            self.resume_manual_review_sol_to_glc(request_id, "dry-run trial", "dry-run", now);
        // Roll back FIRST, before interpreting the result, so no early
        // return can ever leave the trial committed.
        self.rollback_admin_action()?;
        Ok(match attempted {
            Ok(ResumeManualReviewOutcome::Resumed) => ResumeDryRunOutcome::WouldResume,
            Ok(ResumeManualReviewOutcome::AlreadyResumed { state }) => {
                ResumeDryRunOutcome::AlreadyResumed { state }
            }
            Err(e) => ResumeDryRunOutcome::WouldRefuse {
                reason: e.to_string(),
            },
        })
    }

    /// Resumes a `SolToGlc` request `fold_sol_deposit` parked in
    /// `ManualReview` for one of the fold-time reasons on
    /// [`Self::RECOVERABLE_MANUAL_REVIEW_REASONS`] — never a request in
    /// `ManualReview` for any other reason. The membership test is
    /// [`Self::is_recoverable_manual_review_reason`], the same one every
    /// read-only discovery surface uses, so what this function accepts
    /// and what an operator is shown as a candidate are one list. Applies the SAME
    /// `reserved_liquidity`/`pending_obligations` increment a successful
    /// fold would have applied, refusing (no override) if that increment
    /// would breach the reserve invariant right now — the identical
    /// `available_capacity` check `fold_sol_deposit`/`create_request`
    /// already use. Deliberately does NOT consult `paused`/
    /// `admission_closed` at all: admission may remain closed while this
    /// resumes an already-accepted obligation (docs/09-runbook.md's
    /// "Admission control (Solana->Goldcoin)" section — this command never
    /// admits anything new, it only unblocks something already accepted).
    ///
    /// Idempotent: calling this again once the request has already moved
    /// past `ManualReview` (by a prior call to this same command) is a
    /// safe no-op reporting [`ResumeManualReviewOutcome::AlreadyResumed`],
    /// never a second reservation. Never creates a new row, never touches
    /// `source_obligation_index` — this transitions the EXISTING request
    /// in place, so a duplicate obligation is impossible by construction,
    /// not just by convention. The idempotency check itself does not
    /// filter by `actor` (see the query below) — the `(ManualReview ->
    /// SourceFinalized)` transition is only ever written here, by any
    /// caller, so its mere presence is unambiguous proof of a prior
    /// resume regardless of which actor performed it.
    ///
    /// `actor` is recorded verbatim in `bridge_request_state_log` — pass
    /// `"operator"` for a human-initiated `glc-admin resume-manual-review`
    /// call, or `"auto-resume"` for `Orchestrator::
    /// tick_auto_resume_utxo_liquidity_backlog`'s automatic recovery.
    /// Every other safety check below is identical regardless of `actor`;
    /// this parameter affects only the audit trail, never eligibility.
    pub fn resume_manual_review_sol_to_glc(
        &mut self,
        request_id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<ResumeManualReviewOutcome, LedgerError> {
        self.resume_manual_review_inbound(Direction::SolToGlc, request_id, note, actor, now)
    }

    /// The `RhnToGlc` twin of [`Self::resume_manual_review_sol_to_glc`],
    /// resuming a request [`Self::fold_robinhood_deposit`] parked in
    /// `ManualReview` for one of the same fold-time reasons.
    ///
    /// Every guarantee documented on the Solana wrapper holds here
    /// verbatim, because it is literally the same function body
    /// ([`Self::resume_manual_review_inbound`]): the same recoverable-
    /// reason list, the same UNCONDITIONAL re-check of both rate limits,
    /// the same UTXO floor, reserve invariant and admission safety
    /// buffer, the same in-place transition (never a new row, so a
    /// duplicate obligation or reservation is impossible by construction),
    /// the same idempotent `AlreadyResumed`, and the same permanent
    /// refusal once a refund lifecycle has begun.
    ///
    /// The three direction-specific parts are the ones that CANNOT be
    /// shared, and each is documented where it branches: which error a
    /// wrong-direction request gets, where the durable refund-lifecycle
    /// marker lives (`solana_refunds` vs. a `Refund` row in
    /// `robinhood_transactions`), and which source-wallet limiter applies
    /// (Solana `requester` vs. Robinhood `depositor` — never shared; see
    /// [`Self::source_wallet_rate_limit_blocker_created_at`]).
    pub fn resume_manual_review_rhn_to_glc(
        &mut self,
        request_id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<ResumeManualReviewOutcome, LedgerError> {
        self.resume_manual_review_inbound(Direction::RhnToGlc, request_id, note, actor, now)
    }

    /// Reason string every hold/release writes to `bridge_request_state_log`
    /// (a `ManualReview -> ManualReview` row, so the Explorer shows the
    /// operator act without inventing a state).
    pub const AUTO_RESUME_HOLD_TRANSITION_REASON: &str = "auto_resume_hold";
    pub const AUTO_RESUME_HOLD_RELEASED_TRANSITION_REASON: &str = "auto_resume_hold_released";
    /// State-log reason a fold writes when the rapid-burst rule holds a
    /// deposit (`ledger::rapid_burst`), and the reasons an operator's
    /// `process`/`refund` decision on a held row writes.
    pub const RAPID_BURST_HOLD_TRANSITION_REASON: &str = "rapid_burst_hold";
    pub const OPERATOR_DECISION_PROCESS_TRANSITION_REASON: &str = "operator_decision_process";
    pub const OPERATOR_DECISION_REFUND_TRANSITION_REASON: &str = "operator_decision_refund";
    /// `hold_reason` of an operator hold (a rapid-burst hold's is
    /// `rapid_burst:<rule>` — [`RapidBurstRule::hold_reason`]).
    pub const OPERATOR_HOLD_REASON: &str = "operator_hold";

    /// Places an explicit operator hold on ONE `ManualReview` request
    /// (schema v29 hold marker + v30 disposition — see `apply_v29`/
    /// `apply_v30`).
    ///
    /// Refuses, without writing, unless the row is currently
    /// `ManualReview`, has no destination txid and no destination payout
    /// row — the same "nothing has been paid" predicates the resume and
    /// refund paths apply, so a request already processing can never be
    /// marked — and is not a rapid-burst hold (that classification is
    /// the fold's evidence and is never overwritten; decide it with
    /// `process`/`refund` instead). Idempotent on an already-held row
    /// (note, marker time and `review_after` are replaced; one audit
    /// row is written; `hold_started_at` keeps the ORIGINAL time).
    ///
    /// `review_after` is informational — the moment the operator
    /// intends to revisit the row (`None` = at their discretion). The
    /// hold is INDEFINITE: nothing expires it, and only
    /// [`Self::clear_manual_review_hold`] or an explicit decision
    /// ([`Self::process_held_manual_review`] /
    /// [`Self::record_operator_decision`]) ends it. Touches nothing
    /// else: no state, note, reserve figure or other row changes, and a
    /// row folded after this call is unaffected because folds never
    /// read or write the hold columns.
    pub fn set_manual_review_hold(
        &mut self,
        request_id: i64,
        review_after: Option<i64>,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let note = note.trim();
        if note.is_empty() {
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "a hold needs a non-empty note".to_string(),
            });
        }
        let tx = write_tx(&mut self.conn)?;
        let disposition = Self::refuse_unless_holdable(&tx, request_id)?;
        if disposition == ManualReviewDisposition::RapidBurstHold {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "this is a rapid-burst hold — its classification is not replaced by an \
                         operator hold; decide it with `manual-review-process` or \
                         `manual-review-refund`"
                    .to_string(),
            });
        }
        tx.execute(
            "UPDATE bridge_requests
                SET auto_resume_hold_note = ?1, auto_resume_hold_until = ?2,
                    manual_review_disposition = ?3, hold_reason = ?4, held_by = ?5,
                    hold_started_at = COALESCE(hold_started_at, ?6), review_after = ?2,
                    operator_decision = NULL, operator_decision_at = NULL, operator_note = NULL
              WHERE id = ?7",
            rusqlite::params![
                note,
                review_after,
                ManualReviewDisposition::OperatorHold,
                Self::OPERATOR_HOLD_REASON,
                actor,
                now,
                request_id
            ],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::ManualReview),
            RequestState::ManualReview,
            now,
            Some(Self::AUTO_RESUME_HOLD_TRANSITION_REASON),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Releases an OPERATOR hold placed by [`Self::set_manual_review_hold`]
    /// — the row becomes an ordinary park again (disposition `normal`,
    /// eligible for auto-resume exactly as an unheld park with its
    /// original `manual_review_note`). The decision (`release`), its time
    /// and note stay on the row as audit. A no-op (`Ok(false)`) on a row
    /// that is not held; `Ok(true)` when a hold was removed. Never
    /// changes state.
    ///
    /// Refuses a rapid-burst hold outright: the only ways out of one are
    /// `process` and `refund`, because "let it drain on its own" is
    /// precisely what the classification exists to prevent.
    pub fn clear_manual_review_hold(
        &mut self,
        request_id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<bool, LedgerError> {
        let note = note.trim();
        let tx = write_tx(&mut self.conn)?;
        let held: Option<(Option<String>, ManualReviewDisposition)> = tx
            .query_row(
                "SELECT auto_resume_hold_note, manual_review_disposition
                   FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((held_note, disposition)) = held else {
            tx.rollback()?;
            return Err(LedgerError::RequestNotFound(request_id));
        };
        if disposition == ManualReviewDisposition::RapidBurstHold {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "a rapid-burst hold cannot be released — it ends only with an explicit \
                         `manual-review-process` or `manual-review-refund` decision"
                    .to_string(),
            });
        }
        if held_note.is_none() && disposition == ManualReviewDisposition::Normal {
            tx.rollback()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE bridge_requests
                SET auto_resume_hold_note = NULL, auto_resume_hold_until = NULL,
                    manual_review_disposition = ?1,
                    operator_decision = ?2, operator_decision_at = ?3,
                    operator_note = ?4
              WHERE id = ?5",
            rusqlite::params![
                ManualReviewDisposition::Normal,
                OperatorDecision::Release,
                now,
                if note.is_empty() { None } else { Some(note) },
                request_id
            ],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::ManualReview),
            RequestState::ManualReview,
            now,
            Some(Self::AUTO_RESUME_HOLD_RELEASED_TRANSITION_REASON),
            actor,
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Every request currently HELD ([`BridgeRequest::is_held`]) or that
    /// ever carried a hold marker, oldest first — whatever its state (a
    /// held row that was refunded keeps its markers, which is the audit
    /// trail; the listing shows the state beside it).
    pub fn held_manual_review_requests(&self) -> Result<Vec<BridgeRequest>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_REQUEST_PREFIX}
              WHERE auto_resume_hold_note IS NOT NULL
                 OR manual_review_disposition <> 'normal'
              ORDER BY id"
        ))?;
        let rows = stmt
            .query_map([], row_to_request)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Records the explicit operator decision that ends a hold, on ONE
    /// held `ManualReview` request, and returns the row as it stood
    /// BEFORE the write (for the caller's audit old-value).
    ///
    /// Refuses, without writing, unless ALL of: the row exists and is
    /// currently `ManualReview`; it is held ([`BridgeRequest::is_held`]);
    /// no destination txid and no destination payout row (the same
    /// "nothing has been paid" predicates every resume/refund path
    /// applies — a double payout is impossible by construction); no
    /// refund lifecycle has begun; and, for a rapid-burst hold, the
    /// minimum review hold has elapsed (`now >= review_after`) — unless
    /// `emergency` is set, which is accepted ONLY for `refund` (the
    /// emergency exit returns funds; it never pays out early) and is
    /// recorded in the state log as such.
    ///
    /// `process` clears the v29 hold marker in the same write so the
    /// shared resume can proceed — callers MUST run that resume inside
    /// the same transaction ([`Self::process_held_manual_review`]) so a
    /// refused resume rolls the decision back too. `refund` leaves the
    /// marker in place (the hold keeps the row FOR refunding) and only
    /// records the decision, which the refund begin-paths then require
    /// ([`Self::refuse_unless_decided`]). `release` is not a decision
    /// this records — use [`Self::clear_manual_review_hold`].
    pub fn record_operator_decision(
        &mut self,
        request_id: i64,
        decision: OperatorDecision,
        note: &str,
        actor: &str,
        emergency: bool,
        now: i64,
    ) -> Result<BridgeRequest, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let before = Self::record_operator_decision_in(
            &tx, request_id, decision, note, actor, emergency, now,
        )?;
        tx.commit()?;
        Ok(before)
    }

    fn record_operator_decision_in(
        tx: &Connection,
        request_id: i64,
        decision: OperatorDecision,
        note: &str,
        actor: &str,
        emergency: bool,
        now: i64,
    ) -> Result<BridgeRequest, LedgerError> {
        let refuse = |detail: String| LedgerError::ManualReviewNotRecoverable {
            id: request_id,
            detail,
        };
        let note = note.trim();
        if note.is_empty() {
            return Err(refuse(
                "an operator decision needs a non-empty note".to_string(),
            ));
        }
        if decision == OperatorDecision::Release {
            return Err(refuse(
                "`release` is recorded by clear_manual_review_hold, not as a decision".to_string(),
            ));
        }
        if emergency && decision != OperatorDecision::Refund {
            return Err(refuse(
                "the emergency override applies to `refund` only — a payout is never brought \
                 forward"
                    .to_string(),
            ));
        }
        Self::refuse_unless_holdable(tx, request_id)?;
        let before = tx
            .query_row(SELECT_REQUEST, [request_id], row_to_request)
            .optional()?
            .ok_or(LedgerError::RequestNotFound(request_id))?;
        if !before.is_held() {
            return Err(refuse(format!(
                "not held (disposition {}) — an operator decision applies to a held request \
                 only; use resume-manual-review / refund-manual-review for an ordinary park",
                before.manual_review_disposition.as_str()
            )));
        }
        if Self::refund_lifecycle_exists_in(tx, request_id)? {
            return Err(refuse(
                "a refund lifecycle already exists for this request".to_string(),
            ));
        }
        if before.manual_review_disposition == ManualReviewDisposition::RapidBurstHold
            && !before.review_available(now)
            && !emergency
        {
            return Err(refuse(format!(
                "rapid-burst hold: the minimum review hold has not elapsed (review_after={}, \
                 now={}) — decide after that moment, or `--emergency` for a refund",
                before.review_after.unwrap_or_default(),
                now
            )));
        }
        let reason = match decision {
            OperatorDecision::Process => Self::OPERATOR_DECISION_PROCESS_TRANSITION_REASON,
            OperatorDecision::Refund => Self::OPERATOR_DECISION_REFUND_TRANSITION_REASON,
            OperatorDecision::Release => unreachable!("refused above"),
        };
        let stored_note = if emergency {
            format!("EMERGENCY (before review_after): {note}")
        } else {
            note.to_string()
        };
        match decision {
            OperatorDecision::Process => tx.execute(
                "UPDATE bridge_requests
                    SET operator_decision = ?1, operator_decision_at = ?2, operator_note = ?3,
                        auto_resume_hold_note = NULL, auto_resume_hold_until = NULL
                  WHERE id = ?4",
                rusqlite::params![decision, now, stored_note, request_id],
            )?,
            _ => tx.execute(
                "UPDATE bridge_requests
                    SET operator_decision = ?1, operator_decision_at = ?2, operator_note = ?3
                  WHERE id = ?4",
                rusqlite::params![decision, now, stored_note, request_id],
            )?,
        };
        log_transition(
            tx,
            request_id,
            Some(RequestState::ManualReview),
            RequestState::ManualReview,
            now,
            Some(reason),
            actor,
        )?;
        Ok(before)
    }

    /// The `process` decision, end to end, as ONE atomic unit: records
    /// the decision ([`Self::record_operator_decision`]), then runs the
    /// SAME shared resume every other recovery path uses — inbound
    /// ([`Self::resume_manual_review_sol_to_glc`] /
    /// [`Self::resume_manual_review_rhn_to_glc`]) or cross-route
    /// ([`Self::resume_manual_review_cross_route`]) by the row's own
    /// direction — which independently re-checks state, the fold-time
    /// reason, the refund lifecycle, both rolling-24h windows, the
    /// mature-UTXO floor, the safety buffer and the reserve invariant
    /// under the write lock. A refused resume rolls the decision back
    /// with it: the row stays held, unchanged, and the refusal is
    /// returned. There is no second payout implementation here.
    pub fn process_held_manual_review(
        &mut self,
        request_id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<ResumeManualReviewOutcome, LedgerError> {
        self.atomically("manual_review_process", |ledger| {
            let before = ledger.record_operator_decision(
                request_id,
                OperatorDecision::Process,
                note,
                actor,
                false,
                now,
            )?;
            let outcome = match before.direction {
                Direction::SolToGlc => {
                    ledger.resume_manual_review_sol_to_glc(request_id, note, actor, now)?
                }
                Direction::RhnToGlc => {
                    ledger.resume_manual_review_rhn_to_glc(request_id, note, actor, now)?
                }
                Direction::SolToRhn | Direction::RhnToSol => ledger
                    .resume_manual_review_cross_route(
                        before.direction,
                        request_id,
                        note,
                        actor,
                        now,
                    )?,
                Direction::GlcToSol | Direction::GlcToRhn => {
                    return Err(LedgerError::ManualReviewNotRecoverable {
                        id: request_id,
                        detail: format!(
                            "direction {:?} has no ManualReview resume path — a Goldcoin-sourced \
                             park is refunded, never re-admitted",
                            before.direction
                        ),
                    })
                }
            };
            Ok(outcome)
        })
    }

    /// Runs `f` as one unit: a `BEGIN IMMEDIATE` when standalone, a
    /// SAVEPOINT when an admin-action scope is already open
    /// ([`Self::begin_admin_action`]). Every `write_tx` inside `f` nests
    /// as its own savepoint, so a failure in the LAST of several
    /// mutations rolls back the earlier ones too — which is the whole
    /// point (a recorded decision must never outlive a refused resume).
    fn atomically<T>(
        &mut self,
        name: &str,
        f: impl FnOnce(&mut Self) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        let standalone = self.conn.is_autocommit();
        if standalone {
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
        } else {
            self.conn.execute_batch(&format!("SAVEPOINT {name}"))?;
        }
        match f(self) {
            Ok(v) => {
                if standalone {
                    self.conn.execute_batch("COMMIT")?;
                } else {
                    self.conn.execute_batch(&format!("RELEASE {name}"))?;
                }
                Ok(v)
            }
            Err(e) => {
                let _ = if standalone {
                    self.conn.execute_batch("ROLLBACK")
                } else {
                    self.conn
                        .execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"))
                };
                Err(e)
            }
        }
    }

    /// The "nothing has been paid, nothing is processing" predicate a
    /// hold or a decision requires: the row exists, is `ManualReview`,
    /// has no destination txid and no destination payout row. Returns
    /// the row's disposition for the caller's own rule.
    fn refuse_unless_holdable(
        tx: &Connection,
        request_id: i64,
    ) -> Result<ManualReviewDisposition, LedgerError> {
        let row: Option<(RequestState, Option<Vec<u8>>, ManualReviewDisposition)> = tx
            .query_row(
                "SELECT state, destination_txid, manual_review_disposition
                   FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((state, destination_txid, disposition)) = row else {
            return Err(LedgerError::RequestNotFound(request_id));
        };
        if state != RequestState::ManualReview {
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!(
                    "state is {state:?}, not ManualReview — a hold applies only to parked requests"
                ),
            });
        }
        if destination_txid.is_some() {
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "a destination txid is recorded — this request has been paid out"
                    .to_string(),
            });
        }
        let payout_rows: i64 = tx.query_row(
            "SELECT (SELECT COUNT(*) FROM goldcoin_payouts WHERE request_id = ?1)
                  + (SELECT COUNT(*) FROM robinhood_transactions
                      WHERE request_id = ?1 AND kind <> 'Refund')",
            [request_id],
            |r| r.get(0),
        )?;
        if payout_rows > 0 {
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "a destination payout row already exists — this request is processing"
                    .to_string(),
            });
        }
        Ok(disposition)
    }

    /// Whether ANY refund lifecycle marker exists for `request_id`: a
    /// `solana_refunds` row, a `goldcoin_refunds` row, or a `Refund`
    /// `robinhood_transactions` row. The same three markers the resume
    /// paths exclude on, gathered once.
    fn refund_lifecycle_exists_in(tx: &Connection, request_id: i64) -> Result<bool, LedgerError> {
        let n: i64 = tx.query_row(
            "SELECT (SELECT COUNT(*) FROM solana_refunds WHERE request_id = ?1)
                  + (SELECT COUNT(*) FROM goldcoin_refunds WHERE request_id = ?1)
                  + (SELECT COUNT(*) FROM robinhood_transactions
                      WHERE request_id = ?1 AND kind = 'Refund')",
            [request_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// The one predicate both resume entry points apply first: a held
    /// row is refused, by whoever asks (operator or the automatic pass),
    /// until the hold is ended by an explicit operator act. Refund paths
    /// deliberately do NOT call this — a hold keeps a row FOR refunding
    /// — they call [`Self::refuse_unless_decided`] instead.
    fn refuse_if_auto_resume_held(tx: &Connection, request_id: i64) -> Result<(), LedgerError> {
        #[allow(clippy::type_complexity)]
        let hold: Option<(
            Option<String>,
            Option<i64>,
            ManualReviewDisposition,
            Option<OperatorDecision>,
            Option<i64>,
        )> = tx
            .query_row(
                "SELECT auto_resume_hold_note, auto_resume_hold_until,
                        manual_review_disposition, operator_decision, review_after
                   FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((note, until, disposition, decision, review_after)) = hold else {
            return Ok(());
        };
        if disposition == ManualReviewDisposition::RapidBurstHold && decision.is_none() {
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!(
                    "held: rapid-burst hold (review_after={}) — resumes only through an explicit \
                     `glc-admin manual-review-process` decision, never automatically",
                    review_after.unwrap_or_default()
                ),
            });
        }
        if let Some(note) = note {
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!(
                    "held by operator (auto_resume_hold{}): {note} — release with \
                     `glc-admin manual-review-release`, or decide with \
                     `manual-review-process`, before resuming",
                    until.map(|u| format!(" until {u}")).unwrap_or_default()
                ),
            });
        }
        if disposition != ManualReviewDisposition::Normal && decision.is_none() {
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!(
                    "held ({}) with no operator decision recorded",
                    disposition.as_str()
                ),
            });
        }
        Ok(())
    }

    /// The refund-side twin of [`Self::refuse_if_auto_resume_held`]: a
    /// HELD row may enter a refund lifecycle only once the operator has
    /// recorded the `refund` decision ([`Self::record_operator_decision`])
    /// — so a held row is never refunded automatically, by a script, or
    /// by a command that did not state the decision. An unheld row is
    /// unaffected (every pre-v30 refund semantics applies unchanged).
    /// Returns the detail to refuse with, `None` when the refund may
    /// proceed.
    pub(crate) fn refund_hold_blocker_in(
        tx: &Connection,
        request_id: i64,
    ) -> Result<Option<String>, LedgerError> {
        #[allow(clippy::type_complexity)]
        let row: Option<(
            Option<String>,
            ManualReviewDisposition,
            Option<OperatorDecision>,
            Option<i64>,
        )> = tx
            .query_row(
                "SELECT auto_resume_hold_note, manual_review_disposition, operator_decision,
                        review_after
                   FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((note, disposition, decision, review_after)) = row else {
            return Ok(None);
        };
        let held = note.is_some() || disposition != ManualReviewDisposition::Normal;
        if !held {
            return Ok(None);
        }
        match decision {
            Some(OperatorDecision::Refund) => Ok(None),
            Some(other) => Ok(Some(format!(
                "held ({}) with operator decision `{}` recorded — not `refund`",
                disposition.as_str(),
                other.as_str()
            ))),
            None => Ok(Some(format!(
                "held ({}{}) — record the operator decision first: \
                 `glc-admin manual-review-refund` (it re-runs this refund once recorded)",
                disposition.as_str(),
                review_after
                    .map(|t| format!(", review_after={t}"))
                    .unwrap_or_default()
            ))),
        }
    }

    /// [`Self::refuse_unless_decided`] on the ledger's own connection,
    /// for callers outside a transaction (the Robinhood refund flow).
    pub fn refuse_refund_unless_decided(&self, request_id: i64) -> Result<(), LedgerError> {
        Self::refuse_unless_decided(&self.conn, request_id)
    }

    /// [`Self::refund_hold_blocker_in`] as a refusal.
    pub(crate) fn refuse_unless_decided(
        tx: &Connection,
        request_id: i64,
    ) -> Result<(), LedgerError> {
        match Self::refund_hold_blocker_in(tx, request_id)? {
            None => Ok(()),
            Some(detail) => Err(LedgerError::RefundNotEligible {
                id: request_id,
                detail,
            }),
        }
    }

    /// The one body behind both resume wrappers.
    ///
    /// Written as a single function on purpose. A parallel Robinhood
    /// implementation would have been free to drift on any of the safety
    /// checks between here and the commit — and the one that matters
    /// most, the unconditional rate-limit re-check that stops an operator
    /// resuming past a live window, is exactly the kind a second copy
    /// quietly loses. `expected_direction` is the ONLY policy input;
    /// everything else is identical by construction rather than by
    /// review.
    fn resume_manual_review_inbound(
        &mut self,
        expected_direction: Direction,
        request_id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<ResumeManualReviewOutcome, LedgerError> {
        debug_assert!(
            expected_direction.destination_is_goldcoin(),
            "resume_manual_review_inbound is only meaningful for an inbound-to-Goldcoin route"
        );
        let tx = write_tx(&mut self.conn)?;
        Self::refuse_if_auto_resume_held(&tx, request_id)?;

        #[allow(clippy::type_complexity)]
        let row: Option<(
            Direction,
            RequestState,
            Option<String>,
            i64,
            Option<i64>,
            Option<Vec<u8>>,
            Vec<u8>,
            i64,
            Option<Vec<u8>>,
        )> = tx
            .query_row(
                "SELECT direction, state, manual_review_note, net_destination_atomic,
                        source_finalized_at, destination_txid, recipient, created_at,
                        source_wallet
                 FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            direction,
            state,
            manual_review_note,
            net_destination_atomic,
            source_finalized_at,
            destination_txid,
            recipient,
            candidate_created_at,
            source_wallet,
        )) = row
        else {
            tx.rollback()?;
            return Err(LedgerError::RequestNotFound(request_id));
        };

        if direction != expected_direction {
            tx.rollback()?;
            // Two variants rather than one parameterized message: each
            // names the fold path that actually parks that direction, so
            // an operator who ran the wrong command is told which one to
            // run instead.
            return Err(match expected_direction {
                Direction::RhnToGlc => LedgerError::NotARhnToGlcRequest {
                    id: request_id,
                    actual_direction: direction,
                },
                _ => LedgerError::NotASolToGlcRequest {
                    id: request_id,
                    actual_direction: direction,
                },
            });
        }
        // A refund lifecycle, once begun, is one-way and permanent: a
        // request with a `solana_refunds` row (or in any refund state)
        // can NEVER be resumed, by any surface — CLI, admin API, or the
        // daemon's auto-resume, all of which come through this one
        // function. Checked against the ROW, not just the state, so an
        // out-of-band `bridge_requests.state` edit cannot re-open a
        // refunded request (defense in depth; the state check below
        // would refuse those too, but with a less actionable error).
        //
        // The durable marker is per-direction because the two routes
        // refund over different chains and record it in different tables:
        // `solana_refunds` for `SolToGlc` (a `rebalance_withdraw`), and a
        // `Refund`-kind row in `robinhood_transactions` for `RhnToGlc` (an
        // `executeRefund`). Both are written the moment a refund is
        // authorized, which is what makes this a real defence against an
        // out-of-band `state` edit rather than a restatement of the state
        // check below.
        let refund_state: Option<String> = match expected_direction {
            Direction::RhnToGlc => tx
                .query_row(
                    "SELECT state FROM robinhood_transactions
                     WHERE request_id = ?1 AND kind = 'Refund'",
                    [request_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?,
            _ => tx
                .query_row(
                    "SELECT state FROM solana_refunds WHERE request_id = ?1",
                    [request_id],
                    |r| r.get::<_, SolanaRefundState>(0),
                )
                .optional()?
                .map(|s| s.as_str().to_string()),
        };
        if let Some(refund_state) = refund_state {
            tx.rollback()?;
            return Err(LedgerError::RefundLifecycleExists {
                id: request_id,
                refund_state,
            });
        }
        // The source wallet whose 24-hour window this resume must respect:
        // `source_wallet` (schema v28), written by every fold in the same
        // statement as the row, and backfilled for every row that predates
        // it from the identity each route used to keep elsewhere. A NULL
        // FAILS CLOSED — the resume is refused rather than proceeding with
        // no source-wallet check at all, because "we could not find the
        // depositor" must never read as "the depositor is not limited."
        let Some(source_wallet) = source_wallet.filter(|w: &Vec<u8>| !w.is_empty()) else {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!(
                    "{} request has no source wallet recorded, so its source-wallet window \
                     cannot be checked",
                    direction.as_str()
                ),
            });
        };

        if state != RequestState::ManualReview {
            // Distinguishes a genuine repeat call (this exact command
            // already resumed this request, whether by an operator or by
            // automatic recovery) from a request that reached
            // SourceFinalized some other way (e.g. a normal fold) and was
            // never in ManualReview to begin with — the latter must still
            // be refused, not reported as a harmless no-op. No `actor`
            // filter: this exact (from = ManualReview, to =
            // SourceFinalized) transition is written ONLY by this
            // function (verified: no other call site in this file logs
            // it), so its mere presence — regardless of which actor
            // performed it — is unambiguous proof of a prior resume.
            let previously_resumed: bool = tx
                .query_row(
                    "SELECT 1 FROM bridge_request_state_log
                     WHERE request_id = ?1 AND from_state = ?2 AND to_state = ?3 LIMIT 1",
                    rusqlite::params![
                        request_id,
                        RequestState::ManualReview,
                        RequestState::SourceFinalized
                    ],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            tx.rollback()?;
            if previously_resumed {
                return Ok(ResumeManualReviewOutcome::AlreadyResumed { state });
            }
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!("state is {state:?}, not ManualReview"),
            });
        }

        // Read from `RECOVERABLE_MANUAL_REVIEW_REASONS` via the shared
        // predicate, never a second inline arm list: this function is the
        // enforcing path, and an inline copy here is exactly what drifted
        // out of step with the listing when the admission safety buffer's
        // reason was added (see that constant's docs).
        if !Self::is_recoverable_manual_review_reason(manual_review_note.as_deref()) {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!(
                    "manual_review_note {manual_review_note:?} is not a known recoverable reason"
                ),
            });
        }

        if source_finalized_at.is_none() {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "source deposit is not finalized".to_string(),
            });
        }

        if destination_txid.is_some() {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "a destination transaction already exists".to_string(),
            });
        }

        let existing_payout: Option<i64> = tx
            .query_row(
                "SELECT request_id FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        if existing_payout.is_some() {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "a Goldcoin payout already exists for this request".to_string(),
            });
        }

        // Both wallet windows, checked UNCONDITIONALLY, regardless of the
        // request's original `manual_review_note` — this is what makes
        // "manual operator resume must not bypass the 24-hour window"
        // true even for a request that was never parked for this reason
        // in the first place. Same query as every fold's check
        // (`Self::resume_wallet_windows`), so the two can never drift
        // apart on which states count.
        //
        // Only a STRICT PREDECESSOR — an earlier row, ordered by
        // `(created_at, id)` — may ever count as a blocker here. Without
        // this ordering restriction, a later-arriving sibling row to the
        // same wallet (itself still parked, since it necessarily arrived
        // after this one and so was itself limited) would shadow-block
        // this earlier, rightfully-next-in-line candidate — inverting
        // oldest-first draining, and in the worst case letting a steady
        // trickle of new same-wallet arrivals starve the oldest parked
        // request indefinitely. Restricting to `(created_at, id) <
        // (candidate's own)` makes that structurally impossible: this
        // candidate's eligibility can only ever depend on rows that
        // already existed before it did, never on ones that showed up
        // later. `(created_at, id)` rather than `created_at` alone breaks
        // ties deterministically when two rows share the same `created_at`
        // (insertion/id order is itself a legitimate secondary ordering,
        // since ids are assigned in strict creation order).
        //
        // Source first, so a wallet that is itself still inside its
        // window is reported ahead of a destination finding, matching
        // the eligibility API's precedence — a resume is refused either
        // way if EITHER independent window still applies.
        if let Err(window) = Self::resume_wallet_windows(
            &tx,
            direction,
            request_id,
            candidate_created_at,
            &source_wallet,
            &recipient,
            now,
        ) {
            tx.rollback()?;
            return Err(window);
        }

        let reserve = ReserveDirection::GoldcoinReserve;
        let (balance, protected_minimum, reserved, min_available_utxo_count): (i64, i64, i64, i64) =
            tx.query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity,
                    utxo_pool_min_available_count
             FROM reserve_ledger WHERE direction = ?1",
                [reserve],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;

        // The SAME count-based admission gate `fold_sol_deposit` applies to
        // a brand-new obligation (docs/09-runbook.md's "UTXO liquidity"
        // section), applied here to something already accepted: resuming a
        // parked request re-admits real demand on the mature UTXO pool
        // exactly as a fresh fold would, so it must never bypass the same
        // floor just because the request was accepted once before. `== 0`
        // means backpressure is disabled — identical short-circuit to
        // `fold_sol_deposit`'s own.
        let available_utxo_count: i64 = tx.query_row(
            &format!(
                "SELECT COUNT(*) FROM vault_utxos v
             WHERE v.state = 'Available'
               AND {deposit_excl}
               AND {claim_excl}",
                claim_excl = live_split_claim_exclusion("v"),
                deposit_excl = unfinalized_goldcoin_deposit_exclusion("v")
            ),
            [],
            |r| r.get(0),
        )?;
        let utxo_liquidity_ok =
            min_available_utxo_count == 0 || available_utxo_count > min_available_utxo_count;
        if !utxo_liquidity_ok {
            tx.rollback()?;
            return Err(LedgerError::UtxoLiquidityLow {
                request_id,
                available_utxo_count,
                min_available_count: min_available_utxo_count,
            });
        }

        let available = balance - protected_minimum - reserved;
        // Equivalent to requiring the reserve invariant still hold AFTER
        // this reservation is applied (balance >= protected_minimum +
        // reserved + net_destination_atomic) — the same check
        // `create_request`/`fold_sol_deposit` already use to admit
        // anything new, applied here to something already accepted.
        if net_destination_atomic > available {
            tx.rollback()?;
            return Err(LedgerError::InvariantViolated {
                direction: reserve,
                balance,
                protected_minimum,
                reserved_liquidity: reserved + net_destination_atomic,
            });
        }

        // The SAME confirmed-liquidity admission buffer `fold_sol_deposit`
        // applies to a brand-new obligation (docs/09-runbook.md's
        // "Confirmed-liquidity admission safety buffer"), applied here for
        // exactly the reason the UTXO-count floor above already is: a
        // resume re-admits real demand onto the reserve precisely as a
        // fresh fold would, so it must never bypass a floor a fresh fold
        // would have been held back by just because this request was
        // accepted once before. Same formula as the fold's per-request
        // half — `balance >= protected_minimum + reserved +
        // net_destination_atomic + buffer` — and the same disabled
        // short-circuit at `buffer <= 0`.
        //
        // Transient and self-clearing: the identical call succeeds once
        // headroom recovers. A deposit that will genuinely never be paid
        // out still has the refund path
        // (`REFUNDABLE_MANUAL_REVIEW_REASONS`), so this can hold a request
        // back but never strand it.
        //
        // Deliberately does NOT consult the direction-wide gate state
        // (`liquidity_admission_closed`), only the buffer arithmetic —
        // matching this function's existing posture of not checking
        // `admission_closed`/`paused`: those gates govern admitting NEW
        // demand, and a resume of an already-received deposit is judged on
        // whether the reserve can actually carry it right now.
        let (resume_buffer_atomic, _resume_reopen_atomic, _resume_closed) =
            Self::read_liquidity_admission_row(&tx, reserve)?;
        if resume_buffer_atomic > 0 && available - net_destination_atomic < resume_buffer_atomic {
            tx.rollback()?;
            return Err(LedgerError::AdmissionLiquidityBufferLow {
                request_id,
                headroom: available,
                net_destination_atomic,
                buffer_atomic: resume_buffer_atomic,
            });
        }

        tx.execute(
            "UPDATE bridge_requests SET state = ?1, manual_review_note = NULL WHERE id = ?2",
            rusqlite::params![RequestState::SourceFinalized, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::ManualReview),
            RequestState::SourceFinalized,
            now,
            Some(note),
            actor,
        )?;
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1,
                pending_obligations = pending_obligations + ?1 WHERE direction = ?2",
            rusqlite::params![net_destination_atomic, reserve],
        )?;
        tx.commit()?;
        Ok(ResumeManualReviewOutcome::Resumed)
    }

    // ------------------------------------------- ManualReview refunds (Solana) --
    //
    // Operator-driven refund of a fold-parked SolToGlc deposit back to the
    // ORIGINAL Solana depositor, via the on-chain `rebalance_withdraw`
    // instruction (admin signature + threshold attestation + global pause
    // + protected minimum + nonce replay guard — none of it weakened
    // here). The ledger side below owns eligibility, the one-refund-per-
    // request guarantees, the request state machine
    // (`ManualReview -> RefundPending -> RefundBroadcast -> Refunded`),
    // and the accounting integration; all chain I/O and transaction
    // construction live in `solana::refund`. See docs/09-runbook.md
    // "ManualReview refunds (Solana->Goldcoin)".

    /// The ManualReview reasons eligible for a refund — decision recorded
    /// 2026-09-01: the conservative, explicit whitelist. Every entry is a
    /// FOLD-TIME park reason (`fold_sol_deposit`), and every fold-time
    /// park structurally satisfies the two premises a safe refund needs:
    ///
    /// 1. **The deposit is finalized and verified**: `fold_sol_deposit`
    ///    only ever runs for a `WithdrawalObligation` the indexer read at
    ///    `finalized` commitment, and stamps `source_finalized_at` on the
    ///    parked row itself.
    /// 2. **No Goldcoin capacity was ever reserved**: the
    ///    `reserved_liquidity`/`pending_obligations` increment runs only
    ///    in the `capacity_ok` branch — a park happens INSTEAD of it, so
    ///    a refund has nothing to release.
    ///
    /// The wallet-window reasons (both roles, both spellings) are
    /// included because both premises hold for them exactly as for the
    /// capacity reasons (pinned by
    /// `ledger::tests::rate_limited_park_holds_no_reservation...`), and
    /// the existing resume path — which pays OUT, strictly more generous
    /// than returning the depositor's own funds — already treats them as
    /// recoverable. Everything else (the GlcToSol-only reasons
    /// `late_deposit_no_capacity`/`deposit_amount_mismatch: ...`/
    /// `deposit_spent_before_finalized`, a NULL note, any future or
    /// unknown string) is refused — an ambiguous reason is excluded, not
    /// broadened; the independent settlement-evidence checks run
    /// regardless.
    pub const REFUNDABLE_MANUAL_REVIEW_REASONS: [&'static str; 11] = [
        Self::MANUAL_REVIEW_REASON_ADMISSION_CLOSED,
        // Both premises hold identically to the reserve-wide reason
        // above: the park happened INSTEAD OF reserving Goldcoin
        // capacity, on an already-finalized deposit. A route an operator
        // has closed may stay closed indefinitely, so leaving this out
        // would make it the one park with no exit.
        Self::MANUAL_REVIEW_REASON_ROUTE_ADMISSION_CLOSED,
        Self::MANUAL_REVIEW_REASON_PAUSED,
        Self::MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY,
        Self::MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW,
        // Same two premises as every other entry: the park happened
        // INSTEAD OF reserving Goldcoin capacity, on an already-finalized
        // deposit. A deposit held back by the safety buffer is exactly the
        // kind that may end up genuinely unpayable, so it must remain
        // refundable rather than becoming the one park with no exit.
        Self::MANUAL_REVIEW_REASON_LIQUIDITY_BUFFER_LOW,
        Self::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT,
        Self::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT,
        Self::LEGACY_MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED,
        Self::LEGACY_MANUAL_REVIEW_REASON_SOURCE_WALLET_RATE_LIMITED,
        // A rapid-burst hold is refundable — but only once the operator
        // has recorded the `refund` decision (`refuse_unless_decided`,
        // checked by every refund begin-path). The reason being listed
        // is necessary, never sufficient.
        Self::MANUAL_REVIEW_REASON_RAPID_BURST_HOLD,
    ];

    /// The `SolToRhn`-only fold-time park reasons that are refundable in
    /// addition to [`Self::REFUNDABLE_MANUAL_REVIEW_REASONS`].
    ///
    /// Both hold the two premises that list is built on — the park is
    /// written by `fold_sol_deposit_to_robinhood` on a deposit the indexer
    /// read at `finalized` commitment, INSTEAD OF reserving any capacity —
    /// and neither can occur for `SolToGlc`, which has no route gate at
    /// fold time and validates its destination at payout time instead:
    ///
    /// - `route_disabled_at_fold`: the deposit landed while the route was
    ///   closed. A closed route may stay closed indefinitely, and this
    ///   park is deliberately NOT resumable, so a refund is its only exit.
    /// - `undeliverable destination: ...`: the `0x` payload is not a
    ///   usable EVM address (bad hex, bad checksum, the zero address). It
    ///   will never be payable, so a refund is its only exit. Matched by
    ///   prefix because the detail names the specific parse failure.
    pub const SOL_TO_RHN_REFUNDABLE_REASON: &'static str =
        Self::MANUAL_REVIEW_REASON_ROUTE_DISABLED;
    pub const SOL_TO_RHN_REFUNDABLE_REASON_PREFIX: &'static str = "undeliverable destination";

    /// Whether `note` is a fold-time park reason a Solana-side refund may
    /// act on for a request of `direction` — the whitelist, plus the two
    /// `SolToRhn`-only reasons above for that direction alone.
    pub fn is_refundable_manual_review_reason(direction: Direction, note: Option<&str>) -> bool {
        let Some(note) = note else {
            return false;
        };
        if Self::REFUNDABLE_MANUAL_REVIEW_REASONS.contains(&note) {
            return true;
        }
        direction == Direction::SolToRhn
            && (note == Self::SOL_TO_RHN_REFUNDABLE_REASON
                || note.starts_with(Self::SOL_TO_RHN_REFUNDABLE_REASON_PREFIX))
    }

    /// High bit of the `rebalance_withdraw` nonce space, reserved for
    /// ManualReview refunds: `nonce = DOMAIN | request_id`. Ordinary
    /// operator rebalance withdrawals use small monotonic counters or
    /// Unix timestamps (RESERVE_EMERGENCY_WITHDRAWAL_RUNBOOK.md), all far
    /// below `2^63`, so the two namespaces can never collide — and one
    /// request maps to exactly one nonce forever, which makes the nonce's
    /// on-chain `rebalance_withdrawal` PDA a per-request replay guard
    /// that survives even a database restore.
    pub const SOLANA_REFUND_NONCE_DOMAIN: u64 = 1 << 63;

    /// The deterministic refund nonce for `request_id`. Fails (rather
    /// than wrapping) on a non-positive id — SQLite rowids start at 1.
    pub fn solana_refund_nonce(request_id: i64) -> Result<u64, LedgerError> {
        if request_id <= 0 {
            return Err(LedgerError::RefundNotEligible {
                id: request_id,
                detail: "request id must be positive".to_string(),
            });
        }
        Ok(Self::SOLANA_REFUND_NONCE_DOMAIN | request_id as u64)
    }

    /// One shared evaluation of every DATABASE-side refund precondition —
    /// used read-only by the CLI dry run and, inside its own write
    /// transaction, by [`Ledger::begin_solana_refund`], so the printed
    /// checks and the enforced gate are the same code.
    fn evaluate_solana_refund_db_checks(
        conn: &Connection,
        request_id: i64,
    ) -> Result<SolanaRefundDbChecks, LedgerError> {
        #[allow(clippy::type_complexity)]
        let row: Option<(
            Direction,
            RequestState,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            Option<i64>,
        )> = conn
            .query_row(
                "SELECT direction, state, manual_review_note, source_finalized_at,
                        source_obligation_index, requester, destination_txid, settled_at
                 FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            direction,
            state,
            manual_review_reason,
            source_finalized_at,
            source_obligation_index,
            requester,
            destination_txid,
            settled_at,
        )) = row
        else {
            return Err(LedgerError::RequestNotFound(request_id));
        };

        // No destination payout of EITHER shape: a `goldcoin_payouts` row
        // (the `SolToGlc` payout) or a `Payout`-kind `robinhood_transactions`
        // row (the `SolToRhn` payout). Both are checked for both
        // directions — a Robinhood payout row naming a `SolToGlc` request
        // is a contradiction, and a refund is not the moment to discover
        // one.
        let no_goldcoin_payout = conn
            .query_row(
                "SELECT 1 FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |_| Ok(()),
            )
            .optional()?
            .is_none()
            && conn
                .query_row(
                    "SELECT 1 FROM robinhood_transactions
                     WHERE request_id = ?1 AND kind = 'Payout'",
                    [request_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_none();
        // Any transition INTO SourceFinalized or beyond means the request
        // was not a pure fold-time park (it was resumed, or advanced some
        // other way) and may hold reserved liquidity — never refundable.
        let never_advanced_past_manual_review = conn
            .query_row(
                "SELECT 1 FROM bridge_request_state_log
                 WHERE request_id = ?1
                   AND to_state IN ('SourceFinalized', 'SettlementAuthorized',
                                    'DestinationSubmitted', 'DestinationConfirmed', 'Settled')
                 LIMIT 1",
                [request_id],
                |_| Ok(()),
            )
            .optional()?
            .is_none();
        let existing_refund: Option<SolanaRefundState> = conn
            .query_row(
                "SELECT state FROM solana_refunds WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;

        let reason_whitelisted =
            Self::is_refundable_manual_review_reason(direction, manual_review_reason.as_deref());

        Ok(SolanaRefundDbChecks {
            direction,
            // Both Solana-SOURCED directions refund through the same
            // `refund_withdraw`: the deposit is the same
            // `WithdrawalObligation` whichever chain it was bound for.
            direction_ok: direction.source_is_solana(),
            state,
            state_is_manual_review: state == RequestState::ManualReview,
            manual_review_reason,
            reason_whitelisted,
            source_finalized: source_finalized_at.is_some(),
            has_obligation_index: source_obligation_index.is_some(),
            has_requester: requester.as_deref().is_some_and(|r| r.len() == 32),
            no_destination_txid: destination_txid.is_none(),
            not_settled: settled_at.is_none(),
            no_goldcoin_payout,
            never_advanced_past_manual_review,
            hold_blocker: Self::refund_hold_blocker_in(conn, request_id)?,
            existing_refund,
        })
    }

    /// Read-only view of the database-side refund checks — the dry run's
    /// data source. Contacts nothing, writes nothing.
    pub fn solana_refund_db_checks(
        &self,
        request_id: i64,
    ) -> Result<SolanaRefundDbChecks, LedgerError> {
        Self::evaluate_solana_refund_db_checks(&self.conn, request_id)
    }

    fn evaluate_solana_refund_capacity(
        conn: &Connection,
        request_id: i64,
        amount_solana_atomic: u64,
    ) -> Result<SolanaRefundCapacityCheck, LedgerError> {
        let (total_reserve_balance, protected_minimum, reserved_liquidity): (i64, i64, i64) = conn
            .query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
                 FROM reserve_ledger WHERE direction = ?1",
                [ReserveDirection::SolanaReserve],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(LedgerError::ReserveNotInitialized(
                ReserveDirection::SolanaReserve,
            ))?;
        // Other refunds already committed but not yet reflected in the
        // cached balance (their book decrement happens at Confirmed).
        let other_open_refunds_atomic: i64 = conn.query_row(
            "SELECT COALESCE(SUM(amount_solana_atomic), 0) FROM solana_refunds
             WHERE state IN ('Pending', 'Broadcast') AND request_id != ?1",
            [request_id],
            |r| r.get(0),
        )?;
        let available = total_reserve_balance
            - protected_minimum
            - reserved_liquidity
            - other_open_refunds_atomic;
        Ok(SolanaRefundCapacityCheck {
            amount_solana_atomic,
            total_reserve_balance,
            protected_minimum,
            reserved_liquidity,
            other_open_refunds_atomic,
            ok: (amount_solana_atomic as i64) <= available && amount_solana_atomic > 0,
        })
    }

    /// Read-only SolanaReserve capacity check for a refund of
    /// `amount_solana_atomic` — the dry run's data source; enforced again
    /// inside [`Ledger::begin_solana_refund`]'s own transaction.
    pub fn solana_refund_capacity(
        &self,
        request_id: i64,
        amount_solana_atomic: u64,
    ) -> Result<SolanaRefundCapacityCheck, LedgerError> {
        Self::evaluate_solana_refund_capacity(&self.conn, request_id, amount_solana_atomic)
    }

    pub fn get_solana_refund(&self, request_id: i64) -> Result<Option<SolanaRefund>, LedgerError> {
        self.conn
            .query_row(
                &format!("{SELECT_SOLANA_REFUND} WHERE request_id = ?1"),
                [request_id],
                row_to_solana_refund,
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Every refund lifecycle, oldest first; `open_only` restricts to
    /// rows not yet `Confirmed`. Read-only.
    pub fn list_solana_refunds(&self, open_only: bool) -> Result<Vec<SolanaRefund>, LedgerError> {
        let sql = if open_only {
            format!(
                "{SELECT_SOLANA_REFUND} WHERE state IN ('Pending', 'Broadcast') \
                 ORDER BY request_id"
            )
        } else {
            format!("{SELECT_SOLANA_REFUND} ORDER BY request_id")
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], row_to_solana_refund)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Begins the refund lifecycle: re-runs EVERY database-side
    /// eligibility check inside one `BEGIN IMMEDIATE` transaction,
    /// cross-checks the caller's chain-verified inputs against the stored
    /// request row, checks SolanaReserve capacity, then atomically
    /// inserts the `solana_refunds` row (state `Pending`, with the
    /// deterministic nonce) and moves the request
    /// `ManualReview -> RefundPending`.
    ///
    /// Concurrency: two racing calls serialize on SQLite's write lock;
    /// the loser re-reads state under the lock, finds either the state no
    /// longer `ManualReview` or the refund row already present, and
    /// refuses — the `request_id` PRIMARY KEY and UNIQUE nonce are the
    /// structural backstop even if every check were somehow bypassed.
    ///
    /// Deliberately does NOT release any Goldcoin-side reservation: the
    /// `never_advanced_past_manual_review` check PROVES none was ever
    /// applied for an eligible request, and any request where one was is
    /// refused outright (fail closed, never subtract blindly).
    pub fn begin_solana_refund(
        &mut self,
        request_id: i64,
        verified: &VerifiedRefundInputs,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let nonce = Self::solana_refund_nonce(request_id)?;
        assert!(
            !note.trim().is_empty(),
            "caller must supply a non-empty note"
        );
        let tx = write_tx(&mut self.conn)?;

        let checks = Self::evaluate_solana_refund_db_checks(&tx, request_id)?;
        if let Some(detail) = checks.first_failure_for_begin() {
            tx.rollback()?;
            return Err(LedgerError::RefundNotEligible {
                id: request_id,
                detail,
            });
        }

        // Cross-check the caller's chain-verified inputs against the
        // stored row — the two derive from the same on-chain obligation,
        // so ANY disagreement means tampering or corruption somewhere and
        // is a hard refusal, never a "pick one side" decision.
        let (stored_obligation_index, stored_requester, stored_gross): (i64, Vec<u8>, i64) = tx
            .query_row(
                "SELECT source_obligation_index, requester, gross_amount_atomic
                 FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
        if stored_obligation_index as u64 != verified.obligation_index {
            tx.rollback()?;
            return Err(LedgerError::RefundNotEligible {
                id: request_id,
                detail: format!(
                    "stored source_obligation_index {stored_obligation_index} does not match the \
                     on-chain-verified obligation index {}",
                    verified.obligation_index
                ),
            });
        }
        if stored_requester.as_slice() != verified.requester {
            tx.rollback()?;
            return Err(LedgerError::RefundNotEligible {
                id: request_id,
                detail: "stored requester does not match the on-chain obligation's requester"
                    .to_string(),
            });
        }
        if stored_gross as u64 != verified.gross_canonical_atomic {
            tx.rollback()?;
            return Err(LedgerError::RefundNotEligible {
                id: request_id,
                detail: format!(
                    "stored gross_amount_atomic {stored_gross} does not match the verified \
                     canonical gross {}",
                    verified.gross_canonical_atomic
                ),
            });
        }

        let capacity =
            Self::evaluate_solana_refund_capacity(&tx, request_id, verified.amount_solana_atomic)?;
        if !capacity.ok {
            tx.rollback()?;
            return Err(LedgerError::InvariantViolated {
                direction: ReserveDirection::SolanaReserve,
                balance: capacity.total_reserve_balance,
                protected_minimum: capacity.protected_minimum,
                reserved_liquidity: capacity.reserved_liquidity
                    + capacity.other_open_refunds_atomic
                    + verified.amount_solana_atomic as i64,
            });
        }

        tx.execute(
            "INSERT INTO solana_refunds
                (request_id, obligation_index, nonce, amount_solana_atomic, requester,
                 destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'Pending', ?12)",
            rusqlite::params![
                request_id,
                verified.obligation_index as i64,
                nonce as i64,
                verified.amount_solana_atomic as i64,
                verified.requester.as_slice(),
                verified.destination_token_account.as_slice(),
                verified.reserve_mint.as_slice(),
                verified.token_program.as_slice(),
                checks
                    .manual_review_reason
                    .as_deref()
                    .expect("reason_whitelisted implies a reason"),
                note.trim(),
                actor,
                now,
            ],
        )?;
        // The request row's own manual_review_note (and every source/
        // deposit column) is deliberately left untouched — the refund
        // never overwrites original evidence.
        tx.execute(
            "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![RequestState::RefundPending, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::ManualReview),
            RequestState::RefundPending,
            now,
            Some(note.trim()),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records the refund transaction's signature and moves the lifecycle
    /// to `Broadcast` / the request to `RefundBroadcast`. Called BEFORE
    /// the actual network send, so real fund movement is never
    /// un-evidenced: a crash between this commit and the send is
    /// recovered by the deterministic nonce (check the
    /// `rebalance_withdrawal` PDA / recorded signature), never by
    /// constructing a second transfer.
    ///
    /// Latest-wins on re-record: a recovery re-sign after blockhash
    /// expiry (same nonce, fresh blockhash, therefore a new signature)
    /// overwrites `refund_signature`, exactly like
    /// [`Ledger::record_goldcoin_completion_submitted`]. Refused once
    /// `Confirmed`.
    pub fn record_solana_refund_broadcast(
        &mut self,
        request_id: i64,
        refund_signature: &str,
        recent_blockhash: &str,
        attestation_epoch: u64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (refund_state, request_state): (SolanaRefundState, RequestState) = {
            let refund_state: Option<SolanaRefundState> = tx
                .query_row(
                    "SELECT state FROM solana_refunds WHERE request_id = ?1",
                    [request_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(refund_state) = refund_state else {
                tx.rollback()?;
                return Err(LedgerError::RefundNotFound(request_id));
            };
            let request_state: RequestState = tx.query_row(
                "SELECT state FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )?;
            (refund_state, request_state)
        };
        if refund_state == SolanaRefundState::Confirmed {
            tx.rollback()?;
            return Err(LedgerError::RefundWrongState {
                id: request_id,
                expected: "Pending or Broadcast",
                actual: refund_state.as_str().to_string(),
            });
        }
        assert!(
            matches!(
                request_state,
                RequestState::RefundPending | RequestState::RefundBroadcast
            ),
            "refund row is {refund_state:?} but request {request_id} is {request_state:?} — \
             caller bug or out-of-band edit",
        );

        tx.execute(
            "UPDATE solana_refunds
             SET state = 'Broadcast', refund_signature = ?1, recent_blockhash = ?2,
                 attestation_epoch = ?3, broadcast_at = ?4
             WHERE request_id = ?5",
            rusqlite::params![
                refund_signature,
                recent_blockhash,
                attestation_epoch as i64,
                now,
                request_id
            ],
        )?;
        if request_state == RequestState::RefundPending {
            tx.execute(
                "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
                rusqlite::params![RequestState::RefundBroadcast, request_id],
            )?;
            log_transition(
                &tx,
                request_id,
                Some(RequestState::RefundPending),
                RequestState::RefundBroadcast,
                now,
                Some(&format!("refund tx {refund_signature}")),
                "system",
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Terminal transition: the refund transaction confirmed at
    /// `finalized` commitment. Atomically marks the refund `Confirmed`,
    /// the request `Refunded`, and debits the SolanaReserve cached
    /// `total_reserve_balance` by the refunded amount — the same
    /// immediate self-caused-drop bookkeeping settlement does
    /// (`mark_release_confirmed`'s rationale), so reconciliation never
    /// misreads the refund as an unexplained loss. Idempotent: calling
    /// again once `Confirmed` is a no-op.
    pub fn mark_solana_refund_confirmed(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(SolanaRefundState, i64)> = tx
            .query_row(
                "SELECT state, amount_solana_atomic FROM solana_refunds WHERE request_id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((refund_state, amount_solana_atomic)) = row else {
            tx.rollback()?;
            return Err(LedgerError::RefundNotFound(request_id));
        };
        if refund_state == SolanaRefundState::Confirmed {
            tx.rollback()?;
            return Ok(());
        }
        if refund_state != SolanaRefundState::Broadcast {
            tx.rollback()?;
            return Err(LedgerError::RefundWrongState {
                id: request_id,
                expected: "Broadcast",
                actual: refund_state.as_str().to_string(),
            });
        }
        let request_state: RequestState = tx.query_row(
            "SELECT state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        assert_eq!(
            request_state,
            RequestState::RefundBroadcast,
            "refund row is Broadcast but request {request_id} is {request_state:?} — caller bug \
             or out-of-band edit",
        );
        let signature: Option<String> = tx.query_row(
            "SELECT refund_signature FROM solana_refunds WHERE request_id = ?1",
            [request_id],
            |r| r.get(0),
        )?;

        tx.execute(
            "UPDATE solana_refunds SET state = 'Confirmed', confirmed_at = ?1 WHERE request_id = ?2",
            rusqlite::params![now, request_id],
        )?;
        tx.execute(
            "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![RequestState::Refunded, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::RefundBroadcast),
            RequestState::Refunded,
            now,
            signature.as_deref(),
            "system",
        )?;
        // The refund left the reserve on-chain; reflect it in the cached
        // book immediately (never touches reserved_liquidity/
        // pending_obligations/settled_liquidity_total — an eligible
        // refund provably never held any of those, and it is not a
        // settlement).
        tx.execute(
            "UPDATE reserve_ledger SET total_reserve_balance = total_reserve_balance - ?1
             WHERE direction = ?2",
            rusqlite::params![amount_solana_atomic, ReserveDirection::SolanaReserve],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn last_synced_obligation_count(&self) -> Result<u64, LedgerError> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT last_obligation_count FROM solana_indexer_state WHERE id = 0",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(n as u64)
    }

    pub fn set_last_synced_obligation_count(
        &mut self,
        count: u64,
        slot: u64,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO solana_indexer_state (id, last_obligation_count, last_checked_slot, updated_at)
             VALUES (0, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET last_obligation_count = ?1, last_checked_slot = ?2, updated_at = ?3",
            rusqlite::params![count as i64, slot as i64, now],
        )?;
        Ok(())
    }

    /// `(total_reserve_balance, protected_minimum, reserved_liquidity, pending_obligations)`
    /// — used by the reconciliation engine.
    pub fn reserve_snapshot(
        &self,
        direction: ReserveDirection,
    ) -> Result<(u64, u64, u64, u64), LedgerError> {
        self.conn
            .query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity, pending_obligations
                 FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64, r.get::<_, i64>(3)? as u64)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => LedgerError::ReserveNotInitialized(direction),
                other => LedgerError::Sqlite(other),
            })
    }

    /// Sum of `net_destination_atomic` across every request whose
    /// settlement has been broadcast to `direction`'s chain but whose
    /// value the cached book has NOT yet been debited for — the real,
    /// currently-pending amount that can legitimately explain an
    /// observed balance drop at the exact instant a reconciliation tick
    /// runs before this service's own indexer has caught up
    /// (docs/24-load-soak-harness.md's documented `InFlightExplained`
    /// gap; reconciliation module docs).
    ///
    /// # Which states count, per direction
    ///
    /// `DestinationSubmitted` always: the destination transaction is on
    /// its way and nothing has been debited. `DestinationConfirmed` only
    /// for a direction whose destination reserve is debited at `Settled`
    /// (`Direction::destination_debited_at_destination_confirmed` is
    /// `false`: the two Goldcoin-bound routes, whose vault is
    /// UTXO-reconciled). For every other direction the book was debited
    /// the moment the destination leg went final, and a
    /// `DestinationConfirmed` row — which `SolToRhn`/`RhnToSol` occupy
    /// for as long as their source-side close-out takes — must NOT be
    /// counted again: the observed balance and the cached balance
    /// already agree on that value, so counting it would explain a
    /// second, genuine drop of the same size as "in flight".
    ///
    /// For `GoldcoinReserve` specifically, also adds the FULL input value
    /// (`payout_atomic + change_atomic + fee_atomic`, not just the net
    /// payout amount) of every `goldcoin_payouts` row still in
    /// `Broadcast` state — a UTXO-based-chain-specific effect with no
    /// SolanaReserve equivalent: spending the vault's UTXO to fund a
    /// payout makes that UTXO's *entire* value (the paid-out portion AND
    /// its change) temporarily invisible to a confirmed-only balance read
    /// until the transaction itself confirms and the change output
    /// matures, even though none of that value has actually left the
    /// vault's control yet (the change returns to it). Only the net
    /// payout amount would under-explain this drop.
    ///
    /// For `RobinhoodReserve`, also adds every outbound
    /// `robinhood_transactions` operation this service has broadcast whose
    /// debit against the Robinhood book has not landed yet
    /// ([`Ledger::robinhood_outbound_in_flight_atomic`]): a Robinhood-bound
    /// request never passes through `DestinationSubmitted`, so the generic
    /// term above cannot see its payout in flight.
    ///
    /// Used only to CAP how much of an already-observed drop
    /// reconciliation treats as explained — it never manufactures
    /// headroom, and the hard solvency invariant in `reconcile` is
    /// checked against the real observed balance independent of this
    /// figure.
    pub fn pending_destination_settlement_amount(
        &self,
        direction: ReserveDirection,
        now: i64,
    ) -> Result<u64, LedgerError> {
        // Which settlement directions DRAW DOWN this reserve. Two of the
        // three reserves are now the destination of more than one
        // direction — the Goldcoin vault pays out both `SolToGlc` and
        // `RhnToGlc` — so this is a SET, not a single value. Summing only
        // one of them would understate the pending draw and let
        // reconciliation read a real, explained settlement as an
        // unexplained balance drop.
        //
        // DERIVED from `Direction::destination_reserve` rather than
        // listed, so a direction added to the enum is automatically
        // counted against the reserve it draws down.
        let bridge_directions: Vec<Direction> = Direction::ALL
            .into_iter()
            .filter(|d| d.destination_reserve() == direction)
            .collect();
        let mut settlement_amount: i64 = 0;
        for bridge_direction in &bridge_directions {
            // See the function docs: a `DestinationConfirmed` row is
            // still pending against the BOOK only where the reserve is
            // debited at `Settled`. The two literals are the complete
            // set of states between "destination transaction sent" and
            // "book debited" for each shape; `Settled` never counts.
            let pending_states = if bridge_direction.destination_debited_at_destination_confirmed()
            {
                "('DestinationSubmitted')"
            } else {
                "('DestinationSubmitted', 'DestinationConfirmed')"
            };
            settlement_amount += self.conn.query_row(
                &format!(
                    "SELECT COALESCE(SUM(net_destination_atomic), 0) FROM bridge_requests
                     WHERE direction = ?1 AND state IN {pending_states}"
                ),
                [bridge_direction],
                |r| r.get::<_, i64>(0),
            )?;
        }
        let mut total = settlement_amount as u64;
        if direction == ReserveDirection::SolanaReserve {
            // A ManualReview refund whose transaction is recorded/
            // broadcast but not yet Confirmed is this reserve's own
            // in-flight outflow: the on-chain balance may already show
            // the drop while the cached book still doesn't (the book is
            // debited atomically at `mark_solana_refund_confirmed`,
            // mirroring settlement's confirm-time debit). Without this
            // term, a reconciliation pass landing inside that window
            // would classify the refund as an unexplained drop and latch
            // the sticky auto-pause. Bounded by real, recorded refund
            // rows — never a guess — and capped by `raw_drop` at the
            // call site like every other explanation term.
            let broadcast_refund_value: i64 = self.conn.query_row(
                "SELECT COALESCE(SUM(amount_solana_atomic), 0) FROM solana_refunds
                 WHERE state = 'Broadcast'",
                [],
                |r| r.get(0),
            )?;
            total = total.saturating_add(broadcast_refund_value as u64);
        }
        if direction == ReserveDirection::GoldcoinReserve {
            let broadcast_payout_value: i64 = self.conn.query_row(
                "SELECT COALESCE(SUM(payout_atomic + change_atomic + fee_atomic), 0)
                 FROM goldcoin_payouts WHERE state = 'Broadcast'",
                [],
                |r| r.get(0),
            )?;
            total = total.saturating_add(broadcast_payout_value as u64);
            // Additional, live-state-grounded coverage for change fan-out
            // (docs/09-runbook.md's "UTXO liquidity" section): the term
            // above only covers a payout while its OWN lifecycle state is
            // still `Broadcast` — once it reaches `Confirmed` (its own tx
            // has enough confirmations) but its change output(s) haven't
            // yet independently reached `vault_min_confirmations` maturity
            // in `vault_utxos` (a separate, differently-configured
            // threshold), that term stops counting it even though the
            // change is still genuinely immature. `own_unconfirmed_change_
            // atomic` closes that gap by checking the PHYSICAL UTXO state
            // directly rather than the payout's lifecycle state, so it is
            // never stale and never double-counts a change output that
            // has already matured (which the `Broadcast`-only term above
            // also cannot do, since maturity always implies the amount is
            // already reflected in a fresh `observed_balance`). The two
            // terms overlap (both can cover the same change amount while a
            // payout is genuinely `Broadcast` and still immature) — purely
            // additive over-explanation, capped by `raw_drop` at the call
            // site, never a weakening of the hard invariant or of
            // `unexplained_drop` detection for any amount beyond what is
            // genuinely, currently known to be this service's own in-flight
            // change.
            total = total.saturating_add(self.own_unconfirmed_change_atomic(now)?);
            // The real Goldcoin network fee genuinely, permanently leaves
            // the vault the instant a payout broadcasts — unlike the
            // payout (covered by `net_destination_atomic` above, for as
            // long as `bridge_requests.state` remains DestinationSubmitted/
            // DestinationConfirmed) and the change (covered by
            // `own_unconfirmed_change_atomic`), there is no OTHER term
            // that ever explains it once the payout moves past
            // `Broadcast`. Narrow and small in practice (one real network
            // fee), but a genuine drop that must still be accounted for at
            // `reconciliation_tolerance = 0` if a reconciliation catch-up
            // happens to land after the payout has already confirmed.
            let confirmed_fee_value: i64 = self.conn.query_row(
                "SELECT COALESCE(SUM(fee_atomic), 0) FROM goldcoin_payouts WHERE state = 'Confirmed'",
                [],
                |r| r.get(0),
            )?;
            total = total.saturating_add(confirmed_fee_value as u64);
            // A vault UTXO split's network fee is the same kind of genuine,
            // permanent departure as a payout's fee above: the source's
            // full value disappears from a confirmed-only balance read the
            // instant the split broadcasts, its chunk outputs are covered
            // by `own_unconfirmed_change_atomic` (inserted as `Unconfirmed`
            // rows atomically at broadcast — `record_vault_utxo_split_
            // broadcast`), and matured chunks are already back in
            // `observed_balance` — leaving exactly the fee with no other
            // term to explain it. Unlike a payout's fee, nothing ever
            // debits `total_reserve_balance` for a split (a split is not a
            // settlement), so the book stays permanently high by exactly
            // this sum and the term must keep explaining it FOREVER — the
            // `('Broadcast','Confirmed')` filter is deliberate, not a
            // missing retirement. This is exact accounting of real fees,
            // not slack: any unexplained loss still shows up ON TOP of it.
            // (Follow-up considered and deferred: debiting the book at
            // `Confirmed` and retiring the term, matching payout
            // settlement — an accounting-model change needing its own
            // review; cumulative magnitude is ~0.008 GLC per split.)
            // The same missing-inputs grace filter as the chunk terms:
            // a split whose transaction is very likely never confirming
            // never actually paid its fee either.
            let split_fee_value: i64 = self.conn.query_row(
                &format!(
                    "SELECT COALESCE(SUM(s.fee_atomic), 0) FROM vault_utxo_splits s WHERE {}",
                    split_chunks_still_explainable("?1")
                ),
                [now],
                |r| r.get(0),
            )?;
            total = total.saturating_add(split_fee_value as u64);
        }
        if direction == ReserveDirection::RobinhoodReserve {
            total = total.saturating_add(self.robinhood_outbound_in_flight_atomic(now)?);
        }
        Ok(total)
    }

    /// Value this service has already sent OUT of the Robinhood custody
    /// contract through its own `robinhood_transactions` rows, whose
    /// debit against the Robinhood book has not landed yet — the
    /// Robinhood-side twin of the `goldcoin_payouts`/`solana_refunds`
    /// `Broadcast` terms above, and the fix for a real production
    /// incident (2026-09-12, request 4039): a `GlcToRhn` payout's
    /// `executePayout` mined and `balanceOf(bridge)` dropped by the net
    /// amount one reconciliation tick BEFORE the settlement loop read the
    /// receipt and debited the book, so `reconcile` classified the
    /// service's own payout as an unexplained drop and auto-paused the
    /// reserve at `reconciliation_tolerance = 0`.
    ///
    /// # Why `bridge_requests.state` alone cannot see this
    ///
    /// A Robinhood-bound request stays `SourceFinalized` for the whole
    /// outbound lifecycle — `Authorizing -> Authorized -> Signed ->
    /// Broadcast -> Included -> Finalized` all live on the
    /// `robinhood_transactions` row, and `record_robinhood_broadcast`
    /// touches the request row not at all — so the `DestinationSubmitted`
    /// filter the generic term applies never matches one. The Goldcoin and
    /// Solana legs each move the request to `DestinationSubmitted` at
    /// broadcast; this leg records the same fact on its own operation
    /// row instead, and this term reads it from there.
    ///
    /// # Which rows count, and for exactly how long
    ///
    /// An OUTBOUND kind only — `Payout` (`GlcToRhn`/`SolToRhn`, the value
    /// leaves the contract for the recipient) and `TreasuryWithdraw` (it
    /// leaves for the treasury). `Settlement` moves no tokens, and a
    /// `Refund` returns an encumbered deposit the book never counted as
    /// reserve in the first place (`mark_robinhood_refund_confirmed`).
    ///
    /// The row counts from the moment its bytes were handed to a node
    /// (`Broadcast` — it may already be mined before the next receipt
    /// poll) through `Included` (mined, awaiting depth) and, once
    /// `Finalized`, until the SAME operation's request-side completion
    /// debits the book: `mark_robinhood_payout_settled` moves the request
    /// to `DestinationConfirmed`/`Settled` and debits in one transaction,
    /// `confirm_rebalance` moves the rebalance request to `Confirmed` and
    /// debits in one transaction. That request-side state — not the
    /// operation's own — is the retirement condition, so the amount stops
    /// being explained exactly when, and exactly once, the cached balance
    /// starts reflecting it. Counting `Finalized` too closes the window
    /// between `update_robinhood_confirmations` committing the promotion
    /// and `on_finalized` running the debit (an `eth_call` to the replay
    /// guard sits between the two).
    ///
    /// # What never counts
    ///
    /// - `Reverted`: the contract state did not change and no value
    ///   left; the request is parked for a human.
    /// - `ManualReview`: the stale-broadcast rule (`submitter::
    ///   broadcast_is_stale`) or a contradicted receipt already handed the
    ///   operation to an operator. If such a transaction mines later, the
    ///   drop is exactly the kind of surprise reconciliation must alarm.
    /// - A `Finalized` row whose request side still has not completed
    ///   `UNRESOLVED_BROADCAST_INCIDENT_SECS` after `finalized_at`: that
    ///   is an incident (the operation completed on chain and the ledger
    ///   never recorded it), and an incident must not keep explaining a
    ///   drop of its size indefinitely. The term is bounded by real,
    ///   recorded rows and by time; it never manufactures headroom, and
    ///   `reconcile` caps it at the observed drop and checks the hard
    ///   invariant independently of it.
    fn robinhood_outbound_in_flight_atomic(&self, now: i64) -> Result<u64, LedgerError> {
        // The same boundary as `submitter::broadcast_is_stale`: an
        // operation is an incident once the threshold has ELAPSED.
        let finalized_after = now - crate::robinhood::submitter::UNRESOLVED_BROADCAST_INCIDENT_SECS;
        // `Payout`: the amount the book will be debited by is the request's
        // own `net_destination_atomic` (canonical 8 dp) — the same column
        // `mark_robinhood_payout_settled` debits, so the explained figure
        // and the eventual debit are one number.
        let payout_value: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(r.net_destination_atomic), 0)
               FROM robinhood_transactions t
               JOIN bridge_requests r ON r.id = t.request_id
              WHERE t.kind = 'Payout'
                AND (t.state IN ('Broadcast', 'Included')
                     OR (t.state = 'Finalized' AND t.finalized_at > ?1))
                AND r.state NOT IN ('DestinationConfirmed', 'Settled')",
            [finalized_after],
            |r| r.get(0),
        )?;
        // `TreasuryWithdraw`: `confirm_rebalance` debits the approved
        // `amount_atomic` and moves the rebalance request to `Confirmed`
        // in one transaction.
        let withdraw_value: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(b.amount_atomic), 0)
               FROM robinhood_transactions t
               JOIN rebalance_requests b ON b.id = t.rebalance_request_id
              WHERE t.kind = 'TreasuryWithdraw'
                AND (t.state IN ('Broadcast', 'Included')
                     OR (t.state = 'Finalized' AND t.finalized_at > ?1))
                AND b.state <> 'Confirmed'",
            [finalized_after],
            |r| r.get(0),
        )?;
        Ok((payout_value as u64).saturating_add(withdraw_value as u64))
    }

    /// `(total_reserve_balance, protected_minimum, target_reserve,
    /// warning_reserve, critical_reserve)` — the full threshold-band
    /// configuration for `direction` (docs/09-runbook.md "Threshold bands
    /// and responses"), used by [`crate::rebalance::assess`] to classify
    /// imbalance severity and suggest a rebalance amount from values the
    /// operator already configured, never an invented one.
    pub fn reserve_thresholds(
        &self,
        direction: ReserveDirection,
    ) -> Result<(u64, u64, u64, u64, u64), LedgerError> {
        self.conn
            .query_row(
                "SELECT total_reserve_balance, protected_minimum, target_reserve, \
                 warning_reserve, critical_reserve FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)? as u64,
                        r.get::<_, i64>(1)? as u64,
                        r.get::<_, i64>(2)? as u64,
                        r.get::<_, i64>(3)? as u64,
                        r.get::<_, i64>(4)? as u64,
                    ))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })
    }

    /// Cumulative fee revenue recognized (at settlement, not reservation)
    /// on `direction`'s row — ALWAYS canonical units (`amount_conversion::
    /// CanonicalAtomic`) regardless of which reserve the row belongs to, a
    /// deliberate exception from that row's other native-unit columns
    /// (docs/20-bridge-fee.md). Reporting/audit only: never subtracted
    /// from [`Ledger::available_capacity`] — see that function's doc
    /// comment for why no such subtraction is needed.
    pub fn accrued_fees(&self, direction: ReserveDirection) -> Result<u64, LedgerError> {
        self.conn
            .query_row(
                "SELECT accrued_fees_atomic FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v as u64)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_reconciliation_finding(
        &mut self,
        direction: ReserveDirection,
        expected: i64,
        observed: i64,
        delta: i64,
        classification: &str,
        auto_paused: bool,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO reconciliation_findings
                (detected_at, direction, expected, observed, delta, classification, auto_paused)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                now,
                direction.as_str(),
                expected,
                observed,
                delta,
                classification,
                auto_paused as i64
            ],
        )?;
        Ok(())
    }

    /// One page of `reconciliation_findings`, newest first — the real,
    /// already-persisted per-tick balance history behind the public
    /// `/reserves/history` read-projection (`api::BridgeApi::
    /// reserves_history`). Never fabricates a data point: every row here
    /// is a reconciliation tick that actually ran (including `SKIPPED`
    /// ones, whose `classification` says so explicitly rather than the
    /// gap being silently absent). `id` is the append-only, strictly
    /// monotonic pagination cursor — safe even when two rows share the
    /// same `detected_at` second.
    pub fn reconciliation_findings_page(
        &self,
        direction: Option<ReserveDirection>,
        before_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<ReconciliationFindingRow>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, direction, detected_at, expected, observed, delta, classification, \
             auto_paused FROM reconciliation_findings \
             WHERE (?1 IS NULL OR direction = ?1) AND (?2 IS NULL OR id < ?2) \
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![direction, before_id, limit as i64], |r| {
                Ok(ReconciliationFindingRow {
                    id: r.get(0)?,
                    direction: r.get(1)?,
                    detected_at: r.get(2)?,
                    expected: r.get(3)?,
                    observed: r.get(4)?,
                    delta: r.get(5)?,
                    classification: r.get(6)?,
                    auto_paused: r.get::<_, i64>(7)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One page of `bridge_request_state_log`, newest first, joined to
    /// each row's own `direction` — the real, already-persisted
    /// user-facing settlement lifecycle behind the public
    /// `/explorer/events` read-projection (`api::BridgeApi::
    /// explorer_events`). Deliberately scoped to `bridge_request_state_log`
    /// only: `rebalance_state_log`/`custody_transition_state_log` carry
    /// real operator identities (an approver's name) and internal
    /// tx_reference values, which have no place on a public feed — that
    /// audit trail stays operator-only via `glc-admin rebalance-list`/
    /// `custody-list`. `id` is the strictly monotonic pagination cursor.
    pub fn explorer_events_page(
        &self,
        direction: Option<Direction>,
        to_state: Option<RequestState>,
        before_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<ExplorerEventRow>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT srl.id, srl.request_id, br.direction, srl.from_state, srl.to_state, \
             srl.at, srl.reason \
             FROM bridge_request_state_log srl \
             JOIN bridge_requests br ON br.id = srl.request_id \
             WHERE (?1 IS NULL OR br.direction = ?1) \
               AND (?2 IS NULL OR srl.to_state = ?2) \
               AND (?3 IS NULL OR srl.id < ?3) \
             ORDER BY srl.id DESC LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![direction, to_state, before_id, limit as i64],
                |r| {
                    let from_state: Option<String> = r.get(3)?;
                    let to_state: String = r.get(4)?;
                    Ok(ExplorerEventRow {
                        id: r.get(0)?,
                        request_id: r.get(1)?,
                        direction: r.get(2)?,
                        from_state: from_state.map(|s| s.parse().unwrap()),
                        to_state: to_state.parse().unwrap(),
                        at: r.get(5)?,
                        reason: r.get(6)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---------------------------------------------------- Goldcoin chain tracking --

    /// The locally indexed tip, if any.
    pub fn goldcoin_chain_tip(&self) -> Result<Option<(i64, [u8; 32])>, LedgerError> {
        self.conn
            .query_row(
                "SELECT height, hash FROM goldcoin_indexed_blocks ORDER BY height DESC LIMIT 1",
                [],
                |r| {
                    let h: Vec<u8> = r.get(1)?;
                    Ok((r.get::<_, i64>(0)?, to_array32(&h)))
                },
            )
            .optional()
            .map_err(LedgerError::from)
    }

    pub fn goldcoin_block_hash_at(&self, height: i64) -> Result<Option<[u8; 32]>, LedgerError> {
        self.conn
            .query_row(
                "SELECT hash FROM goldcoin_indexed_blocks WHERE height = ?1",
                [height],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map(|o| o.map(|v| to_array32(&v)))
            .map_err(LedgerError::from)
    }

    pub fn goldcoin_ingest_block(
        &mut self,
        height: i64,
        hash: [u8; 32],
        prev_hash: [u8; 32],
        block_time: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO goldcoin_indexed_blocks (height, hash, prev_hash, block_time, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(height) DO UPDATE SET hash = excluded.hash, prev_hash = excluded.prev_hash,
                block_time = excluded.block_time, indexed_at = excluded.indexed_at",
            rusqlite::params![
                height,
                hash.as_slice(),
                prev_hash.as_slice(),
                block_time,
                now
            ],
        )?;
        Ok(())
    }

    /// Rolls back locally indexed blocks above `fork_height`, records a
    /// reorg event, and reorgs (via [`Ledger::mark_glc_reorged`]) every
    /// active Goldcoin-sourced request whose source block was orphaned.
    /// `SourceFinalized`-or-later requests are never touched here — a
    /// post-finality reorg is a distinct, non-automatic incident (see
    /// `mark_glc_reorged`'s panic guard and docs/10-threat-model.md).
    pub fn goldcoin_rollback_reorg(
        &mut self,
        fork_height: i64,
        fork_hash: [u8; 32],
        old_tip_height: i64,
        old_tip_hash: [u8; 32],
        now: i64,
    ) -> Result<i64, LedgerError> {
        let tx = write_tx(&mut self.conn)?;

        let affected: Vec<i64> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT id FROM bridge_requests
                 WHERE direction IN {sources} AND state IN ('DepositObserved','Confirming')
                   AND source_block_height > ?1",
                sources = Direction::SOURCE_IS_GOLDCOIN_SQL_IN
            ))?;
            let rows: Result<Vec<i64>, _> = stmt.query_map([fork_height], |r| r.get(0))?.collect();
            rows?
        };
        for id in &affected {
            let state: RequestState = tx.query_row(
                "SELECT state FROM bridge_requests WHERE id = ?1",
                [id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO bridge_request_state_log (request_id, from_state, to_state, at, reason, actor)
                 VALUES (?1, ?2, 'Reorged', ?3, 'block_orphaned', 'system')",
                rusqlite::params![id, state.as_str(), now],
            )?;
            tx.execute(
                "UPDATE bridge_requests SET state = 'AwaitingDeposit', source_txid = NULL,
                    source_vout = NULL, source_block_height = NULL, source_block_hash = NULL,
                    source_confirmations = 0 WHERE id = ?1",
                rusqlite::params![id],
            )?;
            tx.execute(
                "INSERT INTO bridge_request_state_log (request_id, from_state, to_state, at, reason, actor)
                 VALUES (?1, 'Reorged', 'AwaitingDeposit', ?2, NULL, 'system')",
                rusqlite::params![id, now],
            )?;
        }

        tx.execute(
            "DELETE FROM goldcoin_indexed_blocks WHERE height > ?1",
            [fork_height],
        )?;
        tx.execute(
            "INSERT INTO goldcoin_reorg_events
                (detected_at, fork_height, old_tip_height, old_tip_hash, new_tip_height, new_tip_hash, orphaned_count)
             VALUES (?1, ?2, ?3, ?4, ?2, ?5, ?6)",
            rusqlite::params![now, fork_height, old_tip_height, old_tip_hash.as_slice(), fork_hash.as_slice(), affected.len() as i64],
        )?;
        tx.commit()?;
        Ok(affected.len() as i64)
    }

    /// Read-only check: which Goldcoin-sourced requests, already told
    /// their deposit was final (`source_finalized_at IS NOT NULL`), had their
    /// source block above `fork_height` — i.e. would be orphaned by
    /// rolling back to `fork_height` (docs/22-production-readiness-
    /// review.md P1 "dedicated post-finality reorg protection",
    /// docs/10-threat-model.md). Callers (`goldcoin::indexer::Indexer::
    /// tick`) run this BEFORE [`Ledger::goldcoin_rollback_reorg`], which
    /// deliberately only ever touches pre-finality requests — a non-empty
    /// result here means the reorg about to be rolled back is not routine
    /// and must be handled via [`Ledger::record_post_finality_reorg`]
    /// instead of (not in addition to) the normal rollback path.
    pub fn detect_post_finality_reorg(&self, fork_height: i64) -> Result<Vec<i64>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id FROM bridge_requests
             WHERE direction IN {sources} AND source_finalized_at IS NOT NULL
               AND source_block_height > ?1",
            sources = Direction::SOURCE_IS_GOLDCOIN_SQL_IN
        ))?;
        let rows: Result<Vec<i64>, _> = stmt.query_map([fork_height], |r| r.get(0))?.collect();
        Ok(rows?)
    }

    /// Records the dedicated post-finality-reorg audit event and, per
    /// docs/10-threat-model.md ("should be treated as an automatic
    /// global-pause trigger... never classified as WITHIN_TOLERANCE"),
    /// pauses BOTH reserve directions — not just the Goldcoin one — since
    /// a previously-final Goldcoin observation turning out reversible
    /// undermines confidence in the ledger's Goldcoin-side bookkeeping
    /// that both bridge directions ultimately rely on. Like every other
    /// pause in this codebase, never auto-cleared; an operator must
    /// explicitly `set_paused(.., false, ..)` (`glc-admin unpause`/
    /// `onchain-unpause`) after investigating.
    pub fn record_post_finality_reorg(
        &mut self,
        fork_height: i64,
        old_tip_height: i64,
        affected_request_ids: &[i64],
        now: i64,
    ) -> Result<i64, LedgerError> {
        let ids_json =
            serde_json::to_string(affected_request_ids).expect("Vec<i64> always serializes");
        let tx = write_tx(&mut self.conn)?;
        tx.execute(
            "INSERT INTO post_finality_reorg_events
                (detected_at, fork_height, old_tip_height, affected_request_ids, auto_paused)
             VALUES (?1, ?2, ?3, ?4, 1)",
            rusqlite::params![now, fork_height, old_tip_height, ids_json],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;

        let reason = format!(
            "post-finality Goldcoin reorg detected: fork_height={fork_height} (old tip \
             {old_tip_height}), {} already-finalized request(s) affected — see \
             post_finality_reorg_events #{id}",
            affected_request_ids.len()
        );
        self.set_paused(ReserveDirection::GoldcoinReserve, true, Some(&reason))?;
        self.set_paused(ReserveDirection::SolanaReserve, true, Some(&reason))?;
        Ok(id)
    }

    /// Count of post-finality-reorg events ever recorded — surfaced by
    /// `ops::health`/`glc-admin status` so this is visible without an
    /// operator having to know to query the table directly.
    pub fn post_finality_reorg_event_count(&self) -> Result<i64, LedgerError> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM post_finality_reorg_events", [], |r| {
                r.get(0)
            })?)
    }

    /// Cumulative amount ever settled for a direction — an accounting
    /// counter, not part of the capacity formula (docs/05-reserve-
    /// accounting.md).
    pub fn settled_liquidity(&self, direction: ReserveDirection) -> Result<u64, LedgerError> {
        self.conn
            .query_row(
                "SELECT settled_liquidity_total FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v as u64)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })
    }

    pub fn goldcoin_reorg_event_count(&self) -> Result<i64, LedgerError> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM goldcoin_reorg_events", [], |r| {
                r.get(0)
            })?)
    }

    // ------------------------------------------------------- Solana release --

    /// `SourceFinalized -> DestinationSubmitted` for a request whose
    /// DESTINATION is the Solana reserve (`GlcToSol`, `RhnToSol`): the
    /// `release_from_reserve` transaction (carrying the threshold
    /// attestation proof) has been submitted to Solana, `signature` is its
    /// transaction signature. Unlike the Goldcoin payout path, there is no
    /// separate "Signed" step to persist — attestation collection happens
    /// in-memory, off the database, immediately before submission
    /// ([`crate::signing::attestation`]). Idempotent: a no-op once already
    /// `DestinationSubmitted` or later.
    pub fn record_release_submitted(
        &mut self,
        request_id: i64,
        signature: [u8; 64],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, state): (Direction, RequestState) = tx.query_row(
            "SELECT direction, state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert!(
            direction.destination_is_solana(),
            "record_release_submitted on a {} request, whose destination is not Solana",
            direction.as_str()
        );
        if state != RequestState::SourceFinalized {
            tx.rollback()?;
            return Ok(());
        }
        tx.execute(
            "UPDATE bridge_requests SET state = 'DestinationSubmitted', destination_txid = ?1 WHERE id = ?2",
            rusqlite::params![signature.as_slice(), request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::DestinationSubmitted,
            now,
            None,
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `DestinationSubmitted -> DestinationConfirmed` (and, for `GlcToSol`,
    /// straight on to `Settled`) for a request whose DESTINATION is the
    /// Solana reserve: the `release_from_reserve` transaction has
    /// confirmed on Solana at `finalized` commitment.
    ///
    /// For `GlcToSol` there is no further on-chain step after this
    /// confirms (the release instruction itself both creates the
    /// replay-guard `DepositClaim` and moves the funds), so
    /// `DestinationConfirmed` and `Settled` are reached together.
    ///
    /// For `RhnToSol` the release is likewise the moment value leaves the
    /// Solana reserve — so the reserve accounting below moves NOW, in
    /// both directions — but the request stops at `DestinationConfirmed`:
    /// the source obligation on the Robinhood custody contract is still
    /// `Pending`, and closing it (`executeSettlement`, which destroys the
    /// depositor's refund path) is the LAST step and happens only from
    /// this state, exactly as `RhnToGlc` settles only after its Goldcoin
    /// payout confirmed. See `Ledger::mark_robinhood_settlement_confirmed`.
    ///
    /// Moves the amount out of `reserved_liquidity`/`pending_obligations`
    /// into `settled_liquidity_total` for the Solana reserve and
    /// decrements its cached balance (docs/05-reserve-accounting.md);
    /// accrues the fee on the SOURCE reserve. Idempotent: a no-op if
    /// already at or past the state it produces.
    pub fn mark_release_confirmed(&mut self, request_id: i64, now: i64) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, state, amount, fee): (Direction, RequestState, i64, i64) = tx.query_row(
            "SELECT direction, state, net_destination_atomic, fee_amount_atomic FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        assert!(
            direction.destination_is_solana(),
            "mark_release_confirmed on a {} request, whose destination is not Solana",
            direction.as_str()
        );
        if state == RequestState::Settled
            || (state == RequestState::DestinationConfirmed && !direction.settles_on_release())
        {
            tx.rollback()?;
            return Ok(());
        }
        assert_eq!(
            state,
            RequestState::DestinationSubmitted,
            "mark_release_confirmed on unexpected bridge_request state"
        );
        tx.execute(
            "UPDATE bridge_requests SET state = 'DestinationConfirmed' WHERE id = ?1",
            [request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::DestinationConfirmed,
            now,
            None,
            "system",
        )?;
        if direction.settles_on_release() {
            tx.execute(
                "UPDATE bridge_requests SET state = 'Settled', settled_at = ?1 WHERE id = ?2",
                rusqlite::params![now, request_id],
            )?;
            log_transition(
                &tx,
                request_id,
                Some(RequestState::DestinationConfirmed),
                RequestState::Settled,
                now,
                None,
                "system",
            )?;
        }
        // `total_reserve_balance` is decremented here, not left for the next
        // reconciliation pass to discover: reconciliation flags any drop
        // between its cached balance and a fresh on-chain read as an
        // unexplained breach (and pauses the reserve, one-way). A confirmed
        // release is an *explained* drop this service itself caused, so the
        // cache must reflect it immediately — otherwise the very next
        // reconcile sees the real chain balance already down by `amount`
        // while its own cache is still stale, and misclassifies a routine
        // settlement as a breach.
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity - ?1, pending_obligations = pending_obligations - ?1,
                settled_liquidity_total = settled_liquidity_total + ?1, total_reserve_balance = total_reserve_balance - ?1
                WHERE direction = 'SolanaReserve'",
            [amount],
        )?;
        // The fee for a release is collected on the SOURCE side (Goldcoin
        // for `GlcToSol`, Robinhood for `RhnToSol` — docs/20-bridge-fee.md:
        // "the fee remains on the source side where it was collected"),
        // in canonical units — a separate row from the SolanaReserve
        // update above, and deliberately never netted against it.
        tx.execute(
            "UPDATE reserve_ledger SET accrued_fees_atomic = accrued_fees_atomic + ?1
                WHERE direction = ?2",
            rusqlite::params![fee, direction.source_reserve()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ----------------------------------------------------------------- queries --

    /// Request ids with a `goldcoin_payouts` row currently in `state`
    /// (`'Built'|'Signed'|'Broadcast'|'Confirmed'|'Completed'`), oldest
    /// first — what [`crate::orchestrator`] polls to drive the Goldcoin
    /// payout lifecycle forward.
    pub fn goldcoin_payouts_in_state(&self, state: &str) -> Result<Vec<i64>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT request_id FROM goldcoin_payouts WHERE state = ?1 ORDER BY request_id",
        )?;
        let rows = stmt
            .query_map([state], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Request ids with a `goldcoin_refunds` row currently in `state`
    /// (`'Built'|'Signed'|'Broadcast'|'Refunded'`), oldest first — the
    /// GlcToSol-refund twin of [`Self::goldcoin_payouts_in_state`], and
    /// what [`crate::orchestrator`]'s refund-confirmation reconciliation
    /// pass polls to find broadcasts whose depth still needs checking.
    /// Backed by the existing `ix_goldcoin_refunds_state` index.
    pub fn goldcoin_refunds_in_state(&self, state: &str) -> Result<Vec<i64>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT request_id FROM goldcoin_refunds WHERE state = ?1 ORDER BY request_id",
        )?;
        let rows = stmt
            .query_map([state], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The request's `destination_confirmations` — the operator-facing
    /// mirror of the destination leg's observed confirmation depth (kept
    /// fresh by [`Ledger::update_goldcoin_payout_confirmations`] for as
    /// long as the leg is live, including after `DestinationConfirmed`).
    pub fn destination_confirmations(&self, request_id: i64) -> Result<i64, LedgerError> {
        self.conn
            .query_row(
                "SELECT destination_confirmations FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(LedgerError::RequestNotFound(request_id))
    }

    pub fn requests_by_state(
        &self,
        direction: Direction,
        state: RequestState,
    ) -> Result<Vec<BridgeRequest>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_REQUEST_PREFIX} WHERE direction = ?1 AND state = ?2 ORDER BY id"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![direction, state], row_to_request)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Per-`RequestState` counts for `direction` — the count-only
    /// equivalent of [`Ledger::requests_by_state`], used by the public
    /// `/stats` read-projection (`api::BridgeApi::stats`) so a large
    /// `bridge_requests` table is aggregated in SQL rather than fetched
    /// row-by-row into memory just to be counted.
    pub fn request_state_counts(
        &self,
        direction: Direction,
    ) -> Result<Vec<(RequestState, i64)>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT state, COUNT(*) FROM bridge_requests WHERE direction = ?1 GROUP BY state",
        )?;
        let rows = stmt
            .query_map([direction], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One page of `bridge_requests`, newest first — the real,
    /// already-persisted request list behind the public `GET /transfers`
    /// read-projection (`api::BridgeApi::list_transfers`), i.e. a
    /// wallet-scoped "my activity" view.
    ///
    /// `address` matches the caller's OWN address against whichever
    /// column actually carries it for each direction. There is no common
    /// column and no common width, so the filter is chain-tagged
    /// ([`TransferAddressFilter`]) and each variant is restricted to the
    /// directions where that chain's addresses live:
    ///
    /// | Filter | Direction | Column |
    /// |---|---|---|
    /// | [`TransferAddressFilter::Solana`] | `GlcToSol`, `RhnToSol` | `recipient` — the destination the depositor chose |
    /// | [`TransferAddressFilter::Solana`] | `SolToGlc`, `SolToRhn` | `requester` — the depositor the Solana indexer observed |
    /// | [`TransferAddressFilter::Evm`] | `GlcToRhn`, `SolToRhn` | `recipient` — the payout destination the depositor chose |
    /// | [`TransferAddressFilter::Evm`] | `RhnToGlc`, `RhnToSol` | `robinhood_deposit_observations.depositor` — the wallet the custody contract recorded |
    ///
    /// Every direction of a variant is checked together, so a caller
    /// need not know which one applies to them; the OTHER chain's
    /// directions are never consulted, so a cross-chain false match is
    /// structurally impossible rather than merely improbable. (SQLite's
    /// blob comparison is length-sensitive, so a 20-byte value could not
    /// equal a 32-byte `recipient` even without the direction predicate —
    /// the predicate is there so the guarantee does not rest on that
    /// coincidence, and so a Goldcoin address's ASCII bytes in
    /// `SolToGlc.recipient` can never be reached by either variant.)
    ///
    /// The `RhnToGlc` leg reads the depositor back through the
    /// observation's `folded_request_id` link because a Robinhood fold
    /// leaves `bridge_requests.requester` NULL — that column is a fixed
    /// `[u8; 32]` Solana pubkey and a 20-byte EVM address is not one.
    /// Reorged sightings are excluded: an orphaned observation is not
    /// evidence that this depositor funded this request.
    pub fn transfers_page(
        &self,
        address: Option<TransferAddressFilter>,
        state: Option<RequestState>,
        before_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<BridgeRequest>, LedgerError> {
        // Two independently-bound parameters rather than one: exactly one
        // is ever non-NULL, and the other's whole clause collapses to
        // false. A single shared parameter would let one chain's bytes be
        // offered to the other chain's columns.
        let (solana, evm): (Option<Vec<u8>>, Option<Vec<u8>>) = match address {
            None => (None, None),
            Some(TransferAddressFilter::Solana(a)) => (Some(a.to_vec()), None),
            Some(TransferAddressFilter::Evm(a)) => (None, Some(a.to_vec())),
        };
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_REQUEST_PREFIX} WHERE \
             ((?1 IS NULL AND ?2 IS NULL) \
              OR (direction IN ('GlcToSol','RhnToSol') AND recipient = ?1) \
              OR (direction IN ('SolToGlc','SolToRhn') AND requester = ?1) \
              OR (direction IN ('GlcToRhn','SolToRhn') AND recipient = ?2) \
              OR (direction IN ('RhnToGlc','RhnToSol') AND EXISTS ( \
                    SELECT 1 FROM robinhood_deposit_observations o \
                    WHERE o.folded_request_id = bridge_requests.id \
                      AND o.finality <> 'Reorged' \
                      AND o.depositor = ?2))) \
             AND (?3 IS NULL OR state = ?3) \
             AND (?4 IS NULL OR id < ?4) \
             ORDER BY id DESC LIMIT ?5"
        ))?;
        let rows = stmt
            .query_map(
                rusqlite::params![solana, evm, state, before_id, limit as i64],
                row_to_request,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn state_log(&self, request_id: i64) -> Result<Vec<StateLogEntry>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT from_state, to_state, at, reason FROM bridge_request_state_log
             WHERE request_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map([request_id], |r| {
                let from: Option<String> = r.get(0)?;
                let to: String = r.get(1)?;
                Ok((
                    from.map(|s| s.parse().unwrap()),
                    to.parse().unwrap(),
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // --------------------------------------------------- admin audit log --

    /// Opens the outer transaction an admin mutation and its audit row
    /// share, so the two are ATOMIC: either the mutation commits together
    /// with its audit row, or neither persists. While this scope is open,
    /// every Ledger mutation's own [`WriteTx`] becomes a savepoint nested
    /// inside it — a validated refusal rolls back only the mutation's
    /// writes, and the scope then commits just the failure audit row.
    /// `BEGIN IMMEDIATE`, same write-lock posture as every standalone
    /// mutation. If the caller drops the `Ledger` without committing
    /// (error path, panic), SQLite rolls the whole scope back with the
    /// connection.
    ///
    /// `pub(crate)` on purpose: the only caller is
    /// `admin_api::audited_mutation`, which commits or rolls back on
    /// every path. A leaked open scope would silently downgrade every
    /// later mutation on the connection to a savepoint whose "commit" is
    /// only a RELEASE — never expose this trio for ad-hoc use.
    pub(crate) fn begin_admin_action(&mut self) -> Result<(), LedgerError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    pub(crate) fn commit_admin_action(&mut self) -> Result<(), LedgerError> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub(crate) fn rollback_admin_action(&mut self) -> Result<(), LedgerError> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    /// Appends one admin mutation attempt to the append-only
    /// `admin_audit_log` (schema v15) and returns the new row id. Called
    /// for every privileged mutation ATTEMPT — successes and refusals
    /// alike (`AdminAuditOutcome::Error` carries the operator-visible
    /// failure message). The schema `CHECK`s `actor`/`action`/`note`
    /// non-empty, so a caller that forgets to enforce the mandatory note
    /// fails closed here rather than writing a noteless row. Rows are
    /// never updated or deleted by anything in this crate.
    pub fn append_admin_audit(&mut self, entry: &AdminAuditEntry) -> Result<i64, LedgerError> {
        let (outcome, error) = match &entry.outcome {
            AdminAuditOutcome::Success => ("success", None),
            AdminAuditOutcome::Error(message) => ("error", Some(message.as_str())),
        };
        self.conn.execute(
            "INSERT INTO admin_audit_log
                 (at, actor, action, target, old_value, new_value, note, outcome, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                entry.at,
                entry.actor,
                entry.action,
                entry.target,
                entry.old_value,
                entry.new_value,
                entry.note,
                outcome,
                error,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Reads `admin_audit_log` rows newest-first with keyset pagination
    /// (`filter.before_id` = "rows older than this id"), optionally
    /// restricted to one `action` slug and/or one `actor`. `limit` is
    /// clamped into `1..=200` (the public API's `MAX_PAGE_LIMIT`
    /// discipline; a zero limit would be a permanently empty page that
    /// reads as "no audit rows") and defaults to 50.
    pub fn list_admin_audit(
        &self,
        filter: &AdminAuditFilter,
    ) -> Result<Vec<AdminAuditRow>, LedgerError> {
        let limit = i64::from(filter.limit.unwrap_or(50).clamp(1, 200));
        let mut stmt = self.conn.prepare(
            "SELECT id, at, actor, action, target, old_value, new_value, note, outcome, error
             FROM admin_audit_log
             WHERE (?1 IS NULL OR id < ?1)
               AND (?2 IS NULL OR action = ?2)
               AND (?3 IS NULL OR actor = ?3)
             ORDER BY id DESC
             LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![filter.before_id, filter.action, filter.actor, limit],
                |r| {
                    let outcome: String = r.get(8)?;
                    let error: Option<String> = r.get(9)?;
                    Ok(AdminAuditRow {
                        id: r.get(0)?,
                        at: r.get(1)?,
                        actor: r.get(2)?,
                        action: r.get(3)?,
                        target: r.get(4)?,
                        old_value: r.get(5)?,
                        new_value: r.get(6)?,
                        note: r.get(7)?,
                        outcome: if outcome == "success" {
                            AdminAuditOutcome::Success
                        } else {
                            AdminAuditOutcome::Error(error.unwrap_or_default())
                        },
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Direct SQL access for tests that need queries not otherwise exposed.
    /// Kept `pub(crate)` and test-only — production code (including
    /// `reconciliation`) should add a typed method above instead of
    /// reaching for raw SQL.
    #[cfg(test)]
    pub(crate) fn raw(&self) -> &Connection {
        &self.conn
    }

    // ------------------------------------------------------- Goldcoin vault --

    /// Reconciles observed vault UTXOs (from a live `listunspent` read)
    /// against `vault_utxos`: promotes `Unconfirmed -> Available` once
    /// `confirmations >= min_confirmations`, inserts newly-seen outputs,
    /// and marks any previously `Available`/`Unconfirmed` outpoint that no
    /// longer appears as `Spent` (something external moved it — e.g. a
    /// rebalance, or, if it happens unexpectedly, an anomaly reconciliation
    /// should catch separately). Never disturbs a `Reserved` or `Spent`
    /// row — same discipline the old bridge's `sync_vault_utxos` used
    /// (docs/01-reuse-inventory.md).
    pub fn sync_vault_utxos(
        &mut self,
        observed: &[(crate::goldcoin::coin::VaultUtxo, i64, String)], // (utxo, confirmations, script_pubkey_hex)
        min_confirmations: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let mut still_present = std::collections::HashSet::new();
        for (utxo, confirmations, script_pubkey_hex) in observed {
            still_present.insert((utxo.txid.to_vec(), utxo.vout));
            let state = if *confirmations >= min_confirmations {
                "Available"
            } else {
                "Unconfirmed"
            };
            // Resurrection rule (2026-08-30 review, blocker: one missed
            // snapshot must never permanently destroy accounting state):
            // a row this service itself marked spent (`spent_by_txid` set
            // by a broadcast payout/split — a transaction WE signed) is
            // sticky forever, since offering it to selection again would
            // double-spend our own in-flight transaction; so is any row
            // that was ever payout-reserved (`reserved_by` set — belt for
            // rows settled before `spent_by_txid` was recorded on this
            // path, which persist in real ledgers). A row the ABSENCE
            // branch below inferred spent (both markers NULL) is a chain
            // observation, and a fresh `listunspent` snapshot reporting
            // the outpoint unspent again (parent re-broadcast after
            // eviction, reorg restored it) is the same class of
            // observation — chain truth wins in both directions.
            tx.execute(
                "INSERT INTO vault_utxos (txid, vout, amount_atomic, script_pubkey_hex, confirmations, first_seen_at, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(txid, vout) DO UPDATE SET
                    confirmations = excluded.confirmations,
                    state = CASE
                        WHEN vault_utxos.state = 'Reserved' THEN 'Reserved'
                        WHEN vault_utxos.state = 'Spent'
                             AND (vault_utxos.spent_by_txid IS NOT NULL
                                  OR vault_utxos.reserved_by IS NOT NULL) THEN 'Spent'
                        ELSE excluded.state END",
                rusqlite::params![utxo.txid.as_slice(), utxo.vout, utxo.amount_atomic as i64, script_pubkey_hex, confirmations, now, state],
            )?;
        }

        let mut stmt = tx.prepare(
            "SELECT txid, vout FROM vault_utxos WHERE state IN ('Available','Unconfirmed')",
        )?;
        let tracked: Vec<(Vec<u8>, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        for (txid, vout) in tracked {
            if !still_present.contains(&(txid.clone(), vout as u32)) {
                // Chunk outputs of a split still in `Broadcast` are
                // exempt from the absence flip: their transaction lives
                // or dies with its mempool acceptance, and the shaping
                // lifecycle owns that fate explicitly (first confirmation
                // -> `Confirmed`; evicted -> re-broadcast the exact
                // stored bytes; inputs gone -> `Abandoned`, which marks
                // these rows `Spent` itself). Flipping them here on one
                // missed snapshot erased the `own_unconfirmed_change`
                // term mid-eviction and auto-paused a healthy reserve
                // (2026-08-30 review). Deliberately NOT extended to
                // 0-conf payout change: its disappearance must keep
                // removing it from the selectable pools immediately, as
                // the zero-conf policy's own tests pin — those rows take
                // the ordinary flip below and rely on the resurrection
                // rule above once the parent is restored.
                tx.execute(
                    "UPDATE vault_utxos SET state = 'Spent'
                     WHERE txid = ?1 AND vout = ?2
                       AND NOT EXISTS (
                         SELECT 1 FROM vault_utxo_splits s
                          WHERE s.txid = vault_utxos.txid AND s.state = 'Broadcast'
                       )",
                    rusqlite::params![txid, vout],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// UTXOs available for coin selection, sorted `(amount DESC, txid ASC,
    /// vout ASC)` — [`crate::goldcoin::coin::select`] requires this exact
    /// order for its selection to be deterministic.
    /// Excludes any UTXO still backing an unfinalized Goldcoin-sourced
    /// deposit — see [`unfinalized_goldcoin_deposit_exclusion`], which is
    /// the fragment used here and by every other query that draws on the
    /// same pool.
    pub fn available_vault_utxos(
        &self,
    ) -> Result<Vec<crate::goldcoin::coin::VaultUtxo>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT v.txid, v.vout, v.amount_atomic, v.script_pubkey_hex FROM vault_utxos v
             WHERE v.state = 'Available'
               AND {deposit_excl}
               AND {claim_excl}
             ORDER BY v.amount_atomic DESC, v.txid ASC, v.vout ASC",
            claim_excl = live_split_claim_exclusion("v"),
            deposit_excl = unfinalized_goldcoin_deposit_exclusion("v")
        ))?;
        let rows = stmt
            .query_map([], |r| {
                let txid: Vec<u8> = r.get(0)?;
                Ok(crate::goldcoin::coin::VaultUtxo {
                    txid: to_array32(&txid),
                    vout: r.get(1)?,
                    amount_atomic: r.get::<_, i64>(2)? as u64,
                    script_pubkey_hex: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The 0-conf-spendability candidate pool (docs/09-runbook.md
    /// "Zero-conf payout change"): vault UTXOs that are (a) still short of
    /// `vault_min_confirmations` (`state = 'Unconfirmed'`), (b)
    /// AUTHORITATIVELY this service's own payout change — an exact
    /// `(txid, vout)` join against `goldcoin_payout_change_outpoints`,
    /// never a script/address heuristic (external deposits, vault-split
    /// outputs, and anything else without a provenance row NEVER
    /// qualify, at any confirmation count below the threshold), (c) not
    /// on a parent-validation hold (`zero_conf_hold_reason IS NULL` —
    /// see `Orchestrator::tick_validate_zero_conf_parents`), and (d)
    /// within the unconfirmed-ancestry cap: at 0 confirmations the
    /// recorded `unconfirmed_ancestor_depth` must be <= `max_depth`;
    /// from 1 confirmation on, every own-chain ancestor is buried under
    /// output's own confirmation, so the depth cap no longer applies
    /// (the row is still below the external threshold, which is exactly
    /// what this policy makes spendable). `max_depth = 0` disables the
    /// policy outright (kill switch) — this returns an empty pool.
    ///
    /// Same deterministic ordering and deposit-backing exclusion as
    /// [`Ledger::available_vault_utxos`]; callers treat this as
    /// ADDITIONAL liquidity, only after confirmed UTXOs alone were
    /// insufficient (`signing::goldcoin_vault`'s two-phase selection).
    pub fn zero_conf_change_vault_utxos(
        &self,
        max_depth: u32,
    ) -> Result<Vec<crate::goldcoin::coin::VaultUtxo>, LedgerError> {
        Ok(self
            .zero_conf_change_vault_utxos_with_depth(max_depth)?
            .into_iter()
            .map(|c| c.utxo)
            .collect())
    }

    /// [`Ledger::zero_conf_change_vault_utxos`] plus, per candidate, the
    /// figures the recursive mode's per-transaction chain budget needs
    /// (`signing::goldcoin_vault`'s selection): the recorded
    /// `unconfirmed_ancestor_depth`, and the row's live confirmation
    /// count. A candidate with `confirmations >= 1` contributes ZERO to a
    /// new transaction's in-mempool ancestor count (its whole own-chain
    /// ancestry is buried under its confirmation), which is exactly how
    /// the budget accounts for it; recorded depths are upper bounds
    /// (summing them over-counts shared ancestors — conservative, never
    /// permissive).
    pub fn zero_conf_change_vault_utxos_with_depth(
        &self,
        max_depth: u32,
    ) -> Result<Vec<ZeroConfChangeCandidate>, LedgerError> {
        if max_depth == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT v.txid, v.vout, v.amount_atomic, v.script_pubkey_hex,
                    o.unconfirmed_ancestor_depth, v.confirmations
             FROM vault_utxos v
             JOIN goldcoin_payout_change_outpoints o ON o.txid = v.txid AND o.vout = v.vout
             WHERE v.state = 'Unconfirmed'
               AND v.zero_conf_hold_reason IS NULL
               AND (v.confirmations >= 1 OR o.unconfirmed_ancestor_depth <= ?1)
               AND {deposit_excl}
             ORDER BY v.amount_atomic DESC, v.txid ASC, v.vout ASC",
            deposit_excl = unfinalized_goldcoin_deposit_exclusion("v")
        ))?;
        let rows = stmt
            .query_map([max_depth], |r| {
                let txid: Vec<u8> = r.get(0)?;
                Ok(ZeroConfChangeCandidate {
                    utxo: crate::goldcoin::coin::VaultUtxo {
                        txid: to_array32(&txid),
                        vout: r.get(1)?,
                        amount_atomic: r.get::<_, i64>(2)? as u64,
                        script_pubkey_hex: r.get(3)?,
                    },
                    unconfirmed_ancestor_depth: r.get::<_, i64>(4)? as u32,
                    confirmations: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Distinct parent payout txids whose 0-conf change is currently a
    /// policy candidate (or held) — what
    /// `Orchestrator::tick_validate_zero_conf_parents` re-checks against
    /// the live Goldcoin node every tick before any selection may use the
    /// change. Only 0-confirmation rows: once the parent has >= 1
    /// confirmation the chain itself is the acceptance proof.
    pub fn zero_conf_parent_txids(&self) -> Result<Vec<[u8; 32]>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT v.txid FROM vault_utxos v
             JOIN goldcoin_payout_change_outpoints o ON o.txid = v.txid AND o.vout = v.vout
             WHERE v.state = 'Unconfirmed' AND v.confirmations = 0",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let txid: Vec<u8> = r.get(0)?;
                Ok(to_array32(&txid))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Places (reason) or clears (`None`) the parent-validation hold on
    /// every unconfirmed change output of parent payout `txid` — the
    /// reversible, persisted exclusion `zero_conf_change_vault_utxos`
    /// honors. A hold means "the configured node does not currently
    /// know/accept this parent transaction" (evicted, conflicted,
    /// replaced, or an RPC failure — all fail closed identically);
    /// clearing happens only after a fresh successful re-validation.
    pub fn set_zero_conf_hold(
        &mut self,
        txid: [u8; 32],
        reason: Option<&str>,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE vault_utxos SET zero_conf_hold_reason = ?1
              WHERE txid = ?2 AND state = 'Unconfirmed'",
            rusqlite::params![reason, txid.as_slice()],
        )?;
        Ok(())
    }

    /// Sum of `vault_utxos` rows still short of `vault_min_confirmations`
    /// (`state = 'Unconfirmed'`) — value the vault genuinely holds but that
    /// `total_reserve_balance`/reconciliation's `observed_balance`
    /// deliberately excludes until it matures (see `sync_vault_utxos` and
    /// `Orchestrator::tick_goldcoin_reconciliation`, both of which already
    /// filter by `vault_min_confirmations` before that figure is computed
    /// — this method changes nothing about that; it only reads the
    /// portion already being excluded, for display). Purely observational:
    /// never added to `total_reserve_balance`, never consulted by
    /// `reconcile`'s hard invariant or the auto-pause decision. Exists so
    /// an operator seeing a paused reserve can see whether recovery is
    /// already in flight (a large mature-soon change output) rather than
    /// requiring a genuinely new deposit.
    pub fn immature_vault_utxo_total(&self) -> Result<u64, LedgerError> {
        let total: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(amount_atomic), 0) FROM vault_utxos WHERE state = 'Unconfirmed'",
            [],
            |r| r.get(0),
        )?;
        Ok(total as u64)
    }

    /// Sum of `amount_atomic` for `Unconfirmed` `vault_utxos` rows whose
    /// txid matches a KNOWN `goldcoin_payouts` broadcast OR a known
    /// `vault_utxo_splits` broadcast — i.e. value that is temporarily
    /// invisible to a live chain scan for a reason this service itself
    /// already knows about (its own payout's change, or its own split's
    /// chunk outputs, not yet mature), as opposed to any other
    /// still-maturing deposit. A split's outputs carry exactly the same
    /// authoritative provenance as payout change: the txid was computed by
    /// this service from the bytes it itself broadcast
    /// (`record_vault_utxo_split_broadcast`), never trusted from the node.
    /// Since a real Goldcoin transaction's non-vault outputs (the external
    /// destination) never appear in `vault_utxos` at all (this service
    /// only watches its own vault/deposit addresses), a `vault_utxos` row
    /// matching a payout's txid is unambiguously that payout's OWN change
    /// — never its destination. Grounded entirely in live, currently
    /// observed state (never a payout-lifecycle proxy), so it can never
    /// double-count a change output that has already matured to
    /// `Available` (excluded by the `state = 'Unconfirmed'` filter) or
    /// miss one whose parent payout has moved past `Broadcast` while the
    /// physical output is still genuinely immature. See
    /// `Ledger::pending_destination_settlement_amount`'s use of this
    /// alongside (not instead of) its existing `Broadcast`-state term.
    /// `now` drives the missing-inputs grace window: chunks of a split
    /// whose exact bytes the node has refused for missing inputs for
    /// longer than [`SPLIT_MISSING_INPUTS_GRACE_SECS`] are no longer
    /// counted — those outputs are very likely never coming, and
    /// continuing to explain them would pad the solvency invariant over
    /// a genuine conflicting-spend loss (2026-08-31 review, B2).
    pub fn own_unconfirmed_change_atomic(&self, now: i64) -> Result<u64, LedgerError> {
        let total: i64 = self.conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(v.amount_atomic), 0) FROM vault_utxos v
                 WHERE v.state = 'Unconfirmed'
                   AND (EXISTS (SELECT 1 FROM goldcoin_payouts p WHERE p.txid = v.txid)
                     OR EXISTS (SELECT 1 FROM vault_utxo_splits s WHERE s.txid = v.txid AND {}))",
                split_chunks_still_explainable("?1")
            ),
            [now],
            |r| r.get(0),
        )?;
        Ok(total as u64)
    }

    /// Count of still-immature (`Unconfirmed`) chunk outputs belonging to
    /// this service's own broadcast vault-UTXO splits — the guard
    /// `goldcoin::liquidity::run_shaping_tick` uses to avoid stacking a
    /// second self-transaction while the previous one's liquidity is
    /// already en route to maturity. Split outputs only, deliberately NOT
    /// payout change: under continuous traffic there is nearly always
    /// some immature payout change, and gating shaping on that would
    /// starve it exactly when it is needed.
    /// Same missing-inputs grace semantics as
    /// [`Ledger::own_unconfirmed_change_atomic`]: chunks of a
    /// flagged-past-grace split stop gating new shaping — a dead split
    /// must not stall pool recovery on top of masking its loss.
    pub fn unconfirmed_split_chunk_count(&self, now: i64) -> Result<u32, LedgerError> {
        let n: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM vault_utxos v
                 WHERE v.state = 'Unconfirmed'
                   AND EXISTS (SELECT 1 FROM vault_utxo_splits s WHERE s.txid = v.txid AND {})",
                split_chunks_still_explainable("?1")
            ),
            [now],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// `mature_available_atomic`: sum of `available_vault_utxos()` — real,
    /// currently spendable reserve value, the same candidate pool coin
    /// selection draws from. `own_unconfirmed_change_atomic`: this
    /// service's own broadcast-but-immature payout change (see that
    /// method's docs) — known, not missing. `available_utxo_count`/
    /// `unconfirmed_change_utxo_count`: the same two categories, counted
    /// rather than summed — a leading indicator distinct from the value
    /// figures (see docs/09-runbook.md's "UTXO liquidity" section): the
    /// accounting can look healthy while the POOL itself is a single
    /// oversized UTXO or a handful of nearly-exhausted ones.
    pub fn utxo_pool_health(&self, now: i64) -> Result<UtxoPoolHealth, LedgerError> {
        let mature_available_atomic: i64 = self.conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(v.amount_atomic), 0) FROM vault_utxos v
             WHERE v.state = 'Available'
               AND {deposit_excl}
               AND {claim_excl}",
                claim_excl = live_split_claim_exclusion("v"),
                deposit_excl = unfinalized_goldcoin_deposit_exclusion("v")
            ),
            [],
            |r| r.get(0),
        )?;
        let available_utxo_count: i64 = Self::count_available_vault_utxos(&self.conn)?;
        let unconfirmed_change_utxo_count: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM vault_utxos v
                 WHERE v.state = 'Unconfirmed'
                   AND (EXISTS (SELECT 1 FROM goldcoin_payouts p WHERE p.txid = v.txid)
                     OR EXISTS (SELECT 1 FROM vault_utxo_splits s WHERE s.txid = v.txid AND {}))",
                split_chunks_still_explainable("?1")
            ),
            [now],
            |r| r.get(0),
        )?;
        // Zero-conf-policy visibility (docs/09-runbook.md "Zero-conf
        // payout change"): candidates = authoritative payout change still
        // below the confirmed threshold and not on a parent-validation
        // hold — depth-agnostic here (the depth cap is config, applied at
        // selection), so an operator sees the full policy pool alongside,
        // never mixed into, the confirmed figures above.
        let (zero_conf_candidate_atomic, zero_conf_candidate_count): (i64, i64) =
            self.conn.query_row(
                "SELECT COALESCE(SUM(v.amount_atomic), 0), COUNT(*) FROM vault_utxos v
                 JOIN goldcoin_payout_change_outpoints o ON o.txid = v.txid AND o.vout = v.vout
                 WHERE v.state = 'Unconfirmed' AND v.zero_conf_hold_reason IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
        let zero_conf_held_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM vault_utxos v
             JOIN goldcoin_payout_change_outpoints o ON o.txid = v.txid AND o.vout = v.vout
             WHERE v.state = 'Unconfirmed' AND v.zero_conf_hold_reason IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(UtxoPoolHealth {
            mature_available_atomic: mature_available_atomic as u64,
            own_unconfirmed_change_atomic: self.own_unconfirmed_change_atomic(now)?,
            available_utxo_count: available_utxo_count as u32,
            unconfirmed_change_utxo_count: unconfirmed_change_utxo_count as u32,
            zero_conf_change_candidate_atomic: zero_conf_candidate_atomic as u64,
            zero_conf_change_candidate_count: zero_conf_candidate_count as u32,
            zero_conf_change_held_count: zero_conf_held_count as u32,
        })
    }

    /// Atomically reserves `selected` for `request_id`. The guarded
    /// conditional `UPDATE` is the actual concurrency control (SQLite's
    /// write-transaction lock, not an application-level mutex): if a
    /// concurrent reservation already claimed one of these outpoints,
    /// fewer rows match than expected and the whole reservation is rolled
    /// back and reported — never partially reserved.
    ///
    /// A row is reservable when it is `Available` (confirmed at
    /// `vault_min_confirmations`), OR when it satisfies the exact same
    /// 0-conf-payout-change eligibility predicate
    /// [`Ledger::zero_conf_change_vault_utxos`] selects by — re-checked
    /// HERE, inside the reservation's own write transaction, so a row
    /// whose eligibility lapsed between selection and reservation
    /// (parent hold placed, provenance absent) fails the reservation
    /// closed instead of being reserved on stale grounds. An
    /// `Unconfirmed` row without authoritative change provenance can
    /// never be reserved at any `zero_conf_max_depth`.
    /// The `manual_review_note` REASONS a Goldcoin-sourced request may
    /// carry and still be refundable on the Goldcoin side. Conservative on
    /// purpose: the amount mismatch this path was built for, plus the two
    /// wallet-window parks.
    ///
    /// The note's full text is `"<reason>: <detail>"` — only the reason
    /// prefix is ever read, and never the detail. In particular the
    /// `deposit_amount_mismatch` note embeds the observed amount as free
    /// text, and that number is NEVER parsed: the refund principal comes
    /// exclusively from the chain read cross-checked against
    /// `vault_utxos`. A note is an eligibility indicator, not evidence.
    pub const REFUNDABLE_GLC_MANUAL_REVIEW_REASONS: [&'static str; 3] = [
        "deposit_amount_mismatch",
        // A Goldcoin deposit parked at observation time because its
        // funding wallet, or the request's destination wallet, was still
        // inside its rolling 24-hour window (`ledger::wallet_window`).
        // The deposit is real and its principal sits at the request's
        // own deposit address; no Solana/Robinhood payout was started
        // (the park happened INSTEAD of `Confirming`), so returning it is
        // as safe as for an amount mismatch — and, since no resume path
        // exists for a Goldcoin-sourced park, a refund is its exit.
        Self::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT,
        Self::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT,
    ];

    /// The reason prefix of a `manual_review_note`, i.e. everything before
    /// the first `':'`. Split rather than parsed: no field of the detail
    /// is ever interpreted.
    fn manual_review_reason_prefix(note: &str) -> &str {
        note.split(':').next().unwrap_or("").trim()
    }

    /// Every DATABASE-side eligibility check for a Goldcoin refund, run
    /// read-only and reported per-condition. Never mutates.
    ///
    /// This is deliberately NOT the authority. It is the cheap half that
    /// can be answered from local state; `goldcoin::refund` additionally
    /// re-derives the deposit and the destination from Goldcoin RPC and
    /// independently proves no Solana release exists, and BOTH halves must
    /// pass before anything is built.
    pub fn glc_refund_db_checks(&self, request_id: i64) -> Result<GlcRefundDbChecks, LedgerError> {
        let mut c = GlcRefundDbChecks {
            request_found: false,
            direction: None,
            direction_is_goldcoin_sourced: false,
            direction_is_glc_to_sol: false,
            state_is_manual_review: false,
            reason_is_refundable: false,
            has_source_outpoint: false,
            no_goldcoin_payout: false,
            no_destination_txid: false,
            no_settlement_claim: false,
            no_robinhood_payout_started: false,
            robinhood_payout_evidence: Vec::new(),
            no_existing_refund: false,
            durable_observed_amount_atomic: None,
            stored_deposit_script_pubkey_hex: None,
            refusal: None,
        };
        let refuse = |c: &mut GlcRefundDbChecks, msg: String| {
            if c.refusal.is_none() {
                c.refusal = Some(msg);
            }
        };

        let row: Option<GlcRefundEligibilityRow> = self
            .conn
            .query_row(
                "SELECT direction, state, manual_review_note, source_txid, source_vout,
                        destination_txid, settlement_claim_hash
                 FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()?;

        let Some((direction, state, note, source_txid, source_vout, destination_txid, claim_hash)) =
            row
        else {
            refuse(&mut c, format!("bridge request {request_id} not found"));
            return Ok(c);
        };
        c.request_found = true;

        c.direction = Some(direction);

        // THE GATE. Only a Goldcoin-SOURCED request has a Goldcoin
        // principal sitting at a per-request deposit address to return.
        // A `SolToGlc`/`RhnToGlc` request's principal is on its own
        // source chain and is returned by that chain's own path.
        c.direction_is_goldcoin_sourced = direction.source_is_goldcoin();
        if !c.direction_is_goldcoin_sourced {
            refuse(
                &mut c,
                format!(
                    "request {request_id} is {direction:?}, whose source leg is not a Goldcoin \
                     deposit; this command returns the GOLDCOIN principal of a Goldcoin-sourced \
                     request only (a SolToGlc request is refunded with refund-manual-review)"
                ),
            );
        }

        // WHICH PROOF APPLIES. The two Goldcoin-sourced routes settle on
        // different chains and leave different traces, so each is proved
        // not-yet-settled by its own evidence and neither is relaxed to
        // accommodate the other:
        //
        // - `GlcToSol` -> the Solana proof: `destination_txid`,
        //   `settlement_claim_hash`, and — in `goldcoin::refund`, which
        //   is the authority — the on-chain `DepositClaim` PDA.
        // - `GlcToRhn` -> the Robinhood proof: no durable payout state in
        //   `robinhood_transactions`. A Robinhood payout writes none of
        //   the Solana columns, so reading their NULLs as an all-clear
        //   would be proving nothing at all.
        //
        // Both proofs are then evaluated for BOTH routes. That is not a
        // merge into a weaker generic condition — each route still stands
        // or falls on its own proof — it is each route additionally
        // requiring the other's evidence to be ABSENT. A Solana
        // settlement column set on a `GlcToRhn` row, or a Robinhood
        // payout row naming a `GlcToSol` request, is a contradiction, and
        // a refund is not the moment to discover one.
        c.direction_is_glc_to_sol = direction == Direction::GlcToSol;

        c.state_is_manual_review = state == RequestState::ManualReview;
        if !c.state_is_manual_review {
            refuse(
                &mut c,
                format!("request {request_id} is in state {state:?}, not ManualReview"),
            );
        }

        let reason = note.as_deref().map(Self::manual_review_reason_prefix);
        c.reason_is_refundable =
            reason.is_some_and(|r| Self::REFUNDABLE_GLC_MANUAL_REVIEW_REASONS.contains(&r));
        if !c.reason_is_refundable {
            refuse(
                &mut c,
                format!(
                    "manual_review_note reason {:?} is not on the refundable list {:?}",
                    reason.unwrap_or(""),
                    Self::REFUNDABLE_GLC_MANUAL_REVIEW_REASONS
                ),
            );
        }

        c.has_source_outpoint = source_txid.is_some() && source_vout.is_some();
        if !c.has_source_outpoint {
            refuse(
                &mut c,
                format!(
                    "request {request_id} has no recorded source txid/vout, so there is no \
                         deposit to trace or return"
                ),
            );
        }

        let payout_exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT request_id FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        c.no_goldcoin_payout = payout_exists.is_none();
        if !c.no_goldcoin_payout {
            refuse(
                &mut c,
                format!("request {request_id} already has a goldcoin_payouts row"),
            );
        }

        c.no_destination_txid = destination_txid.is_none();
        if !c.no_destination_txid {
            refuse(
                &mut c,
                format!(
                    "request {request_id} already records a destination transaction; a \
                         settlement has begun"
                ),
            );
        }

        c.no_settlement_claim = claim_hash.is_none();
        if !c.no_settlement_claim {
            refuse(
                &mut c,
                format!(
                    "request {request_id} already records a settlement claim hash; a Solana \
                         release has been authorized"
                ),
            );
        }

        // The Robinhood half. Answered entirely from committed rows, so a
        // daemon restart cannot make an in-flight payout look refundable
        // — there is no in-memory state involved to lose.
        c.robinhood_payout_evidence = Self::robinhood_payout_evidence_in(&self.conn, request_id)?;
        c.no_robinhood_payout_started = c.robinhood_payout_evidence.is_empty();
        if let Some(reason) = c.robinhood_payout_evidence.first().map(|e| e.reason()) {
            refuse(&mut c, format!("request {request_id}: {reason}"));
        }

        let existing: Option<GoldcoinRefundState> = self
            .conn
            .query_row(
                "SELECT state FROM goldcoin_refunds WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        c.no_existing_refund = existing.is_none();
        if let Some(existing_state) = existing {
            refuse(
                &mut c,
                format!(
                    "request {request_id} already has a Goldcoin refund in state {}",
                    existing_state.as_str()
                ),
            );
        }
        // Held (schema v30) without a recorded `refund` decision: the
        // dry run reports it, and `begin_goldcoin_refund` refuses it.
        if let Some(detail) = Self::refund_hold_blocker_in(&self.conn, request_id)? {
            refuse(&mut c, format!("request {request_id} is {detail}"));
        }

        {
            // The durable witnesses live on the REQUEST ROW, written by
            // the indexer at park time. Absence of `observed_amount_atomic`
            // is NOT a refusal: it means the row predates schema v20, and
            // the caller reports the reduced-assurance legacy mode. A
            // dishonest backfill (from the note, or from a re-read of
            // chain history) is exactly what the column exists to avoid.
            let (durable, stored_script): (Option<i64>, Option<String>) = self
                .conn
                .query_row(
                    "SELECT observed_amount_atomic, deposit_script_pubkey_hex
                     FROM bridge_requests WHERE id = ?1",
                    [request_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .unwrap_or((None, None));
            c.durable_observed_amount_atomic = durable.map(|v| v as u64);
            c.stored_deposit_script_pubkey_hex = stored_script;
        }

        Ok(c)
    }

    /// Opens the refund lifecycle atomically: re-runs every database check
    /// under the write lock, records the row in `Built`, reserves the
    /// selected vault inputs, and moves the request to `RefundPending`.
    ///
    /// `observed_amount_atomic`, `refund_dest_*` and the source-input
    /// outpoint are supplied by the caller because only the caller has
    /// Goldcoin RPC — but they are not taken on trust: the caller
    /// (`goldcoin::refund::execute_refund`) derives them from chain and
    /// this function re-verifies `observed_amount_atomic` against the
    /// `vault_utxos` indexed witness before writing anything. A chain/DB
    /// disagreement fails the whole transaction.
    ///
    /// Deliberately does NOT release the request's stranded SolanaReserve
    /// reservation. That happens once, at the terminal transition
    /// ([`Self::record_goldcoin_refund_confirmed`]) — freeing capacity
    /// while the refund could still fail would let new demand consume
    /// liquidity against an obligation that is not yet discharged.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_goldcoin_refund(
        &mut self,
        request_id: i64,
        observed_amount_atomic: u64,
        source_input_txid: [u8; 32],
        source_input_vout: u32,
        refund_dest_p2pkh_hash: [u8; 20],
        refund_dest_address: &str,
        fee_atomic: u64,
        inputs: &[crate::goldcoin::coin::VaultUtxo],
        unsigned_tx_hex: &str,
        note: &str,
        created_by: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if note.trim().is_empty() {
            return Err(LedgerError::GlcRefundNotEligible {
                id: request_id,
                detail: "an operator note is required".to_string(),
            });
        }
        if inputs.is_empty() {
            return Err(LedgerError::GlcRefundNotEligible {
                id: request_id,
                detail: "refund transaction has no inputs".to_string(),
            });
        }

        let tx = write_tx(&mut self.conn)?;
        // A held row (schema v30) refunds only on a recorded `refund`
        // decision — same gate as the Solana and Robinhood refund paths.
        if let Some(detail) = Self::refund_hold_blocker_in(&tx, request_id)? {
            tx.rollback()?;
            return Err(LedgerError::GlcRefundNotEligible {
                id: request_id,
                detail,
            });
        }

        let (direction, state, note_db, source_txid, source_vout, destination_txid, claim_hash): GlcRefundEligibilityRow = tx
            .query_row(
                "SELECT direction, state, manual_review_note, source_txid, source_vout,
                        destination_txid, settlement_claim_hash
                 FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()?
            .ok_or(LedgerError::RequestNotFound(request_id))?;

        if !direction.source_is_goldcoin() {
            tx.rollback()?;
            return Err(LedgerError::NotAGoldcoinSourcedRequest {
                id: request_id,
                actual_direction: direction,
            });
        }
        if state != RequestState::ManualReview {
            tx.rollback()?;
            return Err(LedgerError::GlcRefundNotEligible {
                id: request_id,
                detail: format!("state is {state:?}, not ManualReview"),
            });
        }
        let reason = note_db
            .as_deref()
            .map(Self::manual_review_reason_prefix)
            .unwrap_or("");
        if !Self::REFUNDABLE_GLC_MANUAL_REVIEW_REASONS.contains(&reason) {
            tx.rollback()?;
            return Err(LedgerError::GlcRefundNotEligible {
                id: request_id,
                detail: format!("manual_review_note reason {reason:?} is not refundable"),
            });
        }
        if destination_txid.is_some() || claim_hash.is_some() {
            tx.rollback()?;
            return Err(LedgerError::GlcRefundNotEligible {
                id: request_id,
                detail: "a Solana settlement has already begun for this request".to_string(),
            });
        }
        // The ROBINHOOD half of "no settlement has begun", re-run HERE
        // inside the writing transaction rather than trusted from the
        // caller's earlier dry run. Between a dry run and an execute the
        // settlement loop may have started a payout, and the whole point
        // of re-checking under the write lock is that such a race resolves
        // to a refusal rather than to two payments.
        //
        // Identical query to the one `glc_refund_db_checks` reports, so
        // the printable view and the enforced gate cannot disagree.
        let robinhood_evidence = Self::robinhood_payout_evidence_in(&tx, request_id)?;
        if let Some(first) = robinhood_evidence.first() {
            tx.rollback()?;
            return Err(LedgerError::GlcRefundNotEligible {
                id: request_id,
                detail: first.reason(),
            });
        }
        let payout_exists: Option<i64> = tx
            .query_row(
                "SELECT request_id FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        if payout_exists.is_some() {
            tx.rollback()?;
            return Err(LedgerError::GlcRefundNotEligible {
                id: request_id,
                detail: "a goldcoin_payouts row already exists".to_string(),
            });
        }
        let existing: Option<GoldcoinRefundState> = tx
            .query_row(
                "SELECT state FROM goldcoin_refunds WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing_state) = existing {
            tx.rollback()?;
            return Err(LedgerError::GoldcoinRefundExists {
                id: request_id,
                refund_state: existing_state.as_str().to_string(),
            });
        }

        let (Some(source_txid), Some(source_vout)) = (source_txid, source_vout) else {
            tx.rollback()?;
            return Err(LedgerError::GlcRefundNotEligible {
                id: request_id,
                detail: "no recorded source txid/vout".to_string(),
            });
        };

        // The DURABLE amount witness must agree with the chain-derived
        // principal the caller measured — the DB half of "observed amount
        // differs from independent evidence -> fail closed", re-checked
        // here inside the writing transaction so a concurrent change
        // between the caller's check and this insert cannot slip through.
        //
        // Deliberately NOT `vault_utxos`: that table is listunspent-derived
        // root-vault spendable inventory, and nothing imports a
        // per-request derived P2SH into the node, so a request-specific
        // deposit can never appear there. Requiring it made every
        // per-request deposit unrefundable.
        //
        // A NULL witness means the request predates schema v20. That is
        // the documented LEGACY path: permitted, because every other
        // binding (outpoint, independently derived deposit script, stored
        // script agreement, confirmations, single input, prevout trace, no
        // release, no prior refund) still had to pass on the caller's
        // side, and the principal came from the verified RPC read. It is
        // never reconstructed from `manual_review_note`.
        let durable: Option<i64> = tx
            .query_row(
                "SELECT observed_amount_atomic FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(durable) = durable {
            if durable as u64 != observed_amount_atomic {
                tx.rollback()?;
                return Err(LedgerError::GlcRefundNotEligible {
                    id: request_id,
                    detail: format!(
                        "chain-derived observed amount {observed_amount_atomic} does not equal \
                         the durable ledger witness {durable}"
                    ),
                });
            }
        }

        tx.execute(
            "INSERT INTO goldcoin_refunds
                (request_id, source_txid, source_vout, observed_amount_atomic,
                 source_input_txid, source_input_vout, refund_dest_p2pkh_hash,
                 refund_dest_address, refund_amount_atomic, fee_atomic, unsigned_tx_hex,
                 state, manual_review_reason, note, created_by, built_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'Built', ?12, ?13, ?14, ?15)",
            rusqlite::params![
                request_id,
                source_txid.as_slice(),
                source_vout,
                observed_amount_atomic as i64,
                source_input_txid.as_slice(),
                source_input_vout,
                refund_dest_p2pkh_hash.as_slice(),
                refund_dest_address,
                // Policy: the recipient receives the FULL observed deposit;
                // the miner fee is separate vault expenditure. The schema
                // CHECK enforces this equality independently.
                observed_amount_atomic as i64,
                fee_atomic as i64,
                unsigned_tx_hex,
                reason,
                note,
                created_by,
                now,
            ],
        )?;

        for (order, utxo) in inputs.iter().enumerate() {
            tx.execute(
                "INSERT INTO goldcoin_refund_inputs
                    (request_id, input_order, txid, vout, amount_atomic)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    request_id,
                    order as i64,
                    utxo.txid.as_slice(),
                    utxo.vout,
                    utxo.amount_atomic as i64,
                ],
            )?;
            let updated = tx.execute(
                "UPDATE vault_utxos SET state = 'Reserved', reserved_by = ?1, reserved_at = ?2
                 WHERE txid = ?3 AND vout = ?4 AND state = 'Available'",
                rusqlite::params![request_id, now, utxo.txid.as_slice(), utxo.vout],
            )?;
            if updated != 1 {
                tx.rollback()?;
                return Err(LedgerError::GlcRefundNotEligible {
                    id: request_id,
                    detail: format!(
                        "vault UTXO {}:{} was not Available at reservation time; another \
                         settlement took it first",
                        crate::goldcoin::hex::encode(&utxo.txid),
                        utxo.vout
                    ),
                });
            }
        }

        tx.execute(
            "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![RequestState::RefundPending, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::ManualReview),
            RequestState::RefundPending,
            now,
            Some("glc_refund_started"),
            created_by,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records the signed transaction. Written BEFORE broadcast, so a
    /// crash between the two is recoverable by re-broadcasting these exact
    /// bytes rather than by building anything new.
    pub fn record_goldcoin_refund_signed(
        &mut self,
        request_id: i64,
        signed_tx_hex: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let state: GoldcoinRefundState = tx
            .query_row(
                "SELECT state FROM goldcoin_refunds WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(LedgerError::GoldcoinRefundNotFound(request_id))?;
        // Idempotent re-entry: already signed is a no-op, never a re-sign.
        if state != GoldcoinRefundState::Built {
            tx.rollback()?;
            if state == GoldcoinRefundState::Signed {
                return Ok(());
            }
            return Err(LedgerError::GoldcoinRefundWrongState {
                id: request_id,
                expected: "Built",
                actual: state.as_str().to_string(),
            });
        }
        tx.execute(
            "UPDATE goldcoin_refunds SET signed_tx_hex = ?1, signed_at = ?2, state = 'Signed'
             WHERE request_id = ?3",
            rusqlite::params![signed_tx_hex, now, request_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records the broadcast txid and moves the request to
    /// `RefundBroadcast`. Idempotent: re-recording the SAME txid on an
    /// already-broadcast refund succeeds (that is exactly what a crash
    /// retry does), while a DIFFERENT txid is refused — it would mean a
    /// second transaction exists for one refund.
    pub fn record_goldcoin_refund_broadcast(
        &mut self,
        request_id: i64,
        txid: [u8; 32],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (state, existing_txid): (GoldcoinRefundState, Option<Vec<u8>>) = tx
            .query_row(
                "SELECT state, txid FROM goldcoin_refunds WHERE request_id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(LedgerError::GoldcoinRefundNotFound(request_id))?;

        if let Some(existing) = existing_txid {
            if existing.as_slice() != txid.as_slice() {
                tx.rollback()?;
                return Err(LedgerError::GlcRefundNotEligible {
                    id: request_id,
                    detail: format!(
                        "refund already recorded txid {} but broadcast returned {}; refusing to \
                         overwrite — two transactions may exist for one refund",
                        crate::goldcoin::hex::encode(&existing),
                        crate::goldcoin::hex::encode(&txid)
                    ),
                });
            }
            tx.rollback()?;
            return Ok(());
        }

        if state != GoldcoinRefundState::Signed {
            tx.rollback()?;
            return Err(LedgerError::GoldcoinRefundWrongState {
                id: request_id,
                expected: "Signed",
                actual: state.as_str().to_string(),
            });
        }

        tx.execute(
            "UPDATE goldcoin_refunds SET txid = ?1, broadcast_at = ?2, state = 'Broadcast'
             WHERE request_id = ?3",
            rusqlite::params![txid.as_slice(), now, request_id],
        )?;
        tx.execute(
            "UPDATE vault_utxos SET state = 'Spent', spent_by_txid = ?1
             WHERE txid IN (SELECT txid FROM goldcoin_refund_inputs WHERE request_id = ?2)
               AND vout IN (SELECT vout FROM goldcoin_refund_inputs WHERE request_id = ?2)
               AND reserved_by = ?2",
            rusqlite::params![txid.as_slice(), request_id],
        )?;
        tx.execute(
            "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![RequestState::RefundBroadcast, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::RefundPending),
            RequestState::RefundBroadcast,
            now,
            Some("glc_refund_broadcast"),
            "operator",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Advances the observed confirmation count without changing state.
    pub fn update_goldcoin_refund_confirmations(
        &mut self,
        request_id: i64,
        confirmations: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE goldcoin_refunds SET confirmations = ?1 WHERE request_id = ?2",
            rusqlite::params![confirmations, request_id],
        )?;
        Ok(())
    }

    /// The terminal transition, and the ONLY place the request's stranded
    /// SolanaReserve reservation is released.
    ///
    /// A `GlcToSol` request reserves capacity on the SolanaReserve at
    /// `create_request` time. The amount-mismatch park moved it to
    /// `ManualReview` WITHOUT releasing that reservation, so the capacity
    /// stayed held. Releasing it here — once, and only once the refund has
    /// actually confirmed — discharges the obligation truthfully:
    ///
    /// - not at `Built` or `Signed`, because a refund that never lands
    ///   leaves the deposit outstanding and the obligation real;
    /// - exactly once, enforced by the `reservation_released` flag being
    ///   flipped inside this same transaction and by the schema CHECK that
    ///   permits it only in the terminal state.
    ///
    /// Re-entry after the flag is set is a clean no-op, so a retried
    /// confirmation tick can never double-free capacity.
    pub fn record_goldcoin_refund_confirmed(
        &mut self,
        request_id: i64,
        confirmations: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (state, released): (GoldcoinRefundState, i64) = tx
            .query_row(
                "SELECT state, reservation_released FROM goldcoin_refunds WHERE request_id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(LedgerError::GoldcoinRefundNotFound(request_id))?;

        if state == GoldcoinRefundState::Refunded {
            // Already terminal. Confirmations may still deepen, but the
            // reservation must never be released a second time.
            tx.execute(
                "UPDATE goldcoin_refunds SET confirmations = ?1 WHERE request_id = ?2",
                rusqlite::params![confirmations, request_id],
            )?;
            tx.commit()?;
            return Ok(());
        }
        if state != GoldcoinRefundState::Broadcast {
            tx.rollback()?;
            return Err(LedgerError::GoldcoinRefundWrongState {
                id: request_id,
                expected: "Broadcast",
                actual: state.as_str().to_string(),
            });
        }

        Self::commit_goldcoin_refund_terminal(&tx, request_id, confirmations, released, now)?;
        tx.commit()?;
        Ok(())
    }

    /// The one place the `Broadcast -> Refunded` write lives. Shared by
    /// [`Self::record_goldcoin_refund_confirmed`] and
    /// [`Self::reconcile_goldcoin_refund_confirmations`] so the two cannot
    /// drift on the terminal effects — in particular on the one-shot
    /// reservation release, which is the only irreversible accounting move
    /// a refund makes.
    ///
    /// Caller has already verified the row is in `Broadcast` and read its
    /// `reservation_released` flag inside this same transaction.
    fn commit_goldcoin_refund_terminal(
        tx: &Connection,
        request_id: i64,
        confirmations: i64,
        released: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        tx.execute(
            "UPDATE goldcoin_refunds
             SET state = 'Refunded', refunded_at = ?1, confirmations = ?2,
                 reservation_released = 1
             WHERE request_id = ?3",
            rusqlite::params![now, confirmations, request_id],
        )?;

        if released == 0 {
            let (direction, net_destination_atomic): (Direction, i64) = tx.query_row(
                "SELECT direction, net_destination_atomic FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            // Same accounting move `cancel_request` makes when a reserved
            // request is abandoned before settlement.
            tx.execute(
                "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity - ?1
                 WHERE direction = ?2",
                rusqlite::params![net_destination_atomic, direction.destination_reserve()],
            )?;
        }

        tx.execute(
            "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![RequestState::Refunded, request_id],
        )?;
        log_transition(
            tx,
            request_id,
            Some(RequestState::RefundBroadcast),
            RequestState::Refunded,
            now,
            Some("glc_refund_confirmed"),
            "system",
        )?;
        Ok(())
    }

    /// Records an OBSERVED confirmation depth for an existing refund
    /// broadcast and, only once that depth reaches `required_confirmations`,
    /// performs the terminal `Broadcast -> Refunded` transition — the whole
    /// decision taken atomically, under one write lock, from the state as
    /// it is at that instant.
    ///
    /// Returns `true` only for the call that actually FIRED the transition,
    /// so a caller can count real settlements without re-counting every
    /// subsequent depth refresh.
    ///
    /// # Why the threshold lives here and not in the caller
    ///
    /// [`Self::record_goldcoin_refund_confirmed`] transitions
    /// unconditionally: it is the primitive, and it trusts its caller to
    /// have decided. That is fine for an attended operator action and wrong
    /// for an unattended daemon pass, where "did this transaction reach the
    /// required depth" and "commit the terminal transition" must be one
    /// indivisible decision rather than a check followed by a write that
    /// could be reached with a stale answer. Putting the comparison inside
    /// this transaction means no reordering, retry or concurrent writer can
    /// separate them.
    ///
    /// # Idempotent, in every direction
    ///
    /// - Already `Refunded`: refreshes the recorded depth and returns
    ///   `false`. The reservation is never released twice — the terminal
    ///   write is not re-run at all.
    /// - Still below `required_confirmations`: records the depth and
    ///   returns `false`. Nothing else changes, so the row stays exactly as
    ///   recoverable as it was.
    /// - `Built`/`Signed`: refused with
    ///   [`LedgerError::GoldcoinRefundWrongState`]. A refund that was never
    ///   broadcast has no on-chain depth to reconcile, and this function
    ///   must never be the thing that advances it.
    ///
    /// This function never writes `txid`, `signed_tx_hex` or any other
    /// evidence column: the stored broadcast is authoritative and is only
    /// ever read here.
    pub fn reconcile_goldcoin_refund_confirmations(
        &mut self,
        request_id: i64,
        confirmations: i64,
        required_confirmations: i64,
        now: i64,
    ) -> Result<bool, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (state, released): (GoldcoinRefundState, i64) = tx
            .query_row(
                "SELECT state, reservation_released FROM goldcoin_refunds WHERE request_id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(LedgerError::GoldcoinRefundNotFound(request_id))?;

        if state == GoldcoinRefundState::Refunded {
            tx.execute(
                "UPDATE goldcoin_refunds SET confirmations = ?1 WHERE request_id = ?2",
                rusqlite::params![confirmations, request_id],
            )?;
            tx.commit()?;
            return Ok(false);
        }
        if state != GoldcoinRefundState::Broadcast {
            tx.rollback()?;
            return Err(LedgerError::GoldcoinRefundWrongState {
                id: request_id,
                expected: "Broadcast",
                actual: state.as_str().to_string(),
            });
        }
        if confirmations < required_confirmations {
            tx.execute(
                "UPDATE goldcoin_refunds SET confirmations = ?1 WHERE request_id = ?2",
                rusqlite::params![confirmations, request_id],
            )?;
            tx.commit()?;
            return Ok(false);
        }

        Self::commit_goldcoin_refund_terminal(&tx, request_id, confirmations, released, now)?;
        tx.commit()?;
        Ok(true)
    }

    /// One refund row, or `None`.
    pub fn get_goldcoin_refund(
        &self,
        request_id: i64,
    ) -> Result<Option<GoldcoinRefundRow>, LedgerError> {
        self.conn
            .query_row(
                "SELECT request_id, source_txid, source_vout, observed_amount_atomic,
                        source_input_txid, source_input_vout, refund_dest_p2pkh_hash,
                        refund_dest_address, refund_amount_atomic, fee_atomic, unsigned_tx_hex,
                        signed_tx_hex, txid, confirmations, state, manual_review_reason, note,
                        created_by, built_at, signed_at, broadcast_at, refunded_at,
                        reservation_released
                 FROM goldcoin_refunds WHERE request_id = ?1",
                [request_id],
                map_goldcoin_refund_row,
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Every refund row, newest first. `open_only` excludes the terminal
    /// `Refunded` state — the listing an operator wants when asking "what
    /// is still in flight?".
    pub fn list_goldcoin_refunds(
        &self,
        open_only: bool,
    ) -> Result<Vec<GoldcoinRefundRow>, LedgerError> {
        let sql = if open_only {
            "SELECT request_id, source_txid, source_vout, observed_amount_atomic,
                    source_input_txid, source_input_vout, refund_dest_p2pkh_hash,
                    refund_dest_address, refund_amount_atomic, fee_atomic, unsigned_tx_hex,
                    signed_tx_hex, txid, confirmations, state, manual_review_reason, note,
                    created_by, built_at, signed_at, broadcast_at, refunded_at,
                    reservation_released
             FROM goldcoin_refunds WHERE state != 'Refunded' ORDER BY built_at DESC, request_id DESC"
        } else {
            "SELECT request_id, source_txid, source_vout, observed_amount_atomic,
                    source_input_txid, source_input_vout, refund_dest_p2pkh_hash,
                    refund_dest_address, refund_amount_atomic, fee_atomic, unsigned_tx_hex,
                    signed_tx_hex, txid, confirmations, state, manual_review_reason, note,
                    created_by, built_at, signed_at, broadcast_at, refunded_at,
                    reservation_released
             FROM goldcoin_refunds ORDER BY built_at DESC, request_id DESC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map([], map_goldcoin_refund_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The persisted refund inputs, in construction order — the exact
    /// outpoints a rebuild or a validation pass must see.
    pub fn get_goldcoin_refund_inputs(
        &self,
        request_id: i64,
    ) -> Result<Vec<([u8; 32], u32, u64)>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT txid, vout, amount_atomic FROM goldcoin_refund_inputs
             WHERE request_id = ?1 ORDER BY input_order",
        )?;
        let rows = stmt
            .query_map([request_id], |r| {
                let txid: Vec<u8> = r.get(0)?;
                let vout: u32 = r.get(1)?;
                let amount: i64 = r.get(2)?;
                Ok((txid, vout, amount))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (txid, vout, amount) in rows {
            let mut t = [0u8; 32];
            if txid.len() != 32 {
                return Err(LedgerError::GlcRefundNotEligible {
                    id: request_id,
                    detail: "persisted refund input txid is not 32 bytes".to_string(),
                });
            }
            t.copy_from_slice(&txid);
            out.push((t, vout, amount as u64));
        }
        Ok(out)
    }

    pub fn reserve_vault_utxos(
        &mut self,
        request_id: i64,
        selected: &[crate::goldcoin::coin::VaultUtxo],
        zero_conf_max_depth: u32,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let mut reserved_count = 0usize;
        for u in selected {
            let n = tx.execute(
                &format!(
                    "UPDATE vault_utxos SET state = 'Reserved', reserved_by = ?1, reserved_at = ?2
                 WHERE txid = ?3 AND vout = ?4
                   AND (
                     state = 'Available'
                     OR (
                       ?5 > 0
                       AND state = 'Unconfirmed'
                       AND zero_conf_hold_reason IS NULL
                       AND EXISTS (
                         SELECT 1 FROM goldcoin_payout_change_outpoints o
                          WHERE o.txid = vault_utxos.txid AND o.vout = vault_utxos.vout
                            AND (vault_utxos.confirmations >= 1
                                 OR o.unconfirmed_ancestor_depth <= ?5)
                       )
                     )
                   )
                   AND {claim_excl_uv}",
                    claim_excl_uv = live_split_claim_exclusion("vault_utxos")
                ),
                rusqlite::params![
                    request_id,
                    now,
                    u.txid.as_slice(),
                    u.vout,
                    zero_conf_max_depth
                ],
            )?;
            reserved_count += n;
        }
        if reserved_count != selected.len() {
            tx.rollback()?;
            return Err(LedgerError::VaultUtxoUnavailable {
                requested: selected.len(),
                available: reserved_count,
            });
        }
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------- vault UTXO split --

    /// Read-only lookup of a single `vault_utxos` row — what
    /// [`crate::signing::goldcoin_split`]'s independent re-derivation needs
    /// to confirm a proposed split source is real, mature, and owned by
    /// the script the caller claims, entirely from this ledger's own view
    /// (never trusted from a caller-supplied amount).
    pub fn get_vault_utxo(
        &self,
        txid: [u8; 32],
        vout: u32,
    ) -> Result<Option<VaultUtxoRow>, LedgerError> {
        self.conn
            .query_row(
                "SELECT amount_atomic, script_pubkey_hex, state FROM vault_utxos
                 WHERE txid = ?1 AND vout = ?2",
                rusqlite::params![txid.as_slice(), vout],
                |r| {
                    Ok(VaultUtxoRow {
                        amount_atomic: r.get::<_, i64>(0)? as u64,
                        script_pubkey_hex: r.get(1)?,
                        state: r.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Read-only lookup of any existing split attempt for a given source
    /// outpoint — the idempotency check `glc-admin split-vault-utxo` runs
    /// before ever contacting a signer: a source outpoint that already has
    /// a row here has already been split (or is mid-flight), and must
    /// never be split again.
    pub fn get_vault_utxo_split(
        &self,
        source_txid: [u8; 32],
        source_vout: u32,
    ) -> Result<Option<VaultUtxoSplitSnapshot>, LedgerError> {
        self.conn
            .query_row(
                "SELECT id, source_amount_atomic, chunk_count, chunk_target_atomic, fee_atomic,
                        unsigned_tx_hex, signed_tx_hex, txid, state
                 FROM vault_utxo_splits
                 WHERE source_txid = ?1 AND source_vout = ?2 AND state != 'Abandoned'",
                rusqlite::params![source_txid.as_slice(), source_vout],
                |r| {
                    let txid_vec: Option<Vec<u8>> = r.get(7)?;
                    Ok(VaultUtxoSplitSnapshot {
                        id: r.get(0)?,
                        source_amount_atomic: r.get::<_, i64>(1)? as u64,
                        chunk_count: r.get(2)?,
                        chunk_target_atomic: r.get::<_, i64>(3)? as u64,
                        fee_atomic: r.get::<_, i64>(4)? as u64,
                        unsigned_tx_hex: r.get(5)?,
                        signed_tx_hex: r.get(6)?,
                        txid: txid_vec.map(|v| to_array32(&v)),
                        state: r.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Records a freshly built (not yet signed) vault UTXO split — and,
    /// with it, the CLAIM on the source outpoint: from the moment this
    /// commits, the source is invisible to payout coin selection and
    /// unreservable by any payout ([`Ledger::available_vault_utxos`] and
    /// [`Ledger::reserve_vault_utxos`] both exclude the source of any
    /// live — non-`Abandoned`, non-`Confirmed` — split row), so a payout
    /// and a split can never contend for the same UTXO no matter how the
    /// two processes interleave (2026-08-30 review, blocker: the
    /// CLI-vs-daemon race). The claim is validated here, inside this same
    /// transaction: the source row must exist and be `Available` (a
    /// `Reserved` source is already promised to a payout — claiming it
    /// would be the same double-spend in the other direction). The
    /// explicit existence check, backed by `ux_vault_utxo_splits_source`'s
    /// structural partial-`UNIQUE(source_txid, source_vout) WHERE state !=
    /// 'Abandoned'` guarantee, is the idempotency boundary — an
    /// `Abandoned` prior attempt never blocks a fresh, legitimate split
    /// of the same outpoint.
    #[allow(clippy::too_many_arguments)]
    pub fn record_vault_utxo_split_built(
        &mut self,
        plan: &crate::goldcoin::split::SplitPlan,
        chunk_target_atomic: u64,
        unsigned_tx_hex: &str,
        note: &str,
        now: i64,
    ) -> Result<i64, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT id FROM vault_utxo_splits
                 WHERE source_txid = ?1 AND source_vout = ?2 AND state != 'Abandoned'",
                rusqlite::params![plan.source.txid.as_slice(), plan.source.vout],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_some() {
            tx.rollback()?;
            return Err(LedgerError::VaultUtxoAlreadySplit {
                txid: plan.source.txid,
                vout: plan.source.vout,
            });
        }
        let source_state: Option<String> = tx
            .query_row(
                "SELECT state FROM vault_utxos WHERE txid = ?1 AND vout = ?2",
                rusqlite::params![plan.source.txid.as_slice(), plan.source.vout],
                |r| r.get(0),
            )
            .optional()?;
        match source_state.as_deref() {
            Some("Available") => {}
            None => {
                tx.rollback()?;
                return Err(LedgerError::VaultUtxoNotFound {
                    txid: plan.source.txid,
                    vout: plan.source.vout,
                });
            }
            Some(other) => {
                tx.rollback()?;
                return Err(LedgerError::VaultUtxoNotSplittable {
                    txid: plan.source.txid,
                    vout: plan.source.vout,
                    state: other.to_string(),
                });
            }
        }
        tx.execute(
            "INSERT INTO vault_utxo_splits
                (source_txid, source_vout, source_amount_atomic, chunk_count, chunk_target_atomic,
                 fee_atomic, unsigned_tx_hex, state, note, built_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'Built', ?8, ?9)",
            rusqlite::params![
                plan.source.txid.as_slice(),
                plan.source.vout,
                plan.source.amount_atomic as i64,
                plan.output_count() as i64,
                chunk_target_atomic as i64,
                plan.fee_atomic as i64,
                unsigned_tx_hex,
                note,
                now,
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// `Built -> Signed`.
    pub fn record_vault_utxo_split_signed(
        &mut self,
        id: i64,
        signed_tx_hex: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE vault_utxo_splits SET signed_tx_hex = ?1, state = 'Signed', signed_at = ?2
             WHERE id = ?3 AND state = 'Built'",
            rusqlite::params![signed_tx_hex, now, id],
        )?;
        if n == 0 {
            return Err(LedgerError::VaultUtxoSplitNotFound(id));
        }
        Ok(())
    }

    /// `Signed -> Broadcast`, with EVERY consequence of the broadcast in
    /// the same transaction (2026-08-30 review: the previous two-call
    /// protocol — state transition here, source/chunk bookkeeping in a
    /// separate later call — left a crash window in which the ledger
    /// believed the source was still spendable while the mempool already
    /// spent it):
    ///
    /// 1. the split row itself moves to `Broadcast`;
    /// 2. the source outpoint becomes `Spent` (`spent_by_txid` = the
    ///    split's own txid — the marker [`Ledger::sync_vault_utxos`]'s
    ///    resurrection rule treats as "spent by a transaction this
    ///    service signed", never resurrected);
    /// 3. each chunk output is inserted as an `Unconfirmed` `vault_utxos`
    ///    row (vout = output index) so `own_unconfirmed_change_atomic`
    ///    explains the mature-balance dip with no gap.
    ///
    /// Idempotent: re-recording an already-`Broadcast` split (restart
    /// between broadcast and this call, with a re-broadcast in between)
    /// re-applies effects 2 and 3 harmlessly (`ON CONFLICT DO NOTHING`,
    /// state guards). The amounts/txid come from this service's own
    /// verified [`crate::goldcoin::split::SplitPlan`] and locally-computed
    /// txid — never from node-reported data.
    pub fn record_vault_utxo_split_broadcast(
        &mut self,
        id: i64,
        txid: [u8; 32],
        output_amounts: &[u64],
        vault_script_pubkey_hex: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(Vec<u8>, u32, String)> = tx
            .query_row(
                "SELECT source_txid, source_vout, state FROM vault_utxo_splits WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((source_txid, source_vout, state)) = row else {
            tx.rollback()?;
            return Err(LedgerError::VaultUtxoSplitNotFound(id));
        };
        match state.as_str() {
            "Signed" | "Broadcast" => {}
            other => {
                tx.rollback()?;
                return Err(LedgerError::VaultUtxoSplitNotRecoverable {
                    id,
                    state: other.to_string(),
                });
            }
        }
        tx.execute(
            "UPDATE vault_utxo_splits SET state = 'Broadcast', txid = ?1,
                    broadcast_at = COALESCE(broadcast_at, ?2)
             WHERE id = ?3",
            rusqlite::params![txid.as_slice(), now, id],
        )?;
        // Source -> Spent. Only Available (or, idempotently, already
        // Spent) is acceptable: the claim placed at `Built` time excludes
        // every other transition, so anything else is bookkeeping drift
        // that must fail loudly — inside the transaction, before commit.
        let source_state: Option<String> = tx
            .query_row(
                "SELECT state FROM vault_utxos WHERE txid = ?1 AND vout = ?2",
                rusqlite::params![source_txid.as_slice(), source_vout],
                |r| r.get(0),
            )
            .optional()?;
        match source_state.as_deref() {
            Some("Available") => {
                tx.execute(
                    "UPDATE vault_utxos SET state = 'Spent', spent_by_txid = ?1
                     WHERE txid = ?2 AND vout = ?3",
                    rusqlite::params![txid.as_slice(), source_txid.as_slice(), source_vout],
                )?;
            }
            Some("Spent") => {
                // Idempotent re-run — but ALWAYS backfill `spent_by_txid`:
                // in the crash window between broadcast and this call,
                // the sync's absence flip may have marked the source
                // `Spent` with no marker, and a marker-less Spent row is
                // resurrectable (2026-08-30 third-pass review, finding 1
                // — the resurrection would offer coin selection an
                // outpoint our own signed split still spends).
                tx.execute(
                    "UPDATE vault_utxos SET spent_by_txid = ?1
                     WHERE txid = ?2 AND vout = ?3 AND spent_by_txid IS NULL",
                    rusqlite::params![txid.as_slice(), source_txid.as_slice(), source_vout],
                )?;
            }
            None => {
                tx.rollback()?;
                return Err(LedgerError::VaultUtxoNotFound {
                    txid: to_array32(&source_txid),
                    vout: source_vout,
                });
            }
            Some(other) => {
                tx.rollback()?;
                return Err(LedgerError::VaultUtxoNotSplittable {
                    txid: to_array32(&source_txid),
                    vout: source_vout,
                    state: other.to_string(),
                });
            }
        }
        for (i, &amount) in output_amounts.iter().enumerate() {
            tx.execute(
                "INSERT INTO vault_utxos
                    (txid, vout, amount_atomic, script_pubkey_hex, confirmations, first_seen_at, state)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, 'Unconfirmed')
                 ON CONFLICT(txid, vout) DO NOTHING",
                rusqlite::params![
                    txid.as_slice(),
                    i as u32,
                    amount as i64,
                    vault_script_pubkey_hex,
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// `Broadcast -> Confirmed` — the split transaction has been observed
    /// with at least one confirmation (via its chunk rows' synced
    /// confirmation counts, this service's own chain view — never a
    /// node-claimed status string). Terminal: a confirmed split needs no
    /// further lifecycle driving; its chunks mature through the ordinary
    /// `sync_vault_utxos` path like any other vault output.
    pub fn record_vault_utxo_split_confirmed(
        &mut self,
        id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE vault_utxo_splits
             SET state = 'Confirmed', confirmed_at = ?1, missing_inputs_since = NULL
             WHERE id = ?2 AND state = 'Broadcast'",
            rusqlite::params![now, id],
        )?;
        if n == 0 {
            let state: Option<String> = self
                .conn
                .query_row(
                    "SELECT state FROM vault_utxo_splits WHERE id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?;
            return Err(match state {
                None => LedgerError::VaultUtxoSplitNotFound(id),
                Some(state) => LedgerError::VaultUtxoSplitNotRecoverable { id, state },
            });
        }
        Ok(())
    }

    /// Terminal `-> Abandoned`, from ANY non-terminal state — the
    /// lifecycle's release valve (2026-08-30 review: without one, a
    /// single split whose source became unspendable wedged all automatic
    /// shaping forever, with no non-SQL way out). In one transaction:
    ///
    /// 1. the split row becomes `Abandoned` with the reason on record —
    ///    the row is never deleted (full audit history), but the partial
    ///    unique index stops counting it, so the outpoint can be split
    ///    again later if it genuinely returns;
    /// 2. any chunk rows a `Broadcast` attempt inserted are marked
    ///    `Spent` (they can never exist on-chain — the transaction that
    ///    would have created them is unconfirmable), so no accounting
    ///    term keeps explaining value that is not coming;
    /// 3. the source row is NOT touched. For a split abandoned BEFORE
    ///    broadcast, `sync_vault_utxos` reflects its real on-chain fate
    ///    (spent elsewhere -> `Spent`; still unspent after a reorg ->
    ///    resurrected `Available`, where the lifted claim makes it
    ///    selectable again). For a split abandoned AFTER broadcast the
    ///    source was already marked `Spent` WITH `spent_by_txid`, which
    ///    the resurrection rule deliberately pins forever: the abandoned
    ///    split's fully signed bytes exist and could resurface, so
    ///    re-offering its input to selection would risk double-spending
    ///    our own signature. If a conflicting spend is itself reorged
    ///    out and the value genuinely returns, recovering it is an
    ///    explicit operator decision (reserve-custody runbook), never an
    ///    automatic one.
    ///
    /// `observed_txid`: the transaction id the CALLER derived from the
    /// split's stored signed bytes (`goldcoin::tx::txid_of_serialized`) —
    /// persisted onto the row when the row itself has none (a `Signed`
    /// abandon), so the re-adoption safety net
    /// ([`Ledger::abandoned_splits_with_txid`]) covers every abandonment
    /// of signed bytes, never only post-`Broadcast` ones (2026-08-31
    /// final review, finding 4).
    pub fn abandon_vault_utxo_split(
        &mut self,
        id: i64,
        reason: &str,
        observed_txid: Option<[u8; 32]>,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(Option<Vec<u8>>, String, i64)> = tx
            .query_row(
                "SELECT txid, state, source_amount_atomic FROM vault_utxo_splits WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((split_txid, state, source_amount)) = row else {
            tx.rollback()?;
            return Err(LedgerError::VaultUtxoSplitNotFound(id));
        };
        match state.as_str() {
            "Built" | "Signed" | "Broadcast" => {}
            "Abandoned" => {
                tx.rollback()?;
                return Ok(()); // idempotent
            }
            other => {
                tx.rollback()?;
                return Err(LedgerError::VaultUtxoSplitNotRecoverable {
                    id,
                    state: other.to_string(),
                });
            }
        }
        // Deliberately NO reserve-book mutation here (2026-08-31 final
        // review, findings 2/3; design decision "explicit alarm, no book
        // mutation"): `reconciliation::reconcile` unconditionally
        // converges the cached book to observed reality every pass, so a
        // debit here would double-count (and could underflow). The loss
        // signal is the EXPLICIT alarm instead: reconcile classifies a
        // flagged-past-grace dead split as a Breach in its own right
        // (`Ledger::flagged_dead_splits`), and this row's audit columns
        // are the permanent record of the operator's assertion.
        tx.execute(
            "UPDATE vault_utxo_splits
             SET state = 'Abandoned', abandoned_at = ?1, abandon_reason = ?2,
                 txid = COALESCE(txid, ?3)
             WHERE id = ?4",
            rusqlite::params![
                now,
                reason,
                observed_txid.as_ref().map(|t| t.as_slice()),
                id
            ],
        )?;
        let _ = source_amount;
        let effective_txid: Option<Vec<u8>> =
            split_txid.or_else(|| observed_txid.map(|t| t.to_vec()));
        if let Some(txid) = effective_txid {
            // Both Unconfirmed AND (frozen-)Available chunk rows are
            // cleared: a chunk that matured before a deep reorg keeps a
            // stale 'Available' state the ordinary sync will never decay
            // (2026-08-31 final review, finding 8) — leaving it would
            // hand payout selection a phantom outpoint. Reserved rows are
            // left to their owning payout's own recovery path.
            tx.execute(
                "UPDATE vault_utxos SET state = 'Spent'
                 WHERE txid = ?1 AND state IN ('Unconfirmed','Available')",
                rusqlite::params![txid],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The exact inverse of a post-broadcast abandonment, applied when
    /// the chain proves the abandonment factually wrong: the abandoned
    /// split's transaction has been OBSERVED CONFIRMED on-chain (its
    /// chunk outputs re-synced with >= 1 confirmation). One transaction:
    /// `Abandoned -> Broadcast` (the normal lifecycle then drives it to
    /// `Confirmed` at maturity), the loss debit is credited back, and
    /// any missing-inputs flag is cleared. The `abandoned_at`/
    /// `abandon_reason` audit columns are deliberately KEPT — the
    /// history that an operator abandoned and the chain overruled is
    /// part of the record. Chunk rows need no touch here: the sync's
    /// resurrection rule already revived them from their marker-less
    /// `Spent` state when the outputs reappeared (which is exactly the
    /// observation that triggers this call).
    pub fn readopt_vault_utxo_split(&mut self, id: i64, _now: i64) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(Option<Vec<u8>>, String, i64)> = tx
            .query_row(
                "SELECT txid, state, source_amount_atomic FROM vault_utxo_splits WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((split_txid, state, source_amount)) = row else {
            tx.rollback()?;
            return Err(LedgerError::VaultUtxoSplitNotFound(id));
        };
        if state != "Abandoned" || split_txid.is_none() {
            tx.rollback()?;
            return Err(LedgerError::VaultUtxoSplitNotRecoverable { id, state });
        }
        tx.execute(
            "UPDATE vault_utxo_splits SET state = 'Broadcast', missing_inputs_since = NULL
             WHERE id = ?1",
            [id],
        )?;
        let _ = source_amount; // no book mutation — see abandon_vault_utxo_split
        tx.commit()?;
        Ok(())
    }

    /// Abandoned splits that DID broadcast (txid present) — the set the
    /// lifecycle maintenance watches for on-chain contradiction of the
    /// abandonment (see [`Ledger::readopt_vault_utxo_split`]).
    pub fn abandoned_splits_with_txid(&self) -> Result<Vec<(i64, [u8; 32], i64)>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, txid, abandoned_at FROM vault_utxo_splits
             WHERE state = 'Abandoned' AND txid IS NOT NULL ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let txid: Vec<u8> = r.get(1)?;
                Ok((r.get(0)?, to_array32(&txid), r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// `Broadcast` splits whose exact bytes the node has refused for
    /// missing inputs for longer than [`SPLIT_MISSING_INPUTS_GRACE_SECS`]
    /// — the EXPLICIT dead-split alarm condition
    /// (`reconciliation::reconcile` classifies a non-empty result as a
    /// Breach in its own right): a conflicting spend of vault funds is
    /// the signer-compromise/double-spend threat class, and it must
    /// pause and page, never be silently padded over or silently
    /// dropped (2026-08-31 final review, finding 2).
    pub fn flagged_dead_splits(&self, now: i64) -> Result<Vec<(i64, [u8; 32])>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, txid FROM vault_utxo_splits
             WHERE state = 'Broadcast'
               AND missing_inputs_since IS NOT NULL
               AND missing_inputs_since + ?1 <= ?2
             ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([SPLIT_MISSING_INPUTS_GRACE_SECS, now], |r| {
                let txid: Vec<u8> = r.get(1)?;
                Ok((r.get(0)?, to_array32(&txid)))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Records (idempotently) that a re-broadcast of this `Broadcast`
    /// split's exact bytes was refused for missing inputs — starting the
    /// [`SPLIT_MISSING_INPUTS_GRACE_SECS`] clock after which the
    /// accounting terms stop explaining its chunks. Never overwrites an
    /// existing flag (the clock runs from the FIRST refusal).
    pub fn set_split_missing_inputs(&mut self, id: i64, now: i64) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE vault_utxo_splits SET missing_inputs_since = ?1
             WHERE id = ?2 AND state = 'Broadcast' AND missing_inputs_since IS NULL",
            rusqlite::params![now, id],
        )?;
        Ok(())
    }

    /// Clears the missing-inputs flag — the node accepted or reports the
    /// transaction again, so the earlier refusal was transient (a reorg
    /// race), and the split's chunks are explainable in-flight value
    /// once more.
    pub fn clear_split_missing_inputs(&mut self, id: i64) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE vault_utxo_splits SET missing_inputs_since = NULL WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    /// Every split currently in `Broadcast` — the set the shaping tick's
    /// lifecycle maintenance drives to `Confirmed` (first confirmation
    /// observed), re-broadcasts (evicted from the mempool), or abandons
    /// (inputs genuinely gone). Ordered by id for deterministic
    /// processing.
    pub fn broadcast_vault_utxo_splits(
        &self,
    ) -> Result<Vec<UnconfirmedBroadcastSplit>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, txid, signed_tx_hex FROM vault_utxo_splits
             WHERE state = 'Broadcast' ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let txid: Vec<u8> = r.get(1)?;
                Ok(UnconfirmedBroadcastSplit {
                    id: r.get(0)?,
                    txid: to_array32(&txid),
                    signed_tx_hex: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The highest confirmation count any of `txid`'s outputs currently
    /// carries in `vault_utxos` — how the lifecycle maintenance decides a
    /// broadcast split has been mined (>= 1), from this service's own
    /// synced chain view. `None` when no output row exists at all.
    /// Deliberately NO state filter: a chunk row's recorded confirmation
    /// count proves the transaction was mined regardless of what later
    /// happened to that chunk (matured, got reserved, was spent by a
    /// payout) — filtering states could leave a mined split stuck in
    /// `Broadcast` forever once every chunk had moved on (2026-08-30
    /// re-review, finding 10).
    pub fn max_confirmations_for_txid(&self, txid: [u8; 32]) -> Result<Option<i64>, LedgerError> {
        self.conn
            .query_row(
                "SELECT MAX(confirmations) FROM vault_utxos WHERE txid = ?1",
                rusqlite::params![txid.as_slice()],
                |r| r.get::<_, Option<i64>>(0),
            )
            .map_err(LedgerError::from)
    }

    /// Every split not yet `Broadcast` — what the automatic shaping tick
    /// (`goldcoin::liquidity::run_shaping_tick`) resumes before ever
    /// considering a NEW split: a `Signed` row re-broadcasts its exact
    /// stored bytes (`goldcoin::liquidity::resume_pending_split`), a `Built` row re-signs
    /// its exact reconstructed plan. Ordered by id (oldest first) for
    /// deterministic resume order.
    pub fn pending_vault_utxo_splits(&self) -> Result<Vec<PendingVaultUtxoSplit>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source_txid, source_vout, state FROM vault_utxo_splits
             WHERE state IN ('Built', 'Signed') ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let txid: Vec<u8> = r.get(1)?;
                Ok(PendingVaultUtxoSplit {
                    id: r.get(0)?,
                    source_txid: to_array32(&txid),
                    source_vout: r.get(2)?,
                    state: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ------------------------------------------------------ Goldcoin payout --

    /// Read-only snapshot of a `goldcoin_payouts` row — what an attestation
    /// signer needs from this service's own database to independently
    /// re-derive a `record_goldcoin_completion` claim
    /// ([`crate::signing::attestation`]), combined with its own live
    /// Solana read of the corresponding `WithdrawalObligation`.
    pub fn get_goldcoin_payout(
        &self,
        request_id: i64,
    ) -> Result<Option<GoldcoinPayoutSnapshot>, LedgerError> {
        self.conn
            .query_row(
                "SELECT payout_atomic, txid, state, confirmations, mined_height, onchain_completion_signature, onchain_completion_submitted_at
                 FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| {
                    let txid_vec: Option<Vec<u8>> = r.get(1)?;
                    let sig_vec: Option<Vec<u8>> = r.get(5)?;
                    Ok(GoldcoinPayoutSnapshot {
                        payout_atomic: r.get::<_, i64>(0)? as u64,
                        txid: txid_vec.map(|v| v.try_into().unwrap()),
                        state: r.get(2)?,
                        confirmations: r.get(3)?,
                        mined_height: r.get(4)?,
                        onchain_completion_signature: sig_vec.map(|v| v.try_into().unwrap()),
                        onchain_completion_submitted_at: r.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Records a freshly built (not yet signed) payout, reserving its
    /// inputs' `goldcoin_payout_inputs` rows in the same transaction — the
    /// `UNIQUE(txid, vout)` constraint on that table is the structural
    /// "an outpoint funds at most one payout, ever" guarantee, independent
    /// of `vault_utxos.state` bookkeeping (belt-and-suspenders, same as the
    /// old bridge — docs/01-reuse-inventory.md).
    #[allow(clippy::too_many_arguments)]
    pub fn record_goldcoin_payout_built(
        &mut self,
        request_id: i64,
        plan: &crate::goldcoin::payout::PayoutPlan,
        commitment_hash: [u8; 32],
        unsigned_tx_hex: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT request_id FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_some() {
            tx.rollback()?;
            return Err(LedgerError::PayoutAlreadyExists(request_id));
        }
        // Boundary guard, not just CLI policy: once a refund lifecycle
        // exists for a request, no Goldcoin payout may ever be created
        // for it — the refund returns the source deposit, so a payout on
        // top would be a double-spend of the same obligation. Enforced
        // here, at the single point every payout row is born, regardless
        // of which caller (orchestrator tick, recovery tooling, a future
        // path) asks.
        let refund_state: Option<SolanaRefundState> = tx
            .query_row(
                "SELECT state FROM solana_refunds WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(refund_state) = refund_state {
            tx.rollback()?;
            return Err(LedgerError::RefundLifecycleExists {
                id: request_id,
                refund_state: refund_state.as_str().to_string(),
            });
        }
        tx.execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic, dest_p2pkh_hash,
                 unsigned_tx_hex, state, built_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'Built', ?8)",
            rusqlite::params![
                request_id,
                commitment_hash.as_slice(),
                plan.payout_atomic as i64,
                plan.total_change_atomic() as i64,
                plan.fee_atomic as i64,
                plan.dest_p2pkh_hash.as_slice(),
                unsigned_tx_hex,
                now,
            ],
        )?;
        for (i, input) in plan.inputs.iter().enumerate() {
            tx.execute(
                "INSERT INTO goldcoin_payout_inputs (request_id, input_order, txid, vout, amount_atomic) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![request_id, i as i64, input.txid.as_slice(), input.vout, input.amount_atomic as i64],
            )?;
        }
        for (i, &change_atomic) in plan.change_outputs.iter().enumerate() {
            tx.execute(
                "INSERT INTO goldcoin_payout_change_outputs (request_id, output_order, amount_atomic) VALUES (?1, ?2, ?3)",
                rusqlite::params![request_id, i as i64, change_atomic as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// `Built -> Signed`, and the bridge request `SourceFinalized ->
    /// SettlementAuthorized`: for the Solana->Goldcoin direction, the
    /// threshold of vault-signer partials assembling into a valid signed
    /// transaction IS the settlement authorization (docs/03-architecture.md
    /// — there is no separate Goldcoin-side attestation step, since
    /// Goldcoin has no program layer to attest to).
    pub fn record_goldcoin_payout_signed(
        &mut self,
        request_id: i64,
        signed_tx_hex: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let n = tx.execute(
            "UPDATE goldcoin_payouts SET signed_tx_hex = ?1, state = 'Signed', signed_at = ?2 WHERE request_id = ?3 AND state = 'Built'",
            rusqlite::params![signed_tx_hex, now, request_id],
        )?;
        if n == 0 {
            tx.rollback()?;
            return Err(LedgerError::PayoutNotFound(request_id));
        }
        let state: RequestState = tx.query_row(
            "SELECT state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        assert_eq!(
            state,
            RequestState::SourceFinalized,
            "record_goldcoin_payout_signed on unexpected bridge_request state"
        );
        tx.execute(
            "UPDATE bridge_requests SET state = 'SettlementAuthorized' WHERE id = ?1",
            [request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::SettlementAuthorized,
            now,
            None,
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Signed -> Broadcast`, `SettlementAuthorized -> DestinationSubmitted`.
    /// Idempotent: broadcasting the identical already-broadcast tx again
    /// (e.g. after a restart) is a no-op, matching the RPC client's own
    /// idempotent-broadcast normalization.
    pub fn record_goldcoin_payout_broadcast(
        &mut self,
        request_id: i64,
        txid: [u8; 32],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let current_state: Option<String> = tx
            .query_row(
                "SELECT state FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        match current_state.as_deref() {
            None => {
                tx.rollback()?;
                return Err(LedgerError::PayoutNotFound(request_id));
            }
            Some("Broadcast") | Some("Confirmed") | Some("Completed") => {
                tx.rollback()?;
                return Ok(()); // already broadcast — idempotent no-op
            }
            Some("Signed") => {}
            Some(other) => {
                panic!("record_goldcoin_payout_broadcast on unexpected payout state {other}")
            }
        }
        tx.execute(
            "UPDATE goldcoin_payouts SET state = 'Broadcast', txid = ?1, broadcast_at = ?2 WHERE request_id = ?3",
            rusqlite::params![txid.as_slice(), now, request_id],
        )?;
        // AUTHORITATIVE change provenance for the 0-conf-spendability
        // policy (schema.rs apply_v14): the payout transaction's outputs
        // are [destination, change_0, change_1, ...] in
        // `goldcoin_payout_change_outputs` order (goldcoin::payout's
        // documented layout), so the change outpoints are exactly
        // (txid, 1..=n). Recorded in this same transaction as the
        // broadcast fact itself, so provenance can never lag the txid and
        // survives restart. `unconfirmed_ancestor_depth` = 1 + the
        // deepest still-unconfirmed zero-conf change input this payout
        // consumed (0-conf chain length through this service's OWN
        // payouts; an upper bound — ancestors confirming later only makes
        // reality shallower, never deeper).
        {
            let parent_depth: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(o.unconfirmed_ancestor_depth), 0)
                       FROM goldcoin_payout_inputs i
                       JOIN goldcoin_payout_change_outpoints o
                         ON o.txid = i.txid AND o.vout = i.vout
                       JOIN goldcoin_payouts p ON p.request_id = o.request_id
                      WHERE i.request_id = ?1 AND p.confirmations = 0",
                    [request_id],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let change_amounts: Vec<i64> = {
                let mut stmt = tx.prepare(
                    "SELECT amount_atomic FROM goldcoin_payout_change_outputs
                      WHERE request_id = ?1 ORDER BY output_order",
                )?;
                let rows = stmt
                    .query_map([request_id], |r| r.get(0))?
                    .collect::<Result<Vec<i64>, _>>()?;
                rows
            };
            let change_amounts = if change_amounts.is_empty() {
                // Legacy single-change payout (pre-v12 row shape): the
                // synthesized one-output view `get_goldcoin_payout_full`
                // documents, applied identically here.
                let change_atomic: i64 = tx.query_row(
                    "SELECT change_atomic FROM goldcoin_payouts WHERE request_id = ?1",
                    [request_id],
                    |r| r.get(0),
                )?;
                if change_atomic > 0 {
                    vec![change_atomic]
                } else {
                    Vec::new()
                }
            } else {
                change_amounts
            };
            for (i, amount) in change_amounts.iter().enumerate() {
                tx.execute(
                    "INSERT OR IGNORE INTO goldcoin_payout_change_outpoints
                        (txid, vout, request_id, amount_atomic, unconfirmed_ancestor_depth)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        txid.as_slice(),
                        (i + 1) as i64,
                        request_id,
                        amount,
                        parent_depth + 1
                    ],
                )?;
            }
        }
        let bstate: RequestState = tx.query_row(
            "SELECT state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        assert_eq!(
            bstate,
            RequestState::SettlementAuthorized,
            "record_goldcoin_payout_broadcast on unexpected bridge_request state"
        );
        tx.execute("UPDATE bridge_requests SET state = 'DestinationSubmitted', destination_txid = ?1 WHERE id = ?2", rusqlite::params![txid.as_slice(), request_id])?;
        log_transition(
            &tx,
            request_id,
            Some(bstate),
            RequestState::DestinationSubmitted,
            now,
            None,
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Full read of an existing `goldcoin_payouts` row — everything
    /// [`crate::goldcoin::payout_recovery`] needs to independently
    /// reconstruct and re-verify the exact plan a stuck payout was
    /// originally built from, without selecting anything new. Distinct
    /// from [`Ledger::get_goldcoin_payout`] (which returns only the
    /// narrower [`GoldcoinPayoutSnapshot`] an attestation signer needs)
    /// so that read's shape/call sites are unaffected by this one.
    pub fn get_goldcoin_payout_full(
        &self,
        request_id: i64,
    ) -> Result<Option<GoldcoinPayoutFull>, LedgerError> {
        let row = self
            .conn
            .query_row(
                "SELECT commitment_hash, payout_atomic, change_atomic, fee_atomic,
                        dest_p2pkh_hash, unsigned_tx_hex, signed_tx_hex, state
                 FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| {
                    let commitment_hash: Vec<u8> = r.get(0)?;
                    let dest_p2pkh_hash: Vec<u8> = r.get(4)?;
                    Ok(GoldcoinPayoutFull {
                        commitment_hash: to_array32(&commitment_hash),
                        payout_atomic: r.get::<_, i64>(1)? as u64,
                        change_atomic: r.get::<_, i64>(2)? as u64,
                        change_outputs: Vec::new(), // filled in below
                        fee_atomic: r.get::<_, i64>(3)? as u64,
                        dest_p2pkh_hash: dest_p2pkh_hash.try_into().unwrap(),
                        unsigned_tx_hex: r.get(5)?,
                        signed_tx_hex: r.get(6)?,
                        state: r.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(LedgerError::from)?;
        let Some(mut payout) = row else {
            return Ok(None);
        };
        payout.change_outputs =
            self.goldcoin_payout_change_outputs(request_id, payout.change_atomic)?;
        Ok(Some(payout))
    }

    /// The deterministic change-output breakdown for `request_id`'s
    /// payout, from `goldcoin_payout_change_outputs` in construction
    /// order — or, if that table has no rows for it (a payout built before
    /// schema v12 introduced fan-out), a single synthesized legacy output
    /// equal to `legacy_change_atomic` (empty if that's `0`). Never
    /// backfills the table itself.
    fn goldcoin_payout_change_outputs(
        &self,
        request_id: i64,
        legacy_change_atomic: u64,
    ) -> Result<Vec<u64>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT amount_atomic FROM goldcoin_payout_change_outputs
             WHERE request_id = ?1 ORDER BY output_order ASC",
        )?;
        let rows: Vec<u64> = stmt
            .query_map([request_id], |r| Ok(r.get::<_, i64>(0)? as u64))?
            .collect::<Result<_, _>>()?;
        if !rows.is_empty() {
            return Ok(rows);
        }
        if legacy_change_atomic > 0 {
            Ok(vec![legacy_change_atomic])
        } else {
            Ok(Vec::new())
        }
    }

    /// The exact inputs an existing payout already reserved, in the exact
    /// order they were built with — never a fresh coin selection (those
    /// UTXOs are no longer `state = 'Available'` and so are structurally
    /// invisible to [`Ledger::available_vault_utxos`] regardless). Fails
    /// closed if any row's backing `vault_utxos` entry no longer reads
    /// `state = 'Reserved'` and `reserved_by = request_id` exactly as
    /// [`Ledger::reserve_vault_utxos`] left it — proof nothing about this
    /// reservation drifted between the original build and now.
    pub fn get_goldcoin_payout_inputs(
        &self,
        request_id: i64,
    ) -> Result<Vec<crate::goldcoin::coin::VaultUtxo>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT i.txid, i.vout, i.amount_atomic, v.script_pubkey_hex, v.state, v.reserved_by
             FROM goldcoin_payout_inputs i
             JOIN vault_utxos v ON v.txid = i.txid AND v.vout = i.vout
             WHERE i.request_id = ?1
             ORDER BY i.input_order ASC",
        )?;
        let rows = stmt
            .query_map([request_id], |r| {
                let txid: Vec<u8> = r.get(0)?;
                let amount_atomic: i64 = r.get(2)?;
                let script_pubkey_hex: String = r.get(3)?;
                let state: String = r.get(4)?;
                let reserved_by: Option<i64> = r.get(5)?;
                Ok((
                    crate::goldcoin::coin::VaultUtxo {
                        txid: to_array32(&txid),
                        vout: r.get(1)?,
                        amount_atomic: amount_atomic as u64,
                        script_pubkey_hex,
                    },
                    state,
                    reserved_by,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.is_empty() {
            return Err(LedgerError::PayoutNotFound(request_id));
        }
        let mut utxos = Vec::with_capacity(rows.len());
        for (utxo, state, reserved_by) in rows {
            if state != "Reserved" || reserved_by != Some(request_id) {
                return Err(LedgerError::VaultUtxoReservationDrifted {
                    request_id,
                    txid: utxo.txid,
                    vout: utxo.vout,
                });
            }
            utxos.push(utxo);
        }
        Ok(utxos)
    }

    /// Updates a `Signed` payout's `signed_tx_hex` in place after an
    /// operator-triggered recovery re-signs it
    /// ([`crate::goldcoin::payout_recovery`]) — never changes `state`
    /// (still `Signed` either way) and never touches any other column, so
    /// this can never advance a payout that a concurrent process has
    /// already moved past `Signed`. Guarded to `state = 'Signed'` for the
    /// same reason [`Ledger::record_goldcoin_payout_signed`] guards to
    /// `state = 'Built'`: a mismatched row count means the precondition
    /// this caller checked has already changed underneath it.
    pub fn record_goldcoin_payout_resigned(
        &mut self,
        request_id: i64,
        signed_tx_hex: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE goldcoin_payouts SET signed_tx_hex = ?1, signed_at = ?2 WHERE request_id = ?3 AND state = 'Signed'",
            rusqlite::params![signed_tx_hex, now, request_id],
        )?;
        if n == 0 {
            return Err(LedgerError::PayoutNotFound(request_id));
        }
        Ok(())
    }

    /// Updates confirmation depth; at `required_depth` transitions
    /// `Broadcast -> Confirmed` and `DestinationSubmitted ->
    /// DestinationConfirmed`. Returns whether that transition fired on
    /// THIS call (so a caller refreshing an already-`Confirmed` payout
    /// can tell a re-poll apart from the actual confirmation event).
    /// Also mirrors the depth into the request's own
    /// `bridge_requests.destination_confirmations` — the column operators
    /// and the read-projections look at — for as long as the destination
    /// leg is live (`DestinationSubmitted`/`DestinationConfirmed`), not
    /// only until the transition. Both writes are monotonic (`<`-guarded)
    /// so a lagging RPC answer can never walk an observed depth
    /// backwards. Idempotent under repeated ticks. `tip_height`
    /// is the Goldcoin chain tip as observed by the caller at the same
    /// moment `confirmations` was read, used to back out the payout's
    /// mined height (`tip_height - confirmations + 1`) — recorded once,
    /// the first time `confirmations > 0`, since it never changes
    /// afterwards. This is the height threaded into
    /// `record_goldcoin_completion`'s attestation message
    /// (`shared::claim::goldcoin_completion_message`), so it must be the
    /// real mined height, not a derived/estimated one.
    pub fn update_goldcoin_payout_confirmations(
        &mut self,
        request_id: i64,
        confirmations: i64,
        tip_height: i64,
        required_depth: i64,
        now: i64,
    ) -> Result<bool, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        tx.execute(
            "UPDATE goldcoin_payouts SET confirmations = ?1 WHERE request_id = ?2 AND state IN ('Broadcast','Confirmed') AND confirmations < ?1",
            rusqlite::params![confirmations, request_id],
        )?;
        tx.execute(
            "UPDATE bridge_requests SET destination_confirmations = ?1
                WHERE id = ?2 AND state IN ('DestinationSubmitted','DestinationConfirmed') AND destination_confirmations < ?1",
            rusqlite::params![confirmations, request_id],
        )?;
        if confirmations > 0 {
            tx.execute(
                "UPDATE goldcoin_payouts SET mined_height = ?1 WHERE request_id = ?2 AND mined_height IS NULL",
                rusqlite::params![tip_height - confirmations + 1, request_id],
            )?;
        }
        let mut transitioned = false;
        if confirmations >= required_depth {
            let n = tx.execute("UPDATE goldcoin_payouts SET state = 'Confirmed' WHERE request_id = ?1 AND state = 'Broadcast'", [request_id])?;
            if n > 0 {
                let bstate: RequestState = tx.query_row(
                    "SELECT state FROM bridge_requests WHERE id = ?1",
                    [request_id],
                    |r| r.get(0),
                )?;
                tx.execute(
                    "UPDATE bridge_requests SET state = 'DestinationConfirmed' WHERE id = ?1",
                    [request_id],
                )?;
                log_transition(
                    &tx,
                    request_id,
                    Some(bstate),
                    RequestState::DestinationConfirmed,
                    now,
                    None,
                    "system",
                )?;
                transitioned = true;
            }
        }
        tx.commit()?;
        Ok(transitioned)
    }

    /// Records that `record_goldcoin_completion` has been submitted to
    /// Solana for this request, carrying the transaction signature.
    /// Deliberately does not touch `bridge_requests.state` or
    /// `goldcoin_payouts.state` — the leg is only truly done once that
    /// submission is confirmed (see [`Ledger::mark_goldcoin_completion_confirmed`]).
    /// Latest-wins: a re-submission (the orchestrator re-sends the
    /// completion when an earlier submission's signature has demonstrably
    /// stopped being observable — dropped transaction / expired
    /// blockhash) REPLACES the recorded signature and timestamp, so the
    /// confirmation poller always tracks the newest in-flight attempt.
    /// The signature is a tracking handle, not the settlement fact — that
    /// fact is only ever established by observing the transaction (or the
    /// obligation's terminal on-chain status) succeed. Idempotent: a
    /// no-op if the request has already reached `Settled`.
    pub fn record_goldcoin_completion_submitted(
        &mut self,
        request_id: i64,
        signature: [u8; 64],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let payout_state: Option<String> = tx
            .query_row(
                "SELECT state FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        match payout_state.as_deref() {
            None => {
                tx.rollback()?;
                return Err(LedgerError::PayoutNotFound(request_id));
            }
            Some("Completed") => {
                tx.rollback()?;
                return Ok(());
            }
            Some("Confirmed") => {}
            Some(other) => {
                panic!("record_goldcoin_completion_submitted on unexpected payout state {other}")
            }
        }
        tx.execute(
            "UPDATE goldcoin_payouts SET onchain_completion_signature = ?1, onchain_completion_submitted_at = ?2
                WHERE request_id = ?3",
            rusqlite::params![signature.as_slice(), now, request_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Confirmed -> Completed`, `DestinationConfirmed -> Settled`, gated
    /// on the request's `record_goldcoin_completion` submission having
    /// been recorded first (constants.md/ADR-0018: the completion fact
    /// must be reconstructable from Solana chain state, so this service
    /// never declares a request `Settled` on the strength of its own
    /// database alone). Moves the amount out of
    /// `reserved_liquidity`/`pending_obligations` into
    /// `settled_liquidity_total` (docs/05-reserve-accounting.md) and
    /// spends the reserved vault UTXOs. Idempotent: a no-op if already
    /// `Settled`.
    pub fn mark_goldcoin_completion_confirmed(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let bstate: RequestState = tx.query_row(
            "SELECT state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        if bstate == RequestState::Settled {
            tx.rollback()?;
            return Ok(());
        }
        assert_eq!(
            bstate,
            RequestState::DestinationConfirmed,
            "mark_goldcoin_completion_confirmed on unexpected bridge_request state"
        );
        let has_submission: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM goldcoin_payouts WHERE request_id = ?1 AND onchain_completion_signature IS NOT NULL",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        if has_submission.is_none() {
            tx.rollback()?;
            return Err(LedgerError::CompletionNotSubmitted(request_id));
        }
        let (amount, fee): (i64, i64) = tx.query_row(
            "SELECT net_destination_atomic, fee_amount_atomic FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        tx.execute("UPDATE goldcoin_payouts SET state = 'Completed', completed_at = ?1, onchain_completed_at = ?1 WHERE request_id = ?2", rusqlite::params![now, request_id])?;
        tx.execute(
            "UPDATE bridge_requests SET state = 'Settled', settled_at = ?1 WHERE id = ?2",
            rusqlite::params![now, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(bstate),
            RequestState::Settled,
            now,
            None,
            "system",
        )?;
        // See the matching comment in `mark_release_confirmed`: keep the
        // cached balance self-consistent with a settlement this service
        // itself caused, so reconciliation never mistakes it for an
        // unexplained (and pause-triggering) breach.
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity - ?1, pending_obligations = pending_obligations - ?1,
                settled_liquidity_total = settled_liquidity_total + ?1, total_reserve_balance = total_reserve_balance - ?1
                WHERE direction = 'GoldcoinReserve'",
            [amount],
        )?;
        // The fee for a SolToGlc settlement is collected on the SOURCE side
        // (Solana) — see the matching comment in `mark_release_confirmed`.
        // Always canonical units regardless of which row it's recorded on
        // (docs/20-bridge-fee.md) — never netted against SolanaReserve's
        // own native-unit balance/reserved/settled columns.
        tx.execute(
            "UPDATE reserve_ledger SET accrued_fees_atomic = accrued_fees_atomic + ?1
                WHERE direction = 'SolanaReserve'",
            [fee],
        )?;
        // `spent_by_txid` = the payout's own broadcast txid: the marker
        // `sync_vault_utxos`'s resurrection rule treats as "spent by a
        // transaction this service signed" — without it, a reorg that
        // briefly reports these outpoints unspent again would resurrect
        // them into the selectable pool while our own signed payout still
        // spends them (2026-08-30 re-review, finding 1).
        tx.execute(
            "UPDATE vault_utxos
             SET state = 'Spent',
                 spent_by_txid = (SELECT p.txid FROM goldcoin_payouts p WHERE p.request_id = ?1)
             WHERE reserved_by = ?1",
            [request_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    // -------------------------------------------------------- audit trail --

    /// Freezes the exact canonical attestation-claim message bytes a
    /// signer group attested to, so an offline audit
    /// ([`crate::ops`]/`glc-audit`) can later re-verify self-consistency
    /// (does the stored hash still match the stored bytes?) and
    /// recompute-from-scalar-fields consistency (does re-deriving the
    /// message from this request's current data still produce the same
    /// bytes?) — not merely that a message is *re-derivable* today, which
    /// says nothing about whether the frozen record was tampered with.
    /// Idempotent: a no-op if a record already exists for
    /// `(request_id, action_type)` (this service only ever attests each
    /// action once per request — see `orchestrator::Orchestrator`).
    pub fn record_attestation(
        &mut self,
        request_id: i64,
        action_type: &str,
        message: &[u8],
        now: i64,
    ) -> Result<(), LedgerError> {
        let message_hash = Sha256::digest(message);
        self.conn.execute(
            "INSERT OR IGNORE INTO attestation_records (request_id, action_type, canonical_message, message_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![request_id, action_type, message, message_hash.as_slice(), now],
        )?;
        Ok(())
    }

    /// SQLite's own consistency check — `"ok"` is the only passing result;
    /// anything else names actual corruption. What `glc-audit` runs first,
    /// before trusting anything else it reads.
    pub fn integrity_check(&self) -> Result<String, LedgerError> {
        self.conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(LedgerError::from)
    }

    /// All frozen attestation records, oldest first — what `glc-audit`
    /// walks to recompute-and-diff every one.
    pub fn all_attestation_records(&self) -> Result<Vec<AttestationRecord>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, request_id, action_type, canonical_message, message_hash, created_at
             FROM attestation_records ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AttestationRecord {
                    id: r.get(0)?,
                    request_id: r.get(1)?,
                    action_type: r.get(2)?,
                    canonical_message: r.get(3)?,
                    message_hash: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Appends a signer-identity audit entry (never key material — see
    /// docs/06-schema.md). Best-effort/observability only: never part of
    /// any settlement-safety invariant, so a failure here must never be
    /// allowed to block the action it's logging — callers should log and
    /// continue on error rather than propagate it into a settlement path.
    pub fn record_signature_grant(
        &mut self,
        action_type: &str,
        identity: &str,
        request_id: Option<i64>,
        severity: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO signature_grant_log (at, action_type, identity, request_id, severity)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![now, action_type, identity, request_id, severity],
        )?;
        Ok(())
    }

    // -------------------------------------------------------- rebalancing --
    //
    // Structurally separate from bridge_requests/settlement accounting
    // (docs/05-reserve-accounting.md, docs/22-production-readiness-review.md
    // P1 "rebalancing"): nothing below ever touches `reserved_liquidity`/
    // `pending_obligations`/`bridge_requests`, only `total_reserve_balance`
    // on the one named reserve, once a request reaches `Confirmed`. This
    // ledger tracks the REQUEST and its approval/execution/audit trail; it
    // never signs, constructs, or broadcasts a real fund-moving transaction
    // — `record_rebalance_executed` only ever records evidence
    // (`tx_reference`) of a transfer an operator already authorized and
    // executed through real custody tooling outside this system.

    /// Creates a new rebalance request in `Proposed`, collecting approvals
    /// from here. `reason` is mandatory (matching every other admin-action
    /// audit trail in this codebase — never a silent/blank justification
    /// for a reserve-balance change).
    #[allow(clippy::too_many_arguments)]
    pub fn propose_rebalance(
        &mut self,
        direction: ReserveDirection,
        kind: RebalanceKind,
        amount_atomic: u64,
        reason: &str,
        requested_by: &str,
        required_approvals: u32,
        now: i64,
    ) -> Result<i64, LedgerError> {
        if amount_atomic == 0 {
            return Err(LedgerError::InvalidRebalanceRequest(
                "amount_atomic must be > 0".to_string(),
            ));
        }
        if required_approvals == 0 {
            return Err(LedgerError::InvalidRebalanceRequest(
                "required_approvals must be > 0".to_string(),
            ));
        }
        if reason.trim().is_empty() {
            return Err(LedgerError::InvalidRebalanceRequest(
                "reason must not be empty".to_string(),
            ));
        }
        if requested_by.trim().is_empty() {
            return Err(LedgerError::InvalidRebalanceRequest(
                "requested_by must not be empty".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        tx.execute(
            "INSERT INTO rebalance_requests
                (direction, kind, amount_atomic, state, reason, requested_by, requested_at,
                 required_approvals, approved_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '[]')",
            rusqlite::params![
                direction,
                kind,
                amount_atomic as i64,
                RebalanceState::Proposed,
                reason,
                requested_by,
                now,
                required_approvals,
            ],
        )?;
        let id = tx.last_insert_rowid();
        log_rebalance_transition(
            &tx,
            id,
            None,
            RebalanceState::Proposed,
            now,
            Some(reason),
            requested_by,
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Records `approver`'s approval. Idempotent per approver (approving
    /// twice does not double-count). Transitions `Proposed -> Approved`
    /// once `required_approvals` distinct identities have approved.
    pub fn approve_rebalance(
        &mut self,
        id: i64,
        approver: &str,
        now: i64,
    ) -> Result<RebalanceApprovalOutcome, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(RebalanceState, i64, String)> = tx
            .query_row(
                "SELECT state, required_approvals, approved_by FROM rebalance_requests WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((state, required, approved_json)) = row else {
            tx.rollback()?;
            return Err(LedgerError::RebalanceNotFound(id));
        };
        if state != RebalanceState::Proposed {
            tx.rollback()?;
            return Err(LedgerError::RebalanceWrongState {
                id,
                expected: RebalanceState::Proposed,
                actual: state,
            });
        }
        let mut approvers: Vec<String> = serde_json::from_str(&approved_json).unwrap_or_default();
        if !approvers.iter().any(|a| a == approver) {
            approvers.push(approver.to_string());
        }
        let approved_json =
            serde_json::to_string(&approvers).expect("Vec<String> always serializes");
        let reached = approvers.len() as u32 >= required as u32;
        if reached {
            tx.execute(
                "UPDATE rebalance_requests SET approved_by = ?1, state = ?2, approved_at = ?3 \
                 WHERE id = ?4",
                rusqlite::params![approved_json, RebalanceState::Approved, now, id],
            )?;
            log_rebalance_transition(
                &tx,
                id,
                Some(RebalanceState::Proposed),
                RebalanceState::Approved,
                now,
                Some(&format!(
                    "approval threshold reached ({}/{required})",
                    approvers.len()
                )),
                approver,
            )?;
        } else {
            tx.execute(
                "UPDATE rebalance_requests SET approved_by = ?1 WHERE id = ?2",
                rusqlite::params![approved_json, id],
            )?;
            log_rebalance_transition(
                &tx,
                id,
                Some(RebalanceState::Proposed),
                RebalanceState::Proposed,
                now,
                Some(&format!("approved ({}/{required})", approvers.len())),
                approver,
            )?;
        }
        tx.commit()?;
        Ok(if reached {
            RebalanceApprovalOutcome::ThresholdReached
        } else {
            RebalanceApprovalOutcome::Recorded {
                approvals: approvers.len() as u32,
                required: required as u32,
            }
        })
    }

    /// `Proposed -> Rejected`. Terminal; requires a note (an approver's
    /// reason for declining).
    pub fn reject_rebalance(
        &mut self,
        id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.close_rebalance(
            id,
            &[RebalanceState::Proposed],
            RebalanceState::Rejected,
            note,
            actor,
            now,
        )
    }

    /// `Proposed|Approved -> Cancelled`. Terminal; requires a note.
    pub fn cancel_rebalance(
        &mut self,
        id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.close_rebalance(
            id,
            &[RebalanceState::Proposed, RebalanceState::Approved],
            RebalanceState::Cancelled,
            note,
            actor,
            now,
        )
    }

    fn close_rebalance(
        &mut self,
        id: i64,
        allowed_from: &[RebalanceState],
        to: RebalanceState,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if note.trim().is_empty() {
            return Err(LedgerError::InvalidRebalanceRequest(
                "a note is required".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<RebalanceState> = tx
            .query_row(
                "SELECT state FROM rebalance_requests WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::RebalanceNotFound(id));
        };
        if !allowed_from.contains(&state) {
            tx.rollback()?;
            return Err(LedgerError::RebalanceWrongState {
                id,
                expected: allowed_from[0],
                actual: state,
            });
        }
        tx.execute(
            "UPDATE rebalance_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![to, id],
        )?;
        log_rebalance_transition(&tx, id, Some(state), to, now, Some(note), actor)?;
        tx.commit()?;
        Ok(())
    }

    /// `Approved -> Executed`: records evidence of a real, out-of-band
    /// transfer an operator already authorized and executed — never
    /// broadcasts or signs anything itself. `tx_reference` (a Goldcoin
    /// txid or Solana signature, as text) is UNIQUE across every rebalance
    /// request ever recorded (schema `ux_rebalance_tx_reference`), so
    /// recording the same real transfer twice is a structural, DB-enforced
    /// rejection — the replay guard for this action.
    pub fn record_rebalance_executed(
        &mut self,
        id: i64,
        tx_reference: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if tx_reference.trim().is_empty() {
            return Err(LedgerError::InvalidRebalanceRequest(
                "tx_reference must not be empty".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<RebalanceState> = tx
            .query_row(
                "SELECT state FROM rebalance_requests WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::RebalanceNotFound(id));
        };
        if state != RebalanceState::Approved {
            tx.rollback()?;
            return Err(LedgerError::RebalanceWrongState {
                id,
                expected: RebalanceState::Approved,
                actual: state,
            });
        }
        // A duplicate tx_reference fails here on the UNIQUE index —
        // propagated as LedgerError::Sqlite, fail-closed by construction,
        // not by an application-level check that could be forgotten.
        tx.execute(
            "UPDATE rebalance_requests SET state = ?1, tx_reference = ?2, executed_at = ?3 \
             WHERE id = ?4",
            rusqlite::params![RebalanceState::Executed, tx_reference, now, id],
        )?;
        log_rebalance_transition(
            &tx,
            id,
            Some(RebalanceState::Approved),
            RebalanceState::Executed,
            now,
            Some(&format!("tx_reference={tx_reference}")),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Executed -> Confirmed`: an operator (or an automated reconciliation
    /// cross-check) independently confirms the real balance change the
    /// executed transfer produced. Adjusts `reserve_ledger.
    /// total_reserve_balance` by the observed amount in the same
    /// transaction — mirroring `mark_release_confirmed`'s "keep the cache
    /// self-consistent with a change this service itself caused" rationale
    /// (docs/14-phase6-checkpoint.md bug 3) — so the very next
    /// reconciliation tick sees an already-explained balance rather than
    /// misclassifying an operator-authorized rebalance as an unexplained
    /// breach.
    pub fn confirm_rebalance(
        &mut self,
        id: i64,
        observed_amount_atomic: u64,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(ReserveDirection, RebalanceKind, RebalanceState)> = tx
            .query_row(
                "SELECT direction, kind, state FROM rebalance_requests WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((direction, kind, state)) = row else {
            tx.rollback()?;
            return Err(LedgerError::RebalanceNotFound(id));
        };
        if state != RebalanceState::Executed {
            tx.rollback()?;
            return Err(LedgerError::RebalanceWrongState {
                id,
                expected: RebalanceState::Executed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE rebalance_requests SET state = ?1, observed_amount_atomic = ?2, \
             confirmed_at = ?3 WHERE id = ?4",
            rusqlite::params![
                RebalanceState::Confirmed,
                observed_amount_atomic as i64,
                now,
                id
            ],
        )?;
        let delta: i64 = match kind {
            RebalanceKind::Deposit => observed_amount_atomic as i64,
            RebalanceKind::Withdraw => -(observed_amount_atomic as i64),
        };
        tx.execute(
            "UPDATE reserve_ledger SET total_reserve_balance = total_reserve_balance + ?1, \
             balance_refreshed_at = ?2 WHERE direction = ?3",
            rusqlite::params![delta, now, direction],
        )?;
        log_rebalance_transition(
            &tx,
            id,
            Some(RebalanceState::Executed),
            RebalanceState::Confirmed,
            now,
            Some(&format!("observed_amount_atomic={observed_amount_atomic}")),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Executed -> Failed`: the recorded transfer's expected effect was
    /// never confirmed (or was confirmed wrong) — routed to a state
    /// requiring operator resolution rather than left `Executed` forever,
    /// same discipline as `RequestState::ManualReview`. Deliberately does
    /// NOT touch `total_reserve_balance` — nothing was confirmed to have
    /// happened, so there is nothing to explain away.
    pub fn fail_rebalance(
        &mut self,
        id: i64,
        reason: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if reason.trim().is_empty() {
            return Err(LedgerError::InvalidRebalanceRequest(
                "a failure reason is required".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<RebalanceState> = tx
            .query_row(
                "SELECT state FROM rebalance_requests WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::RebalanceNotFound(id));
        };
        if state != RebalanceState::Executed {
            tx.rollback()?;
            return Err(LedgerError::RebalanceWrongState {
                id,
                expected: RebalanceState::Executed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE rebalance_requests SET state = ?1, failure_reason = ?2 WHERE id = ?3",
            rusqlite::params![RebalanceState::Failed, reason, id],
        )?;
        log_rebalance_transition(
            &tx,
            id,
            Some(RebalanceState::Executed),
            RebalanceState::Failed,
            now,
            Some(reason),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_rebalance(&self, id: i64) -> Result<Option<RebalanceRequest>, LedgerError> {
        self.conn
            .query_row(REBALANCE_SELECT_BY_ID, [id], row_to_rebalance)
            .optional()
            .map_err(LedgerError::from)
    }

    /// All rebalance requests for `direction` (or every direction, if
    /// `None`), optionally restricted to still-open ones
    /// (`RebalanceState::is_open`), newest first.
    pub fn list_rebalances(
        &self,
        direction: Option<ReserveDirection>,
        open_only: bool,
    ) -> Result<Vec<RebalanceRequest>, LedgerError> {
        let mut stmt = self.conn.prepare(REBALANCE_SELECT_ALL)?;
        let rows = stmt
            .query_map([], row_to_rebalance)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter(|r| direction.is_none() || direction == Some(r.direction))
            .filter(|r| !open_only || r.state.is_open())
            .collect())
    }

    pub fn rebalance_state_log(&self, id: i64) -> Result<Vec<RebalanceStateLogEntry>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT from_state, to_state, at, reason, actor FROM rebalance_state_log \
             WHERE rebalance_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ----------------------------------------------- custody transitions --
    //
    // Generic key-rotation / vault-sweep tooling
    // (docs/22-production-readiness-review.md P1 "key rotation / vault
    // sweep tooling"). Shares the rebalancing state machine's core
    // discipline — this ledger only ever tracks the REQUEST, its
    // approvals, and its audit trail; `record_custody_transition_executed`
    // only ever records evidence (`tx_reference`) of a rotation/sweep an
    // operator already authorized and executed through real custody
    // tooling outside this system — plus two extra gates rebalancing
    // doesn't need: the new identity must be independently verified
    // before any approval can begin, and the relevant reserve(s) must
    // already be paused before execution evidence can be recorded.

    /// Creates a new custody transition in `Proposed`. `new_threshold`
    /// only applies to `CustodyTransitionKind::GoldcoinVaultSweep`; must
    /// be `None` for `AttestationKeyRotation`, which has no threshold
    /// concept.
    #[allow(clippy::too_many_arguments)]
    pub fn propose_custody_transition(
        &mut self,
        kind: CustodyTransitionKind,
        old_identities: &[String],
        new_identities: &[String],
        new_threshold: Option<u32>,
        reason: &str,
        requested_by: &str,
        required_approvals: u32,
        now: i64,
    ) -> Result<i64, LedgerError> {
        if new_identities.is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "new_identities must not be empty".to_string(),
            ));
        }
        if kind == CustodyTransitionKind::AttestationKeyRotation && new_threshold.is_some() {
            return Err(LedgerError::InvalidCustodyTransition(
                "new_threshold does not apply to AttestationKeyRotation".to_string(),
            ));
        }
        if required_approvals == 0 {
            return Err(LedgerError::InvalidCustodyTransition(
                "required_approvals must be > 0".to_string(),
            ));
        }
        if reason.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "reason must not be empty".to_string(),
            ));
        }
        if requested_by.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "requested_by must not be empty".to_string(),
            ));
        }
        let old_json =
            serde_json::to_string(old_identities).expect("Vec<String> always serializes");
        let new_json =
            serde_json::to_string(new_identities).expect("Vec<String> always serializes");
        let tx = write_tx(&mut self.conn)?;
        tx.execute(
            "INSERT INTO custody_transitions
                (kind, state, old_identities, new_identities, new_threshold, reason,
                 requested_by, requested_at, required_approvals, approved_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, '[]')",
            rusqlite::params![
                kind,
                CustodyTransitionState::Proposed,
                old_json,
                new_json,
                new_threshold,
                reason,
                requested_by,
                now,
                required_approvals,
            ],
        )?;
        let id = tx.last_insert_rowid();
        log_custody_transition(
            &tx,
            id,
            None,
            CustodyTransitionState::Proposed,
            now,
            Some(reason),
            requested_by,
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// `Proposed -> IdentityVerified`: records that `verifier`
    /// independently checked the claimed new identity (e.g. a signed
    /// challenge against the claimed pubkey/vault descriptor) before any
    /// approval may begin. Required gate, not advisory — `approve_
    /// custody_transition` rejects anything still in `Proposed`.
    pub fn verify_new_identity(
        &mut self,
        id: i64,
        verifier: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if verifier.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "verifier must not be empty".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<CustodyTransitionState> = tx
            .query_row(
                "SELECT state FROM custody_transitions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if state != CustodyTransitionState::Proposed {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: CustodyTransitionState::Proposed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE custody_transitions SET state = ?1, identity_verified_by = ?2, \
             identity_verified_at = ?3 WHERE id = ?4",
            rusqlite::params![CustodyTransitionState::IdentityVerified, verifier, now, id],
        )?;
        log_custody_transition(
            &tx,
            id,
            Some(CustodyTransitionState::Proposed),
            CustodyTransitionState::IdentityVerified,
            now,
            Some("new identity independently verified"),
            verifier,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records `approver`'s approval. Idempotent per approver. Only valid
    /// once the new identity has been verified (`IdentityVerified`).
    /// Transitions `IdentityVerified -> Approved` once
    /// `required_approvals` distinct identities have approved.
    pub fn approve_custody_transition(
        &mut self,
        id: i64,
        approver: &str,
        now: i64,
    ) -> Result<CustodyApprovalOutcome, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(CustodyTransitionState, i64, String)> = tx
            .query_row(
                "SELECT state, required_approvals, approved_by FROM custody_transitions \
                 WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((state, required, approved_json)) = row else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if state != CustodyTransitionState::IdentityVerified {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: CustodyTransitionState::IdentityVerified,
                actual: state,
            });
        }
        let mut approvers: Vec<String> = serde_json::from_str(&approved_json).unwrap_or_default();
        if !approvers.iter().any(|a| a == approver) {
            approvers.push(approver.to_string());
        }
        let approved_json =
            serde_json::to_string(&approvers).expect("Vec<String> always serializes");
        let reached = approvers.len() as u32 >= required as u32;
        if reached {
            tx.execute(
                "UPDATE custody_transitions SET approved_by = ?1, state = ?2, approved_at = ?3 \
                 WHERE id = ?4",
                rusqlite::params![approved_json, CustodyTransitionState::Approved, now, id],
            )?;
            log_custody_transition(
                &tx,
                id,
                Some(CustodyTransitionState::IdentityVerified),
                CustodyTransitionState::Approved,
                now,
                Some(&format!(
                    "approval threshold reached ({}/{required})",
                    approvers.len()
                )),
                approver,
            )?;
        } else {
            tx.execute(
                "UPDATE custody_transitions SET approved_by = ?1 WHERE id = ?2",
                rusqlite::params![approved_json, id],
            )?;
            log_custody_transition(
                &tx,
                id,
                Some(CustodyTransitionState::IdentityVerified),
                CustodyTransitionState::IdentityVerified,
                now,
                Some(&format!("approved ({}/{required})", approvers.len())),
                approver,
            )?;
        }
        tx.commit()?;
        Ok(if reached {
            CustodyApprovalOutcome::ThresholdReached
        } else {
            CustodyApprovalOutcome::Recorded {
                approvals: approvers.len() as u32,
                required: required as u32,
            }
        })
    }

    /// `Proposed|IdentityVerified -> Rejected`. Terminal; requires a note.
    pub fn reject_custody_transition(
        &mut self,
        id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.close_custody_transition(
            id,
            &[
                CustodyTransitionState::Proposed,
                CustodyTransitionState::IdentityVerified,
            ],
            CustodyTransitionState::Rejected,
            note,
            actor,
            now,
        )
    }

    /// `Proposed|IdentityVerified|Approved -> Cancelled`. Terminal;
    /// requires a note.
    pub fn cancel_custody_transition(
        &mut self,
        id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.close_custody_transition(
            id,
            &[
                CustodyTransitionState::Proposed,
                CustodyTransitionState::IdentityVerified,
                CustodyTransitionState::Approved,
            ],
            CustodyTransitionState::Cancelled,
            note,
            actor,
            now,
        )
    }

    fn close_custody_transition(
        &mut self,
        id: i64,
        allowed_from: &[CustodyTransitionState],
        to: CustodyTransitionState,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if note.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "a note is required".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<CustodyTransitionState> = tx
            .query_row(
                "SELECT state FROM custody_transitions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if !allowed_from.contains(&state) {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: allowed_from[0],
                actual: state,
            });
        }
        tx.execute(
            "UPDATE custody_transitions SET state = ?1 WHERE id = ?2",
            rusqlite::params![to, id],
        )?;
        log_custody_transition(&tx, id, Some(state), to, now, Some(note), actor)?;
        tx.commit()?;
        Ok(())
    }

    /// `Approved -> Executed`: records evidence of a real, out-of-band
    /// rotation/sweep an operator already authorized and executed —
    /// never performs the rotation/sweep itself. Enforces the "pause
    /// requirements" invariant: the relevant reserve(s) must already be
    /// paused (`GoldcoinReserve` for `GoldcoinVaultSweep`; BOTH reserves
    /// for `AttestationKeyRotation`, since attestation authorizes both
    /// bridge directions) — an actual precondition, not documentation.
    /// `tx_reference` is UNIQUE across every custody transition ever
    /// recorded (schema `ux_custody_transitions_tx_reference`), the
    /// structural replay guard for this action.
    pub fn record_custody_transition_executed(
        &mut self,
        id: i64,
        tx_reference: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if tx_reference.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "tx_reference must not be empty".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(CustodyTransitionKind, CustodyTransitionState)> = tx
            .query_row(
                "SELECT kind, state FROM custody_transitions WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((kind, state)) = row else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if state != CustodyTransitionState::Approved {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: CustodyTransitionState::Approved,
                actual: state,
            });
        }
        let required_paused: &[ReserveDirection] = match kind {
            CustodyTransitionKind::GoldcoinVaultSweep => &[ReserveDirection::GoldcoinReserve],
            CustodyTransitionKind::AttestationKeyRotation => &[
                ReserveDirection::GoldcoinReserve,
                ReserveDirection::SolanaReserve,
            ],
        };
        for direction in required_paused {
            let paused: i64 = tx.query_row(
                "SELECT paused FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| r.get(0),
            )?;
            if paused == 0 {
                tx.rollback()?;
                return Err(LedgerError::CustodyTransitionRequiresPause {
                    id,
                    direction: *direction,
                });
            }
        }
        // A duplicate tx_reference fails here on the UNIQUE index —
        // propagated as LedgerError::Sqlite, fail-closed by construction.
        tx.execute(
            "UPDATE custody_transitions SET state = ?1, tx_reference = ?2, executed_at = ?3 \
             WHERE id = ?4",
            rusqlite::params![CustodyTransitionState::Executed, tx_reference, now, id],
        )?;
        log_custody_transition(
            &tx,
            id,
            Some(CustodyTransitionState::Approved),
            CustodyTransitionState::Executed,
            now,
            Some(&format!("tx_reference={tx_reference}")),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Executed -> Confirmed`: an operator independently confirms the
    /// new custody identity is active and correct post-transition —
    /// terminal success. Deliberately does not touch reserve pause state
    /// or balance; unpausing after a rotation/sweep is a distinct,
    /// deliberate operator action (`set_paused`), never automatic.
    pub fn confirm_custody_transition(
        &mut self,
        id: i64,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let state: Option<CustodyTransitionState> = tx
            .query_row(
                "SELECT state FROM custody_transitions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if state != CustodyTransitionState::Executed {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: CustodyTransitionState::Executed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE custody_transitions SET state = ?1, confirmed_at = ?2 WHERE id = ?3",
            rusqlite::params![CustodyTransitionState::Confirmed, now, id],
        )?;
        log_custody_transition(
            &tx,
            id,
            Some(CustodyTransitionState::Executed),
            CustodyTransitionState::Confirmed,
            now,
            Some("new custody identity confirmed active"),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Executed -> Failed`: the recorded rotation/sweep's expected new
    /// identity was never confirmed (or confirmed wrong) — requires
    /// operator resolution rather than left `Executed` forever.
    pub fn fail_custody_transition(
        &mut self,
        id: i64,
        reason: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if reason.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "a failure reason is required".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<CustodyTransitionState> = tx
            .query_row(
                "SELECT state FROM custody_transitions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if state != CustodyTransitionState::Executed {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: CustodyTransitionState::Executed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE custody_transitions SET state = ?1, failure_reason = ?2 WHERE id = ?3",
            rusqlite::params![CustodyTransitionState::Failed, reason, id],
        )?;
        log_custody_transition(
            &tx,
            id,
            Some(CustodyTransitionState::Executed),
            CustodyTransitionState::Failed,
            now,
            Some(reason),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Failed -> RolledBack`: records that a failed transition's
    /// real-world effect was reverted back to the old identity, out of
    /// band. Only ever an audit marker of a rollback already performed —
    /// this service never performs the rollback itself.
    pub fn rollback_custody_transition(
        &mut self,
        id: i64,
        reason: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if reason.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "a rollback reason is required".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<CustodyTransitionState> = tx
            .query_row(
                "SELECT state FROM custody_transitions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if state != CustodyTransitionState::Failed {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: CustodyTransitionState::Failed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE custody_transitions SET state = ?1, rolled_back_at = ?2, rollback_reason = ?3 \
             WHERE id = ?4",
            rusqlite::params![CustodyTransitionState::RolledBack, now, reason, id],
        )?;
        log_custody_transition(
            &tx,
            id,
            Some(CustodyTransitionState::Failed),
            CustodyTransitionState::RolledBack,
            now,
            Some(reason),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_custody_transition(
        &self,
        id: i64,
    ) -> Result<Option<CustodyTransition>, LedgerError> {
        self.conn
            .query_row(CUSTODY_SELECT_BY_ID, [id], row_to_custody_transition)
            .optional()
            .map_err(LedgerError::from)
    }

    /// All custody transitions for `kind` (or every kind, if `None`),
    /// optionally restricted to still-open ones
    /// (`CustodyTransitionState::is_open`), newest first.
    pub fn list_custody_transitions(
        &self,
        kind: Option<CustodyTransitionKind>,
        open_only: bool,
    ) -> Result<Vec<CustodyTransition>, LedgerError> {
        let mut stmt = self.conn.prepare(CUSTODY_SELECT_ALL)?;
        let rows = stmt
            .query_map([], row_to_custody_transition)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter(|r| kind.is_none() || kind == Some(r.kind))
            .filter(|r| !open_only || r.state.is_open())
            .collect())
    }

    pub fn custody_transition_state_log(
        &self,
        id: i64,
    ) -> Result<Vec<CustodyTransitionStateLogEntry>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT from_state, to_state, at, reason, actor FROM custody_transition_state_log \
             WHERE transition_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// One row of `reconciliation_findings`. See
/// [`Ledger::reconciliation_findings_page`].
#[derive(Debug, Clone)]
pub struct ReconciliationFindingRow {
    pub id: i64,
    pub direction: ReserveDirection,
    pub detected_at: i64,
    pub expected: i64,
    pub observed: i64,
    pub delta: i64,
    pub classification: String,
    pub auto_paused: bool,
}

/// One row of `bridge_request_state_log`, joined to its request's
/// direction. See [`Ledger::explorer_events_page`].
#[derive(Debug, Clone)]
pub struct ExplorerEventRow {
    pub id: i64,
    pub request_id: i64,
    pub direction: Direction,
    pub from_state: Option<RequestState>,
    pub to_state: RequestState,
    pub at: i64,
    pub reason: Option<String>,
}

/// See [`Ledger::all_attestation_records`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestationRecord {
    pub id: i64,
    pub request_id: i64,
    pub action_type: String,
    pub canonical_message: Vec<u8>,
    pub message_hash: Vec<u8>,
    pub created_at: i64,
}

const SELECT_SOLANA_REFUND: &str =
    "SELECT request_id, obligation_index, nonce, amount_solana_atomic, requester, \
    destination_token_account, reserve_mint, token_program, manual_review_reason, note, \
    created_by, state, attestation_epoch, refund_signature, recent_blockhash, created_at, \
    broadcast_at, confirmed_at FROM solana_refunds";

fn row_to_solana_refund(r: &rusqlite::Row) -> rusqlite::Result<SolanaRefund> {
    let requester_vec: Vec<u8> = r.get(4)?;
    let destination_vec: Vec<u8> = r.get(5)?;
    let mint_vec: Vec<u8> = r.get(6)?;
    let program_vec: Vec<u8> = r.get(7)?;
    let attestation_epoch: Option<i64> = r.get(12)?;
    Ok(SolanaRefund {
        request_id: r.get(0)?,
        obligation_index: r.get::<_, i64>(1)? as u64,
        nonce: r.get::<_, i64>(2)? as u64,
        amount_solana_atomic: r.get::<_, i64>(3)? as u64,
        requester: requester_vec.try_into().unwrap(),
        destination_token_account: destination_vec.try_into().unwrap(),
        reserve_mint: mint_vec.try_into().unwrap(),
        token_program: program_vec.try_into().unwrap(),
        manual_review_reason: r.get(8)?,
        note: r.get(9)?,
        created_by: r.get(10)?,
        state: r.get(11)?,
        attestation_epoch: attestation_epoch.map(|e| e as u64),
        refund_signature: r.get(13)?,
        recent_blockhash: r.get(14)?,
        created_at: r.get(15)?,
        broadcast_at: r.get(16)?,
        confirmed_at: r.get(17)?,
    })
}

const SELECT_REQUEST_PREFIX: &str =
    "SELECT id, direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic, \
    net_amount_atomic, net_destination_atomic, recipient, requester, \
    created_at, reserved_at, reservation_expires_at, source_txid, source_vout, \
    source_obligation_index, source_block_height, source_block_hash, source_confirmations, \
    source_finalized_at, failure_reason, manual_review_note, source_chain, source_contract, \
    source_wallet, auto_resume_hold_note, auto_resume_hold_until, \
    manual_review_disposition, hold_reason, held_by, hold_started_at, review_after, \
    operator_decision, operator_decision_at, operator_note \
    FROM bridge_requests";
const SELECT_REQUEST: &str =
    "SELECT id, direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic, \
    net_amount_atomic, net_destination_atomic, recipient, requester, \
    created_at, reserved_at, reservation_expires_at, source_txid, source_vout, \
    source_obligation_index, source_block_height, source_block_hash, source_confirmations, \
    source_finalized_at, failure_reason, manual_review_note, source_chain, source_contract, \
    source_wallet, auto_resume_hold_note, auto_resume_hold_until, \
    manual_review_disposition, hold_reason, held_by, hold_started_at, review_after, \
    operator_decision, operator_decision_at, operator_note \
    FROM bridge_requests WHERE id = ?1";

fn row_to_request(r: &rusqlite::Row) -> rusqlite::Result<BridgeRequest> {
    let recipient_vec: Vec<u8> = r.get(8)?;
    let requester_vec: Option<Vec<u8>> = r.get(9)?;
    let source_txid_vec: Option<Vec<u8>> = r.get(13)?;
    let source_block_hash_vec: Option<Vec<u8>> = r.get(17)?;
    Ok(BridgeRequest {
        id: r.get(0)?,
        direction: r.get(1)?,
        state: r.get(2)?,
        gross_amount_atomic: r.get::<_, i64>(3)? as u64,
        fee_bps: r.get::<_, i64>(4)? as u64,
        fee_amount_atomic: r.get::<_, i64>(5)? as u64,
        net_amount_atomic: r.get::<_, i64>(6)? as u64,
        net_destination_atomic: r.get::<_, i64>(7)? as u64,
        recipient: recipient_vec,
        requester: requester_vec.map(|v| to_array32(&v)),
        created_at: r.get(10)?,
        reserved_at: r.get(11)?,
        reservation_expires_at: r.get(12)?,
        source_txid: source_txid_vec.map(|v| to_array32(&v)),
        source_vout: r.get::<_, Option<i64>>(14)?.map(|v| v as u32),
        source_obligation_index: r.get::<_, Option<i64>>(15)?.map(|v| v as u64),
        source_block_height: r.get(16)?,
        source_block_hash: source_block_hash_vec.map(|v| to_array32(&v)),
        source_confirmations: r.get(18)?,
        source_finalized_at: r.get(19)?,
        failure_reason: r.get(20)?,
        manual_review_note: r.get(21)?,
        source_chain: r.get(22)?,
        source_contract: r.get(23)?,
        source_wallet: r.get(24)?,
        auto_resume_hold_note: r.get(25)?,
        auto_resume_hold_until: r.get(26)?,
        manual_review_disposition: r.get(27)?,
        hold_reason: r.get(28)?,
        held_by: r.get(29)?,
        hold_started_at: r.get(30)?,
        review_after: r.get(31)?,
        operator_decision: r.get(32)?,
        operator_decision_at: r.get(33)?,
        operator_note: r.get(34)?,
    })
}

/// How long a `Broadcast` split may sit flagged `missing_inputs_since`
/// before the accounting terms STOP explaining its phantom chunk outputs
/// (2026-08-31 production-readiness review, B2): long enough for a reorg
/// race to settle (a transiently refused re-broadcast clears the flag on
/// the next successful probe), short enough that a genuine
/// conflicting-spend loss surfaces as the reconciliation breach it is —
/// instead of being silently padded over forever — within minutes.
pub const SPLIT_MISSING_INPUTS_GRACE_SECS: i64 = 600;

/// The SQL condition under which a split's chunk outputs still count as
/// this service's own explainable in-flight value: a live/confirmed
/// split that is NOT past the missing-inputs grace window. `?N` is the
/// caller's `now` parameter index.
fn split_chunks_still_explainable(now_param: &str) -> String {
    format!(
        "s.state IN ('Broadcast','Confirmed') \
         AND NOT (s.missing_inputs_since IS NOT NULL \
                  AND s.missing_inputs_since + {SPLIT_MISSING_INPUTS_GRACE_SECS} <= {now_param})"
    )
}

/// THE one live-split claim-exclusion predicate (2026-08-31
/// production-readiness review, cleanup finding): from the instant a
/// split's `Built` row commits until it terminally resolves, its source
/// outpoint is claimed — invisible to payout selection, unreservable,
/// and never counted as spendable liquidity by admission backpressure or
/// pool health. Every query over spendable `vault_utxos` MUST include
/// this predicate; it drifted twice during review when copy-pasted, so
/// it is generated from exactly one place. `outer` is the SQL
/// name/alias of the `vault_utxos` row being tested.
/// Excludes any UTXO still backing a Goldcoin-sourced deposit that has
/// not yet reached `SourceFinalized` (`DepositObserved`/`Confirming`).
///
/// A vault payout spending such a UTXO before the deposit's own
/// confirmation depth is reached would strand the request that deposit
/// funded — see `Ledger::mark_glc_deposit_spent_before_finalized`'s
/// fail-closed backstop, which exists because that already happened once
/// before this exclusion did. Ordinary vault change/deposit UTXOs
/// unrelated to any bridge request are unaffected.
///
/// It covers BOTH Goldcoin-sourced directions
/// ([`Direction::SOURCE_IS_GOLDCOIN_SQL_IN`]), and that breadth is the
/// point: a `GlcToRhn` deposit sits at a derived vault address exactly
/// like a `GlcToSol` one, is picked up by the same `list_unspent` sweep,
/// and lands in the same `vault_utxos` table. Excluding only `GlcToSol`
/// would leave a still-confirming `GlcToRhn` deposit selectable as
/// change for an unrelated Goldcoin payout — the identical failure, one
/// direction over.
///
/// `outer` is the alias of the `vault_utxos` row being filtered, so the
/// same fragment serves both the aliased (`v`) and unaliased forms.
fn unfinalized_goldcoin_deposit_exclusion(outer: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM bridge_requests b \
         WHERE b.direction IN {sources} \
           AND b.source_txid = {outer}.txid AND b.source_vout = {outer}.vout \
           AND b.state IN ('DepositObserved', 'Confirming'))",
        sources = Direction::SOURCE_IS_GOLDCOIN_SQL_IN
    )
}

fn live_split_claim_exclusion(outer: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM vault_utxo_splits s \
         WHERE s.source_txid = {outer}.txid AND s.source_vout = {outer}.vout \
           AND s.state IN ('Built','Signed','Broadcast'))"
    )
}

fn to_array32(v: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = v.len().min(32);
    out[..n].copy_from_slice(&v[..n]);
    out
}

fn map_goldcoin_refund_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<GoldcoinRefundRow> {
    fn blob32(v: Vec<u8>) -> [u8; 32] {
        let mut out = [0u8; 32];
        let n = v.len().min(32);
        out[..n].copy_from_slice(&v[..n]);
        out
    }
    fn blob20(v: Vec<u8>) -> [u8; 20] {
        let mut out = [0u8; 20];
        let n = v.len().min(20);
        out[..n].copy_from_slice(&v[..n]);
        out
    }
    let txid: Option<Vec<u8>> = r.get(12)?;
    Ok(GoldcoinRefundRow {
        request_id: r.get(0)?,
        source_txid: blob32(r.get(1)?),
        source_vout: r.get(2)?,
        observed_amount_atomic: r.get::<_, i64>(3)? as u64,
        source_input_txid: blob32(r.get(4)?),
        source_input_vout: r.get(5)?,
        refund_dest_p2pkh_hash: blob20(r.get(6)?),
        refund_dest_address: r.get(7)?,
        refund_amount_atomic: r.get::<_, i64>(8)? as u64,
        fee_atomic: r.get::<_, i64>(9)? as u64,
        unsigned_tx_hex: r.get(10)?,
        signed_tx_hex: r.get(11)?,
        txid: txid.map(blob32),
        confirmations: r.get(13)?,
        state: r.get(14)?,
        manual_review_reason: r.get(15)?,
        note: r.get(16)?,
        created_by: r.get(17)?,
        built_at: r.get(18)?,
        signed_at: r.get(19)?,
        broadcast_at: r.get(20)?,
        refunded_at: r.get(21)?,
        reservation_released: r.get::<_, i64>(22)? == 1,
    })
}

fn log_transition(
    conn: &Connection,
    request_id: i64,
    from: Option<RequestState>,
    to: RequestState,
    at: i64,
    reason: Option<&str>,
    actor: &str,
) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT INTO bridge_request_state_log (request_id, from_state, to_state, at, reason, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            request_id,
            from.map(|s| s.as_str()),
            to.as_str(),
            at,
            reason,
            actor
        ],
    )?;
    Ok(())
}

fn log_rebalance_transition(
    conn: &Connection,
    rebalance_id: i64,
    from: Option<RebalanceState>,
    to: RebalanceState,
    at: i64,
    reason: Option<&str>,
    actor: &str,
) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT INTO rebalance_state_log (rebalance_id, from_state, to_state, at, reason, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            rebalance_id,
            from.map(|s| s.as_str()),
            to.as_str(),
            at,
            reason,
            actor
        ],
    )?;
    Ok(())
}

const REBALANCE_SELECT_BY_ID: &str = "SELECT id, direction, kind, amount_atomic, state, reason, \
     requested_by, requested_at, required_approvals, approved_by, approved_at, tx_reference, \
     executed_at, observed_amount_atomic, confirmed_at, failure_reason \
     FROM rebalance_requests WHERE id = ?1";

const REBALANCE_SELECT_ALL: &str = "SELECT id, direction, kind, amount_atomic, state, reason, \
     requested_by, requested_at, required_approvals, approved_by, approved_at, tx_reference, \
     executed_at, observed_amount_atomic, confirmed_at, failure_reason \
     FROM rebalance_requests ORDER BY id DESC";

fn row_to_rebalance(r: &rusqlite::Row) -> rusqlite::Result<RebalanceRequest> {
    let approved_by_json: String = r.get(9)?;
    let approved_by: Vec<String> = serde_json::from_str(&approved_by_json).unwrap_or_default();
    Ok(RebalanceRequest {
        id: r.get(0)?,
        direction: r.get(1)?,
        kind: r.get(2)?,
        amount_atomic: r.get::<_, i64>(3)? as u64,
        state: r.get(4)?,
        reason: r.get(5)?,
        requested_by: r.get(6)?,
        requested_at: r.get(7)?,
        required_approvals: r.get::<_, i64>(8)? as u32,
        approved_by,
        approved_at: r.get(10)?,
        tx_reference: r.get(11)?,
        executed_at: r.get(12)?,
        observed_amount_atomic: r.get::<_, Option<i64>>(13)?.map(|v| v as u64),
        confirmed_at: r.get(14)?,
        failure_reason: r.get(15)?,
    })
}

fn log_custody_transition(
    conn: &Connection,
    transition_id: i64,
    from: Option<CustodyTransitionState>,
    to: CustodyTransitionState,
    at: i64,
    reason: Option<&str>,
    actor: &str,
) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT INTO custody_transition_state_log
            (transition_id, from_state, to_state, at, reason, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            transition_id,
            from.map(|s| s.as_str()),
            to.as_str(),
            at,
            reason,
            actor
        ],
    )?;
    Ok(())
}

const CUSTODY_SELECT_BY_ID: &str = "SELECT id, kind, state, old_identities, new_identities, \
     new_threshold, reason, requested_by, requested_at, required_approvals, approved_by, \
     approved_at, identity_verified_by, identity_verified_at, tx_reference, executed_at, \
     confirmed_at, failure_reason, rolled_back_at, rollback_reason \
     FROM custody_transitions WHERE id = ?1";

const CUSTODY_SELECT_ALL: &str = "SELECT id, kind, state, old_identities, new_identities, \
     new_threshold, reason, requested_by, requested_at, required_approvals, approved_by, \
     approved_at, identity_verified_by, identity_verified_at, tx_reference, executed_at, \
     confirmed_at, failure_reason, rolled_back_at, rollback_reason \
     FROM custody_transitions ORDER BY id DESC";

fn row_to_custody_transition(r: &rusqlite::Row) -> rusqlite::Result<CustodyTransition> {
    let old_identities_json: String = r.get(3)?;
    let old_identities: Vec<String> =
        serde_json::from_str(&old_identities_json).unwrap_or_default();
    let new_identities_json: String = r.get(4)?;
    let new_identities: Vec<String> =
        serde_json::from_str(&new_identities_json).unwrap_or_default();
    let approved_by_json: String = r.get(10)?;
    let approved_by: Vec<String> = serde_json::from_str(&approved_by_json).unwrap_or_default();
    Ok(CustodyTransition {
        id: r.get(0)?,
        kind: r.get(1)?,
        state: r.get(2)?,
        old_identities,
        new_identities,
        new_threshold: r.get::<_, Option<i64>>(5)?.map(|v| v as u32),
        reason: r.get(6)?,
        requested_by: r.get(7)?,
        requested_at: r.get(8)?,
        required_approvals: r.get::<_, i64>(9)? as u32,
        approved_by,
        approved_at: r.get(11)?,
        identity_verified_by: r.get(12)?,
        identity_verified_at: r.get(13)?,
        tx_reference: r.get(14)?,
        executed_at: r.get(15)?,
        confirmed_at: r.get(16)?,
        failure_reason: r.get(17)?,
        rolled_back_at: r.get(18)?,
        rollback_reason: r.get(19)?,
    })
}

#[cfg(test)]
mod tests;
