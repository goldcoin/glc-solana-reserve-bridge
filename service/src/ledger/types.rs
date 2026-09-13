//! Core types for the reserve ledger (docs/04-state-machines.md,
//! docs/05-reserve-accounting.md).

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

/// Bridge settlement direction — the axis every reserve mutation, every
/// state-machine transition and every `bridge_requests` row is keyed by.
///
/// # Why there are exactly six, and why that took three phases
///
/// [`crate::routes::Route`] has six variants and so, now, does this. For
/// two phases the two Solana<->Robinhood routes deliberately had NO
/// direction: `Route::as_direction` was partial, every value-moving
/// function required a `Direction`, and a route without one could not
/// reach any of them. That firewall was the right posture while no
/// settlement machinery existed for those routes.
///
/// It is lifted here, not weakened: `SolToRhn` and `RhnToSol` now have
/// exactly the machinery their two halves already had — the Solana
/// deposit indexer and the Robinhood payout engine for one, the Robinhood
/// deposit indexer and the Solana reserve release for the other. Nothing
/// new moves value; two existing legs are joined. Enablement is a
/// separate axis entirely ([`crate::routes::RouteGate`]) and both routes
/// ship closed on every gate.
///
/// Adding a variant is deliberately expensive: it is a compile error at
/// every exhaustive `match` in this service, which is exactly the review
/// the change deserves — and exactly the review this widening received.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Goldcoin deposit confirmed -> Solana reserve release.
    GlcToSol,
    /// Solana deposit confirmed -> Goldcoin reserve release.
    SolToGlc,
    /// Goldcoin deposit confirmed -> Robinhood reserve payout
    /// (`executePayout` on the custody contract).
    GlcToRhn,
    /// Robinhood deposit finalized -> Goldcoin reserve payout, followed
    /// by `executeSettlement` on the custody contract. The settlement is
    /// the LAST step, never the first — see
    /// `crate::robinhood::settlement`.
    RhnToGlc,
    /// Solana deposit finalized -> Robinhood reserve payout
    /// (`executePayout` on the custody contract, route `0x03`), followed
    /// by `record_goldcoin_completion` on Solana closing the obligation —
    /// the same close-out `SolToGlc` performs, with the Robinhood payout
    /// transaction as the recorded payout id.
    SolToRhn,
    /// Robinhood deposit finalized -> Solana reserve release
    /// (`release_from_reserve`, keyed by the deposit's own transaction
    /// hash and log index), followed by `executeSettlement` on the
    /// custody contract. As for `RhnToGlc`, the settlement is the LAST
    /// step, never the first.
    RhnToSol,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::GlcToSol => "GlcToSol",
            Direction::SolToGlc => "SolToGlc",
            Direction::GlcToRhn => "GlcToRhn",
            Direction::RhnToGlc => "RhnToGlc",
            Direction::SolToRhn => "SolToRhn",
            Direction::RhnToSol => "RhnToSol",
        }
    }

    /// The reserve a settlement in this direction draws down. Capacity for
    /// a direction depends only on the DESTINATION reserve
    /// (docs/05-reserve-accounting.md).
    pub fn destination_reserve(self) -> ReserveDirection {
        match self {
            Direction::GlcToSol | Direction::RhnToSol => ReserveDirection::SolanaReserve,
            Direction::SolToGlc | Direction::RhnToGlc => ReserveDirection::GoldcoinReserve,
            Direction::GlcToRhn | Direction::SolToRhn => ReserveDirection::RobinhoodReserve,
        }
    }

    /// The reserve this direction's SOURCE deposit landed in — the side
    /// the bridge fee is withheld on (docs/20-bridge-fee.md: "the fee
    /// remains on the source side where it was collected"), and
    /// therefore the row whose `accrued_fees_atomic` a settlement
    /// credits. Always a different reserve from
    /// [`Direction::destination_reserve`]; the two are never netted.
    pub fn source_reserve(self) -> ReserveDirection {
        match self {
            Direction::GlcToSol | Direction::GlcToRhn => ReserveDirection::GoldcoinReserve,
            Direction::SolToGlc | Direction::SolToRhn => ReserveDirection::SolanaReserve,
            Direction::RhnToGlc | Direction::RhnToSol => ReserveDirection::RobinhoodReserve,
        }
    }

    /// Whether this direction's SOURCE leg is a Goldcoin L1 deposit —
    /// i.e. whether it uses the per-request deposit address, the UTXO
    /// indexer and the Goldcoin confirmation policy.
    ///
    /// Exists so the several places that ask "is this a Goldcoin-funded
    /// request?" ask it once, here, rather than each spelling out a
    /// two-arm match that a fifth direction would silently fall through.
    pub fn source_is_goldcoin(self) -> bool {
        matches!(self, Direction::GlcToSol | Direction::GlcToRhn)
    }

    /// The SQL `IN` list naming exactly the directions
    /// [`Direction::source_is_goldcoin`] admits, for the several ledger
    /// queries that must ask the same question in SQL rather than in
    /// Rust (the deposit-script lookup, the watched-address enumeration,
    /// the reorg sweeps, and the coin-selection exclusion that keeps a
    /// still-confirming deposit out of the spendable pool).
    ///
    /// It lives HERE, beside the predicate it mirrors, because the two
    /// drifting apart is silent and expensive: a SQL list that forgot a
    /// direction would let a real deposit fund a payout it was never
    /// meant to, or let a payout spend a UTXO still backing an
    /// unfinalized deposit. `sql_in_matches_source_is_goldcoin` in
    /// `ledger::tests` pins them together, so adding a fifth direction
    /// fails a test rather than quietly changing behaviour.
    pub const SOURCE_IS_GOLDCOIN_SQL_IN: &'static str = "('GlcToSol','GlcToRhn')";

    /// Whether this direction's DESTINATION leg is a Goldcoin L1 payout —
    /// i.e. whether it is settled by building and broadcasting a vault
    /// transaction (`goldcoin::payout`).
    pub fn destination_is_goldcoin(self) -> bool {
        matches!(self, Direction::SolToGlc | Direction::RhnToGlc)
    }

    /// The SQL `IN` list naming exactly the directions
    /// [`Direction::destination_is_goldcoin`] admits — the INBOUND-to-
    /// Goldcoin routes, and therefore the exact set of rows that can
    /// consume a Goldcoin L1 address's rolling-24h payout window
    /// (`Ledger::goldcoin_recipient_rate_limited_until`).
    ///
    /// The destination limit is deliberately GLOBAL across these routes,
    /// not per-route: the rule is "one Goldcoin L1 address may receive at
    /// most one bridge payout in a rolling 24-hour window", and an
    /// address that just took a `SolToGlc` payout has received one
    /// regardless of which chain funds the next attempt. A per-route
    /// spelling would have let the same address collect one payout per
    /// inbound chain per day, which is the bypass this list closes — the
    /// exact counterpart, on the destination side, of what the
    /// source-wallet limit closes on the source side.
    ///
    /// Lives HERE beside the predicate it mirrors for the same reason
    /// [`Direction::SOURCE_IS_GOLDCOIN_SQL_IN`] does, and is pinned to it
    /// by `destination_is_goldcoin_sql_in_matches_the_rust_predicate` in
    /// `ledger::tests`: a fifth inbound-to-Goldcoin direction missing from
    /// this literal would silently get a rate-limit window of its own.
    pub const DESTINATION_IS_GOLDCOIN_SQL_IN: &'static str = "('SolToGlc','RhnToGlc')";

    /// Whether either leg of this direction is the Robinhood custody
    /// contract — i.e. whether settling it requires an EVM transaction.
    pub fn touches_robinhood(self) -> bool {
        matches!(
            self,
            Direction::GlcToRhn | Direction::RhnToGlc | Direction::SolToRhn | Direction::RhnToSol
        )
    }

    /// Whether this direction's SOURCE leg is a Solana `deposit_to_reserve`
    /// obligation — observed by `solana::indexer`, identified by a
    /// `WithdrawalObligation` index, and closed out on Solana by
    /// `record_goldcoin_completion` once the destination payout is final.
    pub fn source_is_solana(self) -> bool {
        matches!(self, Direction::SolToGlc | Direction::SolToRhn)
    }

    /// Whether this direction's DESTINATION leg is a Solana reserve
    /// release — settled by `release_from_reserve` under a threshold
    /// attestation, with the `DepositClaim` PDA as the on-chain replay
    /// guard.
    pub fn destination_is_solana(self) -> bool {
        matches!(self, Direction::GlcToSol | Direction::RhnToSol)
    }

    /// Whether this direction's SOURCE leg is a deposit into the Robinhood
    /// custody contract — observed by `robinhood::indexer`, identified by
    /// a contract-local obligation index, and closed out on Robinhood by
    /// `executeSettlement` once the destination payout is final.
    pub fn source_is_robinhood(self) -> bool {
        matches!(self, Direction::RhnToGlc | Direction::RhnToSol)
    }

    /// Whether this direction's DESTINATION leg is a Robinhood payout
    /// (`executePayout` on the custody contract).
    pub fn destination_is_robinhood(self) -> bool {
        matches!(self, Direction::GlcToRhn | Direction::SolToRhn)
    }

    /// Whether a confirmed Solana `release_from_reserve` IS this
    /// direction's settlement (`GlcToSol`: the Goldcoin deposit needs no
    /// close-out of its own), as opposed to its destination leg only
    /// (`RhnToSol`: the Robinhood obligation must still be settled
    /// on-chain, and only after the release is final).
    pub fn settles_on_release(self) -> bool {
        self == Direction::GlcToSol
    }

    /// Whether a finalized Robinhood `executePayout` IS this direction's
    /// settlement (`GlcToRhn`), as opposed to its destination leg only
    /// (`SolToRhn`: the Solana `WithdrawalObligation` is still `Pending`
    /// and is closed by `record_goldcoin_completion` afterwards, exactly
    /// as `SolToGlc` closes it once its Goldcoin payout confirmed).
    pub fn settles_on_payout(self) -> bool {
        self == Direction::GlcToRhn
    }

    /// Whether the DESTINATION reserve's cached `total_reserve_balance`
    /// is debited when this direction reaches `DestinationConfirmed` —
    /// the moment the destination leg is final — rather than at
    /// `Settled`.
    ///
    /// Two accounting shapes exist, and the reserve decides which:
    ///
    /// - The Goldcoin vault is UTXO-reconciled, so a Goldcoin payout is
    ///   debited only at the request's close-out
    ///   (`Ledger::mark_goldcoin_completion_confirmed` for `SolToGlc`,
    ///   `Ledger::mark_robinhood_settlement_confirmed` for `RhnToGlc`),
    ///   and a request sitting in `DestinationConfirmed` still carries
    ///   its reservation on the book.
    /// - The Solana token account and the Robinhood custody contract are
    ///   compared against a LIVE balance read, so a release or a payout
    ///   is debited the moment it is final
    ///   (`Ledger::mark_release_confirmed`,
    ///   `Ledger::mark_robinhood_payout_settled`) — whether that instant
    ///   is also `Settled` (`GlcToSol`, `GlcToRhn`) or the request then
    ///   waits in `DestinationConfirmed` for its source-side close-out
    ///   (`RhnToSol`, `SolToRhn`).
    ///
    /// The one consumer is `Ledger::pending_destination_settlement_amount`:
    /// a `DestinationConfirmed` row may explain an observed balance drop
    /// only while the book has NOT yet been debited for it. Counting it
    /// after the debit would explain the same value twice — once as the
    /// cached balance's own decrement and once as "in flight" — and mask
    /// a genuine loss of that size until the close-out landed. Pinned
    /// against the ledger's actual behaviour, direction by direction, by
    /// `reconciliation::tests::the_destination_debit_predicate_matches_the_ledger_for_every_direction`.
    pub fn destination_debited_at_destination_confirmed(self) -> bool {
        match self {
            Direction::GlcToSol
            | Direction::GlcToRhn
            | Direction::SolToRhn
            | Direction::RhnToSol => true,
            Direction::SolToGlc | Direction::RhnToGlc => false,
        }
    }

    /// The chain this direction's SOURCE deposit is made on — the chain
    /// whose spelling `BridgeRequest::source_wallet` uses, and the one
    /// the source-wallet 24-hour window is scoped to.
    pub fn source_chain(self) -> crate::routes::Chain {
        crate::routes::Route::from(self).source_chain()
    }

    /// The chain this direction pays out on — the chain whose spelling
    /// `BridgeRequest::recipient` uses, and the one the destination-
    /// wallet 24-hour window is scoped to.
    pub fn destination_chain(self) -> crate::routes::Chain {
        crate::routes::Route::from(self).destination_chain()
    }

    /// The six directions, for exhaustive iteration in tests and
    /// operator listings.
    pub const ALL: [Direction; 6] = [
        Direction::GlcToSol,
        Direction::SolToGlc,
        Direction::GlcToRhn,
        Direction::RhnToGlc,
        Direction::SolToRhn,
        Direction::RhnToSol,
    ];
}

impl std::str::FromStr for Direction {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "GlcToSol" => Ok(Direction::GlcToSol),
            "SolToGlc" => Ok(Direction::SolToGlc),
            "GlcToRhn" => Ok(Direction::GlcToRhn),
            "RhnToGlc" => Ok(Direction::RhnToGlc),
            "SolToRhn" => Ok(Direction::SolToRhn),
            "RhnToSol" => Ok(Direction::RhnToSol),
            other => Err(format!("unknown direction {other:?}")),
        }
    }
}

impl ToSql for Direction {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for Direction {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// Which physical reserve a quantity belongs to (docs/05-reserve-accounting.md).
///
/// # Three separate reserves, never netted
///
/// Each names real value sitting on one specific chain, under one
/// specific custody arrangement. They are accounted independently and no
/// code path adds, subtracts or compares across them: a healthy Goldcoin
/// vault says nothing about whether the Robinhood custody contract can
/// honour a payout, and treating a total as fungible would let a shortfall
/// on one chain be masked by a surplus on another.
///
/// # The Robinhood reserve's UNIT
///
/// Every reserve row's monetary columns are CANONICAL 8-decimal units,
/// `RobinhoodReserve` included — not Robinhood's native 18 decimals. At
/// 18 decimals one whole GLC is 10^18 and the `INTEGER` column would
/// overflow on a single real transfer. Nothing is lost: the two units are
/// related by an exact factor of 10^10 and every amount crossing the
/// boundary must be an exact multiple of it, enforced on both sides (the
/// contract's `_requireCanonicalAmount`, and
/// `RobinhoodAtomic::to_canonical`). See `schema::apply_v23`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReserveDirection {
    GoldcoinReserve,
    SolanaReserve,
    /// GLC held by the `GlcRobinhoodBridge` custody contract on Robinhood
    /// Network. Accounted in canonical units; see the type docs.
    RobinhoodReserve,
}

impl ReserveDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            ReserveDirection::GoldcoinReserve => "GoldcoinReserve",
            ReserveDirection::SolanaReserve => "SolanaReserve",
            ReserveDirection::RobinhoodReserve => "RobinhoodReserve",
        }
    }

    pub const ALL: [ReserveDirection; 3] = [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
        ReserveDirection::RobinhoodReserve,
    ];
}

impl std::str::FromStr for ReserveDirection {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "GoldcoinReserve" => Ok(ReserveDirection::GoldcoinReserve),
            "SolanaReserve" => Ok(ReserveDirection::SolanaReserve),
            "RobinhoodReserve" => Ok(ReserveDirection::RobinhoodReserve),
            other => Err(format!("unknown reserve direction {other:?}")),
        }
    }
}

impl ToSql for ReserveDirection {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for ReserveDirection {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// Which CHAIN a bridge request's SOURCE leg lives on — the first
/// component of a request's durable, chain-qualified source identity
/// (`(source_chain, source_contract, source_obligation_index)`, schema
/// v21).
///
/// # Why this exists
///
/// Until v21 the replay guard for an obligation-indexed source was a
/// single GLOBAL unique index on `bridge_requests.
/// source_obligation_index`. That was correct while exactly one chain
/// could ever produce an obligation index (Solana), and becomes wrong the
/// moment a second contract-local, monotonically-increasing counter
/// exists: obligation 0 on one chain and obligation 0 on another are two
/// entirely different deposits, and a global index would silently reject
/// the second as an already-handled duplicate — folding a real, already
/// irreversible deposit as a no-op. See `schema::apply_v21`.
///
/// # Why a closed TEXT discriminant and not a numeric chain id
///
/// Every other closed enum this schema stores (`direction`, `state`,
/// `kind`, reserve `direction`) is a `TEXT` column with a `CHECK ... IN
/// (...)` constraint, readable in any `sqlite3` session and enforced by
/// the database rather than by convention; this follows that same
/// discipline. It is deliberately NOT a display label and never rendered
/// as one — the API/UI name routes and networks their own way — and it is
/// deliberately NOT the (future) on-chain `PROTOCOL_CHAIN_ID` wire value,
/// which is network-qualified (mainnet/testnet as distinct ids). A ledger
/// database belongs to exactly one deployment on exactly one network, so
/// network qualification would add nothing this column can enforce while
/// forcing the v21 backfill to GUESS which network historical rows came
/// from — a guess no data in the database can settle. When the EVM
/// contract ships, its event's `srcChain` is mapped onto this
/// discriminant by an explicit, tested function; the mapping is where
/// network qualification belongs, not here.
///
/// The durable, cryptographic half of the identity is
/// `source_contract` — for Solana the deployed program id
/// (`glc_reserve_bridge_shared::PROGRAM_ID_BYTES`), for an EVM bridge the
/// deployed contract address. That is what makes "the same obligation
/// index under a successor contract" a distinct row rather than a
/// collision.
/// The `source_contract` recorded for every Solana obligation that
/// PREDATES schema v21 — an explicit "this program identity was never
/// captured", never a claim about which program it was.
///
/// # Why a sentinel rather than the current program id
///
/// `source_contract` is meant to be durable source identity, so it must
/// not assert something that may be false. Nothing in a pre-v21 ledger
/// records which Solana program issued a given obligation: the program id
/// is a COMPILE-TIME constant (`glc_reserve_bridge_shared::
/// PROGRAM_ID_BYTES`), it appears in no `bridge_requests` column, in no
/// config field, and in no indexer-state row. And it has genuinely
/// changed: that constant has held three values, and for the 2026-08-19
/// to 2026-08-20 window the shipped build compiled in an id that is now
/// permanently denylisted (`glc-mainnet-bootstrap`'s
/// `RETIRED_PROGRAM_IDS`; docs/22-production-readiness-review.md P0-6).
/// Stamping today's id onto those rows would manufacture evidence.
///
/// # The evidence that DOES survive, and why it is not used here
///
/// One relation does freeze a program id per row: `attestation_records.
/// canonical_message` bytes `[17..49]`, for every message family
/// (`shared::claim` — the shared 57-byte prefix). But an attestation
/// record only exists once a request reached its attestation step, so a
/// `SolToGlc` obligation sitting in `SourceFinalized`, `ManualReview`, or
/// refunded has none. The identity is therefore recoverable for SOME
/// historical rows and not others — which is exactly the case option (A)
/// of the Phase B brief rules out ("if it cannot be reconstructed
/// reliably for every historical row"). Splitting the column into "real
/// id for rows that happened to attest, sentinel for the rest" would
/// encode a request's LIFECYCLE STAGE into its source identity, which is
/// worse than one honest marker. Nothing is lost by not copying it: the
/// evidence stays exactly where it is, and an auditor can still read the
/// real id straight out of `attestation_records` for any row that has one.
///
/// # Why this exact value
///
/// A Solana program id is exactly 32 bytes and an EVM contract address
/// exactly 20; this is 25 ASCII bytes, so it can never be mistaken for
/// either by length alone, and it reads as itself in a plain `sqlite3`
/// dump instead of looking like an opaque key. It is a permanent
/// constant: changing it would rewrite the identity of historical rows,
/// which is the opposite of what this column is for.
///
/// # What still guards these rows
///
/// Legacy rows do NOT get a weaker replay guard. Because their true
/// contract is unknown, an obligation index they hold could in principle
/// belong to the program running today, so `ux_bridge_requests_solana_
/// obligation` keeps the pre-v21 promise intact — an obligation index is
/// unique across ALL Solana rows, legacy and current alike, exactly as
/// the global index it replaced guaranteed. That guard is scoped
/// `WHERE source_chain = 'solana'`, so it does not touch Robinhood, whose
/// contract-qualified identity is unaffected: obligation N under a
/// Robinhood v1 contract and under its successor remain distinct rows.
///
/// New rows written from v21 onward never carry this value — every fold
/// records the real, exact program id at fold time.
pub const LEGACY_SOLANA_SOURCE_CONTRACT: &[u8] = b"GLC_LEGACY_SOLANA_PRE_V21";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceChain {
    /// A Goldcoin UTXO deposit. Has no contract identity at all (the
    /// source is an outpoint, guarded by `ux_bridge_requests_glc_source`),
    /// and therefore never carries an obligation index.
    Goldcoin,
    /// The `glc-reserve-bridge` Anchor program on Solana. Obligation
    /// indexes are local to that program's deployed address.
    Solana,
    /// The (not yet deployed, not yet enabled) `GlcRobinhoodBridge` EVM
    /// contract. Present in the vocabulary from v21 on so that enabling
    /// the route later needs no further schema migration; nothing in this
    /// binary constructs it on any production path yet.
    Robinhood,
}

impl SourceChain {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceChain::Goldcoin => "goldcoin",
            SourceChain::Solana => "solana",
            SourceChain::Robinhood => "robinhood",
        }
    }

    /// Whether this chain identifies its deposits by a contract-local
    /// obligation index (and therefore MUST carry a `source_contract`),
    /// or by a transaction outpoint.
    pub fn has_contract_identity(self) -> bool {
        match self {
            SourceChain::Goldcoin => false,
            SourceChain::Solana | SourceChain::Robinhood => true,
        }
    }
}

impl std::str::FromStr for SourceChain {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "goldcoin" => Ok(SourceChain::Goldcoin),
            "solana" => Ok(SourceChain::Solana),
            "robinhood" => Ok(SourceChain::Robinhood),
            other => Err(format!("unknown source chain {other:?}")),
        }
    }
}

impl ToSql for SourceChain {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for SourceChain {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// Bridge-request lifecycle state (docs/04-state-machines.md). This phase's
/// code (chain plumbing + ledger, no signing client yet) only ever produces
/// states up to and including `SourceFinalized`, plus the error states
/// reachable before that point (`Expired`, `Cancelled`, `Reorged`,
/// `ManualReview`). `SettlementAuthorized` onward is a later phase's work
/// (attestation signing clients / orchestrator) — the states are defined
/// here in full because they are part of one continuous state machine, not
/// because this phase reaches them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    LiquidityReserved,
    AwaitingDeposit,
    DepositObserved,
    Confirming,
    SourceFinalized,
    SettlementAuthorized,
    DestinationSubmitted,
    DestinationConfirmed,
    Settled,
    Expired,
    Cancelled,
    Reorged,
    InsufficientReserveAtSettlement,
    DestinationSubmissionFailed,
    ManualReview,
    Failed,
    /// A `SolToGlc` refund lifecycle has begun for this request
    /// (`Ledger::begin_solana_refund`): a `solana_refunds` row exists, the
    /// on-chain refund transaction has NOT been broadcast yet. From this
    /// state on the request is permanently ineligible for resume and for
    /// any Goldcoin payout — the refund lifecycle is one-way
    /// (`RefundPending -> RefundBroadcast -> Refunded`), never back to
    /// `ManualReview`.
    RefundPending,
    /// The refund's `rebalance_withdraw` transaction has been signed and
    /// recorded (and broadcast, or is about to be — the record is written
    /// BEFORE the send so a crash between the two is recoverable from the
    /// deterministic refund nonce, never by building a second transfer).
    RefundBroadcast,
    /// Terminal: the refund transaction confirmed at `finalized`
    /// commitment and the deposited amount was returned to the original
    /// depositor's own token account. Like `Settled`, nothing ever
    /// transitions out of this state.
    Refunded,
}

impl RequestState {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestState::LiquidityReserved => "LiquidityReserved",
            RequestState::AwaitingDeposit => "AwaitingDeposit",
            RequestState::DepositObserved => "DepositObserved",
            RequestState::Confirming => "Confirming",
            RequestState::SourceFinalized => "SourceFinalized",
            RequestState::SettlementAuthorized => "SettlementAuthorized",
            RequestState::DestinationSubmitted => "DestinationSubmitted",
            RequestState::DestinationConfirmed => "DestinationConfirmed",
            RequestState::Settled => "Settled",
            RequestState::Expired => "Expired",
            RequestState::Cancelled => "Cancelled",
            RequestState::Reorged => "Reorged",
            RequestState::InsufficientReserveAtSettlement => "InsufficientReserveAtSettlement",
            RequestState::DestinationSubmissionFailed => "DestinationSubmissionFailed",
            RequestState::ManualReview => "ManualReview",
            RequestState::Failed => "Failed",
            RequestState::RefundPending => "RefundPending",
            RequestState::RefundBroadcast => "RefundBroadcast",
            RequestState::Refunded => "Refunded",
        }
    }

    /// Non-terminal states whose reserved amount still counts against
    /// `reserved_liquidity` (docs/05-reserve-accounting.md). The refund
    /// states (`RefundPending`/`RefundBroadcast`/`Refunded`) are
    /// deliberately NOT here: a refundable request was fold-parked in
    /// `ManualReview` before any reservation was ever applied
    /// (`Ledger::begin_solana_refund` proves this per-request rather than
    /// assuming it), so no refund state ever holds Goldcoin-side
    /// liquidity.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            RequestState::LiquidityReserved
                | RequestState::AwaitingDeposit
                | RequestState::DepositObserved
                | RequestState::Confirming
                | RequestState::SourceFinalized
                | RequestState::SettlementAuthorized
                | RequestState::DestinationSubmitted
        )
    }

    /// True for the three refund-lifecycle states. A request in any of
    /// them (or with a `solana_refunds` row at all — the stronger check
    /// [`super::Ledger::resume_manual_review_sol_to_glc`] performs) can
    /// never be resumed and never receive a Goldcoin payout.
    pub fn is_refund_lifecycle(self) -> bool {
        matches!(
            self,
            RequestState::RefundPending | RequestState::RefundBroadcast | RequestState::Refunded
        )
    }
}

impl std::str::FromStr for RequestState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "LiquidityReserved" => RequestState::LiquidityReserved,
            "AwaitingDeposit" => RequestState::AwaitingDeposit,
            "DepositObserved" => RequestState::DepositObserved,
            "Confirming" => RequestState::Confirming,
            "SourceFinalized" => RequestState::SourceFinalized,
            "SettlementAuthorized" => RequestState::SettlementAuthorized,
            "DestinationSubmitted" => RequestState::DestinationSubmitted,
            "DestinationConfirmed" => RequestState::DestinationConfirmed,
            "Settled" => RequestState::Settled,
            "Expired" => RequestState::Expired,
            "Cancelled" => RequestState::Cancelled,
            "Reorged" => RequestState::Reorged,
            "InsufficientReserveAtSettlement" => RequestState::InsufficientReserveAtSettlement,
            "DestinationSubmissionFailed" => RequestState::DestinationSubmissionFailed,
            "ManualReview" => RequestState::ManualReview,
            "Failed" => RequestState::Failed,
            "RefundPending" => RequestState::RefundPending,
            "RefundBroadcast" => RequestState::RefundBroadcast,
            "Refunded" => RequestState::Refunded,
            other => return Err(format!("unknown request state {other:?}")),
        })
    }
}

impl ToSql for RequestState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for RequestState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// A caller-supplied "my activity" address filter for the public
/// `GET /transfers` listing ([`Ledger::transfers_page`]).
///
/// # Why this is a chain-tagged enum rather than a byte blob
///
/// The four routes carry the caller's own address in four different
/// columns, in two different widths, on two different chains. A single
/// untagged `Vec<u8>` filter could match a 20-byte EVM address against a
/// column that holds Solana pubkeys (or a Goldcoin address's ASCII bytes)
/// purely by coincidence of length, and there would be nothing in the
/// type to stop it. Tagging the chain at parse time means the SQL can
/// restrict each variant to the directions and columns where that chain's
/// addresses actually live, so a cross-chain match is not merely unlikely
/// — it is not expressible.
///
/// The two variants are also structurally unconfusable on the wire: a
/// base58 Solana pubkey can never begin with `0`, because `0` is not in
/// the base58 alphabet, and an EVM address must begin with exactly `0x`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferAddressFilter {
    /// A 32-byte Solana pubkey. Matches `GlcToSol.recipient` (the
    /// destination the caller chose) and `SolToGlc.requester` (the
    /// depositor this service's Solana indexer observed on-chain).
    Solana([u8; 32]),
    /// A 20-byte EVM account address on Robinhood Network. Matches
    /// `GlcToRhn.recipient` (the payout destination the caller chose) and,
    /// for `RhnToGlc`, the folded observation's own `depositor` — the
    /// wallet the custody contract recorded when the deposit landed. That
    /// one is NOT a `bridge_requests` column: `requester` is a fixed
    /// `[u8; 32]` Solana pubkey and a Robinhood fold deliberately leaves
    /// it `NULL`, so the depositor is read back through
    /// `robinhood_deposit_observations.folded_request_id`.
    Evm([u8; 20]),
}

/// A row of `bridge_requests`.
///
/// `recipient` is variable-length, NOT a fixed 32 bytes: for `GlcToSol` it
/// is a 32-byte Solana pubkey, but for `SolToGlc` it is an opaque ASCII
/// Goldcoin address (up to 64 bytes, same `MAX_GLC_ADDRESS_LEN` convention
/// as the on-chain `WithdrawalObligation.glc_address` — see
/// `programs/glc-reserve-bridge/src/constants.rs`). A fixed `[u8; 32]` here
/// would silently truncate a real Goldcoin address; this was caught during
/// implementation of the Solana-side fold and fixed before it shipped (see
/// IMPLEMENTATION_LOG.md).
#[derive(Debug, Clone)]
pub struct BridgeRequest {
    pub id: i64,
    pub direction: Direction,
    pub state: RequestState,
    /// What the user declared/deposited, in the ledger's canonical
    /// accounting unit (8 decimals — `amount_conversion::CanonicalAtomic`;
    /// docs/20-bridge-fee.md). NOT what actually settles — see
    /// [`BridgeRequest::net_amount_atomic`].
    pub gross_amount_atomic: u64,
    /// The fee rate actually applied to this request, in basis points —
    /// the fee-POLICY SNAPSHOT taken at creation/fold time
    /// (this request's ROUTE's configured rate as of that moment —
    /// `fees::RouteFees`), immutable historical accounting thereafter.
    /// Every settlement/attestation/recovery path validates and settles
    /// the request at THIS rate, not whatever the config says now
    /// (`amount_conversion::verify_fee_breakdown`), so an in-flight
    /// request survives a fee-rate change. The stored fee and net must
    /// reconcile EXACTLY against this rate and the stored gross, and the
    /// settlement is built from the freshly recomputed figures rather
    /// than the stored ones — docs/20-bridge-fee.md's fee-bypass
    /// protection.
    pub fee_bps: u64,
    /// Canonical units. `gross_amount_atomic == fee_amount_atomic +
    /// net_amount_atomic` always holds (`amount_conversion::compute_fee`).
    pub fee_amount_atomic: u64,
    /// Canonical units — the real-world GLC entitlement actually delivered
    /// (destination payout before chain-specific unit conversion).
    pub net_amount_atomic: u64,
    /// Same net entitlement as [`BridgeRequest::net_amount_atomic`], but in
    /// the DESTINATION reserve's own native chain unit — the amount
    /// actually reserved/settled against `reserve_ledger`'s capacity
    /// counters and, for `GlcToSol`, the exact amount
    /// `release_from_reserve` transfers on Solana.
    pub net_destination_atomic: u64,
    pub recipient: Vec<u8>,
    pub requester: Option<[u8; 32]>,
    /// The wallet that funded (or, for a not-yet-funded Goldcoin-sourced
    /// request, DECLARED it will fund) this request's source deposit,
    /// spelled the way the source chain spells it — the identity the
    /// rolling-24h source-wallet window is keyed on
    /// (`Ledger::wallet_window_blocker_created_at`; schema v28). A
    /// 32-byte pubkey for a Solana-sourced request (equal to
    /// `requester`), the 20-byte recorded depositor for a
    /// Robinhood-sourced one, the funding address's text (or raw prevout
    /// script, when it is not a standard address) for a Goldcoin-sourced
    /// one. `None` when the source is not known yet, or was never
    /// recorded (rows that predate v28 on the Goldcoin-sourced routes).
    pub source_wallet: Option<Vec<u8>>,
    pub created_at: i64,
    pub reserved_at: Option<i64>,
    pub reservation_expires_at: Option<i64>,
    /// Which chain this request's SOURCE leg lives on. Together with
    /// [`BridgeRequest::source_contract`] and
    /// [`BridgeRequest::source_obligation_index`] this is the request's
    /// durable, chain-qualified source identity (schema v21) — the thing
    /// the replay guard is keyed on, so that obligation N under one
    /// contract can never be mistaken for obligation N under another.
    pub source_chain: SourceChain,
    /// The deployed contract/program whose LOCAL obligation counter
    /// produced [`BridgeRequest::source_obligation_index`], as raw
    /// identity bytes: the 32-byte Solana program id
    /// (`glc_reserve_bridge_shared::PROGRAM_ID_BYTES`), or a 20-byte EVM
    /// contract address. `None` exactly when the source chain has no
    /// contract identity at all (Goldcoin, whose source is an outpoint) —
    /// a schema `CHECK` enforces that correspondence in both directions.
    pub source_contract: Option<Vec<u8>>,
    pub source_txid: Option<[u8; 32]>,
    pub source_vout: Option<u32>,
    pub source_obligation_index: Option<u64>,
    pub source_block_height: Option<i64>,
    pub source_block_hash: Option<[u8; 32]>,
    pub source_confirmations: i64,
    pub source_finalized_at: Option<i64>,
    pub failure_reason: Option<String>,
    pub manual_review_note: Option<String>,
    /// Operator-placed auto-resume hold (schema v29). `Some` means the
    /// daemon's automatic ManualReview recovery skips this row and every
    /// resume entry point refuses it until `clear_manual_review_hold`;
    /// refund tooling ignores it. Never set by a fold — a new row is
    /// always unheld.
    pub auto_resume_hold_note: Option<String>,
    /// Informational companion to the hold: when the operator intends to
    /// act on the row (unix seconds). Has no effect on the daemon.
    pub auto_resume_hold_until: Option<i64>,
}

/// The full gross/fee/net breakdown for one new bridge request, as the
/// caller (`api.rs` for `GlcToSol`, `solana::indexer` for `SolToGlc`) must
/// compute it via `amount_conversion::compute_fee` before calling
/// [`super::Ledger::create_request`]/[`super::Ledger::fold_sol_deposit`] —
/// the ledger itself never computes a conversion or a fee; it only stores
/// and enforces capacity against what it's given (docs/20-bridge-fee.md).
/// All fields are canonical EXCEPT `net_destination_atomic` — see
/// [`BridgeRequest::net_destination_atomic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestAmounts {
    pub gross_atomic: u64,
    pub fee_bps: u64,
    pub fee_atomic: u64,
    pub net_atomic: u64,
    pub net_destination_atomic: u64,
}

/// Which direction a rebalance moves real, already-existing funds
/// (docs/05-reserve-accounting.md, docs/22-production-readiness-review.md
/// P1 "rebalancing"). Structurally distinct from `Direction`
/// (`GlcToSol`/`SolToGlc`, user settlements) — a rebalance never touches
/// `bridge_requests`, `reserved_liquidity`, or `pending_obligations`, only
/// `total_reserve_balance` on the ONE named reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebalanceKind {
    /// Real funds moved INTO the named reserve from outside it (a
    /// treasury top-up, or funds swept from the other reserve's own
    /// excess by whatever real transfer the operator actually executes).
    Deposit,
    /// Real funds moved OUT of the named reserve (e.g. sweeping surplus
    /// to cold storage, or funding the other reserve's shortfall).
    Withdraw,
}

impl RebalanceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RebalanceKind::Deposit => "Deposit",
            RebalanceKind::Withdraw => "Withdraw",
        }
    }
}

impl std::str::FromStr for RebalanceKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Deposit" => Ok(RebalanceKind::Deposit),
            "Withdraw" => Ok(RebalanceKind::Withdraw),
            other => Err(format!("unknown rebalance kind {other:?}")),
        }
    }
}

impl ToSql for RebalanceKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for RebalanceKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// Rebalance-request lifecycle (docs/22-production-readiness-review.md P1
/// "rebalancing"). Deliberately never reaches a state that implies THIS
/// service broadcast or signed a real fund-moving transaction — the
/// transition into `Executed` only ever records evidence (`tx_reference`)
/// of a transfer some operator authorized and executed entirely out of
/// band, through whatever real custody tooling holds the actual keys
/// (docs/02-trust-model.md). This ledger tracks the REQUEST, its
/// approvals, and its audit trail; it never moves funds itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebalanceState {
    /// Created; collecting the configured number of approvals.
    Proposed,
    /// Approval threshold reached; awaiting out-of-band execution.
    Approved,
    /// An operator recorded a real `tx_reference` for a transfer they
    /// already authorized and executed outside this system.
    Executed,
    /// The resulting real balance change was independently confirmed
    /// (operator-reported observation, cross-checked against the next
    /// live reconciliation read) — terminal success.
    Confirmed,
    /// An approver declined before execution — terminal.
    Rejected,
    /// Withdrawn by an operator before execution — terminal.
    Cancelled,
    /// Execution was recorded but the expected effect was never
    /// confirmed (or was confirmed to be wrong) — routed here rather than
    /// silently left `Executed` forever; requires operator resolution,
    /// same discipline as `RequestState::ManualReview`.
    Failed,
}

impl RebalanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            RebalanceState::Proposed => "Proposed",
            RebalanceState::Approved => "Approved",
            RebalanceState::Executed => "Executed",
            RebalanceState::Confirmed => "Confirmed",
            RebalanceState::Rejected => "Rejected",
            RebalanceState::Cancelled => "Cancelled",
            RebalanceState::Failed => "Failed",
        }
    }

    /// Non-terminal — still expected to move forward or be explicitly
    /// closed out by an operator.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            RebalanceState::Proposed | RebalanceState::Approved | RebalanceState::Executed
        )
    }
}

impl std::str::FromStr for RebalanceState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Proposed" => RebalanceState::Proposed,
            "Approved" => RebalanceState::Approved,
            "Executed" => RebalanceState::Executed,
            "Confirmed" => RebalanceState::Confirmed,
            "Rejected" => RebalanceState::Rejected,
            "Cancelled" => RebalanceState::Cancelled,
            "Failed" => RebalanceState::Failed,
            other => return Err(format!("unknown rebalance state {other:?}")),
        })
    }
}

impl ToSql for RebalanceState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for RebalanceState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// A row of `rebalance_requests`. `amount_atomic` is always in
/// `direction`'s own native chain unit (Goldcoin-native or the Solana
/// reserve mint's live decimals) — a rebalance never involves a
/// cross-chain conversion, since it moves one already-existing asset
/// within one chain (into or out of that chain's own reserve), unlike a
/// bridge settlement.
#[derive(Debug, Clone)]
pub struct RebalanceRequest {
    pub id: i64,
    pub direction: ReserveDirection,
    pub kind: RebalanceKind,
    pub amount_atomic: u64,
    pub state: RebalanceState,
    pub reason: String,
    pub requested_by: String,
    pub requested_at: i64,
    pub required_approvals: u32,
    /// JSON array of approving identities, never key material.
    pub approved_by: Vec<String>,
    pub approved_at: Option<i64>,
    pub tx_reference: Option<String>,
    pub executed_at: Option<i64>,
    pub observed_amount_atomic: Option<u64>,
    pub confirmed_at: Option<i64>,
    pub failure_reason: Option<String>,
}

/// Which custody surface a transition rotates
/// (docs/22-production-readiness-review.md P1 "key rotation / vault
/// sweep tooling").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyTransitionKind {
    /// Retiring one or more ed25519 attestation signer identities in
    /// favor of a new set (`signing::attestation`). Authorizes BOTH
    /// bridge directions, so `record_custody_transition_executed`
    /// requires both reserves paused first.
    AttestationKeyRotation,
    /// Sweeping the Goldcoin P2SH multisig vault
    /// (`signing::goldcoin_vault`) to a new vault identity/threshold.
    /// Only the Goldcoin reserve need be paused first.
    GoldcoinVaultSweep,
}

impl CustodyTransitionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CustodyTransitionKind::AttestationKeyRotation => "AttestationKeyRotation",
            CustodyTransitionKind::GoldcoinVaultSweep => "GoldcoinVaultSweep",
        }
    }
}

impl std::str::FromStr for CustodyTransitionKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "AttestationKeyRotation" => Ok(CustodyTransitionKind::AttestationKeyRotation),
            "GoldcoinVaultSweep" => Ok(CustodyTransitionKind::GoldcoinVaultSweep),
            other => Err(format!("unknown custody transition kind {other:?}")),
        }
    }
}

impl ToSql for CustodyTransitionKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for CustodyTransitionKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// Custody-transition lifecycle
/// (docs/22-production-readiness-review.md P1 "key rotation / vault
/// sweep tooling"). Extends the rebalance shape with one required extra
/// gate: a new identity must be independently verified BEFORE any
/// approval can be recorded, modeling "verification of new signer
/// identity before activation" as enforced, not advisory. Like
/// `RebalanceState`, `Executed` only ever records evidence of a real
/// rotation/sweep executed out of band — this service never generates
/// keys, signs, or broadcasts the transition itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyTransitionState {
    /// Created; new identity not yet verified.
    Proposed,
    /// The new signer identity has been independently verified
    /// (e.g. a signed challenge checked against the claimed public
    /// key/vault descriptor) — required before approvals may begin.
    IdentityVerified,
    /// Approval threshold reached; awaiting out-of-band execution.
    Approved,
    /// An operator recorded a real `tx_reference`/rotation evidence for
    /// a transition already authorized and executed outside this
    /// system. Requires the relevant reserve(s) already paused.
    Executed,
    /// The new custody identity was independently confirmed active and
    /// correct post-transition — terminal success.
    Confirmed,
    /// An approver declined before execution — terminal.
    Rejected,
    /// Withdrawn by an operator before execution — terminal.
    Cancelled,
    /// Execution was recorded but the expected new-identity state was
    /// never confirmed (or confirmed wrong) — requires operator
    /// resolution, same discipline as `RebalanceState::Failed`.
    Failed,
    /// An operator recorded that a `Failed` transition's real-world
    /// effect was reverted back to the old identity out of band. Only
    /// ever an audit marker of a real rollback already performed — this
    /// service never performs the rollback itself.
    RolledBack,
}

impl CustodyTransitionState {
    pub fn as_str(self) -> &'static str {
        match self {
            CustodyTransitionState::Proposed => "Proposed",
            CustodyTransitionState::IdentityVerified => "IdentityVerified",
            CustodyTransitionState::Approved => "Approved",
            CustodyTransitionState::Executed => "Executed",
            CustodyTransitionState::Confirmed => "Confirmed",
            CustodyTransitionState::Rejected => "Rejected",
            CustodyTransitionState::Cancelled => "Cancelled",
            CustodyTransitionState::Failed => "Failed",
            CustodyTransitionState::RolledBack => "RolledBack",
        }
    }

    /// Non-terminal — still expected to move forward or be explicitly
    /// closed out by an operator.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            CustodyTransitionState::Proposed
                | CustodyTransitionState::IdentityVerified
                | CustodyTransitionState::Approved
                | CustodyTransitionState::Executed
        )
    }
}

impl std::str::FromStr for CustodyTransitionState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Proposed" => CustodyTransitionState::Proposed,
            "IdentityVerified" => CustodyTransitionState::IdentityVerified,
            "Approved" => CustodyTransitionState::Approved,
            "Executed" => CustodyTransitionState::Executed,
            "Confirmed" => CustodyTransitionState::Confirmed,
            "Rejected" => CustodyTransitionState::Rejected,
            "Cancelled" => CustodyTransitionState::Cancelled,
            "Failed" => CustodyTransitionState::Failed,
            "RolledBack" => CustodyTransitionState::RolledBack,
            other => return Err(format!("unknown custody transition state {other:?}")),
        })
    }
}

impl ToSql for CustodyTransitionState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for CustodyTransitionState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// A row of `custody_transitions`. `old_identities`/`new_identities` are
/// JSON arrays of opaque public identity strings (pubkeys/vault
/// descriptors) — never key material. `new_threshold` only applies to
/// `GoldcoinVaultSweep` (a new multisig M-of-N); left `None` for
/// `AttestationKeyRotation`, which has no threshold concept.
#[derive(Debug, Clone)]
pub struct CustodyTransition {
    pub id: i64,
    pub kind: CustodyTransitionKind,
    pub state: CustodyTransitionState,
    pub old_identities: Vec<String>,
    pub new_identities: Vec<String>,
    pub new_threshold: Option<u32>,
    pub reason: String,
    pub requested_by: String,
    pub requested_at: i64,
    pub required_approvals: u32,
    pub approved_by: Vec<String>,
    pub approved_at: Option<i64>,
    pub identity_verified_by: Option<String>,
    pub identity_verified_at: Option<i64>,
    pub tx_reference: Option<String>,
    pub executed_at: Option<i64>,
    pub confirmed_at: Option<i64>,
    pub failure_reason: Option<String>,
    pub rolled_back_at: Option<i64>,
    pub rollback_reason: Option<String>,
}

/// How an admin mutation attempt ended, for the `admin_audit_log`
/// (`Ledger::append_admin_audit`). Failed attempts are recorded too —
/// "an operator tried and was refused" is itself audit-relevant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminAuditOutcome {
    Success,
    /// The refusal/failure message shown to the operator (a `LedgerError`
    /// display string, typically) — never internal paths or secrets.
    Error(String),
}

/// One admin mutation attempt to append via [`crate::ledger::Ledger::
/// append_admin_audit`]. `old_value`/`new_value` are small JSON or plain
/// display snapshots of the mutated setting, captured by the caller
/// BEFORE and after (or as-requested) the mutation.
#[derive(Debug, Clone)]
pub struct AdminAuditEntry {
    pub at: i64,
    /// Operator identity: the admin-API operator name the bearer token
    /// resolved to, or `cli:<user>` for `glc-admin` invocations.
    pub actor: String,
    /// Machine-readable action slug: `pause`, `unpause`,
    /// `admission_open`, `admission_close`, `resume_manual_review`,
    /// `rebalance_propose`, ...
    pub action: String,
    /// What was acted on: a direction, request id, or rebalance id.
    pub target: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    /// Mandatory operator-supplied reason; the schema `CHECK`s it
    /// non-empty.
    pub note: String,
    pub outcome: AdminAuditOutcome,
}

/// A stored `admin_audit_log` row ([`crate::ledger::Ledger::
/// list_admin_audit`]).
#[derive(Debug, Clone)]
pub struct AdminAuditRow {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub note: String,
    pub outcome: AdminAuditOutcome,
}

/// Keyset-paginated filter for [`crate::ledger::Ledger::
/// list_admin_audit`]: rows with `id < before_id` (newest first), capped
/// at `limit`, optionally restricted to one action slug and/or actor.
#[derive(Debug, Clone, Default)]
pub struct AdminAuditFilter {
    pub before_id: Option<i64>,
    pub limit: Option<u32>,
    pub action: Option<String>,
    pub actor: Option<String>,
}

/// Lifecycle state of one `solana_refunds` row. Mirrors the owning
/// request's own `RefundPending`/`RefundBroadcast`/`Refunded` states 1:1
/// — the request state drives visibility/gating in every daemon loop, the
/// refund row carries the artifact (nonce, amounts, destination,
/// signature) and the structural one-refund-per-request guarantees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolanaRefundState {
    /// Row created; nothing signed or broadcast yet. Safe to re-run the
    /// refund command — it resumes from attestation.
    Pending,
    /// The refund transaction's signature has been recorded (recorded
    /// BEFORE the send, so a crash between record and broadcast is
    /// recoverable). At most one such transaction can ever land: the
    /// deterministic refund nonce's `rebalance_withdrawal` PDA is the
    /// on-chain replay guard.
    Broadcast,
    /// Terminal: confirmed at `finalized` commitment; the request is
    /// `Refunded`.
    Confirmed,
}

impl SolanaRefundState {
    pub fn as_str(self) -> &'static str {
        match self {
            SolanaRefundState::Pending => "Pending",
            SolanaRefundState::Broadcast => "Broadcast",
            SolanaRefundState::Confirmed => "Confirmed",
        }
    }
}

impl std::str::FromStr for SolanaRefundState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Pending" => SolanaRefundState::Pending,
            "Broadcast" => SolanaRefundState::Broadcast,
            "Confirmed" => SolanaRefundState::Confirmed,
            other => return Err(format!("unknown solana refund state {other:?}")),
        })
    }
}

impl ToSql for SolanaRefundState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for SolanaRefundState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// A row of `solana_refunds` — the full audit/idempotency record of one
/// ManualReview refund lifecycle. `request_id` is the table's PRIMARY KEY
/// (at most one refund lifecycle per request, structurally), `nonce` and
/// `obligation_index` are additionally UNIQUE, and the nonce doubles as
/// the on-chain replay guard: `rebalance_withdraw`'s `rebalance_withdrawal`
/// PDA `init` makes a second transfer under the same nonce impossible on
/// chain, independent of any database state.
#[derive(Debug, Clone)]
pub struct SolanaRefund {
    pub request_id: i64,
    /// The on-chain `WithdrawalObligation.index` this refund returns —
    /// the canonical identity of the original, finalized Solana deposit.
    pub obligation_index: u64,
    /// `refund domain bit | request_id` — see
    /// [`crate::ledger::Ledger::solana_refund_nonce`].
    pub nonce: u64,
    /// Exact gross deposited amount, in the reserve mint's own native
    /// atomic units (the on-chain `WithdrawalObligation.amount`). No fee
    /// is deducted: for SolToGlc the bridge fee only ever accrues at
    /// settlement, which a refunded request never reaches.
    pub amount_solana_atomic: u64,
    /// The original depositor's wallet (32 bytes) — copied from
    /// `bridge_requests.requester`, itself decoded from the on-chain
    /// obligation's `requester` field, and re-verified against a fresh
    /// finalized read of that obligation before any transfer.
    pub requester: [u8; 32],
    /// The canonical ATA of (`requester`, reserve mint, reserve token
    /// program) — always derived, never operator-supplied.
    pub destination_token_account: [u8; 32],
    pub reserve_mint: [u8; 32],
    pub token_program: [u8; 32],
    /// Frozen copy of the request's `manual_review_note` at refund-begin
    /// time (the request row's own copy is preserved too — never
    /// overwritten by the refund lifecycle).
    pub manual_review_reason: String,
    pub note: String,
    pub created_by: String,
    pub state: SolanaRefundState,
    pub attestation_epoch: Option<u64>,
    /// Base58 transaction signature of the (latest) refund broadcast.
    /// Never key material.
    pub refund_signature: Option<String>,
    /// The (latest) broadcast transaction's recent blockhash — recovery
    /// uses it to POSITIVELY determine the transaction can no longer land
    /// before ever rebuilding under the same nonce.
    pub recent_blockhash: Option<String>,
    pub created_at: i64,
    pub broadcast_at: Option<i64>,
    pub confirmed_at: Option<i64>,
}
