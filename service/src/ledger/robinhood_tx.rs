//! The durable record of every OUTBOUND Robinhood transaction: its
//! authorization, its nonce, its signed bytes, its broadcast history and
//! its receipt.
//!
//! # This module is the idempotency design
//!
//! Phase F's hardest requirement is that no restart, at any point, can
//! produce a duplicate payout, a duplicate settlement, a duplicate
//! refund, or a second nonce for one unresolved broadcast. That is not
//! achieved by careful ordering of statements in the orchestrator — an
//! orchestrator can be killed between any two statements — but by making
//! the duplicate UNREPRESENTABLE:
//!
//! | Duplicate | What prevents it |
//! |---|---|
//! | Two payouts for one request | `ux_robinhood_tx_operation` on `(kind, request_id)` |
//! | Two operations under one nonce | `ux_robinhood_tx_nonce` on `(submitter, chain_id, nonce)` |
//! | Two operations claiming one contract request id | `ux_robinhood_tx_contract_request` |
//! | A broadcast with no persisted nonce or bytes | a table CHECK on `state` |
//! | A completed operation with no successful receipt | a table CHECK on `state` |
//! | A third signature, or two from one signer | the signatures table's PK and unique index |
//!
//! Every one of those is a database constraint. A bug in this file, or in
//! the orchestrator, produces a constraint violation and a stalled
//! request — never a second transfer.
//!
//! # Allocation reads the ledger, never the chain
//!
//! [`Ledger::allocate_robinhood_nonce`] takes the next nonce from the
//! MAXIMUM already recorded in this table, inside the same write
//! transaction that stores it. `eth_getTransactionCount` is a
//! reconciliation input recorded separately
//! ([`Ledger::record_evm_submitter_nonce`]) and is used only as a FLOOR:
//! it can reveal that the chain has moved ahead of this ledger (another
//! process, or a restored backup), never that it has moved behind.
//!
//! Deriving the allocator from the chain instead would reintroduce
//! exactly the race the durable table exists to close: two allocations
//! between two RPC round trips would both see the same count.
//!
//! # Nothing here broadcasts, signs, or reads a chain
//!
//! This module is `rusqlite` only. Every function takes what it is told
//! and enforces what the schema says; the decisions about WHETHER to
//! sign, broadcast or replace live in [`crate::robinhood::submitter`],
//! where the RPC client is.

use rusqlite::OptionalExtension;

use super::{write_tx, Ledger, LedgerError};

/// Which of the three operations a row describes.
///
/// A closed enum rather than a string: the kind decides which columns are
/// populated and which action byte is bound, and both correspondences are
/// additionally enforced by table CHECKs (schema v23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RobinhoodTxKind {
    /// `GlcToRhn`: pay reserve GLC to a Robinhood recipient.
    Payout,
    /// `RhnToGlc`: record an obligation as settled, AFTER its Goldcoin
    /// payout confirmed. Never before — see
    /// [`Ledger::begin_robinhood_settlement`].
    Settlement,
    /// `RhnToGlc`: return an obligation's exact principal to its
    /// depositor.
    Refund,
    /// No route: move reserve GLC to the contract's immutable `TREASURY`,
    /// settling an approved `rebalance_requests` row rather than a bridge
    /// request. The EVM counterpart of the Solana `treasury_withdraw`
    /// instruction. Added in schema v26.
    TreasuryWithdraw,
}

impl RobinhoodTxKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RobinhoodTxKind::Payout => "Payout",
            RobinhoodTxKind::Settlement => "Settlement",
            RobinhoodTxKind::Refund => "Refund",
            RobinhoodTxKind::TreasuryWithdraw => "TreasuryWithdraw",
        }
    }

    /// The contract's ACTION discriminator this kind is authorized under.
    pub fn action(self) -> u8 {
        match self {
            RobinhoodTxKind::Payout => crate::robinhood::auth::ACTION_PAYOUT,
            RobinhoodTxKind::Refund => crate::robinhood::auth::ACTION_REFUND,
            RobinhoodTxKind::Settlement => crate::robinhood::auth::ACTION_SETTLE,
            RobinhoodTxKind::TreasuryWithdraw => crate::robinhood::auth::ACTION_TREASURY_WITHDRAW,
        }
    }

    /// Whether this kind settles a `bridge_requests` row (as opposed to a
    /// `rebalance_requests` row).
    pub fn settles_a_bridge_request(self) -> bool {
        !matches!(self, RobinhoodTxKind::TreasuryWithdraw)
    }

    pub const ALL: [RobinhoodTxKind; 4] = [
        RobinhoodTxKind::Payout,
        RobinhoodTxKind::Settlement,
        RobinhoodTxKind::Refund,
        RobinhoodTxKind::TreasuryWithdraw,
    ];
}

impl std::str::FromStr for RobinhoodTxKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Payout" => Ok(RobinhoodTxKind::Payout),
            "Settlement" => Ok(RobinhoodTxKind::Settlement),
            "Refund" => Ok(RobinhoodTxKind::Refund),
            "TreasuryWithdraw" => Ok(RobinhoodTxKind::TreasuryWithdraw),
            other => Err(format!("unknown Robinhood transaction kind {other:?}")),
        }
    }
}

impl rusqlite::ToSql for RobinhoodTxKind {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
    }
}

impl rusqlite::types::FromSql for RobinhoodTxKind {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|_| rusqlite::types::FromSqlError::InvalidType)
    }
}

/// Where one outbound operation has got to.
///
/// The order below is the only order these are reached in, and the
/// forward-only-ness is enforced by [`Ledger`]'s transition functions
/// rather than by callers remembering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RobinhoodTxState {
    /// The row exists and the authorization payload is fixed, but fewer
    /// than two valid signatures have been collected.
    ///
    /// The row is written BEFORE the first signer is asked, so a crash
    /// mid-collection resumes with the same payload rather than minting a
    /// second, differently-expiring one.
    Authorizing,
    /// Exactly two signatures from two distinct authorized signers are
    /// stored, each verified locally against the recorded digest.
    Authorized,
    /// A nonce is allocated and the signed transaction bytes are
    /// persisted. Nothing has been sent.
    Signed,
    /// The bytes have been handed to a node at least once. This state
    /// says NOTHING about whether the node received them: a broadcast
    /// whose result was a transport failure sits here too, which is
    /// exactly why re-broadcasting the identical bytes is the only safe
    /// recovery.
    Broadcast,
    /// A receipt was read back with `status = 1`. Included and
    /// successful, but not yet deep enough to be irreversible.
    Included,
    /// Included, successful, and at or past the configured confirmation
    /// depth. Terminal, and the ONLY state from which the bridge request
    /// itself may be completed.
    Finalized,
    /// A receipt was read back with `status = 0`. The transaction was
    /// mined and REVERTED: it consumed its nonce and its gas and achieved
    /// nothing.
    ///
    /// Terminal for this row. It is deliberately NOT retried under a
    /// fresh nonce — a revert means a precondition the contract checks
    /// was false, and re-sending the same call would revert identically
    /// while spending more gas. The operation moves to
    /// [`RobinhoodTxState::ManualReview`] via an explicit decision, not a
    /// loop.
    Reverted,
    /// Stopped for a human. Reached from a revert, from an exhausted
    /// replacement budget, from a post-finality contradiction, or from
    /// any disagreement between what this ledger believes and what the
    /// chain reports.
    ManualReview,
}

impl RobinhoodTxState {
    pub fn as_str(self) -> &'static str {
        match self {
            RobinhoodTxState::Authorizing => "Authorizing",
            RobinhoodTxState::Authorized => "Authorized",
            RobinhoodTxState::Signed => "Signed",
            RobinhoodTxState::Broadcast => "Broadcast",
            RobinhoodTxState::Included => "Included",
            RobinhoodTxState::Finalized => "Finalized",
            RobinhoodTxState::Reverted => "Reverted",
            RobinhoodTxState::ManualReview => "ManualReview",
        }
    }

    /// Whether this row will never move again without a human.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RobinhoodTxState::Finalized
                | RobinhoodTxState::Reverted
                | RobinhoodTxState::ManualReview
        )
    }

    /// Whether the signed bytes may be sitting in a mempool or a block
    /// right now — i.e. whether this row's nonce is committed to a
    /// specific transaction that must never be replaced by a different
    /// one.
    pub fn is_in_flight(self) -> bool {
        matches!(
            self,
            RobinhoodTxState::Broadcast | RobinhoodTxState::Included
        )
    }
}

impl std::str::FromStr for RobinhoodTxState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Authorizing" => RobinhoodTxState::Authorizing,
            "Authorized" => RobinhoodTxState::Authorized,
            "Signed" => RobinhoodTxState::Signed,
            "Broadcast" => RobinhoodTxState::Broadcast,
            "Included" => RobinhoodTxState::Included,
            "Finalized" => RobinhoodTxState::Finalized,
            "Reverted" => RobinhoodTxState::Reverted,
            "ManualReview" => RobinhoodTxState::ManualReview,
            other => return Err(format!("unknown Robinhood transaction state {other:?}")),
        })
    }
}

impl rusqlite::ToSql for RobinhoodTxState {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
    }
}

impl rusqlite::types::FromSql for RobinhoodTxState {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|_| rusqlite::types::FromSqlError::InvalidType)
    }
}

/// Everything needed to CREATE one operation row, i.e. the authorization
/// payload plus the identity it was built against.
///
/// Deliberately one struct rather than fifteen positional arguments: the
/// fields are mostly `u64`s and 20/32-byte blobs, and a transposed pair
/// would compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRobinhoodTx {
    pub kind: RobinhoodTxKind,
    /// The `bridge_requests` row this settles. `None` exactly for a
    /// treasury withdrawal.
    pub request_id: Option<i64>,
    /// The `rebalance_requests` row this settles. `Some` exactly for a
    /// treasury withdrawal.
    pub rebalance_request_id: Option<i64>,
    /// `None` exactly for a treasury withdrawal, which binds no route.
    pub route: Option<crate::routes::Route>,
    pub bridge_contract: [u8; 20],
    pub chain_id: u64,
    pub contract_request_id: [u8; 32],
    /// `Some` for settlement and refund; `None` for a payout, which
    /// settles a deposit on another chain entirely.
    pub obligation_index: Option<u64>,
    /// `Some` for payout and refund; `None` for a settlement, which moves
    /// nothing.
    pub recipient: Option<[u8; 20]>,
    /// The exact 32-byte big-endian `uint256`, never narrowed.
    pub amount_robinhood: Option<[u8; 32]>,
    pub signer_epoch: u64,
    pub expiry: u64,
    /// The EIP-712 digest the quorum will sign. Persisted so that every
    /// later use can re-derive it and compare, catching a payload that
    /// changed underneath already-collected signatures.
    pub auth_digest: [u8; 32],
}

/// One operation row, read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodTx {
    pub id: i64,
    pub kind: RobinhoodTxKind,
    /// See [`NewRobinhoodTx::request_id`].
    pub request_id: Option<i64>,
    /// See [`NewRobinhoodTx::rebalance_request_id`].
    pub rebalance_request_id: Option<i64>,
    /// See [`NewRobinhoodTx::route`].
    pub route: Option<crate::routes::Route>,
    pub bridge_contract: [u8; 20],
    pub chain_id: u64,
    pub action: u8,
    pub contract_request_id: [u8; 32],
    pub obligation_index: Option<u64>,
    pub recipient: Option<[u8; 20]>,
    pub amount_robinhood: Option<[u8; 32]>,
    pub signer_epoch: u64,
    pub expiry: u64,
    pub auth_digest: [u8; 32],
    pub submitter: Option<[u8; 20]>,
    pub nonce: Option<u64>,
    pub envelope: Option<String>,
    pub gas_limit: Option<u64>,
    pub fee_summary: Option<String>,
    pub raw_tx: Option<Vec<u8>>,
    pub tx_hash: Option<[u8; 32]>,
    pub state: RobinhoodTxState,
    pub first_broadcast_at: Option<i64>,
    pub last_broadcast_at: Option<i64>,
    pub broadcast_attempts: i64,
    pub replacement_attempts: i64,
    pub receipt_status: Option<i64>,
    pub receipt_block_number: Option<i64>,
    pub receipt_block_hash: Option<[u8; 32]>,
    pub confirmations: i64,
    pub finalized_at: Option<i64>,
    pub failure_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl RobinhoodTx {
    /// The ledger row this operation settles, for operator output and
    /// error messages: `"request N"` for the three bridge-request kinds,
    /// `"rebalance N"` for a treasury withdrawal.
    pub fn subject(&self) -> String {
        match (self.request_id, self.rebalance_request_id) {
            (Some(id), _) => format!("request {id}"),
            (None, Some(id)) => format!("rebalance {id}"),
            (None, None) => format!("operation #{}", self.id),
        }
    }

    /// The bridge request this settles, or a typed error for the kind
    /// that settles none — so a caller on a bridge-request path never
    /// silently reads a withdrawal as request `0`.
    pub fn bridge_request_id(&self) -> Result<i64, LedgerError> {
        self.request_id
            .ok_or_else(|| LedgerError::RobinhoodTxInvalid {
                id: self.id,
                detail: format!(
                    "a {} operation settles no bridge request",
                    self.kind.as_str()
                ),
            })
    }
}

/// One stored authorization signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodAuthSignature {
    pub position: i64,
    /// The address RECOVERED locally from the signature over the recorded
    /// digest — never a value a signer merely claimed.
    pub signer: [u8; 20],
    pub signature: [u8; 65],
}

/// What [`Ledger::begin_robinhood_tx`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeginTxOutcome {
    /// A new row was created.
    Created { id: i64 },
    /// A row for this `(kind, request_id)` already exists. The NORMAL
    /// result of a re-tick or a restart, and the reason a duplicate can
    /// never be created: the caller resumes the existing operation.
    Exists { id: i64 },
}

fn blob<const N: usize>(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; N], rusqlite::Error> {
    let bytes: Vec<u8> = row.get(index)?;
    <[u8; N]>::try_from(bytes.as_slice()).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            format!("expected {N} bytes").into(),
        )
    })
}

fn opt_blob<const N: usize>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Option<[u8; N]>, rusqlite::Error> {
    let bytes: Option<Vec<u8>> = row.get(index)?;
    match bytes {
        None => Ok(None),
        Some(bytes) => <[u8; N]>::try_from(bytes.as_slice())
            .map(Some)
            .map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Blob,
                    format!("expected {N} bytes").into(),
                )
            }),
    }
}

const TX_COLUMNS: &str = "id, kind, request_id, route, bridge_contract, chain_id, action, \
     contract_request_id, obligation_index, recipient, amount_robinhood, signer_epoch, expiry, \
     auth_digest, submitter, nonce, envelope, gas_limit, fee_summary, raw_tx, tx_hash, state, \
     first_broadcast_at, last_broadcast_at, broadcast_attempts, replacement_attempts, \
     receipt_status, receipt_block_number, receipt_block_hash, confirmations, finalized_at, \
     failure_reason, created_at, updated_at, rebalance_request_id";

fn decode_tx(row: &rusqlite::Row<'_>) -> Result<RobinhoodTx, rusqlite::Error> {
    let route_text: Option<String> = row.get(3)?;
    let route: Option<crate::routes::Route> = match route_text {
        None => None,
        Some(text) => Some(text.parse().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                format!("unknown route {text:?}").into(),
            )
        })?),
    };
    Ok(RobinhoodTx {
        id: row.get(0)?,
        kind: row.get(1)?,
        request_id: row.get(2)?,
        rebalance_request_id: row.get(34)?,
        route,
        bridge_contract: blob::<20>(row, 4)?,
        chain_id: row.get::<_, i64>(5)? as u64,
        action: row.get::<_, i64>(6)? as u8,
        contract_request_id: blob::<32>(row, 7)?,
        obligation_index: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
        recipient: opt_blob::<20>(row, 9)?,
        amount_robinhood: opt_blob::<32>(row, 10)?,
        signer_epoch: row.get::<_, i64>(11)? as u64,
        expiry: row.get::<_, i64>(12)? as u64,
        auth_digest: blob::<32>(row, 13)?,
        submitter: opt_blob::<20>(row, 14)?,
        nonce: row.get::<_, Option<i64>>(15)?.map(|v| v as u64),
        envelope: row.get(16)?,
        gas_limit: row.get::<_, Option<i64>>(17)?.map(|v| v as u64),
        fee_summary: row.get(18)?,
        raw_tx: row.get(19)?,
        tx_hash: opt_blob::<32>(row, 20)?,
        state: row.get(21)?,
        first_broadcast_at: row.get(22)?,
        last_broadcast_at: row.get(23)?,
        broadcast_attempts: row.get(24)?,
        replacement_attempts: row.get(25)?,
        receipt_status: row.get(26)?,
        receipt_block_number: row.get(27)?,
        receipt_block_hash: opt_blob::<32>(row, 28)?,
        confirmations: row.get(29)?,
        finalized_at: row.get(30)?,
        failure_reason: row.get(31)?,
        created_at: row.get(32)?,
        updated_at: row.get(33)?,
    })
}

impl Ledger {
    /// Creates the operation row, or reports that one already exists.
    ///
    /// Written BEFORE any signer is contacted. That ordering is what makes
    /// a crash during signature collection recoverable: the payload —
    /// including its `expiry`, which is a wall-clock deadline — is fixed
    /// at this moment and every later step re-reads it rather than
    /// recomputing it from a clock that has since moved.
    ///
    /// Idempotent by the unique index on `(kind, request_id)`, not by a
    /// prior SELECT: two ticks racing each other both attempt the insert
    /// and exactly one wins, which a check-then-insert could not
    /// guarantee.
    pub fn begin_robinhood_tx(
        &mut self,
        new: &NewRobinhoodTx,
        now: i64,
    ) -> Result<BeginTxOutcome, LedgerError> {
        // The shape the schema enforces, checked here too so the error
        // names the mistake rather than a CHECK constraint.
        let settles_bridge = new.kind.settles_a_bridge_request();
        if settles_bridge != new.request_id.is_some()
            || settles_bridge == new.rebalance_request_id.is_some()
            || settles_bridge != new.route.is_some()
        {
            return Err(LedgerError::RobinhoodTxInvalid {
                id: 0,
                detail: format!(
                    "a {} operation must name {} and no other subject",
                    new.kind.as_str(),
                    if settles_bridge {
                        "a bridge request and a route"
                    } else {
                        "a rebalance request and no route"
                    }
                ),
            });
        }
        let tx = write_tx(&mut self.conn)?;
        let existing: Option<i64> = match (new.request_id, new.rebalance_request_id) {
            (Some(request_id), _) => tx
                .query_row(
                    "SELECT id FROM robinhood_transactions WHERE kind = ?1 AND request_id = ?2",
                    rusqlite::params![new.kind, request_id],
                    |r| r.get(0),
                )
                .optional()?,
            (None, Some(rebalance_id)) => tx
                .query_row(
                    "SELECT id FROM robinhood_transactions WHERE rebalance_request_id = ?1",
                    [rebalance_id],
                    |r| r.get(0),
                )
                .optional()?,
            (None, None) => unreachable!("shape checked above"),
        };
        if let Some(id) = existing {
            tx.rollback()?;
            return Ok(BeginTxOutcome::Exists { id });
        }
        tx.execute(
            "INSERT INTO robinhood_transactions
                (kind, request_id, route, bridge_contract, chain_id, action,
                 contract_request_id, obligation_index, recipient, amount_robinhood,
                 signer_epoch, expiry, auth_digest, state, created_at, updated_at,
                 rebalance_request_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'Authorizing', ?14, ?14,
                     ?15)",
            rusqlite::params![
                new.kind,
                new.request_id,
                new.route.map(|r| r.as_str()),
                &new.bridge_contract[..],
                new.chain_id as i64,
                i64::from(new.kind.action()),
                &new.contract_request_id[..],
                new.obligation_index.map(|v| v as i64),
                new.recipient.map(|r| r.to_vec()),
                new.amount_robinhood.map(|a| a.to_vec()),
                new.signer_epoch as i64,
                new.expiry as i64,
                &new.auth_digest[..],
                now,
                new.rebalance_request_id,
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(BeginTxOutcome::Created { id })
    }

    /// The treasury-withdrawal operation for one rebalance request, if
    /// one was ever begun. At most one exists (`ux_robinhood_tx_rebalance`).
    pub fn get_robinhood_tx_for_rebalance(
        &self,
        rebalance_id: i64,
    ) -> Result<Option<RobinhoodTx>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TX_COLUMNS} FROM robinhood_transactions WHERE rebalance_request_id = ?1"
        ))?;
        Ok(stmt.query_row([rebalance_id], decode_tx).optional()?)
    }

    /// Every treasury-withdrawal operation, newest first.
    pub fn robinhood_treasury_withdrawals(&self) -> Result<Vec<RobinhoodTx>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TX_COLUMNS} FROM robinhood_transactions
             WHERE kind = 'TreasuryWithdraw' ORDER BY id DESC"
        ))?;
        let rows = stmt.query_map([], decode_tx)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// One operation row by its ledger id.
    pub fn get_robinhood_tx(&self, id: i64) -> Result<Option<RobinhoodTx>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TX_COLUMNS} FROM robinhood_transactions WHERE id = ?1"
        ))?;
        Ok(stmt.query_row([id], decode_tx).optional()?)
    }

    /// One operation row by the bridge request and kind it belongs to.
    pub fn get_robinhood_tx_for(
        &self,
        kind: RobinhoodTxKind,
        request_id: i64,
    ) -> Result<Option<RobinhoodTx>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TX_COLUMNS} FROM robinhood_transactions
             WHERE kind = ?1 AND request_id = ?2"
        ))?;
        Ok(stmt
            .query_row(rusqlite::params![kind, request_id], decode_tx)
            .optional()?)
    }

    /// Every operation currently in `state`, oldest first — what the
    /// orchestrator's phases poll.
    pub fn robinhood_txs_in_state(
        &self,
        state: RobinhoodTxState,
    ) -> Result<Vec<RobinhoodTx>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TX_COLUMNS} FROM robinhood_transactions WHERE state = ?1 ORDER BY id"
        ))?;
        let rows: Result<Vec<RobinhoodTx>, _> = stmt.query_map([state], decode_tx)?.collect();
        Ok(rows?)
    }

    /// The signatures collected for one operation, in position order.
    pub fn robinhood_auth_signatures(
        &self,
        tx_id: i64,
    ) -> Result<Vec<RobinhoodAuthSignature>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT position, signer, signature FROM robinhood_authorization_signatures
             WHERE transaction_id = ?1 ORDER BY position",
        )?;
        let rows: Result<Vec<RobinhoodAuthSignature>, _> = stmt
            .query_map([tx_id], |r| {
                Ok(RobinhoodAuthSignature {
                    position: r.get(0)?,
                    signer: blob::<20>(r, 1)?,
                    signature: blob::<65>(r, 2)?,
                })
            })?
            .collect();
        Ok(rows?)
    }

    /// Records the collected quorum and moves the operation to
    /// `Authorized`.
    ///
    /// Takes BOTH signatures at once, and exactly two, because a quorum
    /// is not a quorum until it is complete: storing one and then failing
    /// would leave a row that looks partially authorized, and there is no
    /// use for a single signature. The distinctness of the two signers is
    /// enforced by a unique index as well as checked here — the contract
    /// reverts on `DuplicateSignerSignature`, and this makes storing the
    /// pair that would trigger it impossible.
    ///
    /// Idempotent: called again with the same pair on an already-
    /// `Authorized` row, it verifies the stored pair matches and returns
    /// without writing. Called with a DIFFERENT pair, it refuses — that
    /// is a second, competing authorization for one operation.
    pub fn record_robinhood_authorization(
        &mut self,
        tx_id: i64,
        signatures: &[([u8; 20], [u8; 65])],
        now: i64,
    ) -> Result<(), LedgerError> {
        if signatures.len() != 2 {
            return Err(LedgerError::RobinhoodTxInvalid {
                id: tx_id,
                detail: format!(
                    "a quorum is exactly two signatures, not {}: the contract requires exactly \
                     SIGNER_THRESHOLD and reverts on any other count",
                    signatures.len()
                ),
            });
        }
        if signatures[0].0 == signatures[1].0 {
            return Err(LedgerError::RobinhoodTxInvalid {
                id: tx_id,
                detail: "both signatures recovered to the same signer: two signatures from one \
                         custody domain is a quorum of one"
                    .to_string(),
            });
        }

        let tx = write_tx(&mut self.conn)?;
        let state: RobinhoodTxState = tx.query_row(
            "SELECT state FROM robinhood_transactions WHERE id = ?1",
            [tx_id],
            |r| r.get(0),
        )?;

        let stored: Vec<([u8; 20], [u8; 65])> = {
            let mut stmt = tx.prepare(
                "SELECT signer, signature FROM robinhood_authorization_signatures
                 WHERE transaction_id = ?1 ORDER BY position",
            )?;
            let rows: Result<Vec<_>, _> = stmt
                .query_map([tx_id], |r| Ok((blob::<20>(r, 0)?, blob::<65>(r, 1)?)))?
                .collect();
            rows?
        };
        if !stored.is_empty() {
            // Already authorized. The only safe outcomes are "the same
            // pair" (a no-op) and a refusal.
            let matches = stored.len() == signatures.len()
                && stored
                    .iter()
                    .zip(signatures.iter())
                    .all(|(a, b)| a.0 == b.0 && a.1 == b.1);
            tx.rollback()?;
            return if matches {
                Ok(())
            } else {
                Err(LedgerError::RobinhoodTxInvalid {
                    id: tx_id,
                    detail: "a DIFFERENT authorization is already stored for this operation — \
                             one operation is authorized once, and replacing a stored quorum \
                             would make it impossible to say which one was broadcast"
                        .to_string(),
                })
            };
        }

        if state != RobinhoodTxState::Authorizing {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxWrongState {
                id: tx_id,
                expected: RobinhoodTxState::Authorizing,
                actual: state,
            });
        }

        for (position, (signer, signature)) in signatures.iter().enumerate() {
            tx.execute(
                "INSERT INTO robinhood_authorization_signatures
                    (transaction_id, position, signer, signature, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![tx_id, position as i64, &signer[..], &signature[..], now],
            )?;
        }
        tx.execute(
            "UPDATE robinhood_transactions SET state = 'Authorized', updated_at = ?2
             WHERE id = ?1",
            rusqlite::params![tx_id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The submitter's last observed on-chain nonce count, as reported by
    /// `eth_getTransactionCount(submitter, "pending")`.
    pub fn evm_submitter_nonce(
        &self,
        submitter: [u8; 20],
        chain_id: u64,
    ) -> Result<Option<(u64, i64)>, LedgerError> {
        Ok(self
            .conn
            .query_row(
                "SELECT observed_nonce, observed_at FROM evm_submitter_state
                 WHERE submitter = ?1 AND chain_id = ?2",
                rusqlite::params![&submitter[..], chain_id as i64],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)?)),
            )
            .optional()?)
    }

    /// The highest nonce ever allocated to `submitter` on `chain_id`,
    /// across operations in EVERY state.
    ///
    /// The same query [`Ledger::allocate_robinhood_nonce`] uses to pick
    /// the next one, exposed read-only so an operator surface reports the
    /// number the allocator will actually act on. Deliberately not
    /// restricted to in-flight rows: a finalized operation's nonce is
    /// still spent, and reporting only the unresolved ones would show a
    /// nonce gap that does not exist.
    pub fn highest_robinhood_nonce(
        &self,
        submitter: [u8; 20],
        chain_id: u64,
    ) -> Result<Option<u64>, LedgerError> {
        let highest: Option<i64> = self.conn.query_row(
            "SELECT MAX(nonce) FROM robinhood_transactions
             WHERE submitter = ?1 AND chain_id = ?2",
            rusqlite::params![&submitter[..], chain_id as i64],
            |r| r.get(0),
        )?;
        Ok(highest.map(|n| n as u64))
    }

    /// Records a fresh `eth_getTransactionCount(..., "pending")`
    /// observation.
    ///
    /// MONOTONIC: an observation lower than one already stored is
    /// recorded as a no-op rather than moving the cursor backwards. A
    /// lagging RPC replica answering with a stale count must never be
    /// able to make this service believe a nonce it already used is free
    /// again.
    pub fn record_evm_submitter_nonce(
        &mut self,
        submitter: [u8; 20],
        chain_id: u64,
        observed_nonce: u64,
        now: i64,
    ) -> Result<u64, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT observed_nonce FROM evm_submitter_state
                 WHERE submitter = ?1 AND chain_id = ?2",
                rusqlite::params![&submitter[..], chain_id as i64],
                |r| r.get(0),
            )
            .optional()?;
        let highest = current.unwrap_or(0).max(observed_nonce as i64);
        tx.execute(
            "INSERT INTO evm_submitter_state (submitter, chain_id, observed_nonce, observed_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(submitter, chain_id)
             DO UPDATE SET observed_nonce = ?3, observed_at = ?4",
            rusqlite::params![&submitter[..], chain_id as i64, highest, now],
        )?;
        tx.commit()?;
        Ok(highest as u64)
    }

    /// Allocates the next nonce for one operation and persists it, in one
    /// transaction.
    ///
    /// # The allocation rule
    ///
    /// `next = max(highest nonce this ledger has ever recorded for this
    /// (submitter, chain) + 1, the highest observed pending count)`.
    ///
    /// The first term is the authority: it is durable, it is written in
    /// the same transaction as the operation that owns it, and the unique
    /// index on `(submitter, chain_id, nonce)` makes a collision a
    /// constraint violation rather than a duplicate transfer.
    ///
    /// The second term is a FLOOR, never a replacement. It can only ever
    /// move the allocation FORWARD, which is the safe direction: it
    /// catches the case where the chain has moved ahead of this ledger —
    /// another process using the same key, or a ledger restored from a
    /// backup — and would otherwise cause every allocation to collide
    /// with an already-mined nonce. It can never move it backwards, which
    /// would hand out a nonce this ledger already committed to a
    /// transaction.
    ///
    /// # Idempotence
    ///
    /// An operation that already has a nonce keeps it, and this returns
    /// that nonce unchanged. That is the whole point: after a restart at
    /// ANY point, the same operation is re-broadcast under the SAME
    /// nonce, and "the broadcast result was uncertain" never becomes a
    /// reason to allocate a second one.
    pub fn allocate_robinhood_nonce(
        &mut self,
        tx_id: i64,
        submitter: [u8; 20],
        chain_id: u64,
        now: i64,
    ) -> Result<u64, LedgerError> {
        let tx = write_tx(&mut self.conn)?;

        let (state, existing_nonce, existing_submitter, row_chain_id): (
            RobinhoodTxState,
            Option<i64>,
            Option<Vec<u8>>,
            i64,
        ) = tx.query_row(
            "SELECT state, nonce, submitter, chain_id FROM robinhood_transactions WHERE id = ?1",
            [tx_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;

        // The chain the operation was AUTHORIZED for decides which nonce
        // sequence it draws from — never a parameter that could name a
        // different one. Without this, allocating against chain B for a
        // row authorized on chain A would take a nonce out of B's
        // sequence and write it onto a row the unique index counts under
        // A, which is exactly how two operations end up sharing one.
        if row_chain_id != chain_id as i64 {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: tx_id,
                detail: format!(
                    "this operation was authorized for chain {row_chain_id} but a nonce was \
                     requested on chain {chain_id}; a nonce sequence belongs to one account on \
                     one chain"
                ),
            });
        }

        if let Some(nonce) = existing_nonce {
            // Already allocated. The submitter must still be the same
            // account, or this ledger's nonce sequence belongs to a key
            // this process no longer holds.
            let stored = existing_submitter.unwrap_or_default();
            tx.rollback()?;
            if stored.as_slice() != submitter.as_slice() {
                return Err(LedgerError::RobinhoodTxInvalid {
                    id: tx_id,
                    detail: "this operation's nonce was allocated for a DIFFERENT submitter \
                             account; the configured submitter key has changed and its nonce \
                             sequence is not this one's"
                        .to_string(),
                });
            }
            return Ok(nonce as u64);
        }

        if state != RobinhoodTxState::Authorized {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxWrongState {
                id: tx_id,
                expected: RobinhoodTxState::Authorized,
                actual: state,
            });
        }

        let highest_recorded: Option<i64> = tx.query_row(
            "SELECT MAX(nonce) FROM robinhood_transactions
             WHERE submitter = ?1 AND chain_id = ?2",
            rusqlite::params![&submitter[..], chain_id as i64],
            |r| r.get(0),
        )?;
        let observed_floor: i64 = tx
            .query_row(
                "SELECT observed_nonce FROM evm_submitter_state
                 WHERE submitter = ?1 AND chain_id = ?2",
                rusqlite::params![&submitter[..], chain_id as i64],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let next = highest_recorded.map_or(0, |n| n + 1).max(observed_floor);

        // Persisted BEFORE the transaction is signed, let alone
        // broadcast. If the process dies immediately after this commit,
        // the nonce is durably owned by this operation and no other
        // operation can take it.
        tx.execute(
            "UPDATE robinhood_transactions
                SET submitter = ?2, nonce = ?3, updated_at = ?4
             WHERE id = ?1",
            rusqlite::params![tx_id, &submitter[..], next, now],
        )?;
        tx.commit()?;
        Ok(next as u64)
    }

    /// Persists the signed transaction bytes and moves to `Signed`.
    ///
    /// Written before the first broadcast, and — for the first signing —
    /// never rewritten afterwards. A later replacement goes through
    /// [`Ledger::record_robinhood_replacement`], which is a separate,
    /// explicitly-counted operation rather than an ordinary update, so
    /// "the bytes changed" is always a deliberate, recorded act.
    ///
    /// Idempotent: re-signing the same operation produces byte-identical
    /// bytes (RFC-6979 determinism, proven in `crate::evm::tx`), so a
    /// repeat call with the same bytes is a no-op. Different bytes on an
    /// already-signed row are refused.
    #[allow(clippy::too_many_arguments)]
    pub fn record_robinhood_signed(
        &mut self,
        tx_id: i64,
        envelope: &str,
        gas_limit: u64,
        fee_summary: &str,
        raw_tx: &[u8],
        tx_hash: [u8; 32],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (state, stored_raw): (RobinhoodTxState, Option<Vec<u8>>) = tx.query_row(
            "SELECT state, raw_tx FROM robinhood_transactions WHERE id = ?1",
            [tx_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        if let Some(stored) = stored_raw {
            tx.rollback()?;
            return if stored == raw_tx {
                Ok(())
            } else {
                Err(LedgerError::RobinhoodTxInvalid {
                    id: tx_id,
                    detail: "different signed bytes are already stored for this operation — a \
                             replacement must go through record_robinhood_replacement so that \
                             it is counted and visible"
                        .to_string(),
                })
            };
        }
        if state != RobinhoodTxState::Authorized {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxWrongState {
                id: tx_id,
                expected: RobinhoodTxState::Authorized,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE robinhood_transactions
                SET envelope = ?2, gas_limit = ?3, fee_summary = ?4, raw_tx = ?5, tx_hash = ?6,
                    state = 'Signed', updated_at = ?7
             WHERE id = ?1",
            rusqlite::params![
                tx_id,
                envelope,
                gas_limit as i64,
                fee_summary,
                raw_tx,
                &tx_hash[..],
                now
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records that the signed bytes were handed to a node.
    ///
    /// Called on EVERY attempt, including a re-broadcast of bytes that
    /// were already sent and including one whose result was a transport
    /// failure. The count is what tells an operator the difference
    /// between a transaction nobody has sent and one that has been sent
    /// forty times.
    pub fn record_robinhood_broadcast(&mut self, tx_id: i64, now: i64) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let state: RobinhoodTxState = tx.query_row(
            "SELECT state FROM robinhood_transactions WHERE id = ?1",
            [tx_id],
            |r| r.get(0),
        )?;
        if !matches!(
            state,
            RobinhoodTxState::Signed | RobinhoodTxState::Broadcast
        ) {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxWrongState {
                id: tx_id,
                expected: RobinhoodTxState::Signed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE robinhood_transactions
                SET state = 'Broadcast',
                    first_broadcast_at = COALESCE(first_broadcast_at, ?2),
                    last_broadcast_at = ?2,
                    broadcast_attempts = broadcast_attempts + 1,
                    updated_at = ?2
             WHERE id = ?1",
            rusqlite::params![tx_id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces an in-flight transaction's bytes with a fee-bumped
    /// version under the SAME nonce.
    ///
    /// The nonce is deliberately not a parameter and is never touched:
    /// a replacement is the same operation at a higher fee, and
    /// allocating a new nonce would make it a SECOND transaction racing
    /// the first — with both able to mine.
    ///
    /// `replacement_attempts` is incremented and bounded by the caller
    /// against its configured budget; the count is durable so a restart
    /// cannot reset it and start bumping forever.
    pub fn record_robinhood_replacement(
        &mut self,
        tx_id: i64,
        fee_summary: &str,
        raw_tx: &[u8],
        tx_hash: [u8; 32],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (state, nonce): (RobinhoodTxState, Option<i64>) = tx.query_row(
            "SELECT state, nonce FROM robinhood_transactions WHERE id = ?1",
            [tx_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if state != RobinhoodTxState::Broadcast {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxWrongState {
                id: tx_id,
                expected: RobinhoodTxState::Broadcast,
                actual: state,
            });
        }
        if nonce.is_none() {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: tx_id,
                detail: "a broadcast operation with no nonce cannot be replaced".to_string(),
            });
        }
        tx.execute(
            "UPDATE robinhood_transactions
                SET fee_summary = ?2, raw_tx = ?3, tx_hash = ?4,
                    replacement_attempts = replacement_attempts + 1,
                    updated_at = ?5
             WHERE id = ?1",
            rusqlite::params![tx_id, fee_summary, raw_tx, &tx_hash[..], now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records a receipt that was read back from the chain.
    ///
    /// `success = false` — a REVERTED transaction — moves the row to
    /// `Reverted`, a terminal state. It is deliberately not retried under
    /// a fresh nonce: a revert means a precondition the contract checks
    /// was false, so re-sending the same call would revert identically
    /// while spending more gas, and re-sending a DIFFERENT call would be
    /// a different operation than the one that was authorized.
    ///
    /// `success = true` moves to `Included`. Included is not finished —
    /// see [`Ledger::update_robinhood_confirmations`].
    pub fn record_robinhood_receipt(
        &mut self,
        tx_id: i64,
        success: bool,
        block_number: u64,
        block_hash: [u8; 32],
        now: i64,
    ) -> Result<RobinhoodTxState, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (state, stored_block): (RobinhoodTxState, Option<i64>) = tx.query_row(
            "SELECT state, receipt_block_number FROM robinhood_transactions WHERE id = ?1",
            [tx_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        // A receipt for a transaction this ledger already believes was
        // included in a DIFFERENT block is a post-inclusion reorg, and it
        // is not reconciled automatically at any depth: the two facts
        // cannot both be true, and choosing between them is a human's
        // decision.
        if let Some(stored) = stored_block {
            if stored != block_number as i64 {
                tx.execute(
                    "UPDATE robinhood_transactions
                        SET state = 'ManualReview', failure_reason = ?2, updated_at = ?3
                     WHERE id = ?1",
                    rusqlite::params![
                        tx_id,
                        format!(
                            "receipt moved from block {stored} to block {block_number}: the \
                             chain has contradicted an inclusion this service already recorded"
                        ),
                        now
                    ],
                )?;
                tx.commit()?;
                return Ok(RobinhoodTxState::ManualReview);
            }
        }

        if state.is_terminal() {
            tx.rollback()?;
            return Ok(state);
        }
        if !matches!(
            state,
            RobinhoodTxState::Broadcast | RobinhoodTxState::Included
        ) {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxWrongState {
                id: tx_id,
                expected: RobinhoodTxState::Broadcast,
                actual: state,
            });
        }

        let next = if success {
            RobinhoodTxState::Included
        } else {
            RobinhoodTxState::Reverted
        };
        tx.execute(
            "UPDATE robinhood_transactions
                SET state = ?2, receipt_status = ?3, receipt_block_number = ?4,
                    receipt_block_hash = ?5, failure_reason = ?6, updated_at = ?7
             WHERE id = ?1",
            rusqlite::params![
                tx_id,
                next,
                i64::from(success),
                block_number as i64,
                &block_hash[..],
                if success {
                    None
                } else {
                    Some(
                        "the transaction was mined and REVERTED: it consumed its nonce and its \
                         gas and achieved nothing. Not retried automatically — a revert means a \
                         contract-side precondition was false.",
                    )
                },
                now
            ],
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// Updates the observed confirmation depth of an `Included`
    /// transaction, promoting it to `Finalized` once it reaches
    /// `required`.
    ///
    /// Returns `true` only on the tick that actually fires the
    /// `Included -> Finalized` transition, so a caller counting
    /// completions never counts one twice.
    pub fn update_robinhood_confirmations(
        &mut self,
        tx_id: i64,
        confirmations: i64,
        required: u64,
        now: i64,
    ) -> Result<bool, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let state: RobinhoodTxState = tx.query_row(
            "SELECT state FROM robinhood_transactions WHERE id = ?1",
            [tx_id],
            |r| r.get(0),
        )?;
        if state == RobinhoodTxState::Finalized {
            tx.execute(
                "UPDATE robinhood_transactions SET confirmations = ?2, updated_at = ?3
                 WHERE id = ?1",
                rusqlite::params![tx_id, confirmations, now],
            )?;
            tx.commit()?;
            return Ok(false);
        }
        if state != RobinhoodTxState::Included {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxWrongState {
                id: tx_id,
                expected: RobinhoodTxState::Included,
                actual: state,
            });
        }
        let promote = confirmations >= required as i64;
        tx.execute(
            "UPDATE robinhood_transactions
                SET confirmations = ?2,
                    state = CASE WHEN ?3 THEN 'Finalized' ELSE state END,
                    finalized_at = CASE WHEN ?3 THEN ?4 ELSE finalized_at END,
                    updated_at = ?4
             WHERE id = ?1",
            rusqlite::params![tx_id, confirmations, promote, now],
        )?;
        tx.commit()?;
        Ok(promote)
    }

    /// Stops one operation for a human, with a reason.
    ///
    /// The one transition that can be reached from any non-terminal
    /// state, and never automatically reversed. `Finalized` is
    /// deliberately NOT overridable: a completed operation cannot be
    /// un-completed by a later disagreement, and a disagreement about a
    /// finalized operation is a reserve-level incident rather than a
    /// per-row one.
    pub fn mark_robinhood_tx_manual_review(
        &mut self,
        tx_id: i64,
        reason: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let state: RobinhoodTxState = tx.query_row(
            "SELECT state FROM robinhood_transactions WHERE id = ?1",
            [tx_id],
            |r| r.get(0),
        )?;
        if state == RobinhoodTxState::Finalized {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: tx_id,
                detail: "a finalized operation cannot be moved to ManualReview: it completed, \
                         and a later disagreement about it is a reserve-level incident"
                    .to_string(),
            });
        }
        tx.execute(
            "UPDATE robinhood_transactions
                SET state = 'ManualReview', failure_reason = ?2, updated_at = ?3
             WHERE id = ?1",
            rusqlite::params![tx_id, reason, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records that the contract's own replay guard says this operation
    /// already executed — the settlement witness of last resort.
    ///
    /// Reached when a broadcast's fate is unknown (dropped transaction,
    /// receipt aged out of a node's index) and
    /// `requestExecuted(action, requestId)` answers `true`. The operation
    /// IS done; what is missing is only this service's record of it.
    ///
    /// Deliberately does NOT invent a receipt: `receipt_status` stays
    /// null and the row is marked `ManualReview` with an explicit reason,
    /// because "the contract says it happened" and "this service watched
    /// it happen" are different strengths of evidence and the difference
    /// belongs in the record. The reserve bookkeeping is reconciled by an
    /// operator with the on-chain event in front of them.
    pub fn record_robinhood_already_executed(
        &mut self,
        tx_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.mark_robinhood_tx_manual_review(
            tx_id,
            "the contract's replay guard reports this (action, requestId) as ALREADY EXECUTED, \
             but this service never observed a successful receipt for it. The operation \
             happened; only the local record of it is missing. Reconcile against the on-chain \
             event before touching this request — do NOT re-authorize it.",
            now,
        )
    }
}

#[cfg(test)]
mod tests;

impl Ledger {
    /// Folds one FINAL Robinhood deposit observation into exactly one
    /// `bridge_requests` row, and links the observation to it.
    ///
    /// # This is `fold_sol_deposit`'s twin, and deliberately so
    ///
    /// The two answer the same question about two chains: an irreversible
    /// deposit has been observed on a source chain, and a DESTINATION
    /// reserve is being asked to pay it out. Every gate below is the same
    /// gate, evaluated the same way, inside one write transaction so that
    /// the state a decision was made against and the decision itself
    /// commit or roll back together.
    ///
    /// # Which direction, and which reserve
    ///
    /// The observation's own `route` — read off the contract's
    /// `DepositCreated` log, never chosen here — decides:
    ///
    /// - `RhnToGlc` draws on `GoldcoinReserve`. `amounts.net_destination_atomic`
    ///   must equal `amounts.net_atomic` (Goldcoin-native IS canonical).
    /// - `RhnToSol` draws on `SolanaReserve`, accounted in the reserve
    ///   mint's own live decimals, so `amounts.net_destination_atomic` is
    ///   the net narrowed by `CanonicalAtomic::to_solana` — and a net
    ///   that does not narrow exactly never reaches this function as
    ///   payable: `robinhood::fold` parks it with an explicit `refusal`.
    ///
    /// # Two deliberate differences from the Solana twin, each with a reason
    ///
    /// - **The rolling-24h wallet windows are keyed per route.** Both
    ///   apply on both routes (`ledger::wallet_window`): the recorded
    ///   `depositor` on the source leg, and the destination bytes — a
    ///   Goldcoin address for `RhnToGlc`, a Solana pubkey for `RhnToSol`
    ///   — on the destination leg, each scoped to its own chain. The
    ///   Solana program's release window and the custody contract's
    ///   inbound window still bound an `RhnToSol` payout on-chain,
    ///   independently.
    /// - **The UTXO-pool backpressure applies to `RhnToGlc` only**, for
    ///   the obvious reason: only that payout comes out of the Goldcoin
    ///   vault's mature UTXO pool. The shared evaluator already skips the
    ///   floor for any reserve other than `GoldcoinReserve`.
    ///
    /// # A closed route parks rather than refuses
    ///
    /// `route_open == false` produces a `ManualReview` row, not an error.
    ///
    /// Every gate parks rather than drops (the Robinhood-side deposit is
    /// already real), each with its own `manual_review_note` so the cause
    /// is never ambiguous, and a park takes NO reserve capacity.
    /// The deposit already happened; see [`super::super::robinhood::fold`]'s
    /// module docs for why refusing to record it would be the worse
    /// outcome.
    pub fn fold_robinhood_deposit(
        &mut self,
        observation: &super::RobinhoodObservationRow,
        amounts: super::RequestAmounts,
        destination: Option<&[u8]>,
        route_open: bool,
        refusal: Option<&str>,
        now: i64,
    ) -> Result<crate::robinhood::fold::FoldOutcome, LedgerError> {
        use crate::robinhood::fold::FoldOutcome;

        let obligation_index = observation.observation.obligation_index;
        let source_contract = observation.observation.source_contract;
        let direction = match observation.observation.route {
            crate::routes::Route::RhnToGlc => super::Direction::RhnToGlc,
            crate::routes::Route::RhnToSol => super::Direction::RhnToSol,
            other => {
                return Err(LedgerError::RobinhoodTxInvalid {
                    id: observation.id,
                    detail: format!(
                        "observation {obligation_index} is on route {}, which is not a Robinhood \
                         deposit route",
                        other.as_str()
                    ),
                })
            }
        };
        if direction == super::Direction::RhnToGlc {
            assert_eq!(
                amounts.net_destination_atomic, amounts.net_atomic,
                "an RhnToGlc request reserves against the canonical-unit GoldcoinReserve row"
            );
        }

        let tx = write_tx(&mut self.conn)?;

        // The durable, chain-and-contract-qualified identity (schema
        // v21). Contract-scoped, unlike the Solana pre-check: every
        // Robinhood row was written by an indexer that knew exactly which
        // configured contract it read the log from, so there is no
        // legacy-sentinel case and obligation N under a successor
        // deployment is legitimately a different deposit.
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM bridge_requests
                 WHERE source_chain = 'robinhood'
                   AND source_contract = ?1
                   AND source_obligation_index = ?2",
                rusqlite::params![&source_contract[..], obligation_index as i64],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(request_id) = existing {
            tx.rollback()?;
            return Ok(FoldOutcome::AlreadyFolded { request_id });
        }

        // The destination reserve pays this out, so its gates decide.
        let reserve = direction.destination_reserve();
        let available = Self::reserve_headroom(&tx, reserve)?;

        // The confirmed-liquidity admission gate, evaluated and its
        // hysteresis state written HERE — inside the same transaction as
        // the admission decision it governs — exactly as
        // `fold_sol_deposit` does. A separate read could be overtaken by
        // a concurrent fold between the check and the write.
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

        // The recipient is the destination BYTES: for `RhnToGlc` an
        // opaque ASCII Goldcoin address exactly as `SolToGlc` stores it;
        // for `RhnToSol` the 32-byte Solana pubkey exactly as `GlcToSol`
        // stores it. When the destination is undeliverable the raw
        // payload is stored instead of a parsed form — the column must
        // record what the depositor actually asked for, including when
        // that is unusable, because it is the evidence a refund decision
        // rests on.
        //
        // Resolved HERE, before the admission gate rather than just before
        // the INSERT, because the rolling-24h destination limit below is
        // keyed on exactly these bytes.
        let recipient: Vec<u8> = match destination {
            Some(bytes) => bytes.to_vec(),
            None => observation.observation.destination.clone(),
        };

        // The two rolling-24h wallet windows (`ledger::wallet_window`),
        // the SAME rule `fold_sol_deposit` applies to a Solana obligation
        // — same window constant, same shared state exclude-list, same
        // matching semantics, read through the SAME ledger function
        // rather than a Robinhood-only reimplementation — keyed for this
        // observation's own route.
        //
        // The destination leg is scoped to the destination CHAIN across
        // every route paying out on it: for `RhnToGlc` a Goldcoin L1
        // address that just received a `SolToGlc` payout is blocked here
        // too (and vice versa); for `RhnToSol` a Solana pubkey that just
        // received a `GlcToSol` release likewise. An undeliverable
        // destination has no wallet to ask about and is parked for that
        // reason regardless.
        //
        // The source leg is keyed on the custody contract's own recorded
        // `depositor`, scoped to every Robinhood-sourced route, and
        // completely independent of any Solana wallet's window.
        let limits: crate::ledger::admission::InboundRateLimits =
            Self::route_wallet_eligibility_in(
                &tx,
                direction,
                Some(&observation.observation.depositor[..]),
                destination,
                now,
                crate::ledger::WalletWindowScope::NewRequest,
            )?
            .into();

        // THE admission decision, taken by the one shared evaluator
        // (`crate::ledger::admission`) that `fold_sol_deposit` and the
        // public API's per-route `available` also call. Every
        // direction-wide gate — `paused`, `admission_closed`, the
        // confirmed-liquidity hysteresis and its per-request buffer, the
        // mature-UTXO pool floor and the plain capacity check — is
        // evaluated there, once, in one ranking, together with this
        // route's own `route_admission` gate (v25/v27).
        //
        // What DOES stay here, ranked above the shared decision below:
        // this route's ENABLEMENT gate (`route_open`, a different axis —
        // `crate::routes::RouteGate`) and the destination's
        // deliverability. Neither is a property of any reserve.
        let gates = crate::ledger::admission::InboundAdmissionGates::read(
            &tx,
            direction,
            liquidity_admission_closed,
        )?;
        let reserve_blocker = gates.blocker(amounts.net_destination_atomic as i64, limits);

        // The rapid-burst rule (`ledger::rapid_burst`, schema v30), on
        // the custody contract's own recorded `depositor` and the
        // recipient — ranked above every other reason, as in
        // `fold_sol_deposit`.
        let burst = Self::rapid_burst_verdict_in(
            &tx,
            direction,
            Some(&observation.observation.depositor[..]),
            destination,
            now,
            crate::ledger::WalletWindowScope::NewRequest,
        )?;

        let payable = route_open
            && refusal.is_none()
            && destination.is_some()
            && reserve_blocker.is_none()
            && burst.is_none();

        // The refusal an operator sees, ranked most specific first. The
        // route-specific conditions are ranked above the shared
        // reserve-side ranking, which supplies the rest verbatim — so
        // the same reserve situation still produces the same
        // `manual_review_note` on either inbound route, and now cannot
        // stop doing so.
        let note: Option<String> = if payable {
            None
        } else if burst.is_some() {
            Some(Self::MANUAL_REVIEW_REASON_RAPID_BURST_HOLD.to_string())
        } else if let Some(explicit) = refusal {
            Some(explicit.to_string())
        } else if destination.is_none() {
            Some("undeliverable destination".to_string())
        } else if !route_open {
            Some(Self::MANUAL_REVIEW_REASON_ROUTE_DISABLED.to_string())
        } else {
            Some(
                reserve_blocker
                    .map(|b| b.manual_review_note())
                    .unwrap_or(Self::MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY)
                    .to_string(),
            )
        };

        let state = if payable {
            super::RequestState::SourceFinalized
        } else {
            super::RequestState::ManualReview
        };

        // `source_txid`/`source_vout` carry the deposit's own Robinhood
        // transaction hash and log index for `RhnToSol` — the source
        // identity the Solana `release_from_reserve` claim binds and the
        // `DepositClaim` PDA is keyed on (the same "one source
        // transaction, one index within it" shape as a Goldcoin
        // outpoint). Both are facts the indexer decoded from the
        // finalized log, never values chosen here. `RhnToGlc` rows keep
        // both NULL, exactly as before this route existed: nothing on
        // that route reads them, and its public `source_txid` stays
        // what it has always been.
        let (source_txid, source_vout): (Option<&[u8]>, Option<u32>) =
            if direction == super::Direction::RhnToSol {
                let log_index = u32::try_from(observation.observation.log_index).map_err(|_| {
                    LedgerError::RobinhoodTxInvalid {
                        id: observation.id,
                        detail: format!(
                            "observation {obligation_index}'s log index {} does not fit the \
                                 32-bit source index the release claim binds",
                            observation.observation.log_index
                        ),
                    }
                })?;
                (Some(&observation.observation.tx_hash[..]), Some(log_index))
            } else {
                (None, None)
            };

        tx.execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at,
                 reserved_at, source_chain, source_contract, source_obligation_index,
                 source_txid, source_vout,
                 source_block_height, source_block_hash, source_confirmations,
                 source_finalized_at, manual_review_note, source_wallet)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, 'robinhood', ?10, ?11,
                     ?12, ?13, ?14, ?15, 1, ?9, ?16, ?17)",
            rusqlite::params![
                direction,
                state,
                amounts.gross_atomic as i64,
                amounts.fee_bps as i64,
                amounts.fee_atomic as i64,
                amounts.net_atomic as i64,
                amounts.net_destination_atomic as i64,
                recipient,
                now,
                &source_contract[..],
                obligation_index as i64,
                source_txid,
                source_vout,
                observation.observation.block_number as i64,
                &observation.observation.block_hash[..],
                note.as_deref(),
                &observation.observation.depositor[..],
            ],
        )?;
        let request_id = tx.last_insert_rowid();
        super::log_transition(
            &tx,
            request_id,
            None,
            state,
            now,
            Some("fold_robinhood_deposit"),
            "system",
        )?;
        if let Some(matched) = &burst {
            Self::mark_rapid_burst_hold_in(&tx, request_id, matched, now)?;
        }

        // The link back to the observation. Its unique index is the
        // second half of the replay guard: one observation can name at
        // most one request, and one request can be named by at most one
        // observation.
        tx.execute(
            "UPDATE robinhood_deposit_observations SET folded_request_id = ?2 WHERE id = ?1",
            rusqlite::params![observation.id, request_id],
        )?;

        if payable {
            tx.execute(
                "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1,
                    pending_obligations = pending_obligations + ?1 WHERE direction = ?2",
                rusqlite::params![amounts.net_destination_atomic as i64, reserve],
            )?;
        }
        tx.commit()?;

        Ok(if payable {
            FoldOutcome::FoldedFinalized { request_id }
        } else {
            FoldOutcome::FoldedManualReview { request_id }
        })
    }

    /// FINAL observations that have not been folded yet, oldest first —
    /// what the fold phase polls.
    pub fn unfolded_final_robinhood_observations(
        &self,
    ) -> Result<Vec<super::RobinhoodObservationRow>, LedgerError> {
        self.robinhood_observations_where("finality = 'Final' AND folded_request_id IS NULL")
    }

    /// The observation a request was folded from, if any.
    pub fn robinhood_observation_for_request(
        &self,
        request_id: i64,
    ) -> Result<Option<super::RobinhoodObservationRow>, LedgerError> {
        Ok(self
            .robinhood_observations_where(&format!("folded_request_id = {request_id}"))?
            .pop())
    }

    /// Marks one observation settled — the local mirror of the
    /// contract's own `ObligationStatus::Settled`.
    ///
    /// Written only AFTER the on-chain `executeSettlement` transaction
    /// reached its configured confirmation depth, never on broadcast.
    /// The contract's status is the authority; this column exists so an
    /// operator can see the same fact without an RPC call.
    pub fn mark_robinhood_observation_settled(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        tx.execute(
            "UPDATE robinhood_deposit_observations
                SET settled = 1 WHERE folded_request_id = ?1",
            [request_id],
        )?;
        let _ = now;
        tx.commit()?;
        Ok(())
    }
}

impl Ledger {
    /// `DestinationSubmitted -> DestinationConfirmed` (and, for `GlcToRhn`,
    /// straight on to `Settled`) for a request whose `executePayout`
    /// transaction reached the configured Robinhood confirmation depth.
    ///
    /// # Why `GlcToRhn` settles here and `SolToRhn` does not
    ///
    /// A `GlcToRhn` payout IS the settlement: the GLC has left the
    /// custody contract and reached the recipient, and there is no
    /// further on-chain step. That is exactly the shape
    /// [`Ledger::mark_release_confirmed`] has for `GlcToSol`, whose
    /// `release_from_reserve` instruction likewise both moves the funds
    /// and creates the replay guard in one transaction — and this mirrors
    /// it deliberately rather than inventing a second shape.
    ///
    /// A `SolToRhn` payout is the destination leg only. The value has
    /// left the Robinhood reserve — so the reserve accounting below moves
    /// NOW, for both directions — but the Solana `WithdrawalObligation`
    /// that funded it is still `Pending` on-chain, i.e. still refundable
    /// by `refund_withdraw`. Closing it (`record_goldcoin_completion`,
    /// with this payout's transaction hash as the recorded payout id) is
    /// the same close-out `SolToGlc` performs after its Goldcoin payout
    /// confirmed, and the request reaches `Settled` only when that
    /// completion is confirmed on Solana
    /// ([`Ledger::mark_robinhood_payout_completion_confirmed`]).
    ///
    /// The `RhnToGlc` direction is NOT like either: its payout and its
    /// settlement are two transactions on two chains, and the second must
    /// follow the first. See
    /// [`Ledger::mark_robinhood_settlement_confirmed`].
    ///
    /// Moves the amount out of `reserved_liquidity`/`pending_obligations`
    /// into `settled_liquidity_total` for the ROBINHOOD reserve, and
    /// decrements its cached balance — the same "keep the cache
    /// self-consistent with a settlement this service itself caused"
    /// discipline `mark_release_confirmed` documents, so reconciliation
    /// never reads a routine settlement as an unexplained breach. The fee
    /// accrues on the SOURCE reserve.
    ///
    /// Idempotent: a no-op if already at or past the state it produces.
    pub fn mark_robinhood_payout_settled(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, state, amount, fee): (super::Direction, super::RequestState, i64, i64) = tx
            .query_row(
                "SELECT direction, state, net_destination_atomic, fee_amount_atomic
                 FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
        if !direction.destination_is_robinhood() {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: request_id,
                detail: format!(
                    "mark_robinhood_payout_settled on a {} request",
                    direction.as_str()
                ),
            });
        }
        if state == super::RequestState::Settled
            || (state == super::RequestState::DestinationConfirmed
                && !direction.settles_on_payout())
        {
            tx.rollback()?;
            return Ok(());
        }

        let mut transitions = vec![(state, super::RequestState::DestinationConfirmed)];
        if direction.settles_on_payout() {
            transitions.push((
                super::RequestState::DestinationConfirmed,
                super::RequestState::Settled,
            ));
        }
        for (from, to) in transitions {
            tx.execute(
                "UPDATE bridge_requests SET state = ?2 WHERE id = ?1",
                rusqlite::params![request_id, to],
            )?;
            super::log_transition(&tx, request_id, Some(from), to, now, None, "system")?;
        }
        if direction.settles_on_payout() {
            tx.execute(
                "UPDATE bridge_requests SET settled_at = ?1 WHERE id = ?2",
                rusqlite::params![now, request_id],
            )?;
        }

        tx.execute(
            "UPDATE reserve_ledger
                SET reserved_liquidity = reserved_liquidity - ?1,
                    pending_obligations = pending_obligations - ?1,
                    settled_liquidity_total = settled_liquidity_total + ?1,
                    total_reserve_balance = total_reserve_balance - ?1
             WHERE direction = 'RobinhoodReserve'",
            [amount],
        )?;
        // The fee for a Robinhood payout is collected on the SOURCE side —
        // Goldcoin for `GlcToRhn`, Solana for `SolToRhn` — where it was
        // actually withheld from the deposit (docs/20-bridge-fee.md: "the
        // fee remains on the source side where it was collected").
        // Canonical units, on a separate row, never netted against the
        // Robinhood reserve's own columns.
        tx.execute(
            "UPDATE reserve_ledger SET accrued_fees_atomic = accrued_fees_atomic + ?1
             WHERE direction = ?2",
            rusqlite::params![fee, direction.source_reserve()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records that the `record_goldcoin_completion` transaction closing a
    /// `SolToRhn` request's Solana obligation has been submitted —
    /// `signature` is its Solana transaction signature — on the request's
    /// finalized `Payout` operation row, the `SolToRhn` mirror of
    /// [`Ledger::record_goldcoin_completion_submitted`]. A no-op once the
    /// request is `Settled`.
    pub fn record_robinhood_payout_completion_submitted(
        &mut self,
        request_id: i64,
        signature: [u8; 64],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, state): (super::Direction, super::RequestState) = tx.query_row(
            "SELECT direction, state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if direction != super::Direction::SolToRhn {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: request_id,
                detail: format!(
                    "record_robinhood_payout_completion_submitted on a {} request",
                    direction.as_str()
                ),
            });
        }
        if state == super::RequestState::Settled {
            tx.rollback()?;
            return Ok(());
        }
        if state != super::RequestState::DestinationConfirmed {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: request_id,
                detail: format!(
                    "a Solana completion is recorded only once the Robinhood payout is FINAL \
                     (DestinationConfirmed), but this request is in {}",
                    state.as_str()
                ),
            });
        }
        let updated = tx.execute(
            "UPDATE robinhood_transactions
                SET onchain_completion_signature = ?1, onchain_completion_submitted_at = ?2
              WHERE request_id = ?3 AND kind = 'Payout' AND state = 'Finalized'",
            rusqlite::params![signature.as_slice(), now, request_id],
        )?;
        if updated != 1 {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: request_id,
                detail: "no FINALIZED Robinhood payout row exists to record the completion on"
                    .to_string(),
            });
        }
        tx.commit()?;
        Ok(())
    }

    /// The recorded Solana completion submission for a `SolToRhn`
    /// request's payout row: `(signature, submitted_at)`, or `None` when
    /// none has been recorded (or no finalized payout row exists).
    pub fn robinhood_payout_completion_submission(
        &self,
        request_id: i64,
    ) -> Result<Option<([u8; 64], i64)>, LedgerError> {
        let row: Option<(Option<Vec<u8>>, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT onchain_completion_signature, onchain_completion_submitted_at
                   FROM robinhood_transactions
                  WHERE request_id = ?1 AND kind = 'Payout' AND state = 'Finalized'",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((Some(sig), Some(at))) => {
                let sig = <[u8; 64]>::try_from(sig.as_slice()).map_err(|_| {
                    LedgerError::RobinhoodTxInvalid {
                        id: request_id,
                        detail: "the recorded completion signature is not 64 bytes".to_string(),
                    }
                })?;
                Some((sig, at))
            }
            _ => None,
        })
    }

    /// `DestinationConfirmed -> Settled` for a `SolToRhn` request whose
    /// `record_goldcoin_completion` confirmed on Solana — the mirror of
    /// [`Ledger::mark_goldcoin_completion_confirmed`], gated the same way
    /// on the submission having been recorded first, so this service
    /// never declares a `SolToRhn` request `Settled` on the strength of
    /// its own database alone.
    ///
    /// Touches NO reserve counter: the Robinhood reserve accounting moved
    /// when the payout finalized ([`Ledger::mark_robinhood_payout_settled`]),
    /// which is when the value actually left. Idempotent.
    pub fn mark_robinhood_payout_completion_confirmed(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, state): (super::Direction, super::RequestState) = tx.query_row(
            "SELECT direction, state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if direction != super::Direction::SolToRhn {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: request_id,
                detail: format!(
                    "mark_robinhood_payout_completion_confirmed on a {} request",
                    direction.as_str()
                ),
            });
        }
        if state == super::RequestState::Settled {
            tx.rollback()?;
            return Ok(());
        }
        if state != super::RequestState::DestinationConfirmed {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: request_id,
                detail: format!(
                    "a SolToRhn request settles from DestinationConfirmed, but this one is in {}",
                    state.as_str()
                ),
            });
        }
        let has_submission: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM robinhood_transactions
                  WHERE request_id = ?1 AND kind = 'Payout'
                    AND onchain_completion_signature IS NOT NULL",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        if has_submission.is_none() {
            tx.rollback()?;
            return Err(LedgerError::CompletionNotSubmitted(request_id));
        }
        tx.execute(
            "UPDATE bridge_requests SET state = 'Settled', settled_at = ?1 WHERE id = ?2",
            rusqlite::params![now, request_id],
        )?;
        super::log_transition(
            &tx,
            request_id,
            Some(state),
            super::RequestState::Settled,
            now,
            None,
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `DestinationConfirmed -> Settled` for a Robinhood-SOURCED request
    /// (`RhnToGlc`, `RhnToSol`) whose `executeSettlement` transaction
    /// reached the configured Robinhood confirmation depth.
    ///
    /// # The precondition this enforces
    ///
    /// The request must ALREADY be in `DestinationConfirmed`, which it
    /// reaches only when its destination payout was verified final — the
    /// Goldcoin payout at the required depth
    /// (`Ledger::update_goldcoin_payout_confirmations`) for `RhnToGlc`,
    /// the Solana release at `finalized` commitment
    /// (`Ledger::mark_release_confirmed`) for `RhnToSol`. This function
    /// does not create that state and cannot be reached without it, so
    /// there is no path by which an obligation is marked settled before
    /// the payout that justifies it confirmed.
    ///
    /// # Accounting differs by destination, for one reason
    ///
    /// `RhnToGlc`'s Goldcoin reserve accounting moves HERE, the same as
    /// [`Ledger::mark_goldcoin_completion_confirmed`]'s (the same reserve
    /// pays out, in the same units, and the Goldcoin vault's balance is
    /// UTXO-reconciled, so a payout that has confirmed but not completed
    /// is explained by `pending_destination_settlement_amount`). The
    /// payout row is likewise closed.
    ///
    /// `RhnToSol`'s Solana reserve accounting already moved when the
    /// release confirmed (`mark_release_confirmed`), because the Solana
    /// reserve's cached balance is compared against a live token-account
    /// read and must reflect a release the moment it is final. Nothing
    /// moves here; this is the state transition only.
    ///
    /// Idempotent: a no-op if already `Settled`.
    pub fn mark_robinhood_settlement_confirmed(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, state, amount, fee): (super::Direction, super::RequestState, i64, i64) = tx
            .query_row(
                "SELECT direction, state, net_destination_atomic, fee_amount_atomic
                 FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
        if !direction.source_is_robinhood() {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: request_id,
                detail: format!(
                    "mark_robinhood_settlement_confirmed on a {} request",
                    direction.as_str()
                ),
            });
        }
        if state == super::RequestState::Settled {
            tx.rollback()?;
            return Ok(());
        }
        if state != super::RequestState::DestinationConfirmed {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: request_id,
                detail: format!(
                    "an obligation may only be settled from DestinationConfirmed — the state a \
                     request reaches when its destination payout confirmed — but this request \
                     is in {}",
                    state.as_str()
                ),
            });
        }

        if direction == super::Direction::RhnToGlc {
            tx.execute(
                "UPDATE goldcoin_payouts SET state = 'Completed', completed_at = ?1
                 WHERE request_id = ?2 AND state = 'Confirmed'",
                rusqlite::params![now, request_id],
            )?;
        }
        tx.execute(
            "UPDATE bridge_requests SET state = 'Settled', settled_at = ?1 WHERE id = ?2",
            rusqlite::params![now, request_id],
        )?;
        super::log_transition(
            &tx,
            request_id,
            Some(state),
            super::RequestState::Settled,
            now,
            None,
            "system",
        )?;
        if direction == super::Direction::RhnToGlc {
            tx.execute(
                "UPDATE reserve_ledger
                    SET reserved_liquidity = reserved_liquidity - ?1,
                        pending_obligations = pending_obligations - ?1,
                        settled_liquidity_total = settled_liquidity_total + ?1,
                        total_reserve_balance = total_reserve_balance - ?1
                 WHERE direction = 'GoldcoinReserve'",
                [amount],
            )?;
            // The fee was withheld on the SOURCE side: Robinhood.
            tx.execute(
                "UPDATE reserve_ledger SET accrued_fees_atomic = accrued_fees_atomic + ?1
                 WHERE direction = 'RobinhoodReserve'",
                [fee],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// `ManualReview -> RefundPending` for a Robinhood-SOURCED request
    /// (`RhnToGlc`, `RhnToSol`) whose refund has been authorized.
    ///
    /// From this state on the request is permanently ineligible for a
    /// Goldcoin payout: the refund lifecycle is one-way
    /// (`RefundPending -> RefundBroadcast -> Refunded`) and never returns
    /// to `ManualReview`. That is the same one-way discipline
    /// `Ledger::begin_solana_refund` established, and it is what makes
    /// "refunded AND paid out" unreachable rather than merely unlikely.
    pub fn mark_robinhood_refund_pending(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, state): (super::Direction, super::RequestState) = tx.query_row(
            "SELECT direction, state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if !direction.source_is_robinhood() {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: request_id,
                detail: format!(
                    "mark_robinhood_refund_pending on a {} request",
                    direction.as_str()
                ),
            });
        }
        if state.is_refund_lifecycle() {
            tx.rollback()?;
            return Ok(());
        }
        if state != super::RequestState::ManualReview {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: request_id,
                detail: format!(
                    "a refund begins from ManualReview — a human's decision not to complete the \
                     deposit — but this request is in {}",
                    state.as_str()
                ),
            });
        }
        tx.execute(
            "UPDATE bridge_requests SET state = 'RefundPending' WHERE id = ?1",
            [request_id],
        )?;
        super::log_transition(
            &tx,
            request_id,
            Some(state),
            super::RequestState::RefundPending,
            now,
            Some("robinhood_refund_authorized"),
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `RefundPending`/`RefundBroadcast -> Refunded` for a
    /// Robinhood-SOURCED request whose `executeRefund` transaction reached
    /// the configured confirmation depth.
    ///
    /// Terminal. Like `Settled`, nothing ever transitions out of it.
    ///
    /// Deliberately touches NO reserve counter. A refunded deposit never
    /// held a Goldcoin reservation in the first place — it was parked in
    /// `ManualReview` before any reservation was applied — and the
    /// principal it returns was never part of the bridge's own reserve:
    /// it was the depositor's, held as an encumbrance against it. The
    /// contract's `encumberedReserve` is the figure that moves, and it
    /// moves on-chain.
    pub fn mark_robinhood_refund_confirmed(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, state): (super::Direction, super::RequestState) = tx.query_row(
            "SELECT direction, state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if !direction.source_is_robinhood() {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: request_id,
                detail: format!(
                    "mark_robinhood_refund_confirmed on a {} request",
                    direction.as_str()
                ),
            });
        }
        if state == super::RequestState::Refunded {
            tx.rollback()?;
            return Ok(());
        }
        if !matches!(
            state,
            super::RequestState::RefundPending | super::RequestState::RefundBroadcast
        ) {
            tx.rollback()?;
            return Err(LedgerError::RobinhoodTxInvalid {
                id: request_id,
                detail: format!(
                    "only a request already in the refund lifecycle can become Refunded, but \
                     this one is in {}",
                    state.as_str()
                ),
            });
        }
        tx.execute(
            "UPDATE bridge_requests SET state = 'Refunded' WHERE id = ?1",
            [request_id],
        )?;
        super::log_transition(
            &tx,
            request_id,
            Some(state),
            super::RequestState::Refunded,
            now,
            None,
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Parks one bridge request for a human, releasing any reservation it
    /// held.
    ///
    /// Reached when a Robinhood transaction reverts: a reverted payout
    /// means a user is owed money that did not move, a reverted
    /// settlement means an obligation is still refundable after its
    /// Goldcoin payout confirmed, and a reverted refund means a
    /// depositor's principal is still held. All three need a human.
    ///
    /// The reservation is released because it no longer describes
    /// anything: no transaction is in flight against it, and leaving it
    /// would hold capacity that nothing will ever consume. The FUNDS are
    /// untouched — releasing a reservation is bookkeeping, not a
    /// transfer.
    ///
    /// A `Settled` or `Refunded` request is left alone: those are
    /// terminal, and a later disagreement about one is a reserve-level
    /// incident rather than a per-request state change.
    pub fn mark_robinhood_request_manual_review(
        &mut self,
        request_id: i64,
        reason: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, state, amount): (super::Direction, super::RequestState, i64) = tx
            .query_row(
                "SELECT direction, state, net_destination_atomic FROM bridge_requests
                 WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
        if matches!(
            state,
            super::RequestState::Settled | super::RequestState::Refunded
        ) {
            tx.rollback()?;
            return Ok(());
        }
        if state == super::RequestState::ManualReview {
            tx.rollback()?;
            return Ok(());
        }

        // A reservation exists only while the request is in an active
        // state; releasing one that was never taken would drive the
        // counters negative.
        if state.is_active() {
            let reserve = direction.destination_reserve();
            tx.execute(
                "UPDATE reserve_ledger
                    SET reserved_liquidity = reserved_liquidity - ?1,
                        pending_obligations = pending_obligations - ?1
                 WHERE direction = ?2",
                rusqlite::params![amount, reserve],
            )?;
        }

        tx.execute(
            "UPDATE bridge_requests SET state = 'ManualReview', manual_review_note = ?2
             WHERE id = ?1",
            rusqlite::params![request_id, reason],
        )?;
        super::log_transition(
            &tx,
            request_id,
            Some(state),
            super::RequestState::ManualReview,
            now,
            Some(reason),
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }
}

// ------------------------------------ the payout-not-started proof (J) --

/// One piece of DURABLE evidence that Robinhood payout activity has begun
/// — or that this request's Robinhood linkage is not what it should be.
///
/// # Why this type exists
///
/// A `GlcToRhn` request's Goldcoin deposit can be refunded only while it
/// is certain that no custody payout was ever started for it. The
/// `GlcToSol` refund proves the equivalent thing with Solana-shaped
/// evidence — `bridge_requests.destination_txid`,
/// `settlement_claim_hash`, and the on-chain `DepositClaim` PDA — and a
/// Robinhood payout writes NONE of those. Reusing that proof for
/// `GlcToRhn` would be asking three columns that are NULL by
/// construction and reading their silence as an all-clear.
///
/// So the proof is route-specific, and this enum is its vocabulary: each
/// variant is a distinct, durable fact that forbids a refund, carrying
/// enough detail for an operator to see WHY without exposing anything
/// sensitive (see [`RobinhoodPayoutEvidence::reason`]).
///
/// # Why the mere EXISTENCE of a payout row is disqualifying
///
/// [`Ledger::begin_robinhood_tx`] writes the row in `Authorizing`
/// **before the first custody domain is contacted**, and nothing in this
/// service ever deletes a `robinhood_transactions` row. The row is
/// therefore the EARLIEST and a PERMANENT witness: every later step —
/// signatures, nonce allocation, signing, broadcast, replacement,
/// receipt, finality, revert — requires it to already exist, and none of
/// them can erase it. Refusing on the row alone is thus both the
/// simplest predicate and the strictest one, and it needs no reasoning
/// about which fields are populated in which order.
///
/// It is also not a trap for legitimate refunds. A request is refundable
/// only from `ManualReview`, and `Settler::tick_authorize` only ever
/// picks up requests in `SourceFinalized` — so a parked request has no
/// payout row, and a request with a payout row is not parked. The two
/// populations do not overlap in normal operation; an overlap IS the
/// anomaly this refuses on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobinhoodPayoutEvidence {
    /// A `robinhood_transactions` row of kind `Payout` names this
    /// request. Payout activity has begun; how far it got is detail, not
    /// a distinction that could make a refund safe.
    PayoutOperation {
        tx_id: i64,
        state: RobinhoodTxState,
        /// How many authorization signatures are durably stored. The
        /// contract requires exactly two; anything above zero means a
        /// custody domain has already signed for this payout.
        signatures: i64,
        /// A nonce is durably owned by this operation, so the submitter's
        /// sequence is committed to it.
        nonce_allocated: bool,
        /// Signed transaction bytes are persisted. Their CONTENT is never
        /// reported.
        signed_bytes_present: bool,
        /// The hash of those bytes is persisted, so a specific
        /// transaction identity exists and may be findable on chain.
        tx_hash_present: bool,
        broadcast_attempts: i64,
        replacement_attempts: i64,
        /// `Some(1)` included and succeeded, `Some(0)` included and
        /// REVERTED, `None` no receipt read back.
        receipt_status: Option<i64>,
    },
    /// A `robinhood_transactions` row of a kind that must never name a
    /// Goldcoin-sourced request. `Settlement` and `Refund` belong to
    /// `RhnToGlc`; finding one here means the linkage between the two
    /// tables disagrees with itself.
    UnexpectedOperation {
        tx_id: i64,
        kind: RobinhoodTxKind,
        state: RobinhoodTxState,
    },
    /// A Robinhood DEPOSIT observation was folded into this request. A
    /// Goldcoin-sourced request is funded by a Goldcoin outpoint and must
    /// never appear as the destination of an inbound Robinhood fold.
    UnexpectedDepositFold { observation_index: i64 },
}

impl RobinhoodPayoutEvidence {
    /// A short, stable machine-readable code for the operator surface, so
    /// a runbook can name a case without quoting prose.
    pub fn code(&self) -> &'static str {
        match self {
            RobinhoodPayoutEvidence::PayoutOperation { state, .. } => match state {
                RobinhoodTxState::Authorizing => "payout_authorizing",
                RobinhoodTxState::Authorized => "payout_authorized",
                RobinhoodTxState::Signed => "payout_signed",
                RobinhoodTxState::Broadcast => "payout_broadcast",
                RobinhoodTxState::Included => "payout_included",
                RobinhoodTxState::Finalized => "payout_finalized",
                RobinhoodTxState::Reverted => "payout_reverted",
                RobinhoodTxState::ManualReview => "payout_manual_review",
            },
            RobinhoodPayoutEvidence::UnexpectedOperation { .. } => "unexpected_robinhood_operation",
            RobinhoodPayoutEvidence::UnexpectedDepositFold { .. } => "unexpected_deposit_fold",
        }
    }

    /// The operator-facing explanation.
    ///
    /// Deliberately reports the SHAPE of what exists — a nonce was
    /// allocated, bytes were signed, a hash exists, N attempts were made
    /// — and never the signed transaction bytes, the signatures, the
    /// submitter key or any endpoint. An operator deciding whether a
    /// refund is safe needs to know that payout state exists, not what it
    /// contains; `glc-admin robinhood-tx-show` is the deliberate,
    /// separately-invoked place for detail.
    pub fn reason(&self) -> String {
        match self {
            RobinhoodPayoutEvidence::PayoutOperation {
                tx_id,
                state,
                signatures,
                nonce_allocated,
                signed_bytes_present,
                tx_hash_present,
                broadcast_attempts,
                replacement_attempts,
                receipt_status,
            } => {
                let mut reached = Vec::new();
                if *signatures > 0 {
                    reached.push(format!("{signatures} authorization signature(s) persisted"));
                }
                if *nonce_allocated {
                    reached.push("a submitter nonce is allocated".to_string());
                }
                if *signed_bytes_present {
                    reached.push("signed transaction bytes are persisted".to_string());
                }
                if *tx_hash_present {
                    reached.push("a transaction hash is persisted".to_string());
                }
                if *broadcast_attempts > 0 {
                    reached.push(format!("{broadcast_attempts} broadcast attempt(s)"));
                }
                if *replacement_attempts > 0 {
                    reached.push(format!("{replacement_attempts} replacement attempt(s)"));
                }
                match receipt_status {
                    Some(1) => reached.push("a SUCCESSFUL receipt was read back".to_string()),
                    Some(0) => reached.push("a REVERTED receipt was read back".to_string()),
                    _ => {}
                }
                let detail = if reached.is_empty() {
                    "the authorization payload is fixed but no step beyond it is recorded"
                        .to_string()
                } else {
                    reached.join("; ")
                };
                format!(
                    "a Robinhood payout operation (robinhood_transactions id {tx_id}) exists for \
                     this request in state {}: {detail}. A Goldcoin refund would return the \
                     deposit that payout is drawn against",
                    state.as_str()
                )
            }
            RobinhoodPayoutEvidence::UnexpectedOperation { tx_id, kind, state } => format!(
                "robinhood_transactions id {tx_id} is a {} in state {} but names this \
                 Goldcoin-sourced request; that linkage should not exist and this ledger's \
                 Robinhood state cannot be trusted for a refund decision until a human has \
                 looked at it",
                kind.as_str(),
                state.as_str()
            ),
            RobinhoodPayoutEvidence::UnexpectedDepositFold { observation_index } => format!(
                "Robinhood deposit observation {observation_index} records this request as the \
                 row it folded into, but a Goldcoin-sourced request is funded by a Goldcoin \
                 outpoint and can never be the destination of an inbound fold; the ledger's \
                 Robinhood state disagrees with itself"
            ),
        }
    }
}

impl Ledger {
    /// Every durable reason a Goldcoin refund for `request_id` must be
    /// refused on Robinhood grounds. **Empty means proven not started.**
    ///
    /// Read-only, and answered entirely from committed database rows —
    /// never from daemon memory, a tick report, or a live chain read. A
    /// restart therefore changes nothing: an in-flight payout looks
    /// exactly as disqualifying after a crash as before one, which is the
    /// property that makes this usable as a refund precondition at all.
    ///
    /// Run for BOTH Goldcoin-sourced directions, though it is only the
    /// PROOF for `GlcToRhn`. For `GlcToSol` the Solana-shaped proof
    /// remains the authority and is untouched; this runs alongside it as
    /// a tripwire, because a Robinhood row naming a `GlcToSol` request is
    /// a contradiction and a refund is not the moment to discover one.
    /// Neither direction's proof is weakened to accommodate the other —
    /// each keeps its own, and each additionally requires the other's
    /// evidence to be absent.
    pub fn robinhood_payout_evidence(
        &self,
        request_id: i64,
    ) -> Result<Vec<RobinhoodPayoutEvidence>, LedgerError> {
        Self::robinhood_payout_evidence_in(&self.conn, request_id)
    }

    /// [`Ledger::robinhood_payout_evidence`] against an arbitrary
    /// connection, so [`Ledger::begin_goldcoin_refund`] can re-run the
    /// identical query inside its own write transaction. One
    /// implementation: the printable dry-run view and the enforced gate
    /// cannot drift.
    pub(crate) fn robinhood_payout_evidence_in(
        conn: &rusqlite::Connection,
        request_id: i64,
    ) -> Result<Vec<RobinhoodPayoutEvidence>, LedgerError> {
        let mut evidence = Vec::new();

        // The tables arrived with schema v23. A ledger that predates them
        // has no Robinhood state at all, which is the strongest possible
        // "not started" — but it is checked rather than assumed, because
        // a missing table must not surface as an opaque SQL error on a
        // path whose whole job is to be legible.
        if super::schema::table_exists(conn, "robinhood_transactions")? {
            let mut stmt = conn.prepare(
                "SELECT t.id, t.kind, t.state, t.nonce, t.raw_tx IS NOT NULL,
                        t.tx_hash IS NOT NULL, t.broadcast_attempts, t.replacement_attempts,
                        t.receipt_status,
                        (SELECT COUNT(*) FROM robinhood_authorization_signatures s
                          WHERE s.transaction_id = t.id)
                 FROM robinhood_transactions t
                 WHERE t.request_id = ?1
                 ORDER BY t.id",
            )?;
            let rows = stmt
                .query_map([request_id], |r| {
                    let kind: RobinhoodTxKind = r.get(1)?;
                    let state: RobinhoodTxState = r.get(2)?;
                    let nonce: Option<i64> = r.get(3)?;
                    Ok(match kind {
                        RobinhoodTxKind::Payout => RobinhoodPayoutEvidence::PayoutOperation {
                            tx_id: r.get(0)?,
                            state,
                            signatures: r.get(9)?,
                            nonce_allocated: nonce.is_some(),
                            signed_bytes_present: r.get(4)?,
                            tx_hash_present: r.get(5)?,
                            broadcast_attempts: r.get(6)?,
                            replacement_attempts: r.get(7)?,
                            receipt_status: r.get(8)?,
                        },
                        // A withdrawal never carries a request_id, so this
                        // query cannot return one; listed so the match is
                        // total rather than silently defaulted.
                        RobinhoodTxKind::Settlement
                        | RobinhoodTxKind::Refund
                        | RobinhoodTxKind::TreasuryWithdraw => {
                            RobinhoodPayoutEvidence::UnexpectedOperation {
                                tx_id: r.get(0)?,
                                kind,
                                state,
                            }
                        }
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            evidence.extend(rows);
        }

        if super::schema::table_exists(conn, "robinhood_deposit_observations")? {
            let mut stmt = conn.prepare(
                "SELECT source_obligation_index FROM robinhood_deposit_observations
                 WHERE folded_request_id = ?1 ORDER BY source_obligation_index",
            )?;
            let rows = stmt
                .query_map([request_id], |r| {
                    Ok(RobinhoodPayoutEvidence::UnexpectedDepositFold {
                        observation_index: r.get(0)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            evidence.extend(rows);
        }

        Ok(evidence)
    }
}
