//! SQLite schema for the reserve ledger and chain indexers
//! (docs/06-schema.md). Single-file embedded database (rusqlite, bundled
//! SQLite) — same persistence choice the old bridge made for the same
//! reason (docs/01-reuse-inventory.md): a transactional, crash-safe,
//! zero-external-dependency store the indexer and ledger can share.
//!
//! Migrations are forward-only and numbered, applied at startup — same
//! discipline the old bridge's `db.rs` used. This repository starts fresh
//! at schema version 1 (docs/08-migration-strategy.md: there is no live
//! system to migrate data from).

use rusqlite::Connection;

use super::LedgerError;

const CURRENT_SCHEMA_VERSION: i64 = 29;

pub fn open_and_migrate(conn: &Connection) -> Result<(), LedgerError> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(LedgerError::from)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(LedgerError::from)?;

    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);")?;
    let current: Option<i64> = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
            r.get(0)
        })
        .ok();

    // FORWARD-COMPATIBILITY GUARD: refuse a database written by a NEWER
    // binary than this one, instead of silently rewriting its version
    // marker downward.
    //
    // Without this, the `UPDATE schema_version SET version = ?1` at the
    // end of the migration ladder below unconditionally stamps THIS
    // binary's version onto whatever it opened — so rolling back to an
    // older binary would quietly relabel a newer database as older,
    // while that database still physically carries the newer structures
    // and rows. The tables themselves survive (every migration is
    // structurally idempotent, so rolling forward re-applies cleanly and
    // loses nothing), but the version marker would have been a lie in
    // the meantime, and any operator or audit reading it would be
    // misled. Fail loudly and refuse to touch the database at all
    // instead — a rollback that needs an older binary must be a
    // deliberate, evidenced decision (restore a pre-upgrade backup via
    // `scripts/restore-ledger.sh`), never an accident this code paves
    // over. See docs/09-runbook.md "Schema rollback".
    if let Some(current) = current {
        if current > CURRENT_SCHEMA_VERSION {
            return Err(LedgerError::SchemaTooNew {
                found: current,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
    }

    if current.is_none() {
        apply_v1(conn)?;
        apply_v2(conn)?;
        apply_v3(conn)?;
        apply_v4(conn)?;
        apply_v5(conn)?;
        apply_v6(conn)?;
        apply_v7(conn)?;
        apply_v8(conn)?;
        apply_v9(conn)?;
        apply_v10(conn)?;
        apply_v11(conn)?;
        apply_v12(conn)?;
        apply_v13(conn)?;
        apply_v14(conn)?;
        apply_v15(conn)?;
        apply_v16(conn)?;
        apply_v17(conn)?;
        apply_v18(conn)?;
        apply_v19(conn)?;
        apply_v20(conn)?;
        apply_v21(conn)?;
        apply_v22(conn)?;
        apply_v23(conn)?;
        apply_v24(conn)?;
        apply_v25(conn)?;
        apply_v26(conn)?;
        apply_v27(conn)?;
        apply_v28(conn)?;
        apply_v29(conn)?;
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [CURRENT_SCHEMA_VERSION],
        )?;
    } else {
        if current == Some(1) {
            apply_v2(conn)?;
        }
        if current < Some(3) {
            apply_v3(conn)?;
        }
        if current < Some(4) {
            apply_v4(conn)?;
        }
        if current < Some(5) {
            apply_v5(conn)?;
        }
        if current < Some(6) {
            apply_v6(conn)?;
        }
        if current < Some(7) {
            apply_v7(conn)?;
        }
        if current < Some(8) {
            apply_v8(conn)?;
        }
        if current < Some(9) {
            apply_v9(conn)?;
        }
        if current < Some(10) {
            apply_v10(conn)?;
        }
        if current < Some(11) {
            apply_v11(conn)?;
        }
        if current < Some(12) {
            apply_v12(conn)?;
        }
        if current < Some(13) {
            apply_v13(conn)?;
        }
        if current < Some(14) {
            apply_v14(conn)?;
        }
        if current < Some(15) {
            apply_v15(conn)?;
        }
        if current < Some(16) {
            apply_v16(conn)?;
        }
        if current < Some(17) {
            apply_v17(conn)?;
        }
        if current < Some(18) {
            apply_v18(conn)?;
        }
        if current < Some(19) {
            apply_v19(conn)?;
        }
        if current < Some(20) {
            apply_v20(conn)?;
        }
        if current < Some(21) {
            apply_v21(conn)?;
        }
        if current < Some(22) {
            apply_v22(conn)?;
        }
        if current < Some(23) {
            apply_v23(conn)?;
        }
        if current < Some(24) {
            apply_v24(conn)?;
        }
        if current < Some(25) {
            apply_v25(conn)?;
        }
        if current < Some(26) {
            apply_v26(conn)?;
        }
        if current < Some(27) {
            apply_v27(conn)?;
        }
        if current < Some(28) {
            apply_v28(conn)?;
        }
        if current < Some(29) {
            apply_v29(conn)?;
        }
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            [CURRENT_SCHEMA_VERSION],
        )?;
    }

    Ok(())
}

fn apply_v1(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        -- ---------------------------------------------------- Goldcoin chain tracking --
        CREATE TABLE goldcoin_indexed_blocks (
            height      INTEGER PRIMARY KEY,
            hash        BLOB NOT NULL UNIQUE,
            prev_hash   BLOB NOT NULL,
            block_time  INTEGER NOT NULL,
            indexed_at  INTEGER NOT NULL
        );

        CREATE TABLE goldcoin_reorg_events (
            id              INTEGER PRIMARY KEY,
            detected_at     INTEGER NOT NULL,
            fork_height     INTEGER NOT NULL,
            old_tip_height  INTEGER NOT NULL,
            old_tip_hash    BLOB NOT NULL,
            new_tip_height  INTEGER NOT NULL,
            new_tip_hash    BLOB NOT NULL,
            orphaned_count  INTEGER NOT NULL
        );

        -- ----------------------------------------------------- Solana chain tracking --
        -- Singleton: what the Solana indexer last observed, at finalized
        -- commitment. No block-level reorg tracking is needed here (unlike
        -- Goldcoin) because finalized commitment does not reorg in normal
        -- operation (docs/03-architecture.md).
        CREATE TABLE solana_indexer_state (
            id                     INTEGER PRIMARY KEY CHECK (id = 0),
            last_obligation_count  INTEGER NOT NULL DEFAULT 0,
            last_checked_slot      INTEGER NOT NULL DEFAULT 0,
            updated_at             INTEGER NOT NULL
        );

        -- ------------------------------------------------------------ bridge_requests --
        -- The single state-machine table spanning both directions
        -- (docs/04-state-machines.md, docs/06-schema.md).
        CREATE TABLE bridge_requests (
            id                          INTEGER PRIMARY KEY,
            direction                   TEXT NOT NULL CHECK (direction IN ('GlcToSol','SolToGlc')),
            state                       TEXT NOT NULL,
            amount_atomic               INTEGER NOT NULL CHECK (amount_atomic > 0),
            recipient                   BLOB NOT NULL,
            requester                   BLOB,
            created_at                  INTEGER NOT NULL,
            reserved_at                 INTEGER,
            reservation_expires_at      INTEGER,
            -- Goldcoin leg identity (GlcToSol source, or SolToGlc destination
            -- payout — recorded once known):
            source_txid                 BLOB,
            source_vout                 INTEGER,
            -- Solana leg identity (SolToGlc source): the WithdrawalObligation
            -- index is the canonical identifier — see goldcoin/indexer.rs and
            -- solana/indexer.rs module docs for why no separate "signature"
            -- field is needed.
            source_obligation_index     INTEGER,
            source_block_height         INTEGER,
            source_block_hash           BLOB,
            source_confirmations        INTEGER NOT NULL DEFAULT 0,
            source_finalized_at         INTEGER,
            settlement_claim_hash       BLOB,
            destination_txid            BLOB,
            destination_confirmations   INTEGER NOT NULL DEFAULT 0,
            settled_at                  INTEGER,
            failure_reason              TEXT,
            manual_review_note          TEXT
        );

        -- Replay guard (constraint 5), enforced structurally per direction:
        CREATE UNIQUE INDEX ux_bridge_requests_glc_source
            ON bridge_requests(source_txid, source_vout)
            WHERE source_txid IS NOT NULL;
        CREATE UNIQUE INDEX ux_bridge_requests_sol_source
            ON bridge_requests(source_obligation_index)
            WHERE source_obligation_index IS NOT NULL;

        CREATE INDEX ix_bridge_requests_state ON bridge_requests(direction, state);

        -- Append-only audit trail, same discipline as the old bridge's
        -- deposit_state_log/withdrawal_state_log.
        CREATE TABLE bridge_request_state_log (
            id          INTEGER PRIMARY KEY,
            request_id  INTEGER NOT NULL REFERENCES bridge_requests(id),
            from_state  TEXT,
            to_state    TEXT NOT NULL,
            at          INTEGER NOT NULL,
            reason      TEXT,
            actor       TEXT NOT NULL
        );

        -- --------------------------------------------------------------- reserve_ledger --
        CREATE TABLE reserve_ledger (
            direction                  TEXT PRIMARY KEY CHECK (direction IN ('GoldcoinReserve','SolanaReserve')),
            total_reserve_balance      INTEGER NOT NULL,
            balance_refreshed_at       INTEGER NOT NULL,
            protected_minimum          INTEGER NOT NULL,
            target_reserve             INTEGER NOT NULL,
            warning_reserve            INTEGER NOT NULL,
            critical_reserve           INTEGER NOT NULL,
            reserved_liquidity         INTEGER NOT NULL DEFAULT 0,
            pending_obligations        INTEGER NOT NULL DEFAULT 0,
            settled_liquidity_total    INTEGER NOT NULL DEFAULT 0,
            paused                     INTEGER NOT NULL DEFAULT 0,
            pause_reason               TEXT,
            CHECK (critical_reserve > protected_minimum)
        );

        -- ------------------------------------------------------- reconciliation_findings --
        CREATE TABLE reconciliation_findings (
            id              INTEGER PRIMARY KEY,
            detected_at     INTEGER NOT NULL,
            direction       TEXT NOT NULL,
            expected        INTEGER NOT NULL,
            observed        INTEGER NOT NULL,
            delta           INTEGER NOT NULL,
            classification  TEXT NOT NULL,
            auto_paused     INTEGER NOT NULL DEFAULT 0,
            resolved_at     INTEGER,
            resolution_note TEXT
        );
        "#,
    )?;
    Ok(())
}

/// Phase 3: Goldcoin vault UTXO tracking and payout construction/lifecycle.
/// Table/state shapes reused from the old bridge's `vault_utxos`/
/// `withdrawal_payouts`/`withdrawal_payout_inputs` (docs/01-reuse-
/// inventory.md) — reservation lives in this DB, never in `goldcoind`'s own
/// `lockunspent`, because those locks are in-memory only and do not survive
/// a node or service restart (real-node-verified quirk, carried forward).
fn apply_v2(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE vault_utxos (
            txid              BLOB NOT NULL,
            vout              INTEGER NOT NULL,
            amount_atomic     INTEGER NOT NULL,
            script_pubkey_hex TEXT NOT NULL,
            confirmations     INTEGER NOT NULL,
            first_seen_at     INTEGER NOT NULL,
            state             TEXT NOT NULL CHECK (state IN ('Available','Reserved','Spent','Unconfirmed')),
            reserved_by       INTEGER REFERENCES bridge_requests(id),
            reserved_at       INTEGER,
            spent_by_txid     BLOB,
            PRIMARY KEY (txid, vout),
            -- `Reserved` MUST carry a reserved_by; `Spent` legitimately
            -- keeps its `reserved_by` too (audit: which request spent this
            -- outpoint) rather than clearing it, so this only enforces the
            -- direction that matters — not a full biconditional.
            CHECK (state != 'Reserved' OR reserved_by IS NOT NULL)
        );
        CREATE INDEX ix_vault_utxos_state ON vault_utxos(state);

        -- PK on request_id structurally enforces at most one payout ever
        -- built per bridge_request (docs/01-reuse-inventory.md: this and
        -- the UNIQUE below on inputs are "the actual boundary" against
        -- double-pay; everything else is optimization/observability).
        CREATE TABLE goldcoin_payouts (
            request_id            INTEGER PRIMARY KEY REFERENCES bridge_requests(id),
            commitment_hash       BLOB NOT NULL,
            payout_atomic         INTEGER NOT NULL,
            change_atomic         INTEGER NOT NULL,
            fee_atomic            INTEGER NOT NULL,
            dest_p2pkh_hash       BLOB NOT NULL,
            unsigned_tx_hex       TEXT,
            signed_tx_hex         TEXT,
            txid                  BLOB,
            state                 TEXT NOT NULL CHECK (state IN ('Built','Signed','Broadcast','Confirmed','Completed')),
            built_at              INTEGER NOT NULL,
            signed_at             INTEGER,
            broadcast_at          INTEGER,
            confirmations         INTEGER NOT NULL DEFAULT 0,
            completed_at          INTEGER
        );

        CREATE TABLE goldcoin_payout_inputs (
            request_id    INTEGER NOT NULL REFERENCES bridge_requests(id),
            input_order   INTEGER NOT NULL,
            txid          BLOB NOT NULL,
            vout          INTEGER NOT NULL,
            amount_atomic INTEGER NOT NULL,
            UNIQUE (txid, vout)
        );
        "#,
    )?;
    Ok(())
}

/// Phase 4: on-chain completion tracking for the Solana->Goldcoin leg. The
/// Goldcoin payout confirming is not the end of that leg — per
/// docs/03-architecture.md, `record_goldcoin_completion` must land on
/// Solana (threshold-attested) before the obligation is truly `Settled`,
/// so the completion fact is reconstructable from Solana chain state
/// rather than resting solely on this service's own database (same
/// rationale as the old bridge's ADR-0018, reused).
fn apply_v3(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        ALTER TABLE goldcoin_payouts ADD COLUMN mined_height INTEGER;
        ALTER TABLE goldcoin_payouts ADD COLUMN onchain_completion_signature BLOB;
        ALTER TABLE goldcoin_payouts ADD COLUMN onchain_completion_submitted_at INTEGER;
        ALTER TABLE goldcoin_payouts ADD COLUMN onchain_completed_at INTEGER;
        "#,
    )?;
    Ok(())
}

/// Phase 5: frozen attestation-claim artifacts and a signer-identity audit
/// trail (docs/06-schema.md, both specified there since Phase 0/1 but
/// unimplemented until now — `service/ops`/`glc-audit` are what first need
/// them). `attestation_records` persists the exact canonical message bytes
/// an internal signer attested to at the moment it was built, not just the
/// scalar fields it was built from — the same "freeze a copy so a later
/// audit can recompute-and-diff against something, not just re-derive in a
/// vacuum" discipline the old bridge's `StoredClaim`/`StoredPayoutIntent`
/// used (docs/01-reuse-inventory.md, `ops/audit.rs`), adapted here to this
/// bridge's two message families (`shared::claim::release_claim_message`/
/// `goldcoin_completion_message`) instead of its mint-claim format.
/// `signature_grant_log` is the identity-only audit trail from
/// docs/06-schema.md's original design — never key material, just which
/// signer identity granted which category of authorization, when, for
/// which request.
fn apply_v4(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE attestation_records (
            id                 INTEGER PRIMARY KEY,
            request_id         INTEGER NOT NULL REFERENCES bridge_requests(id),
            action_type        TEXT NOT NULL CHECK (action_type IN ('release','completion')),
            canonical_message  BLOB NOT NULL,
            message_hash       BLOB NOT NULL,
            created_at         INTEGER NOT NULL,
            UNIQUE (request_id, action_type)
        );

        CREATE TABLE signature_grant_log (
            id            INTEGER PRIMARY KEY,
            at            INTEGER NOT NULL,
            action_type   TEXT NOT NULL CHECK (action_type IN ('attestation','goldcoin_payout','governance','rebalance')),
            identity      TEXT NOT NULL,
            request_id    INTEGER,
            severity      TEXT NOT NULL CHECK (severity IN ('info','warn'))
        );
        CREATE INDEX ix_signature_grant_log_request ON signature_grant_log(request_id);
        "#,
    )?;
    Ok(())
}

/// Phase 6 (bridge fee): the 3% bridge fee and the reserve-capacity
/// accounting-unit fix it's implemented alongside (docs/20-bridge-fee.md,
/// docs/18-token-2022-support.md's flagged gap). `bridge_requests.
/// amount_atomic` is renamed to `gross_amount_atomic` and three new
/// columns persist the fee breakdown as first-class ledger values, all in
/// the ledger's canonical accounting unit (8 decimals, numerically
/// identical to Goldcoin's own native atomic unit —
/// `amount_conversion::CanonicalAtomic`), for BOTH directions:
///
///   - `gross_amount_atomic` (renamed from `amount_atomic`): what the user
///     declared/deposited, canonical.
///   - `fee_bps`: the fee-POLICY SNAPSHOT actually applied to this
///     request (its ROUTE's configured rate at creation/fold time —
///     `fees::RouteFees`; immutable thereafter). Read back for
///     settlement/attestation validation via
///     `amount_conversion::verify_fee_breakdown`, which recomputes
///     fee/net AT THIS RATE and refuses on mismatch, so in-flight
///     requests survive a rate change without weakening fail-closed
///     validation (see docs/20-bridge-fee.md).
///   - `fee_amount_atomic`, `net_amount_atomic`: canonical; `gross ==
///     fee + net` always holds by construction
///     (`amount_conversion::compute_fee`).
///   - `net_destination_atomic`: the same net entitlement, but in the
///     DESTINATION reserve's own native chain unit — the actual amount
///     reserved/settled against `reserve_ledger`'s capacity counters,
///     since that's what must be compared against a live, native-unit
///     chain balance read. Numerically equal to `net_amount_atomic` for
///     `SolToGlc` (destination is Goldcoin, whose native unit already is
///     canonical); a real, possibly-lossy-checked conversion for
///     `GlcToSol` (destination is the Solana reserve mint's own live
///     decimals).
///
/// `reserve_ledger.reserved_liquidity`/`pending_obligations`/
/// `settled_liquidity_total` switch, alongside this, from tracking GROSS
/// to tracking `net_destination_atomic` — the amount actually committed/
/// released, in that row's own native unit, matching `total_reserve_
/// balance`. `reserve_ledger.accrued_fees_atomic` is new: a running total
/// of fee revenue recognized at settlement, ALWAYS canonical regardless of
/// which row it's on (a deliberate, documented exception — see
/// docs/20-bridge-fee.md's "accrued-fee accounting" section for why it is
/// purely a reporting/audit figure and deliberately never subtracted from
/// `available_capacity`'s arithmetic).
fn apply_v5(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        ALTER TABLE bridge_requests RENAME COLUMN amount_atomic TO gross_amount_atomic;
        ALTER TABLE bridge_requests ADD COLUMN fee_bps INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE bridge_requests ADD COLUMN fee_amount_atomic INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE bridge_requests ADD COLUMN net_amount_atomic INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE bridge_requests ADD COLUMN net_destination_atomic INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE reserve_ledger ADD COLUMN accrued_fees_atomic INTEGER NOT NULL DEFAULT 0;
        "#,
    )?;
    Ok(())
}

/// Rebalancing engineering layer (docs/22-production-readiness-review.md
/// P1 "rebalancing", docs/05-reserve-accounting.md's original
/// `rebalance_events` design). Structurally separate from
/// `bridge_requests`/settlement accounting by construction — no foreign
/// key to `bridge_requests`, and nothing in `Ledger::confirm_rebalance`
/// touches `reserved_liquidity`/`pending_obligations`, only
/// `total_reserve_balance` — so a reconciliation job or an auditor
/// scanning settlement records can never mistake a rebalance for a user
/// bridge transfer, matching docs/05's original design intent.
///
/// `tx_reference` is the real, external evidence of an out-of-band
/// transfer (a Goldcoin txid or a Solana signature, as plain text) an
/// operator already authorized and executed through real custody tooling
/// — this service never constructs or broadcasts that transaction itself
/// (docs/22-production-readiness-review.md: "model it as an explicit
/// externally authorized action/request"). The `UNIQUE` index on it is
/// the structural replay guard: the same real transfer can never be
/// recorded against two different rebalance requests.
fn apply_v6(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE rebalance_requests (
            id                      INTEGER PRIMARY KEY,
            direction               TEXT NOT NULL CHECK (direction IN ('GoldcoinReserve','SolanaReserve')),
            kind                    TEXT NOT NULL CHECK (kind IN ('Deposit','Withdraw')),
            amount_atomic           INTEGER NOT NULL CHECK (amount_atomic > 0),
            state                   TEXT NOT NULL,
            reason                  TEXT NOT NULL,
            requested_by            TEXT NOT NULL,
            requested_at            INTEGER NOT NULL,
            required_approvals      INTEGER NOT NULL CHECK (required_approvals > 0),
            approved_by             TEXT NOT NULL DEFAULT '[]',
            approved_at             INTEGER,
            tx_reference            TEXT,
            executed_at             INTEGER,
            observed_amount_atomic  INTEGER,
            confirmed_at            INTEGER,
            failure_reason          TEXT
        );
        CREATE UNIQUE INDEX ux_rebalance_tx_reference
            ON rebalance_requests(tx_reference)
            WHERE tx_reference IS NOT NULL;
        CREATE INDEX ix_rebalance_requests_state ON rebalance_requests(direction, state);

        -- Append-only audit trail, same discipline as bridge_request_state_log.
        CREATE TABLE rebalance_state_log (
            id             INTEGER PRIMARY KEY,
            rebalance_id   INTEGER NOT NULL REFERENCES rebalance_requests(id),
            from_state     TEXT,
            to_state       TEXT NOT NULL,
            at             INTEGER NOT NULL,
            reason         TEXT,
            actor          TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

/// Dedicated post-finality Goldcoin reorg detection
/// (docs/22-production-readiness-review.md P1, docs/10-threat-model.md's
/// "post-finality reorg" section — previously only incidentally caught,
/// if at all, by the generic reconciliation balance-drop check). Distinct
/// from `goldcoin_reorg_events` (every reorg, routine pre-finality
/// rollbacks included): a row here exists only when a detected reorg's
/// fork point is at or below the source block of at least one
/// `GlcToSol` request that had already been told its deposit was final
/// (`bridge_requests.source_finalized_at IS NOT NULL`) — the exact
/// "previously accepted finalized observation invalidated" event the
/// threat model names as a genuine incident, never routine.
fn apply_v7(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE post_finality_reorg_events (
            id                     INTEGER PRIMARY KEY,
            detected_at            INTEGER NOT NULL,
            fork_height            INTEGER NOT NULL,
            old_tip_height         INTEGER NOT NULL,
            affected_request_ids   TEXT NOT NULL,
            auto_paused            INTEGER NOT NULL DEFAULT 0
        );
        "#,
    )?;
    Ok(())
}

/// Generic key-rotation / vault-sweep custody-transition tooling
/// (docs/22-production-readiness-review.md P1 "key rotation / vault
/// sweep tooling", docs/09-runbook.md's "no procedure exists yet"
/// gap). Covers both `AttestationKeyRotation` (ed25519 signer set) and
/// `GoldcoinVaultSweep` (P2SH multisig vault) with one shared shape,
/// since both are fundamentally "retire an old custody identity, adopt a
/// verified new one" with the same safety requirements. Like
/// `rebalance_requests`, this NEVER records that this service itself
/// generated keys, signed anything, or broadcast a real transaction —
/// `record_custody_transition_executed` only ever records evidence
/// (`tx_reference`) of a real rotation/sweep an operator already
/// authorized and executed through real custody tooling outside this
/// system.
fn apply_v8(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE custody_transitions (
            id                      INTEGER PRIMARY KEY,
            kind                    TEXT NOT NULL CHECK (kind IN ('AttestationKeyRotation','GoldcoinVaultSweep')),
            state                   TEXT NOT NULL,
            old_identities          TEXT NOT NULL,
            new_identities          TEXT NOT NULL,
            new_threshold           INTEGER,
            reason                  TEXT NOT NULL,
            requested_by            TEXT NOT NULL,
            requested_at            INTEGER NOT NULL,
            required_approvals      INTEGER NOT NULL CHECK (required_approvals > 0),
            approved_by             TEXT NOT NULL DEFAULT '[]',
            approved_at             INTEGER,
            identity_verified_by    TEXT,
            identity_verified_at    INTEGER,
            tx_reference            TEXT,
            executed_at             INTEGER,
            confirmed_at            INTEGER,
            failure_reason          TEXT,
            rolled_back_at          INTEGER,
            rollback_reason         TEXT
        );
        CREATE UNIQUE INDEX ux_custody_transitions_tx_reference
            ON custody_transitions(tx_reference)
            WHERE tx_reference IS NOT NULL;
        CREATE INDEX ix_custody_transitions_state ON custody_transitions(kind, state);

        -- Append-only audit trail, same discipline as the other two
        -- state-machine tables above.
        CREATE TABLE custody_transition_state_log (
            id                      INTEGER PRIMARY KEY,
            transition_id           INTEGER NOT NULL REFERENCES custody_transitions(id),
            from_state              TEXT,
            to_state                TEXT NOT NULL,
            at                      INTEGER NOT NULL,
            reason                  TEXT,
            actor                   TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

/// Unique-per-request Goldcoin deposit address (docs: the OP_RETURN-
/// replacement redesign, Step 2 of a staged rollout — Step 1 was the
/// pure derivation helper, `goldcoin::derivation`; this step is ONLY
/// schema/ledger support — no indexer, API, payout, or signer code
/// reads or writes these columns yet).
///
/// `bridge_requests.id` is reused directly as the derivation index (see
/// `goldcoin::derivation`'s own docs) — no separate index/counter
/// column is added here. All three new columns are nullable: `NULL`
/// means "this request has no per-request deposit address assigned"
/// (every existing row, and every future `SolToGlc` row, which has no
/// Goldcoin deposit step at all — direction is enforced by
/// `Ledger::set_goldcoin_deposit_address`, not by a schema CHECK,
/// since a request's direction can't be joined into a column
/// constraint here).
///
/// `deposit_script_pubkey_hex` (the actual on-chain P2SH scriptPubKey a
/// future indexer will match transaction outputs against) is the real
/// lookup key — the partial unique index below is the DATABASE-level
/// guarantee that two different requests can never be assigned the same
/// deposit script, structurally impossible to race past (same pattern
/// already used for `ux_bridge_requests_glc_source`/
/// `ux_custody_transitions_tx_reference` above).
/// Column-level idempotent: every `ADD COLUMN` is skipped if the column is
/// already present, and the index uses `IF NOT EXISTS`. This is deliberately
/// NOT relying solely on `schema_version`-based gating in
/// [`open_and_migrate`] to keep this migration from ever running twice — a
/// production database was found with these exact columns already present
/// (a prior rollout of this same migration) while its recorded
/// `schema_version` did not reflect it, and the un-guarded `ALTER TABLE ADD
/// COLUMN` then failed outright with `duplicate column name`, refusing to
/// start. Structural idempotency here means this function is safe to
/// invoke any number of times, regardless of what `schema_version` says —
/// it converges to the same end state either way, never errors, and never
/// drops or recreates a column that's already there.
fn apply_v9(conn: &Connection) -> Result<(), LedgerError> {
    for column in [
        "deposit_address",
        "deposit_script_pubkey_hex",
        "deposit_redeem_script_hex",
    ] {
        if !column_exists(conn, "bridge_requests", column)? {
            conn.execute(
                &format!("ALTER TABLE bridge_requests ADD COLUMN {column} TEXT"),
                [],
            )?;
        }
    }
    conn.execute_batch(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS ux_bridge_requests_deposit_script
            ON bridge_requests(deposit_script_pubkey_hex)
            WHERE deposit_script_pubkey_hex IS NOT NULL;
        "#,
    )?;
    Ok(())
}

/// Operator-triggered vault UTXO splitting audit trail
/// (`glc-admin split-vault-utxo`, docs/09-runbook.md's "Vault UTXO
/// splitting" section) — a proactive, root-vault-only counterpart to the
/// oversized-UTXO-avoidance fix in `goldcoin::coin::select`: fragments one
/// large mature vault UTXO into several smaller ones, all still owned by
/// the vault, ahead of a future payout ever needing to touch it.
///
/// `UNIQUE(source_txid, source_vout)` is the same "actual boundary against
/// double-processing the same input" pattern `goldcoin_payout_inputs`
/// already uses — a given vault outpoint can be split at most once, ever,
/// structurally, not just by an application-level check. A brand-new
/// table, so `CREATE TABLE IF NOT EXISTS` is naturally idempotent on its
/// own (unlike v9's `ALTER TABLE ADD COLUMN` case, which needed the
/// explicit `column_exists` guard above after a real production
/// `schema_version`/actual-schema desync) — still written defensively with
/// `IF NOT EXISTS` throughout so a repeat invocation, from any state, is
/// always a safe no-op.
fn apply_v10(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS vault_utxo_splits (
            id                    INTEGER PRIMARY KEY,
            source_txid           BLOB NOT NULL,
            source_vout           INTEGER NOT NULL,
            source_amount_atomic  INTEGER NOT NULL,
            chunk_count           INTEGER NOT NULL,
            chunk_target_atomic   INTEGER NOT NULL,
            fee_atomic            INTEGER NOT NULL,
            unsigned_tx_hex       TEXT NOT NULL,
            signed_tx_hex         TEXT,
            txid                  BLOB,
            state                 TEXT NOT NULL CHECK (state IN ('Built','Signed','Broadcast')),
            note                  TEXT NOT NULL,
            built_at              INTEGER NOT NULL,
            signed_at             INTEGER,
            broadcast_at          INTEGER
        );
        CREATE UNIQUE INDEX IF NOT EXISTS ux_vault_utxo_splits_source
            ON vault_utxo_splits(source_txid, source_vout);
        "#,
    )?;
    Ok(())
}

/// Minimal, additive admission-control gate (a separate axis from the
/// existing `reserve_ledger.paused`/`pause_reason` — see
/// `Ledger::set_admission`/`is_admission_closed` and `docs/09-runbook.md`'s
/// "Admission control (Solana->Goldcoin)" section): whether NEW obligations
/// may be admitted, independent of whether payout processing of
/// already-accepted ones continues (which was, and remains, never gated by
/// either flag). `admission_closed` starts `0` (open) on every existing
/// and new row — nothing automatic ever sets it; only the operator, via
/// `glc-admin close-admission`/`open-admission`. Column-level idempotent,
/// same discipline as `apply_v9`.
fn apply_v11(conn: &Connection) -> Result<(), LedgerError> {
    if !column_exists(conn, "reserve_ledger", "admission_closed")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN admission_closed INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(conn, "reserve_ledger", "admission_reason")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN admission_reason TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Deterministic Goldcoin payout change FAN-OUT (docs/09-runbook.md's
/// "UTXO liquidity" section): a payout's change is now zero or more
/// outputs, not one lump — see `goldcoin::coin::finalize_fanout` and
/// `PayoutPlan::change_outputs`. Purely ADDITIVE: `goldcoin_payouts.
/// change_atomic` is untouched (kept as the SUM of every change output,
/// for full backward compatibility with every existing consumer of that
/// column — `Ledger::pending_destination_settlement_amount`'s existing
/// SQL needs no change at all). A row existing before this migration
/// simply has zero rows in the new table for its `request_id`; every read
/// path treats that as "one legacy change output, equal to the persisted
/// `change_atomic`" (see `Ledger::get_goldcoin_payout_full`) — never
/// backfilled, never assumed to need repair.
fn apply_v12(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS goldcoin_payout_change_outputs (
            request_id    INTEGER NOT NULL REFERENCES bridge_requests(id),
            output_order  INTEGER NOT NULL,
            amount_atomic INTEGER NOT NULL,
            PRIMARY KEY (request_id, output_order)
        );
        "#,
    )?;
    // UTXO-liquidity admission backpressure (`Ledger::fold_sol_deposit`,
    // `Ledger::set_utxo_pool_thresholds`): defaults to `0` on every existing
    // and new row, meaning "no backpressure" until an operator/startup
    // config explicitly configures it — column-level idempotent, same
    // discipline as `apply_v9`/`apply_v11`.
    if !column_exists(conn, "reserve_ledger", "utxo_pool_min_available_count")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN utxo_pool_min_available_count INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(conn, "reserve_ledger", "utxo_pool_warning_count")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN utxo_pool_warning_count INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// Purely an index — no data or column change. Supports the SolToGlc
/// per-recipient 24h rate-limit check (`Ledger::fold_sol_deposit`/
/// `Ledger::resume_manual_review_sol_to_glc`), which queries
/// `bridge_requests` by `(direction, recipient, created_at)` on every
/// SolToGlc fold and every resume attempt — a hot path with no existing
/// supporting index (`ix_bridge_requests_state` covers `(direction,
/// state)` only). `IF NOT EXISTS` for the same idempotent-migration
/// discipline as every other `apply_v*` here.
fn apply_v13(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS ix_bridge_requests_recipient_window
            ON bridge_requests(direction, recipient, created_at);
        "#,
    )?;
    Ok(())
}

/// v14 — 0-conf spendability for bridge-created payout change
/// (docs/09-runbook.md "Zero-conf payout change"):
///
/// `goldcoin_payout_change_outpoints` is the AUTHORITATIVE provenance
/// relation for the policy: one row per change output of a payout this
/// service itself broadcast, written in the same ledger transaction that
/// records the broadcast txid (`Ledger::record_goldcoin_payout_broadcast`
/// — change outputs are `outputs[1..]` of the payout transaction, in
/// `goldcoin_payout_change_outputs` order; the destination is always
/// output 0 and never gets a row here, so a payout whose DESTINATION
/// happens to pay a watched vault/deposit script can never be
/// misclassified as change). A vault UTXO qualifies for 0-conf spending
/// ONLY by joining this table on its exact `(txid, vout)` — never by
/// paying a vault script or appearing in a vault-touching transaction.
/// Rows are additive and survive restart; outputs broadcast BEFORE this
/// migration have no row and therefore stay on the external
/// (`vault_min_confirmations`) policy — fail closed, no backfill.
///
/// `unconfirmed_ancestor_depth` is the count of this service's OWN
/// unconfirmed ancestor payout transactions at broadcast time (1 = the
/// parent payout itself was built purely on confirmed inputs; 2 = it
/// spent depth-1 zero-conf change; ...) — an upper bound used to cap
/// unconfirmed chaining (`PayoutPolicy::zero_conf_change_max_depth`).
///
/// `vault_utxos.zero_conf_hold_reason` is a reversible per-output
/// exclusion the orchestrator sets when the parent payout transaction
/// stops being known/accepted by the configured Goldcoin node
/// (`Orchestrator::tick_validate_zero_conf_parents`) and clears when it
/// is accepted again — 0-conf eligibility requires it NULL. Vault-split
/// outputs never appear in the outpoints table (splits are recorded in
/// `vault_utxo_splits`, a different relation) and so never receive the
/// 0-conf policy.
fn apply_v14(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS goldcoin_payout_change_outpoints (
            txid                       BLOB NOT NULL,
            vout                       INTEGER NOT NULL,
            request_id                 INTEGER NOT NULL REFERENCES bridge_requests(id),
            amount_atomic              INTEGER NOT NULL,
            unconfirmed_ancestor_depth INTEGER NOT NULL,
            PRIMARY KEY (txid, vout)
        );
        "#,
    )?;
    if !column_exists(conn, "vault_utxos", "zero_conf_hold_reason")? {
        conn.execute(
            "ALTER TABLE vault_utxos ADD COLUMN zero_conf_hold_reason TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Append-only audit trail for privileged admin operations
/// (`Ledger::append_admin_audit`/`list_admin_audit`). The three existing
/// per-state-machine logs (`bridge_request_state_log`,
/// `rebalance_state_log`, `custody_transition_state_log`) only capture
/// operations that transition one of those machines; admin operations
/// that don't (pause/unpause, admission open/close) previously left their
/// mandatory `--note` in a last-write-wins `reserve_ledger` field, or
/// nowhere at all. This table records every admin mutation ATTEMPT —
/// failures included (`outcome = 'error'`), because "an operator tried
/// and was refused" is itself audit-relevant. `note` is `CHECK`ed
/// non-empty at the schema level, mirroring `glc-admin`'s own
/// `require_note` discipline, so a caller that forgets to enforce it
/// cannot write a noteless row.
fn apply_v15(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS admin_audit_log (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            at        INTEGER NOT NULL,
            actor     TEXT    NOT NULL CHECK (actor <> ''),
            action    TEXT    NOT NULL CHECK (action <> ''),
            target    TEXT,
            old_value TEXT,
            new_value TEXT,
            note      TEXT    NOT NULL CHECK (note <> ''),
            outcome   TEXT    NOT NULL CHECK (outcome IN ('success', 'error')),
            error     TEXT
        );
        CREATE INDEX IF NOT EXISTS ix_admin_audit_log_at
            ON admin_audit_log(at);
        CREATE INDEX IF NOT EXISTS ix_admin_audit_log_action
            ON admin_audit_log(action, id);
        CREATE INDEX IF NOT EXISTS ix_admin_audit_log_actor
            ON admin_audit_log(actor, id);
        "#,
    )?;

    Ok(())
}

/// v16 — the vault-UTXO-split LIFECYCLE (docs/09-runbook.md "Automatic
/// UTXO liquidity shaping"): `vault_utxo_splits` gains two terminal
/// states beyond `Broadcast` — `Confirmed` (the split transaction has at
/// least one confirmation; nothing left to drive) and `Abandoned` (the
/// split can never take effect: its source became unspendable before
/// broadcast, or the node reported the already-broadcast transaction's
/// inputs missing on a re-broadcast attempt) — plus the timestamps/reason
/// recording those transitions. The source-outpoint uniqueness guarantee
/// becomes a PARTIAL unique index excluding `Abandoned` rows: an
/// abandoned attempt keeps its full audit row forever but no longer
/// blocks a later, legitimate split of the same outpoint (the exact wedge
/// the 2026-08-30 review found: one dead row permanently disabled all
/// automatic shaping).
///
/// SQLite cannot ALTER a CHECK constraint, so this is the standard
/// rebuild-and-rename dance. `vault_utxo_splits` holds a handful of rows
/// (one per historical split), so the copy is trivially cheap. Idempotent
/// via the `abandon_reason` column probe, same discipline as v9's
/// `column_exists` guard.
fn apply_v16(conn: &Connection) -> Result<(), LedgerError> {
    if column_exists(conn, "vault_utxo_splits", "missing_inputs_since")? {
        return Ok(());
    }
    // The rebuild MUST be one atomic transaction (2026-08-30 re-review,
    // finding 3): a process killed between CREATE/DROP/RENAME would
    // otherwise leave a hybrid state the idempotency probe above cannot
    // recover — and, past the DROP, would have destroyed split history.
    // SQLite rolls an uncommitted transaction back on the next open, so a
    // kill at any point leaves the original table untouched and this
    // function simply runs again. The DROP IF EXISTS guards the one
    // remaining sliver: a leftover empty _v16 table from a pre-fix binary.
    conn.execute_batch(
        r#"
        BEGIN IMMEDIATE;
        DROP TABLE IF EXISTS vault_utxo_splits_v16;
        CREATE TABLE vault_utxo_splits_v16 (
            id                    INTEGER PRIMARY KEY,
            source_txid           BLOB NOT NULL,
            source_vout           INTEGER NOT NULL,
            source_amount_atomic  INTEGER NOT NULL,
            chunk_count           INTEGER NOT NULL,
            chunk_target_atomic   INTEGER NOT NULL,
            fee_atomic            INTEGER NOT NULL,
            unsigned_tx_hex       TEXT NOT NULL,
            signed_tx_hex         TEXT,
            txid                  BLOB,
            state                 TEXT NOT NULL CHECK (state IN ('Built','Signed','Broadcast','Confirmed','Abandoned')),
            note                  TEXT NOT NULL,
            built_at              INTEGER NOT NULL,
            signed_at             INTEGER,
            broadcast_at          INTEGER,
            confirmed_at          INTEGER,
            abandoned_at          INTEGER,
            abandon_reason        TEXT,
            -- Set the first time a re-broadcast of this split's exact
            -- bytes is refused for missing inputs; cleared when the node
            -- accepts/knows the transaction again. After a grace window
            -- (goldcoin::liquidity), accounting stops explaining the
            -- split's phantom chunks so a genuine conflicting-spend loss
            -- surfaces as the breach it is instead of being silently
            -- padded over (2026-08-31 production-readiness review, B2).
            missing_inputs_since  INTEGER,
            -- The transition facts must travel with their states.
            CHECK (state != 'Abandoned' OR abandon_reason IS NOT NULL)
        );
        INSERT INTO vault_utxo_splits_v16
            (id, source_txid, source_vout, source_amount_atomic, chunk_count,
             chunk_target_atomic, fee_atomic, unsigned_tx_hex, signed_tx_hex,
             txid, state, note, built_at, signed_at, broadcast_at)
        SELECT id, source_txid, source_vout, source_amount_atomic, chunk_count,
               chunk_target_atomic, fee_atomic, unsigned_tx_hex, signed_tx_hex,
               txid, state, note, built_at, signed_at, broadcast_at
        FROM vault_utxo_splits;
        DROP TABLE vault_utxo_splits;
        ALTER TABLE vault_utxo_splits_v16 RENAME TO vault_utxo_splits;
        CREATE UNIQUE INDEX ux_vault_utxo_splits_source
            ON vault_utxo_splits(source_txid, source_vout)
            WHERE state != 'Abandoned';
        -- The lifecycle queries filter by state (pending/broadcast sets)
        -- and match chunk rows by txid every tick.
        CREATE INDEX ix_vault_utxo_splits_state ON vault_utxo_splits(state);
        CREATE INDEX ix_vault_utxo_splits_txid ON vault_utxo_splits(txid);
        COMMIT;
        "#,
    )?;
    Ok(())
}

/// v17 — `solana_refunds`: the audited, structurally-idempotent record of
/// a ManualReview refund lifecycle (docs/09-runbook.md "ManualReview
/// refunds (Solana->Goldcoin)"). One row per refunded request, ever:
///
/// - `request_id INTEGER PRIMARY KEY` — at most one refund lifecycle per
///   bridge request, the same structural boundary `goldcoin_payouts`'
///   PRIMARY KEY provides against double-pay.
/// - `nonce` UNIQUE — the `rebalance_withdraw` nonce, derived
///   deterministically from the request id in a dedicated refund domain
///   (`Ledger::solana_refund_nonce`); its on-chain `rebalance_withdrawal`
///   PDA makes a second transfer under it impossible on chain, so a
///   restored-from-backup database still cannot double-refund.
/// - `obligation_index` UNIQUE — one refund per on-chain deposit
///   obligation, mirroring `ux_bridge_requests_sol_source`.
///
/// The refund never overwrites any original request/deposit evidence: the
/// park reason is COPIED here (`manual_review_reason`) and the request
/// row's own `manual_review_note`/source columns stay untouched.
/// `CREATE TABLE IF NOT EXISTS` keeps this structurally idempotent (the
/// v9/v16 discipline — never rely on the version gate alone).
fn apply_v17(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS solana_refunds (
            request_id                INTEGER PRIMARY KEY REFERENCES bridge_requests(id),
            obligation_index          INTEGER NOT NULL UNIQUE,
            nonce                     INTEGER NOT NULL UNIQUE,
            amount_solana_atomic      INTEGER NOT NULL CHECK (amount_solana_atomic > 0),
            requester                 BLOB NOT NULL,
            destination_token_account BLOB NOT NULL,
            reserve_mint              BLOB NOT NULL,
            token_program             BLOB NOT NULL,
            manual_review_reason      TEXT NOT NULL,
            note                      TEXT NOT NULL CHECK (note <> ''),
            created_by                TEXT NOT NULL CHECK (created_by <> ''),
            state                     TEXT NOT NULL CHECK (state IN ('Pending','Broadcast','Confirmed')),
            attestation_epoch         INTEGER,
            refund_signature          TEXT,
            -- The broadcast transaction's recent blockhash (base58). What
            -- makes crash recovery POSITIVE rather than heuristic: a
            -- rerun may only rebuild (with the SAME nonce) after
            -- observing this blockhash can no longer land AND the nonce
            -- PDA does not exist — never "it has probably expired".
            recent_blockhash          TEXT,
            created_at                INTEGER NOT NULL,
            broadcast_at              INTEGER,
            confirmed_at              INTEGER,
            -- A signature/blockhash/broadcast timestamp may only exist
            -- once the row has actually reached the state that produces
            -- it.
            CHECK (state = 'Pending' OR refund_signature IS NOT NULL),
            CHECK (state = 'Pending' OR recent_blockhash IS NOT NULL),
            CHECK (state = 'Pending' OR broadcast_at IS NOT NULL),
            CHECK (state != 'Confirmed' OR confirmed_at IS NOT NULL)
        );
        CREATE INDEX IF NOT EXISTS ix_solana_refunds_state ON solana_refunds(state);
        "#,
    )?;
    Ok(())
}

/// Confirmed-liquidity admission safety buffer for Solana->Goldcoin
/// (docs/09-runbook.md's "Confirmed-liquidity admission safety buffer"
/// section): a second, AUTOMATIC admission axis that closes SolToGlc
/// admission before confirmed unreserved Goldcoin headroom reaches the
/// hard `protected_minimum`, and reopens it only once headroom has
/// recovered to a strictly higher mark.
///
/// Four columns, all defaulting to "disabled / open" on every existing
/// and new row, so a database that never configures the buffer behaves
/// bit-identically to before this migration:
///
/// - `admission_buffer_atomic` — the close threshold. `0` means the whole
///   feature is disabled, the same short-circuit shape
///   `utxo_pool_min_available_count = 0` already uses.
/// - `admission_reopen_atomic` — the (higher) reopen threshold. The gap
///   between the two IS the hysteresis: between them the gate holds its
///   current state, which is what makes threshold flapping structurally
///   impossible rather than merely unlikely.
/// - `liquidity_admission_closed` — the gate's persisted state. Genuinely
///   stateful: at a headroom between the two thresholds the correct
///   answer depends on which side the reserve arrived from, so it cannot
///   be recomputed from the balance alone.
/// - `liquidity_admission_closed_at` — when the gate last transitioned,
///   for operator/audit visibility only; never read by any decision.
///
/// Deliberately SEPARATE from `admission_closed`/`admission_reason`
/// (v11), which remain operator-only by design (`Ledger::set_admission`'s
/// docs, docs/09-runbook.md: "no automatic reopen, and nothing
/// automatically closes it either"). Overloading that flag would destroy
/// an operator's ability to tell "I closed this" apart from "liquidity
/// closed this", and would let an automatic reopen silently undo a
/// deliberate operator closure. Column-level idempotent, same discipline
/// as `apply_v9`/`apply_v11`/`apply_v12`.
/// v19: the Goldcoin-side refund lifecycle for `GlcToSol` requests parked
/// in `ManualReview` (docs/09-runbook.md "Goldcoin-sourced ManualReview refunds").
///
/// Deliberately a SEPARATE table from `goldcoin_payouts` rather than a
/// new state on it. A payout and a refund are opposite settlements of the
/// same request and must never be representable at once;
/// `goldcoin_payouts` is keyed `request_id PRIMARY KEY`, so reusing it
/// would have made "has a payout" and "has a refund" the same query and
/// lost exactly the distinction the safety checks depend on.
///
/// Three structural guarantees, so a regression in application-level
/// checks still cannot produce a double refund:
///
/// 1. `request_id INTEGER PRIMARY KEY` — at most one refund row per
///    request, ever.
/// 2. `UNIQUE (source_txid, source_vout)` — the same deposit outpoint can
///    never be refunded through two different requests, even if the
///    request-level replay guard were somehow bypassed.
/// 3. `goldcoin_refund_inputs UNIQUE (txid, vout)` — the same vault UTXO
///    can never fund two refunds, mirroring `goldcoin_payout_inputs`,
///    which docs/01-reuse-inventory.md names as the actual double-spend
///    boundary.
///
/// `CREATE TABLE IF NOT EXISTS` keeps this structurally idempotent.
fn apply_v19(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS goldcoin_refunds (
            request_id             INTEGER PRIMARY KEY REFERENCES bridge_requests(id),

            -- The deposit being returned, as independently re-verified
            -- against Goldcoin RPC. NOT copied from the request row
            -- alone: the executing path re-derives these and refuses on
            -- any disagreement.
            source_txid            BLOB NOT NULL,
            source_vout            INTEGER NOT NULL,

            -- The amount ACTUALLY observed on chain in that output. This
            -- is the refund principal. It is never `bridge_requests.
            -- amount_atomic` (the expected gross) and never parsed out of
            -- `manual_review_note`.
            observed_amount_atomic INTEGER NOT NULL CHECK (observed_amount_atomic > 0),

            -- The outpoint the deposit transaction SPENT, and the P2PKH
            -- destination derived from that prevout's own scriptPubKey.
            -- Recorded for audit: it is the whole evidence chain for why
            -- the refund went where it went.
            source_input_txid      BLOB NOT NULL,
            source_input_vout      INTEGER NOT NULL,
            refund_dest_p2pkh_hash BLOB NOT NULL,
            refund_dest_address    TEXT NOT NULL CHECK (refund_dest_address <> ''),

            -- The refund output value. Equal to observed_amount_atomic by
            -- policy (the vault absorbs the miner fee); the CHECK makes
            -- that a schema-level invariant rather than a convention.
            refund_amount_atomic   INTEGER NOT NULL CHECK (refund_amount_atomic > 0),
            fee_atomic             INTEGER NOT NULL CHECK (fee_atomic >= 0),

            unsigned_tx_hex        TEXT,
            signed_tx_hex          TEXT,
            txid                   BLOB,
            confirmations          INTEGER NOT NULL DEFAULT 0,

            state                  TEXT NOT NULL
                                   CHECK (state IN ('Built','Signed','Broadcast','Refunded')),

            manual_review_reason   TEXT NOT NULL,
            note                   TEXT NOT NULL CHECK (note <> ''),
            created_by             TEXT NOT NULL CHECK (created_by <> ''),

            built_at               INTEGER NOT NULL,
            signed_at              INTEGER,
            broadcast_at           INTEGER,
            refunded_at            INTEGER,

            -- Whether the stranded SolanaReserve reservation this request
            -- still held has been released. Guarded by a CHECK so it can
            -- only ever be true in the terminal state: releasing capacity
            -- while a refund might still fail would free liquidity for an
            -- obligation that is not yet actually discharged.
            reservation_released   INTEGER NOT NULL DEFAULT 0
                                   CHECK (reservation_released IN (0, 1)),

            -- ---- table constraints (must follow every column) ----
            --
            -- The recipient receives the FULL observed deposit: the vault
            -- absorbs the miner fee. A schema-level invariant, so it holds
            -- even if the application logic regressed.
            CHECK (refund_amount_atomic = observed_amount_atomic),
            -- Capacity may only be freed in the terminal state.
            CHECK (reservation_released = 0 OR state = 'Refunded'),
            -- Each artifact may exist only once its producing state has
            -- been reached.
            CHECK (state = 'Built' OR signed_tx_hex IS NOT NULL),
            CHECK (state = 'Built' OR signed_at IS NOT NULL),
            CHECK (state IN ('Built','Signed') OR txid IS NOT NULL),
            CHECK (state IN ('Built','Signed') OR broadcast_at IS NOT NULL),
            CHECK (state != 'Refunded' OR refunded_at IS NOT NULL),

            UNIQUE (source_txid, source_vout)
        );

        CREATE TABLE IF NOT EXISTS goldcoin_refund_inputs (
            request_id    INTEGER NOT NULL REFERENCES goldcoin_refunds(request_id),
            input_order   INTEGER NOT NULL,
            txid          BLOB NOT NULL,
            vout          INTEGER NOT NULL,
            amount_atomic INTEGER NOT NULL,
            PRIMARY KEY (request_id, input_order),
            UNIQUE (txid, vout)
        );

        CREATE INDEX IF NOT EXISTS ix_goldcoin_refunds_state
            ON goldcoin_refunds(state);
        "#,
    )?;
    Ok(())
}

fn apply_v18(conn: &Connection) -> Result<(), LedgerError> {
    if !column_exists(conn, "reserve_ledger", "admission_buffer_atomic")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN admission_buffer_atomic INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(conn, "reserve_ledger", "admission_reopen_atomic")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN admission_reopen_atomic INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(conn, "reserve_ledger", "liquidity_admission_closed")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN liquidity_admission_closed INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(conn, "reserve_ledger", "liquidity_admission_closed_at")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN liquidity_admission_closed_at INTEGER",
            [],
        )?;
    }
    Ok(())
}

/// Whether `table` already has a column named `column` — `PRAGMA
/// table_info` rather than a schema-version check, so it reflects the
/// connection's REAL, current structure regardless of how it got that way
/// (a normal migration run, or an out-of-band/partial one). `pub(super)`:
/// also used by `Ledger::record_unmatched_goldcoin_deposit` to add the
/// `reconciled_at` column to `unmatched_goldcoin_deposits`, a table
/// created ad hoc outside the versioned schema-migration system above.
/// Whether `table` exists on this connection, asked of `sqlite_master`
/// rather than of `schema_version`. Same discipline as
/// [`column_exists`]: a migration decides whether it still has work to do
/// from the database's REAL current shape, never from a version marker
/// that could have been stamped by a partial or out-of-band run.
pub(super) fn table_exists(conn: &Connection, table: &str) -> Result<bool, LedgerError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |r| r.get::<_, i64>(0).map(|v| v != 0),
    )?;
    Ok(exists)
}

pub(super) fn column_exists(
    conn: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, LedgerError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == column);
    Ok(exists)
}

/// v20: the durable INDEPENDENT amount witness for a GlcToSol deposit.
///
/// # Why this column exists
///
/// A `GlcToSol` deposit parked in `ManualReview` for
/// `deposit_amount_mismatch` used to leave exactly one durable record of
/// how much was actually received: the free text of `manual_review_note`.
/// That is not evidence — it is an operator-readable message, and parsing
/// a number back out of it would let a malformed or edited note decide a
/// refund amount.
///
/// `vault_utxos` cannot serve as the second witness either: it is
/// `listunspent`-derived spendable inventory for addresses the NODE's
/// wallet owns, and nothing in this service imports a per-request derived
/// P2SH into the node, so a request-specific deposit can never appear
/// there. Requiring it was unsatisfiable by construction.
///
/// So the indexer now persists what it independently decoded, at the same
/// atomic transition that records the source outpoint. A refund can then
/// require `RPC observed amount == observed_amount_atomic` — two
/// independent observations of one fact, taken at different times by
/// different code.
///
/// # Deliberately nullable, deliberately not backfilled
///
/// Rows parked before this migration have no honest value: reconstructing
/// one would mean either re-reading chain history (which this migration
/// must not do) or parsing the note (which is exactly what this column
/// exists to avoid). `NULL` therefore means "this request predates the
/// durable witness", and the refund path treats it as a distinct, clearly
/// reported LEGACY verification mode rather than silently accepting a
/// weaker proof as if it were the strong one.
///
/// Column-level idempotent, matching `apply_v9`'s discipline: safe to run
/// any number of times regardless of what `schema_version` claims.
fn apply_v20(conn: &Connection) -> Result<(), LedgerError> {
    if !column_exists(conn, "bridge_requests", "observed_amount_atomic")? {
        conn.execute(
            "ALTER TABLE bridge_requests ADD COLUMN observed_amount_atomic INTEGER
             CHECK (observed_amount_atomic IS NULL OR observed_amount_atomic > 0)",
            [],
        )?;
    }
    Ok(())
}

/// v21: CHAIN- AND CONTRACT-QUALIFIED obligation identity.
///
/// # The collision this closes
///
/// Since v1 the replay guard for an obligation-indexed source has been a
/// single GLOBAL partial unique index:
///
/// ```sql
/// CREATE UNIQUE INDEX ux_bridge_requests_sol_source
///     ON bridge_requests(source_obligation_index)
///     WHERE source_obligation_index IS NOT NULL;
/// ```
///
/// That is correct only while exactly one contract in the world can ever
/// produce an obligation index. It is about to stop being true: the EVM
/// `GlcRobinhoodBridge` has its own contract-local, monotonically
/// increasing deposit counter that also starts at 0, and a successor
/// deployment of either bridge restarts its counter at 0 again. Under the
/// old index, Robinhood obligation 0 arriving after Solana obligation 0
/// does not raise an error an operator would see — `fold_sol_deposit`'s
/// own pre-check (`SELECT id ... WHERE source_obligation_index = ?1`)
/// reports `AlreadyFolded` and returns success, so a real, already
/// irreversible deposit is recorded as previously handled and never paid
/// out. This is the single highest-severity item in the Robinhood
/// architecture handoff, and it is closed here, before any Robinhood code
/// exists to trip it.
///
/// The durable identity is therefore the triple
///
/// ```text
/// (source_chain, source_contract, source_obligation_index)
/// ```
///
/// - `source_chain` — a closed `TEXT` discriminant
///   (`ledger::types::SourceChain`), the same shape every other closed
///   enum in this schema already uses. Deliberately not a display label
///   and deliberately not the future on-chain network-qualified
///   `PROTOCOL_CHAIN_ID`; see `SourceChain`'s own docs for why.
/// - `source_contract` — the raw identity bytes of the deployed
///   contract/program whose LOCAL counter produced the index: the
///   32-byte Solana program id (`glc_reserve_bridge_shared::
///   PROGRAM_ID_BYTES`, this workspace's single source of truth for it),
///   or a 20-byte EVM contract address. This is the component that makes
///   "obligation N under bridge v1" and "obligation N under its
///   successor v2" two different rows rather than a collision, without
///   anyone having to remember to invent a new chain name per
///   deployment.
/// - `source_obligation_index` — unchanged.
///
/// # Why the table is REBUILT rather than altered
///
/// `DROP INDEX` + `CREATE UNIQUE INDEX` alone would have been enough to
/// re-key the guard, and would have been much cheaper. It is not enough
/// to make the guard SOUND, because SQL NULLs never compare equal inside
/// a unique index: two rows with the same `(chain, index)` but a NULL
/// `source_contract` are not duplicates as far as SQLite is concerned, so
/// a missing contract identity would SILENTLY DISABLE the replay guard
/// instead of tripping it — strictly worse than the collision this
/// migration exists to fix. The invariant "an obligation index always
/// carries a complete identity" therefore has to be a `CHECK`, enforced
/// by the database.
///
/// And a `CHECK` spanning columns cannot be bolted onto this table in
/// place. `ALTER TABLE ... ADD COLUMN` accepts a `CHECK`, but SQLite
/// evaluates that constraint against the rows already present (verified
/// empirically against the bundled 3.45 engine, not assumed from the
/// documentation) — and the rows already present are exactly the ones
/// that cannot satisfy it until they have been backfilled, which cannot
/// happen until the column exists. `NOT NULL` on `source_chain` has the
/// same chicken-and-egg problem, and the only escape SQLite offers —
/// `NOT NULL DEFAULT 'goldcoin'` — would leave a permanent landmine:
/// every future `INSERT` that forgot the column would silently claim the
/// wrong chain. So this is the standard rebuild-and-rename dance, the
/// same one `apply_v16` already performs for `vault_utxo_splits`, run
/// here against a table with NINE foreign-key dependents:
///
/// ```text
/// bridge_request_state_log.request_id      goldcoin_payout_change_outputs.request_id
/// vault_utxos.reserved_by                  goldcoin_payout_change_outpoints.request_id
/// goldcoin_payouts.request_id              solana_refunds.request_id
/// goldcoin_payout_inputs.request_id        goldcoin_refunds.request_id
/// attestation_records.request_id
/// ```
///
/// Every one of those references `bridge_requests(id)`, and `id` is an
/// `INTEGER PRIMARY KEY` — a rowid alias — so the copy carries `id`
/// explicitly and every dependent row keeps pointing at exactly the row
/// it pointed at before. The procedure is SQLite's own documented recipe
/// for this (`ALTER TABLE`, "Making Other Kinds Of Table Schema
/// Changes"): foreign keys OFF (a `PRAGMA` that is a silent no-op inside
/// a transaction, hence set before `BEGIN` and restored after), one
/// `IMMEDIATE` transaction around create/copy/drop/rename/reindex, and
/// `PRAGMA foreign_key_check` verified clean BEFORE the commit, so a
/// migration that would have orphaned a dependent row aborts and leaves
/// the original table untouched instead. There are no triggers or views
/// on this table to recreate (verified: `sqlite_master` carries none).
///
/// # The backfill, and why it is derived rather than guessed
///
/// Every pre-v21 row falls into exactly one of two categories, and both
/// are decided by data already in the row — nothing is read from config,
/// from chain history, or from a free-text note:
///
/// - `source_obligation_index IS NOT NULL`, or `direction = 'SolToGlc'`:
///   a Solana-sourced request, so `source_chain = 'solana'`. (The
///   obligation-index arm is listed first and separately from the
///   direction arm even though the two cannot disagree today: if they
///   ever did, the index — the thing the uniqueness guard is actually
///   keyed on — is what must decide.) Its `source_contract` is
///   `LEGACY_SOLANA_SOURCE_CONTRACT`, NOT this binary's compiled-in
///   program id: nothing in a pre-v21 ledger records which program issued
///   a given obligation, that constant has genuinely held three values,
///   and `source_contract` exists to be durable identity — so it must not
///   assert a program that may be false. The constant's own docs carry
///   the full evidence audit, including the one relation that does freeze
///   a program id per row (`attestation_records.canonical_message`
///   `[17..49]`), why it covers only the subset of rows that reached
///   their attestation step, and why a lifecycle-dependent split identity
///   would be worse than one honest marker.
/// - everything else (`direction = 'GlcToSol'`): a Goldcoin-sourced
///   request, whose source identity is an outpoint and which has no
///   contract at all, so `source_chain = 'goldcoin'` and
///   `source_contract = NULL`. This includes rows that have not yet seen
///   a deposit (`source_txid IS NULL`), for which the direction alone is
///   already decisive — a `GlcToSol` request's source leg is Goldcoin
///   from the moment it is created.
///
/// Every row written from v21 onward records the EXACT contract identity
/// at fold time (`Ledger::fold_sol_deposit` binds
/// `glc_reserve_bridge_shared::PROGRAM_ID_BYTES`), so the legacy marker
/// is confined forever to rows that predate this migration.
///
/// The backfill cannot introduce a uniqueness failure: it assigns ONE
/// `(chain, contract)` pair to every obligation-bearing row, and those
/// rows' indexes were already globally unique under the index this
/// migration replaces, so both new indexes are satisfied by construction.
///
/// # Why the legacy marker does not weaken the replay guard
///
/// Because a legacy row's true contract is unknown, an obligation index
/// it holds could belong to the program running today. Under the
/// chain-and-contract index alone, re-observing that obligation would
/// look like a brand-new deposit — a double-pay, and a REGRESSION against
/// the pre-v21 global index. `ux_bridge_requests_solana_obligation`
/// therefore keeps that promise exactly: an obligation index is unique
/// across ALL Solana rows, legacy and current alike. It is scoped
/// `WHERE source_chain = 'solana'`, so it cannot re-introduce the
/// cross-chain collision this migration exists to fix, and it leaves
/// Robinhood's contract-qualified identity untouched — obligation N under
/// a Robinhood v1 contract and under its successor stay distinct rows.
/// `Ledger::fold_sol_deposit`'s own pre-check is scoped to match, so a
/// re-observed obligation still returns a clean `AlreadyFolded` rather
/// than surfacing as a raw constraint error.
///
/// # What deliberately does NOT change
///
/// `ux_bridge_requests_glc_source` keeps its exact pre-v21 definition:
/// a Goldcoin outpoint is a 32-byte transaction hash, so it cannot
/// collide across chains the way a small counter can, and re-keying a
/// working guard for no safety gain is a risk with no return.
/// `solana_refunds.obligation_index UNIQUE` likewise stays global — that
/// table is Solana-specific by construction and a Robinhood refund
/// lifecycle would be a different relation. `direction`'s `CHECK` is
/// carried over verbatim: no Robinhood direction is admitted here, and
/// nothing in this binary constructs `SourceChain::Robinhood` on any
/// production path. The vocabulary is widened; no route is enabled.
fn apply_v21(conn: &Connection) -> Result<(), LedgerError> {
    // Structural idempotence, the v9/v16 discipline: what decides whether
    // this migration still has work to do is the REAL current shape of
    // the table, never what `schema_version` claims.
    if column_exists(conn, "bridge_requests", "source_chain")? {
        return Ok(());
    }

    // `PRAGMA foreign_keys` is a silent no-op inside a transaction, so it
    // must be toggled here, outside the one below — and restored on every
    // path out, including the failure path.
    let foreign_keys_were_on: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = rebuild_bridge_requests_for_v21(conn);
    if foreign_keys_were_on {
        conn.pragma_update(None, "foreign_keys", "ON")?;
    }
    result
}

/// The v21 rebuild proper (see [`apply_v21`]). Split out so the caller
/// can restore `PRAGMA foreign_keys` on both the success and the failure
/// path.
///
/// Everything below happens inside ONE `IMMEDIATE` transaction. A process
/// killed at any point during it — including after the `DROP` — leaves
/// the original `bridge_requests`, its rows, and its indexes exactly as
/// they were, because SQLite rolls an uncommitted transaction back on the
/// next open; the idempotence probe in [`apply_v21`] then simply sees the
/// old shape again and reruns this from the top.
fn rebuild_bridge_requests_for_v21(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let staged = stage_bridge_requests_v21(conn);
    match staged {
        Ok(()) => {
            conn.execute_batch("COMMIT;")?;
            Ok(())
        }
        Err(e) => {
            // Best-effort: if the rollback itself fails the transaction is
            // still not committed, and the original table still stands.
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

fn stage_bridge_requests_v21(conn: &Connection) -> Result<(), LedgerError> {
    // Every column of the pre-v21 table, in its exact declared order and
    // with its exact declared type/constraints/defaults (v1's original 22,
    // v5's rename + 4 fee columns, v9's 3 deposit-address columns, v20's
    // amount witness), plus the two new identity columns placed with the
    // other `source_*` columns they belong with. Nothing is widened,
    // narrowed, renamed, reordered relative to its neighbours, or dropped.
    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS bridge_requests_v21;
        CREATE TABLE bridge_requests_v21 (
            id                          INTEGER PRIMARY KEY,
            direction                   TEXT NOT NULL CHECK (direction IN ('GlcToSol','SolToGlc')),
            state                       TEXT NOT NULL,
            gross_amount_atomic         INTEGER NOT NULL CHECK (gross_amount_atomic > 0),
            recipient                   BLOB NOT NULL,
            requester                   BLOB,
            created_at                  INTEGER NOT NULL,
            reserved_at                 INTEGER,
            reservation_expires_at      INTEGER,
            -- ---- durable, chain-qualified source identity (v21) ----
            source_chain                TEXT NOT NULL
                                        CHECK (source_chain IN ('goldcoin','solana','robinhood')),
            source_contract             BLOB,
            source_txid                 BLOB,
            source_vout                 INTEGER,
            source_obligation_index     INTEGER,
            source_block_height         INTEGER,
            source_block_hash           BLOB,
            source_confirmations        INTEGER NOT NULL DEFAULT 0,
            source_finalized_at         INTEGER,
            settlement_claim_hash       BLOB,
            destination_txid            BLOB,
            destination_confirmations   INTEGER NOT NULL DEFAULT 0,
            settled_at                  INTEGER,
            failure_reason              TEXT,
            manual_review_note          TEXT,
            fee_bps                     INTEGER NOT NULL DEFAULT 0,
            fee_amount_atomic           INTEGER NOT NULL DEFAULT 0,
            net_amount_atomic           INTEGER NOT NULL DEFAULT 0,
            net_destination_atomic      INTEGER NOT NULL DEFAULT 0,
            deposit_address             TEXT,
            deposit_script_pubkey_hex   TEXT,
            deposit_redeem_script_hex   TEXT,
            observed_amount_atomic      INTEGER
                                        CHECK (observed_amount_atomic IS NULL
                                               OR observed_amount_atomic > 0),

            -- ---- table constraints (must follow every column) ----
            --
            -- A chain that identifies deposits by a contract-local counter
            -- must name that contract; Goldcoin, whose source identity is
            -- an outpoint, must not pretend to have one. Both directions
            -- are enforced so neither half can drift.
            CHECK (source_chain <> 'goldcoin' OR source_contract IS NULL),
            CHECK (source_chain =  'goldcoin' OR source_contract IS NOT NULL),
            -- An empty blob is not an identity.
            CHECK (source_contract IS NULL OR length(source_contract) > 0),
            -- THE load-bearing one: an obligation index may only exist
            -- alongside a complete identity. `ux_bridge_requests_
            -- obligation_source` below cannot enforce this itself —
            -- SQL NULLs never compare equal inside a unique index, so a
            -- NULL `source_contract` would silently DISABLE the replay
            -- guard rather than trip it. (Implied by the two CHECKs above
            -- today, since a NULL contract forces chain = 'goldcoin';
            -- stated separately anyway, because it is the invariant the
            -- guard's soundness actually rests on, and it must survive any
            -- later widening of those two.)
            CHECK (source_obligation_index IS NULL OR source_contract IS NOT NULL)
        );
        "#,
    )?;

    // The copy. `id` is carried explicitly: it is an `INTEGER PRIMARY KEY`
    // (a rowid alias) and every one of the nine foreign-key dependents
    // points at it, so row identity must be preserved exactly, not
    // regenerated. Historical Solana rows are marked with
    // `LEGACY_SOLANA_SOURCE_CONTRACT` — an explicit "this program identity
    // was never recorded", NOT today's program id, which would be a claim
    // this migration has no evidence for (see the backfill section of this
    // migration's docs, and the constant's own).
    conn.execute(
        r#"
        INSERT INTO bridge_requests_v21
            (id, direction, state, gross_amount_atomic, recipient, requester, created_at,
             reserved_at, reservation_expires_at, source_chain, source_contract,
             source_txid, source_vout, source_obligation_index, source_block_height,
             source_block_hash, source_confirmations, source_finalized_at,
             settlement_claim_hash, destination_txid, destination_confirmations, settled_at,
             failure_reason, manual_review_note, fee_bps, fee_amount_atomic,
             net_amount_atomic, net_destination_atomic, deposit_address,
             deposit_script_pubkey_hex, deposit_redeem_script_hex, observed_amount_atomic)
        SELECT
             id, direction, state, gross_amount_atomic, recipient, requester, created_at,
             reserved_at, reservation_expires_at,
             CASE WHEN source_obligation_index IS NOT NULL THEN 'solana'
                  WHEN direction = 'SolToGlc'               THEN 'solana'
                  ELSE 'goldcoin' END,
             CASE WHEN source_obligation_index IS NOT NULL THEN ?1
                  WHEN direction = 'SolToGlc'               THEN ?1
                  ELSE NULL END,
             source_txid, source_vout, source_obligation_index, source_block_height,
             source_block_hash, source_confirmations, source_finalized_at,
             settlement_claim_hash, destination_txid, destination_confirmations, settled_at,
             failure_reason, manual_review_note, fee_bps, fee_amount_atomic,
             net_amount_atomic, net_destination_atomic, deposit_address,
             deposit_script_pubkey_hex, deposit_redeem_script_hex, observed_amount_atomic
        FROM bridge_requests
        "#,
        [crate::ledger::LEGACY_SOLANA_SOURCE_CONTRACT],
    )?;

    // Belt and braces: the copy must have moved every row. A short count
    // is not something to discover later from a reconciliation alarm.
    let (before, after): (i64, i64) = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM bridge_requests),
                (SELECT COUNT(*) FROM bridge_requests_v21)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if before != after {
        return Err(LedgerError::SchemaMigrationFailed(format!(
            "v21 copied {after} of {before} bridge_requests rows"
        )));
    }

    conn.execute_batch(
        r#"
        DROP TABLE bridge_requests;
        ALTER TABLE bridge_requests_v21 RENAME TO bridge_requests;

        -- Every index the pre-v21 table carried, recreated verbatim...
        CREATE UNIQUE INDEX ux_bridge_requests_glc_source
            ON bridge_requests(source_txid, source_vout)
            WHERE source_txid IS NOT NULL;
        CREATE INDEX ix_bridge_requests_state
            ON bridge_requests(direction, state);
        CREATE UNIQUE INDEX ux_bridge_requests_deposit_script
            ON bridge_requests(deposit_script_pubkey_hex)
            WHERE deposit_script_pubkey_hex IS NOT NULL;
        CREATE INDEX ix_bridge_requests_recipient_window
            ON bridge_requests(direction, recipient, created_at);

        -- ...except the one this migration exists to re-key. The old
        -- `ux_bridge_requests_sol_source` was `(source_obligation_index)`
        -- alone; it is deliberately NOT recreated under its old name, so
        -- a database that has been through v21 is self-describing about
        -- which guard it carries.
        CREATE UNIQUE INDEX ux_bridge_requests_obligation_source
            ON bridge_requests(source_chain, source_contract, source_obligation_index)
            WHERE source_obligation_index IS NOT NULL;

        -- LEGACY COMPATIBILITY, Solana only. Historical rows carry
        -- `LEGACY_SOLANA_SOURCE_CONTRACT` because their real program id
        -- was never recorded — which means an index one of them holds
        -- COULD belong to the program running today. The qualified index
        -- above would treat those as two different deposits and let the
        -- obligation be folded a second time, so this keeps the exact
        -- promise the pre-v21 global index made: within Solana, an
        -- obligation index is unique across every row, legacy or current.
        -- Deliberately scoped to one chain, so it cannot re-introduce the
        -- cross-chain collision v21 exists to fix — Robinhood obligation N
        -- under two different contracts stays two distinct rows.
        CREATE UNIQUE INDEX ux_bridge_requests_solana_obligation
            ON bridge_requests(source_obligation_index)
            WHERE source_chain = 'solana' AND source_obligation_index IS NOT NULL;
        "#,
    )?;

    // Nothing may be orphaned or dangling before this is allowed to
    // commit. `foreign_key_check` is scanned with foreign keys disabled
    // (they are, for the duration of the rebuild) — the pragma reports
    // violations regardless of enforcement, which is exactly why the
    // SQLite recipe puts it here.
    let fk_violations: i64 =
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    if fk_violations != 0 {
        return Err(LedgerError::SchemaMigrationFailed(format!(
            "v21 rebuild left {fk_violations} foreign-key violations; rolled back"
        )));
    }
    let integrity: String =
        conn.query_row("SELECT * FROM pragma_integrity_check LIMIT 1", [], |r| {
            r.get(0)
        })?;
    if integrity != "ok" {
        return Err(LedgerError::SchemaMigrationFailed(format!(
            "v21 rebuild failed integrity_check: {integrity}; rolled back"
        )));
    }
    Ok(())
}

/// v22: the Robinhood (EVM) deposit OBSERVATION store, its scan cursor,
/// and its reorg journal.
///
/// # What this migration is, and just as importantly what it is not
///
/// It adds four tables that let the service WATCH `GlcRobinhoodBridge`
/// and write down what it saw. It adds no settlement state, no reserve
/// row, no route enablement, and not one column to `bridge_requests`.
/// The Robinhood routes stay closed by all three gates in
/// `crate::routes::RouteGate` after this migration exactly as they were
/// before it, and `bridge_requests.direction` keeps its v1 `CHECK
/// (direction IN ('GlcToSol','SolToGlc'))` verbatim — so an observation
/// recorded here still cannot become a payable request without a later,
/// separately reviewed migration and code change.
///
/// The observation table states that structurally rather than only in
/// prose: `settled INTEGER NOT NULL DEFAULT 0 CHECK (settled = 0)` is a
/// column whose only permitted value is "not settled". Nothing can flip
/// it, and a future phase that genuinely intends to settle Robinhood
/// deposits has to drop that CHECK in a migration a reviewer will see.
///
/// # Durable identity: the same triple v21 introduced
///
/// `(source_chain, source_contract, source_obligation_index)` — chain
/// discriminant, the 20-byte deployed contract address whose LOCAL
/// counter produced the index, and the index itself. This is deliberately
/// the identical shape `bridge_requests` now carries (v21), because it is
/// the identity a later fold has to preserve, and because the collision
/// v21 exists to prevent (Robinhood obligation 0 versus Solana obligation
/// 0, or obligation N under a successor deployment) is exactly the one
/// this table would otherwise reintroduce in its own namespace.
///
/// `source_contract` is `NOT NULL` with an exact 20-byte length check:
/// unlike v21, which had to admit historical rows of unknown provenance,
/// every row this table will ever hold is written by an indexer that
/// knows precisely which configured contract address it read the log
/// from. There is no legacy sentinel here and there must never be one.
///
/// # Why the obligation index is an INTEGER and the amount is a BLOB
///
/// Both are `uint256` on the wire, and the two are stored differently on
/// purpose:
///
/// - `source_obligation_index` is a contract-local counter
///   (`obligationCount`, incremented once per deposit). It is narrowed to
///   a signed 64-bit integer AT THE DECODER, which errors rather than
///   truncating — a value that does not fit is not a real deposit, it is
///   a malformed or hostile RPC response. Storing it as an INTEGER makes
///   it the same shape as `bridge_requests.source_obligation_index`, so a
///   later fold is a copy rather than a conversion.
/// - `amount_robinhood_atomic` is a token amount in the Robinhood token's
///   own 18 decimals. One whole GLC is 10^18 there, so `i64::MAX` is
///   about 9.2 GLC — an INTEGER column would overflow on a rounding
///   error, let alone a real transfer. It is therefore stored as the
///   exact 32-byte big-endian ABI word, never narrowed and never scaled.
///
/// `amount_canonical_atomic` IS an INTEGER, because the canonical unit is
/// the ledger's own 8-decimal unit and every other monetary column in
/// this schema already uses it. It is not derived here by dividing:
/// `DepositCreated` carries `canonicalAmount` as its own field, the
/// decoder cross-checks that it equals `amount / CANONICAL_SCALE`
/// exactly, and only then is it stored. Two independent sources agreeing
/// is what makes a single stored number trustworthy.
///
/// # The scan cursor is the block table, not a counter
///
/// `robinhood_scanned_blocks` holds `(block_number, block_hash)` for each
/// block the scanner has anchored on, and the cursor is simply its
/// highest row — the same relationship `goldcoin_indexed_blocks` has with
/// `Ledger::goldcoin_chain_tip`. Reusing that shape is what makes the
/// reorg walk possible at all: a block hash commits to its parent hash,
/// and so transitively to its entire ancestry, so an anchor whose hash
/// still matches the live chain certifies every block beneath it.
///
/// Unlike Goldcoin, the anchors are SPARSE. An EVM scanner reads logs in
/// ranges (`eth_getLogs` over `fromBlock..toBlock`) rather than block by
/// block, so it never sees most block headers and must not pretend to:
/// one anchor is written per scanned range, plus one per block that
/// actually contained a deposit. That is enough for the walk, and it is
/// why this table is deliberately NOT named `robinhood_indexed_blocks` —
/// it does not claim to be a complete index of anything.
///
/// # Reorg tombstones, and why the unique index excludes them
///
/// A reorg can genuinely change which deposit an obligation index refers
/// to: the transactions are re-executed on the new chain, and a different
/// interleaving produces a different assignment of counter values. So a
/// provisional observation that is orphaned is marked `finality =
/// 'Reorged'` and kept — an audit record of what this service believed
/// and when — while the identity indexes are scoped `WHERE finality <>
/// 'Reorged'` so the live identity is free to be claimed again by
/// whatever the canonical chain actually contains.
///
/// Tombstoning is only ever applied to `Provisional` rows. A `Final` row
/// being contradicted is not a routine reorg and is not reconciled here
/// or anywhere else automatically: it halts the Robinhood indexer for a
/// human, the same posture `Ledger::detect_post_finality_reorg` takes on
/// the Goldcoin side.
///
/// # Blast radius: a Robinhood halt is Robinhood-local
///
/// `robinhood_indexer_state.halt_reason` is a persisted, Robinhood-only
/// stop. It deliberately does NOT pause either reserve, unlike the
/// Goldcoin post-finality path. A Robinhood route is disabled, settles
/// nothing and holds no reserve, so letting a fault in an observation-only
/// indexer stop live Solana<->Goldcoin traffic would convert a visibility
/// problem into an outage. The halt is reported through the indexer's own
/// health state instead.
fn apply_v22(conn: &Connection) -> Result<(), LedgerError> {
    // Structural idempotence, the v9/v16/v21 discipline: the real current
    // shape of the database decides whether there is work to do.
    if table_exists(conn, "robinhood_deposit_observations")? {
        return Ok(());
    }

    conn.execute_batch(
        r#"
        -- ------------------------------------------- Robinhood scan anchors --
        CREATE TABLE robinhood_scanned_blocks (
            block_number INTEGER PRIMARY KEY CHECK (block_number >= 0),
            block_hash   BLOB NOT NULL CHECK (length(block_hash) = 32),
            scanned_at   INTEGER NOT NULL
        );

        -- -------------------------------------------- Robinhood halt state --
        -- Singleton. Absent row == never started; present row with a NULL
        -- halt_reason == running normally.
        CREATE TABLE robinhood_indexer_state (
            id           INTEGER PRIMARY KEY CHECK (id = 0),
            halt_reason  TEXT,
            halt_detail  TEXT,
            halted_at    INTEGER,
            updated_at   INTEGER NOT NULL,
            -- A halt has a reason and a time, or it is not a halt.
            CHECK ((halt_reason IS NULL) = (halted_at IS NULL)),
            CHECK (halt_detail IS NULL OR halt_reason IS NOT NULL)
        );

        -- -------------------------------------- Robinhood deposit sightings --
        CREATE TABLE robinhood_deposit_observations (
            id                      INTEGER PRIMARY KEY,

            -- ---- durable, chain-and-contract-qualified identity (v21 shape) ----
            source_chain            TEXT NOT NULL CHECK (source_chain = 'robinhood'),
            source_contract         BLOB NOT NULL CHECK (length(source_contract) = 20),
            source_obligation_index INTEGER NOT NULL CHECK (source_obligation_index >= 0),

            -- ---- what the event said ----
            -- Both spellings of the route are stored: the wire byte the
            -- contract emitted, and this service's own route name. Neither
            -- is derived from the other at read time, so a future
            -- renumbering on either side surfaces as a disagreement
            -- between two recorded facts instead of silently re-labelling
            -- history. Only the two INBOUND ids exist here; an outbound or
            -- unknown id is refused by the decoder and never reaches this
            -- table.
            contract_route_id       INTEGER NOT NULL CHECK (contract_route_id IN (2, 4)),
            route                   TEXT NOT NULL CHECK (route IN ('RhnToGlc','RhnToSol')),
            depositor               BLOB NOT NULL CHECK (length(depositor) = 20),
            -- Opaque destination payload on the route's destination
            -- network, exactly as the contract stored it (1..64 bytes,
            -- MAX_DESTINATION_LEN). Never parsed here.
            destination             BLOB NOT NULL
                                    CHECK (length(destination) BETWEEN 1 AND 64),
            -- The exact 32-byte big-endian uint256 word; see this
            -- migration's docs for why this one is not an INTEGER.
            amount_robinhood_atomic BLOB NOT NULL
                                    CHECK (length(amount_robinhood_atomic) = 32),
            amount_canonical_atomic INTEGER NOT NULL CHECK (amount_canonical_atomic > 0),

            -- ---- where the event sits in chain history ----
            tx_hash                 BLOB NOT NULL CHECK (length(tx_hash) = 32),
            log_index               INTEGER NOT NULL CHECK (log_index >= 0),
            block_number            INTEGER NOT NULL CHECK (block_number >= 0),
            block_hash              BLOB NOT NULL CHECK (length(block_hash) = 32),

            -- ---- lifecycle ----
            finality                TEXT NOT NULL
                                    CHECK (finality IN ('Provisional','Final','Reorged')),
            observed_at             INTEGER NOT NULL,
            finalized_at            INTEGER,
            reorged_at              INTEGER,

            -- OBSERVATION ONLY. The single permitted value is 0. This is
            -- not a flag waiting to be flipped: settling a Robinhood
            -- deposit requires dropping this CHECK in a migration, which
            -- is a reviewable change, rather than an UPDATE anyone could
            -- write. See this migration's docs.
            settled                 INTEGER NOT NULL DEFAULT 0 CHECK (settled = 0),

            CHECK ((finality = 'Final')   = (finalized_at IS NOT NULL)),
            CHECK ((finality = 'Reorged') = (reorged_at   IS NOT NULL))
        );

        -- THE replay guard. Scoped past tombstones so an orphaned
        -- provisional sighting does not permanently burn the identity its
        -- obligation index names — see this migration's docs.
        CREATE UNIQUE INDEX ux_robinhood_obligation_source
            ON robinhood_deposit_observations
               (source_chain, source_contract, source_obligation_index)
            WHERE finality <> 'Reorged';

        -- The log's own identity (crate::evm::EvmLogId): one transaction
        -- can emit this event more than once, so the pair is the unit, not
        -- the transaction hash. Independent of the guard above on purpose:
        -- if one log ever claimed two obligation indexes, or two logs one
        -- index, exactly one of these two indexes trips and the indexer
        -- halts rather than recording a story that cannot be true.
        CREATE UNIQUE INDEX ux_robinhood_log_identity
            ON robinhood_deposit_observations (tx_hash, log_index)
            WHERE finality <> 'Reorged';

        CREATE INDEX ix_robinhood_observations_finality
            ON robinhood_deposit_observations (finality, block_number);

        -- ------------------------------------------ Robinhood reorg journal --
        CREATE TABLE robinhood_reorg_events (
            id             INTEGER PRIMARY KEY,
            detected_at    INTEGER NOT NULL,
            fork_block     INTEGER NOT NULL,
            fork_hash      BLOB NOT NULL CHECK (length(fork_hash) = 32),
            old_tip_block  INTEGER NOT NULL,
            old_tip_hash   BLOB NOT NULL CHECK (length(old_tip_hash) = 32),
            orphaned_count INTEGER NOT NULL CHECK (orphaned_count >= 0)
        );
        "#,
    )?;
    Ok(())
}

/// v23: the Robinhood SETTLEMENT schema — the migration that makes the
/// two Goldcoin<->Robinhood routes executable at all.
///
/// # What v22 deliberately refused, and what this reverses
///
/// v22 added an OBSERVATION store and said so structurally: `settled
/// INTEGER NOT NULL DEFAULT 0 CHECK (settled = 0)`, a column whose only
/// permitted value was "not settled", precisely so that settling a
/// Robinhood deposit would require dropping a CHECK in a migration a
/// reviewer would see. This is that migration, and this is that
/// reviewer's paragraph.
///
/// Three widenings and four new tables:
///
/// 1. `bridge_requests.direction` gains `'GlcToRhn'` and `'RhnToGlc'`.
///    The v1 CHECK — an independent backstop underneath `Direction`'s
///    two-variant type — is widened to four, matching the two new
///    variants. `'SolToRhn'`/`'RhnToSol'` are deliberately NOT added:
///    those routes stay non-executable, and a database that cannot spell
///    them is a second, independent guarantee of that on top of the
///    absent `Direction` variants.
/// 2. `reserve_ledger.direction` gains `'RobinhoodReserve'`. A third
///    physical reserve, accounted separately from the Goldcoin and
///    Solana ones and never netted against either.
/// 3. `robinhood_deposit_observations.settled` becomes `CHECK (settled IN
///    (0,1))` and gains `folded_request_id`, the link to the
///    `bridge_requests` row a finalized observation folded into.
///
/// Then the outbound-transaction machinery: `robinhood_transactions`,
/// `robinhood_authorization_signatures`, and `evm_submitter_state`.
///
/// # How the CHECK widenings are performed
///
/// SQLite cannot alter a CHECK constraint, so each of the three tables
/// is rebuilt. The rebuild does NOT retype the table's DDL by hand —
/// `reserve_ledger` alone has accumulated nine `ALTER TABLE ADD COLUMN`
/// migrations since v1, and a hand-written column list is exactly how a
/// column gets silently dropped. Instead
/// [`widen_check_constraint`] reads the table's REAL current DDL out of
/// `sqlite_master`, replaces one exact substring in it, and copies rows
/// through a column list read from `PRAGMA table_info`. Every index and
/// trigger attached to the table is captured before the drop and
/// replayed after the rename, from the same authoritative source.
///
/// The substring must occur exactly once or the migration refuses to
/// run. A CHECK clause that has been reworded since — or one that
/// appears twice because a later column reused the phrasing — is a
/// database this code does not understand, and guessing at it is worse
/// than stopping.
///
/// # Reserve accounting for an 18-decimal chain
///
/// `RobinhoodReserve`'s monetary columns are CANONICAL 8-decimal units,
/// like every other reserve row, NOT Robinhood's native 18 decimals. At
/// 18 decimals one whole GLC is 10^18, so `i64::MAX` is about 9.2 GLC and
/// an INTEGER column would overflow on a single real transfer.
///
/// This loses nothing. The two units are related by an exact factor of
/// 10^10 (`amount_conversion::robinhood::CANONICAL_TO_ROBINHOOD_SCALE`)
/// and every amount that crosses the boundary is required to be an exact
/// multiple of it — the contract's own `_requireCanonicalAmount` refuses
/// anything else on-chain, and `RobinhoodAtomic::to_canonical` refuses
/// anything else off-chain. A Robinhood balance that is not exactly
/// representable in canonical units cannot arise from any path this
/// bridge participates in, and if one is ever observed the conversion
/// fails loudly rather than rounding.
///
/// # The transaction table owns the nonce
///
/// `robinhood_transactions.nonce` is a column of the operation it belongs
/// to, not a row in a separate allocator table. That is the whole
/// idempotency design in one structural decision: a nonce and the
/// operation it was allocated for commit or roll back together, so there
/// is no window in which a nonce exists without an owner or an operation
/// exists with a nonce someone else also holds.
/// `ux_robinhood_tx_nonce` makes the second half of that a database
/// guarantee rather than a code convention.
///
/// # Why the raw transaction is stored
///
/// `raw_tx` holds the exact bytes that were (or are about to be)
/// broadcast, written BEFORE the first `eth_sendRawTransaction` and never
/// rewritten afterwards. After a crash mid-broadcast the service does not
/// have to reconstruct anything or decide whether to re-sign: it
/// re-broadcasts the identical bytes, which is either already in the
/// mempool (`already known`), already mined, or accepted — all three of
/// which converge on the same single transaction. Rebuilding instead
/// would risk producing a second transaction under the same nonce with
/// different fees, which is a replacement race the service did not
/// choose.
fn apply_v23(conn: &Connection) -> Result<(), LedgerError> {
    // Structural idempotence, the v9/v16/v21/v22 discipline: the real
    // current shape of the database decides whether there is work to do.
    if table_exists(conn, "robinhood_transactions")? {
        return Ok(());
    }

    // `PRAGMA foreign_keys` is a silent no-op inside a transaction, so it
    // must be toggled out here — and restored on every path out,
    // including the failure path. Three of the rebuilds below drop a
    // table that other tables reference.
    let foreign_keys_were_on: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = apply_v23_inner(conn);
    if foreign_keys_were_on {
        conn.pragma_update(None, "foreign_keys", "ON")?;
    }
    result
}

/// The v23 body proper, inside ONE `IMMEDIATE` transaction. A process
/// killed at any point — including after a `DROP` — leaves every original
/// table exactly as it was, because SQLite rolls an uncommitted
/// transaction back on the next open; the idempotence probe above then
/// simply sees the old shape again and reruns this from the top.
fn apply_v23_inner(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    match stage_v23(conn) {
        Ok(()) => {
            conn.execute_batch("COMMIT;")?;
            Ok(())
        }
        Err(e) => {
            // Best-effort: if the rollback itself fails the transaction is
            // still not committed, and every original table still stands.
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

fn stage_v23(conn: &Connection) -> Result<(), LedgerError> {
    // ---- 1. bridge_requests: two more settlement directions ----
    widen_check_constraint(
        conn,
        "bridge_requests",
        "direction IN ('GlcToSol','SolToGlc')",
        "direction IN ('GlcToSol','SolToGlc','GlcToRhn','RhnToGlc')",
    )?;

    // ---- 2. reserve_ledger: a third physical reserve ----
    widen_check_constraint(
        conn,
        "reserve_ledger",
        "direction IN ('GoldcoinReserve','SolanaReserve')",
        "direction IN ('GoldcoinReserve','SolanaReserve','RobinhoodReserve')",
    )?;

    // ---- 3. observations: settled becomes a real flag ----
    widen_check_constraint(
        conn,
        "robinhood_deposit_observations",
        "settled                 INTEGER NOT NULL DEFAULT 0 CHECK (settled = 0)",
        "settled                 INTEGER NOT NULL DEFAULT 0 CHECK (settled IN (0,1))",
    )?;
    conn.execute_batch(
        "ALTER TABLE robinhood_deposit_observations
            ADD COLUMN folded_request_id INTEGER REFERENCES bridge_requests(id);",
    )?;
    conn.execute_batch(
        // At most one request per observation, and at most one observation
        // per request. Both halves matter: the first is the replay guard
        // for the fold, the second stops two observations claiming one
        // payout. Scoped past tombstones for the same reason v22's
        // identity indexes are — a reorged sighting must not permanently
        // burn the request it briefly pointed at.
        "CREATE UNIQUE INDEX ux_robinhood_observation_request
             ON robinhood_deposit_observations (folded_request_id)
             WHERE folded_request_id IS NOT NULL AND finality <> 'Reorged';",
    )?;

    // ---- 4. the outbound EVM transaction machinery ----
    conn.execute_batch(
        r#"
        -- --------------------------------------------- submitter nonce state --
        -- The submitter EOA's reconciliation cursor: the highest nonce this
        -- service believes the chain has seen from it, refreshed from
        -- `eth_getTransactionCount(submitter, "pending")` at startup and
        -- whenever a broadcast reports a nonce disagreement.
        --
        -- This is a CACHE and is never the allocator. Allocation reads the
        -- maximum nonce actually recorded in `robinhood_transactions` and
        -- takes the next one, inside the same transaction that writes the
        -- row — so a stale or lost row here cannot cause two operations to
        -- share a nonce, only cause the first allocation after a restart to
        -- start from a conservative place.
        --
        -- Keyed by (submitter, chain_id) so pointing the service at a
        -- different network, or rotating the submitter key, starts a fresh,
        -- separate sequence rather than inheriting a foreign one.
        CREATE TABLE evm_submitter_state (
            submitter       BLOB NOT NULL CHECK (length(submitter) = 20),
            chain_id        INTEGER NOT NULL CHECK (chain_id > 0),
            observed_nonce  INTEGER NOT NULL CHECK (observed_nonce >= 0),
            observed_at     INTEGER NOT NULL,
            PRIMARY KEY (submitter, chain_id)
        );

        -- ------------------------------------- outbound Robinhood operations --
        CREATE TABLE robinhood_transactions (
            id                   INTEGER PRIMARY KEY,

            -- ---- what this operation is ----
            -- 'Payout'      GlcToRhn: pay reserve GLC to a Robinhood recipient.
            -- 'Settlement'  RhnToGlc: mark an obligation settled AFTER its
            --               Goldcoin payout confirmed.
            -- 'Refund'      RhnToGlc: return an obligation's exact principal.
            kind                 TEXT NOT NULL
                                 CHECK (kind IN ('Payout','Settlement','Refund')),
            request_id           INTEGER NOT NULL REFERENCES bridge_requests(id),
            -- Only the two executable routes. The database cannot spell
            -- 'SolToRhn'/'RhnToSol', so a Solana-Robinhood operation cannot
            -- be recorded even if code somehow constructed one.
            route                TEXT NOT NULL CHECK (route IN ('GlcToRhn','RhnToGlc')),

            -- ---- the contract-side identity this operation was authorized against ----
            -- Recorded, not looked up at use time: an operation authorized
            -- against one deployment on one network must never be replayed
            -- against another, and the way to guarantee that is to store what
            -- it was signed for and compare before every broadcast.
            bridge_contract      BLOB NOT NULL CHECK (length(bridge_contract) = 20),
            chain_id             INTEGER NOT NULL CHECK (chain_id > 0),
            -- The contract's ACTION discriminator: 1 payout, 2 refund,
            -- 3 settle. Stored alongside `kind` rather than derived from it,
            -- for the same reason v22 stores both spellings of the route: a
            -- renumbering on either side becomes a disagreement between two
            -- recorded facts instead of a silent relabelling.
            action               INTEGER NOT NULL CHECK (action IN (1,2,3)),
            contract_request_id  BLOB NOT NULL CHECK (length(contract_request_id) = 32),
            -- Settlement and refund name an obligation; a payout does not.
            obligation_index     INTEGER CHECK (obligation_index IS NULL
                                                OR obligation_index >= 0),
            -- Payout and refund name a recipient and an amount; a settlement
            -- moves nothing and names neither.
            recipient            BLOB CHECK (recipient IS NULL OR length(recipient) = 20),
            -- The exact 32-byte big-endian uint256, never narrowed: at 18
            -- decimals an INTEGER column overflows on a single real transfer.
            amount_robinhood     BLOB CHECK (amount_robinhood IS NULL
                                             OR length(amount_robinhood) = 32),
            signer_epoch         INTEGER NOT NULL CHECK (signer_epoch >= 0),
            expiry               INTEGER NOT NULL CHECK (expiry > 0),
            -- The EIP-712 digest the quorum actually signed. Re-derived and
            -- compared on every use, so a payload that changed underneath a
            -- collected signature set is caught rather than broadcast.
            auth_digest          BLOB NOT NULL CHECK (length(auth_digest) = 32),

            -- ---- the submitter and its nonce ----
            submitter            BLOB CHECK (submitter IS NULL OR length(submitter) = 20),
            nonce                INTEGER CHECK (nonce IS NULL OR nonce >= 0),

            -- ---- the signed transaction, written BEFORE the first broadcast ----
            envelope             TEXT CHECK (envelope IS NULL
                                             OR envelope IN ('legacy','eip1559')),
            gas_limit            INTEGER CHECK (gas_limit IS NULL OR gas_limit > 0),
            -- Operator-facing rendering of the fee fields actually signed.
            -- Never parsed; `raw_tx` is the authority on what was sent.
            fee_summary          TEXT,
            raw_tx               BLOB,
            tx_hash              BLOB CHECK (tx_hash IS NULL OR length(tx_hash) = 32),

            -- ---- lifecycle ----
            state                TEXT NOT NULL CHECK (state IN (
                                     'Authorizing','Authorized','Signed','Broadcast',
                                     'Included','Finalized','Reverted','ManualReview')),
            first_broadcast_at   INTEGER,
            last_broadcast_at    INTEGER,
            broadcast_attempts   INTEGER NOT NULL DEFAULT 0
                                 CHECK (broadcast_attempts >= 0),
            replacement_attempts INTEGER NOT NULL DEFAULT 0
                                 CHECK (replacement_attempts >= 0),
            -- 1 = included and succeeded, 0 = included and REVERTED. A
            -- reverted transaction is not a failure to retry blindly; see
            -- `Ledger::record_robinhood_receipt`.
            receipt_status       INTEGER CHECK (receipt_status IS NULL
                                                OR receipt_status IN (0,1)),
            receipt_block_number INTEGER CHECK (receipt_block_number IS NULL
                                                OR receipt_block_number >= 0),
            receipt_block_hash   BLOB CHECK (receipt_block_hash IS NULL
                                             OR length(receipt_block_hash) = 32),
            confirmations        INTEGER NOT NULL DEFAULT 0 CHECK (confirmations >= 0),
            finalized_at         INTEGER,
            failure_reason       TEXT,
            created_at           INTEGER NOT NULL,
            updated_at           INTEGER NOT NULL,

            -- ---- table constraints ----
            -- A settlement or refund names an obligation; a payout must not.
            CHECK ((kind = 'Payout') = (obligation_index IS NULL)),
            -- A settlement moves no value and names no recipient or amount;
            -- a payout and a refund name both, or neither.
            CHECK ((kind = 'Settlement') = (recipient IS NULL)),
            CHECK ((recipient IS NULL) = (amount_robinhood IS NULL)),
            -- The action byte and the kind must agree. Stated as data rather
            -- than trusted: these are two independent recordings of one fact.
            CHECK ((kind = 'Payout')     = (action = 1)),
            CHECK ((kind = 'Refund')     = (action = 2)),
            CHECK ((kind = 'Settlement') = (action = 3)),
            -- A payout is the outbound route; the obligation-closing
            -- operations are the inbound one.
            CHECK ((kind = 'Payout') = (route = 'GlcToRhn')),
            -- A nonce belongs to a submitter. Neither exists without the
            -- other, so a row can never hold a nonce nobody allocated.
            CHECK ((submitter IS NULL) = (nonce IS NULL)),
            -- Signed bytes, their hash and the envelope they were built in
            -- arrive together and are never partially present.
            CHECK ((raw_tx IS NULL) = (tx_hash IS NULL)),
            CHECK ((raw_tx IS NULL) = (envelope IS NULL)),
            -- THE ordering invariant, enforced by the database rather than
            -- by the order of statements in a function: nothing can be
            -- broadcast until it has been signed, and nothing can be signed
            -- until a nonce was allocated for it.
            CHECK (state NOT IN ('Signed','Broadcast','Included','Finalized','Reverted')
                   OR (raw_tx IS NOT NULL AND nonce IS NOT NULL)),
            -- A finalized operation has a successful receipt. There is no
            -- path to 'Finalized' that skips reading one back.
            CHECK (state <> 'Finalized' OR receipt_status = 1),
            CHECK ((finalized_at IS NULL) = (state <> 'Finalized')),
            -- A broadcast has a first-broadcast time, and a first-broadcast
            -- time means it was broadcast.
            CHECK ((first_broadcast_at IS NULL) = (broadcast_attempts = 0))
        );

        -- ONE operation of each kind per bridge request, ever. This is the
        -- structural half of "no duplicate payout, no duplicate settlement,
        -- no duplicate refund" — a second attempt cannot be inserted, so a
        -- duplicate is a constraint violation rather than a second transfer.
        CREATE UNIQUE INDEX ux_robinhood_tx_operation
            ON robinhood_transactions (kind, request_id);

        -- The CONTRACT's own replay key, mirrored: it consumes
        -- `(action, requestId)` exactly once, per deployment. Qualified by
        -- the deployment address and chain so a successor contract's
        -- identical request id is a different row.
        CREATE UNIQUE INDEX ux_robinhood_tx_contract_request
            ON robinhood_transactions (chain_id, bridge_contract, action, contract_request_id);

        -- THE nonce guard. Two operations can never hold the same nonce for
        -- the same submitter on the same chain — not by convention, not by
        -- serialized code, but because the insert fails.
        CREATE UNIQUE INDEX ux_robinhood_tx_nonce
            ON robinhood_transactions (submitter, chain_id, nonce)
            WHERE nonce IS NOT NULL;

        CREATE INDEX ix_robinhood_tx_state ON robinhood_transactions (state, id);

        -- --------------------------------------- collected 2-of-3 signatures --
        -- Durable, so a restart after the quorum was gathered re-broadcasts
        -- the SAME authorization rather than asking three custody domains to
        -- sign again — which would be harmless but slow, and which would
        -- make "how many times was this authorized" an unanswerable question
        -- in an audit.
        CREATE TABLE robinhood_authorization_signatures (
            transaction_id  INTEGER NOT NULL REFERENCES robinhood_transactions(id),
            -- The contract requires EXACTLY two signatures, so the positions
            -- are 0 and 1 and there is no third slot to fill.
            position        INTEGER NOT NULL CHECK (position IN (0,1)),
            -- The address recovered locally from the signature over the
            -- recorded digest — never a value the signer merely claimed.
            signer          BLOB NOT NULL CHECK (length(signer) = 20),
            signature       BLOB NOT NULL CHECK (length(signature) = 65),
            created_at      INTEGER NOT NULL,
            PRIMARY KEY (transaction_id, position)
        );

        -- Distinctness, structurally: the contract reverts on
        -- `DuplicateSignerSignature`, and this makes it impossible to have
        -- stored the pair that would trigger it.
        CREATE UNIQUE INDEX ux_robinhood_auth_signer_distinct
            ON robinhood_authorization_signatures (transaction_id, signer);
        "#,
    )?;

    // Nothing may be orphaned or dangling before this is allowed to
    // commit. `foreign_key_check` reports violations regardless of
    // enforcement, which is why the SQLite recipe puts it here — foreign
    // keys are disabled for the duration of the rebuilds above.
    let fk_violations: i64 =
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    if fk_violations != 0 {
        return Err(LedgerError::SchemaMigrationFailed(format!(
            "v23 rebuild left {fk_violations} foreign-key violations; rolled back"
        )));
    }
    let integrity: String =
        conn.query_row("SELECT * FROM pragma_integrity_check LIMIT 1", [], |r| {
            r.get(0)
        })?;
    if integrity != "ok" {
        return Err(LedgerError::SchemaMigrationFailed(format!(
            "v23 rebuild failed integrity_check: {integrity}; rolled back"
        )));
    }
    Ok(())
}

/// v24 — `bridge_routes`, the persisted LEDGER leg of
/// [`crate::routes::RouteGate`]'s three-place AND.
///
/// Designed in docs/30-robinhood-network-phase1.md and deliberately
/// deferred there ("the deferred v22 migration"), because Phase 1 could
/// not bump `CURRENT_SCHEMA_VERSION` without locking the then-deployed
/// daemon out of any ledger it touched. That constraint is gone: this
/// binary ships the bump together with the table. The number moved
/// 22 -> 24 exactly as that document said to re-check it — v22 and v23
/// were taken in the meantime, and v24 was re-confirmed free across every
/// local and remote ref before this was written.
///
/// # This migration changes no behaviour
///
/// [`crate::ledger::Ledger::route_enabled`] already resolves an absent
/// table and an absent row to [`crate::routes::Route::default_enabled`].
/// The six seeded rows carry EXACTLY those defaults — `1` for the two
/// legacy Solana<->Goldcoin routes, `0` for all four Robinhood routes —
/// so a production ledger that upgrades through this migration admits
/// precisely what it admitted before it, and `GET /chains` reports what
/// it reported before it. What the table adds is not a new verdict but a
/// place to WRITE one: [`crate::ledger::Ledger::set_route_enabled`] (the
/// two EXECUTABLE Robinhood routes only) is the supported way to open a
/// route in ledger state, and it needs a row to update.
/// `the_seeded_rows_match_the_route_registry_defaults` pins the seeded
/// values against `Route::default_enabled` so the two cannot drift.
///
/// Six rows, not four: the custody contract models `SolToRhn`/`RhnToSol`
/// structurally and ships them disabled, so the ledger records their
/// disabled state rather than leaving it unrepresented. Neither is
/// operator-settable — `Route::as_direction` yields `None` for both, so
/// there is no settlement machinery to enable — and
/// `Ledger::set_route_enabled` refuses them.
///
/// # A pre-existing `bridge_routes` of the wrong shape fails LOUDLY
///
/// `CREATE TABLE IF NOT EXISTS` no-ops against a table that already
/// exists, so a database where someone hand-created the two-column
/// `(route_id, enabled)` form that appears in this repository's TEST
/// fixtures would silently skip the create and then fail the seed with a
/// bare "no such column". That case is checked for explicitly, BEFORE
/// this migration writes anything, and reported as the actionable
/// migration refusal it is: `open_and_migrate` returns the error, the
/// version marker is never advanced, and the same binary can simply be
/// run again once the table has been dealt with.
fn apply_v24(conn: &Connection) -> Result<(), LedgerError> {
    let table_exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'bridge_routes')",
        [],
        |r| r.get::<_, i64>(0).map(|v| v != 0),
    )?;
    if table_exists && !column_exists(conn, "bridge_routes", "source_chain")? {
        return Err(LedgerError::SchemaMigrationFailed(
            "v24 found an existing bridge_routes table without a source_chain column — this \
             database carries a hand-created table, not the one this migration defines. Refusing \
             to seed it. Inspect the table's rows, drop it if it is the two-column test-fixture \
             shape, and re-run this binary."
                .to_string(),
        ));
    }

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS bridge_routes (
            route_id          TEXT PRIMARY KEY,
            source_chain      TEXT NOT NULL,
            destination_chain TEXT NOT NULL,
            -- Fail closed: a route with no explicit opinion is a route
            -- that is off. Only the seed below ever writes `1` without an
            -- operator asking for it, and only for the two legacy routes.
            enabled           INTEGER NOT NULL DEFAULT 0,
            disabled_reason   TEXT,
            updated_at        INTEGER NOT NULL
        );

        -- OR IGNORE, never OR REPLACE: on a re-run (or a partially
        -- applied migration) an operator's own `enabled = 1` must survive
        -- untouched. A migration that re-seeded would silently close a
        -- route an operator had deliberately opened.
        INSERT OR IGNORE INTO bridge_routes
            (route_id, source_chain, destination_chain, enabled, disabled_reason, updated_at)
        VALUES
            ('GlcToSol', 'goldcoin',  'solana',    1, NULL, CAST(strftime('%s', 'now') AS INTEGER)),
            ('SolToGlc', 'solana',    'goldcoin',  1, NULL, CAST(strftime('%s', 'now') AS INTEGER)),
            ('GlcToRhn', 'goldcoin',  'robinhood', 0, NULL, CAST(strftime('%s', 'now') AS INTEGER)),
            ('RhnToGlc', 'robinhood', 'goldcoin',  0, NULL, CAST(strftime('%s', 'now') AS INTEGER)),
            ('SolToRhn', 'solana',    'robinhood', 0, NULL, CAST(strftime('%s', 'now') AS INTEGER)),
            ('RhnToSol', 'robinhood', 'solana',    0, NULL, CAST(strftime('%s', 'now') AS INTEGER));
        "#,
    )?;
    Ok(())
}

/// v25 — `route_admission`, a ROUTE-SCOPED admission gate for the two
/// inbound-to-Goldcoin routes.
///
/// # The problem it solves
///
/// Before this table, `SolToGlc` and `RhnToGlc` had exactly one shared
/// admission control between them: the `GoldcoinReserve` row's `paused`
/// and `admission_closed`. Both routes settle out of that one reserve,
/// so closing either flag closed BOTH routes and there was no supported
/// way to hold one open while the other was shut — the `bridge_routes`
/// enable flag is a different axis and
/// [`crate::routes::Route::is_operator_settable`] refuses `SolToGlc`
/// outright.
///
/// This table adds the missing scope. It does NOT replace the
/// reserve-wide controls, which remain exactly what they were: a
/// reserve-wide emergency stop that still closes everything drawing on
/// that reserve. A route is admitted only when the reserve-wide gates
/// AND its own row both say yes — see
/// [`crate::ledger::InboundAdmissionGates::blocker`], which evaluates
/// both in one ranking for both folds and for `GET /chains`.
///
/// # Two rows, not six
///
/// The opposite choice from v24's `bridge_routes`, and deliberately so.
/// `bridge_routes` seeds all six routes because ENABLEMENT is a
/// meaningful question for every route (the custody contract models all
/// four Robinhood ones structurally, so recording their disabled state
/// beats leaving it unrepresented). Route-level ADMISSION is meaningful
/// only where a route draws on a reserve an inbound fold gates against,
/// which is exactly [`crate::ledger::Direction::destination_is_goldcoin`]
/// — `SolToGlc` and `RhnToGlc`.
///
/// So the CHECK on `route_id` is not decoration: it makes the table
/// incapable of holding a row for `GlcToSol`, which is the same refusal
/// `Route::is_admission_settable` and
/// [`crate::ledger::Ledger::set_route_admission`] enforce in Rust, made
/// a second time and independently by the database. A hand-written
/// `INSERT` cannot give `GlcToSol` a route-level off switch, which is
/// precisely the "second, divergent spelling of turn off production
/// traffic" this repository has refused since `RoutesConfig::
/// with_robinhood`.
///
/// # This migration changes no behaviour
///
/// Both seeded rows carry `admission_closed = 0` — OPEN — so a
/// production ledger that upgrades through this migration admits exactly
/// what it admitted before it, and `GET /chains` reports exactly what it
/// reported before it. `the_v25_seed_leaves_every_route_admission_open`
/// pins that.
///
/// What the table adds is not a new verdict but a place to WRITE one.
/// [`crate::ledger::Ledger::set_route_admission`] (the two
/// inbound-to-Goldcoin routes only) is the supported way to close or
/// open a route's admission, and it needs a row to update.
///
/// # Absence resolves to OPEN, and that inverts the usual rule
///
/// [`crate::ledger::Ledger::route_admission_closed`] resolves an absent
/// table and an absent row to `false` (open), where `route_enabled`
/// resolves absence to `Route::default_enabled` and every other gate in
/// this service fails closed.
///
/// The inversion is correct here and is the whole reason the upgrade is
/// safe. Absence of this table IS the pre-v25 state, in which no
/// route-level admission gate existed at all; resolving it to "closed"
/// would close `SolToGlc` on every ledger that has not yet migrated —
/// i.e. the migration itself would be the outage. This gate can only
/// ever SUBTRACT availability from what the reserve-wide gates already
/// allow, so its absence can never admit something the reserve would
/// have refused. Every reserve-wide gate keeps its fail-closed
/// semantics untouched, and a ledger READ ERROR (as opposed to a clean
/// absence) still renders unavailable in `api::route_availability`,
/// which is unchanged.
///
/// # A pre-existing `route_admission` of the wrong shape fails LOUDLY
///
/// Same hazard v24 documents for `bridge_routes`: `CREATE TABLE IF NOT
/// EXISTS` no-ops against a table that already exists, so a database
/// carrying a hand-created `route_admission` of some other shape would
/// silently skip the create and then fail the seed with a bare "no such
/// column". That case is checked for explicitly, BEFORE this migration
/// writes anything, and reported as the actionable migration refusal it
/// is: `open_and_migrate` returns the error, the version marker is never
/// advanced, and the same binary can simply be run again once the table
/// has been dealt with.
fn apply_v25(conn: &Connection) -> Result<(), LedgerError> {
    let table_exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = \
         'route_admission')",
        [],
        |r| r.get::<_, i64>(0).map(|v| v != 0),
    )?;
    if table_exists && !column_exists(conn, "route_admission", "admission_closed")? {
        return Err(LedgerError::SchemaMigrationFailed(
            "v25 found an existing route_admission table without an admission_closed column — \
             this database carries a hand-created table, not the one this migration defines. \
             Refusing to seed it. Inspect the table's rows, drop it if it is not route-admission \
             state this binary wrote, and re-run this binary."
                .to_string(),
        ));
    }

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS route_admission (
            -- The CHECK is the database's own copy of
            -- `Route::is_admission_settable`: route-level admission
            -- exists for the two INBOUND-TO-GOLDCOIN routes and for
            -- nothing else, so a row for any other route cannot be
            -- written here at all — not by this binary, and not by hand.
            route_id                TEXT PRIMARY KEY
                                      CHECK (route_id IN ('SolToGlc','RhnToGlc')),
            -- Fail OPEN, uniquely in this schema: see the module docs
            -- above. A route with no recorded opinion is a route whose
            -- admission nobody has closed, which is the pre-v25 state.
            admission_closed        INTEGER NOT NULL DEFAULT 0
                                      CHECK (admission_closed IN (0,1)),
            admission_closed_reason TEXT,
            updated_at              INTEGER NOT NULL
        );

        -- OR IGNORE, never OR REPLACE — the same discipline v24 records
        -- for `bridge_routes`, pointing the other way: on a re-run (or a
        -- partially applied migration) an operator's own
        -- `admission_closed = 1` must survive untouched. A migration that
        -- re-seeded would silently REOPEN a route an operator had
        -- deliberately closed, which is the direction that moves money.
        INSERT OR IGNORE INTO route_admission
            (route_id, admission_closed, admission_closed_reason, updated_at)
        VALUES
            ('SolToGlc', 0, NULL, CAST(strftime('%s', 'now') AS INTEGER)),
            ('RhnToGlc', 0, NULL, CAST(strftime('%s', 'now') AS INTEGER));
        "#,
    )?;
    Ok(())
}

/// v27 — the two Solana<->Robinhood routes become spellable.
///
/// # What it widens, and why each is a CHECK and not a column
///
/// Three constraints, each the database's own independent copy of a
/// fact the type system also states, and each deliberately left narrow
/// until the machinery behind it existed (docs/32 §11, docs/33 §7):
///
/// 1. `bridge_requests.direction` — `'SolToRhn'` and `'RhnToSol'` join
///    the four settlement directions. A request row for either can now
///    be written by exactly the two folds that produce them
///    (`Ledger::fold_sol_deposit` for a Solana deposit whose destination
///    is an EVM address; `Ledger::fold_robinhood_deposit` for a
///    `DepositCreated` on contract route `0x04`).
/// 2. `robinhood_transactions.route` — a `Payout` on `'SolToRhn'` and a
///    `Settlement`/`Refund` on `'RhnToSol'` are now recordable. The
///    `kind` and `action` CHECKs are untouched: the cross routes use the
///    same three operations, not new ones. Widened against the shape
///    v26 left behind (`route` nullable, NULL exactly for a
///    `TreasuryWithdraw`), so the `route IS NULL OR` arm of both
///    constraints is carried through verbatim: a withdrawal still has
///    no route, and a payout is still exactly an outbound route.
/// 3. `route_admission.route_id` — both cross routes get a route-scoped
///    admission row, seeded OPEN (`admission_closed = 0`) exactly as v25
///    seeded the two Goldcoin-bound routes, and for the same reason: this
///    gate can only ever SUBTRACT availability from what the reserve-wide
///    gates and the enablement gate already allow, and enablement for
///    both routes is still `0` in `bridge_routes` and `false` in every
///    config template. Seeding it closed would add a second switch an
///    operator must flip without adding any safety the enablement gate
///    does not already provide.
///
/// # This migration changes no behaviour
///
/// No row is rewritten, no route is enabled, no reserve figure moves. A
/// production ledger that upgrades through this migration folds, settles
/// and reports exactly what it did before it: the only rows that can
/// exercise the widened constraints are ones a fold writes AFTER an
/// operator has opened a cross route on every gate.
///
/// # Ordering against v26
///
/// v26 REBUILDS `robinhood_transactions` from a literal DDL that spells
/// the two-route vocabulary; v27 then widens that rebuilt table in place
/// (the `from` strings below are v26's exact text). A fresh database and
/// an upgrading one therefore reach the same DDL by the same two steps,
/// and `ux_robinhood_tx_rebalance` — the index v26 adds — is re-attached
/// by the rebuild like every other index on the table.
///
/// # Idempotence
///
/// Structural, the v23 discipline: the real shape of the database decides
/// whether there is work to do. Each widening is skipped when its target
/// DDL already carries the widened text, and the `route_admission` seed
/// is `INSERT OR IGNORE`, so an operator's own `admission_closed = 1` on
/// either new row survives a re-run untouched.
fn apply_v27(conn: &Connection) -> Result<(), LedgerError> {
    // Same reason as v23: `PRAGMA foreign_keys` is a no-op inside a
    // transaction, and `bridge_requests` is referenced by several tables.
    let foreign_keys_were_on: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = apply_v27_inner(conn);
    if foreign_keys_were_on {
        conn.pragma_update(None, "foreign_keys", "ON")?;
    }
    result
}

fn apply_v27_inner(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    match stage_v27(conn) {
        Ok(()) => {
            conn.execute_batch("COMMIT;")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

/// The `bridge_requests.direction` CHECK v27 leaves behind — every
/// settlement direction this binary can spell, and no others. Pinned
/// against `Direction::ALL` by `tests::the_v27_direction_check_names_every_direction`.
pub(super) const V27_DIRECTION_CHECK: &str =
    "direction IN ('GlcToSol','SolToGlc','GlcToRhn','RhnToGlc','SolToRhn','RhnToSol')";

/// The `robinhood_transactions.route` CHECK v27 leaves behind — every
/// route the custody contract models.
pub(super) const V27_ROBINHOOD_TX_ROUTE_CHECK: &str =
    "route IN ('GlcToRhn','RhnToGlc','SolToRhn','RhnToSol')";

/// The kind/route agreement v27 leaves behind on `robinhood_transactions`:
/// a `Payout` row is on an OUTBOUND route and every other kind is on an
/// inbound one — exactly `Direction::destination_is_robinhood`.
pub(super) const V27_PAYOUT_ROUTE_CHECK: &str =
    "CHECK (route IS NULL OR ((kind = 'Payout') = (route IN ('GlcToRhn','SolToRhn'))))";

/// The `route_admission.route_id` CHECK v27 leaves behind — every route
/// `Route::is_admission_settable` admits.
pub(super) const V27_ROUTE_ADMISSION_CHECK: &str =
    "route_id IN ('SolToGlc','RhnToGlc','SolToRhn','RhnToSol')";

fn stage_v27(conn: &Connection) -> Result<(), LedgerError> {
    // ---- 1. bridge_requests: the two cross-route settlement directions ----
    widen_check_constraint_labelled(
        conn,
        "v27",
        "bridge_requests",
        "direction IN ('GlcToSol','SolToGlc','GlcToRhn','RhnToGlc')",
        V27_DIRECTION_CHECK,
    )?;

    // ---- 2. robinhood_transactions: operations on the cross routes ----
    //
    // Two constraints, rebuilt in two passes: the route vocabulary, and
    // the kind/route agreement ("a payout is an outbound route") which
    // must now admit `SolToRhn` as the second outbound route.
    widen_check_constraint_labelled(
        conn,
        "v27",
        "robinhood_transactions",
        "route                TEXT CHECK (route IS NULL OR route IN ('GlcToRhn','RhnToGlc'))",
        &format!(
            "route                TEXT CHECK (route IS NULL OR {V27_ROBINHOOD_TX_ROUTE_CHECK})"
        ),
    )?;
    widen_check_constraint_labelled(
        conn,
        "v27",
        "robinhood_transactions",
        "CHECK (route IS NULL OR ((kind = 'Payout') = (route = 'GlcToRhn')))",
        V27_PAYOUT_ROUTE_CHECK,
    )?;

    // ---- 3. route_admission: a route-scoped gate for each cross route ----
    widen_check_constraint_labelled(
        conn,
        "v27",
        "route_admission",
        "CHECK (route_id IN ('SolToGlc','RhnToGlc'))",
        &format!("CHECK ({V27_ROUTE_ADMISSION_CHECK})"),
    )?;
    conn.execute_batch(
        r#"
        INSERT OR IGNORE INTO route_admission
            (route_id, admission_closed, admission_closed_reason, updated_at)
        VALUES
            ('SolToRhn', 0, NULL, CAST(strftime('%s', 'now') AS INTEGER)),
            ('RhnToSol', 0, NULL, CAST(strftime('%s', 'now') AS INTEGER));
        "#,
    )?;

    // ---- 4. robinhood_transactions: the SolToRhn close-out on Solana ----
    //
    // A `SolToRhn` payout's finality is not the end of its request: the
    // Solana `WithdrawalObligation` that funded it must be closed by
    // `record_goldcoin_completion`, exactly as `SolToGlc` closes its own
    // once the Goldcoin payout confirmed. `goldcoin_payouts` carries that
    // submission for `SolToGlc` in `onchain_completion_signature`/
    // `onchain_completion_submitted_at`; the same two columns, with the
    // same names and meaning, live on the finalized `Payout` row here.
    // NULL for every row of every other kind and route.
    if !column_exists(
        conn,
        "robinhood_transactions",
        "onchain_completion_signature",
    )? {
        conn.execute_batch(
            "ALTER TABLE robinhood_transactions
                ADD COLUMN onchain_completion_signature BLOB
                    CHECK (onchain_completion_signature IS NULL
                           OR length(onchain_completion_signature) = 64);",
        )?;
    }
    if !column_exists(
        conn,
        "robinhood_transactions",
        "onchain_completion_submitted_at",
    )? {
        conn.execute_batch(
            "ALTER TABLE robinhood_transactions ADD COLUMN onchain_completion_submitted_at INTEGER;",
        )?;
    }
    Ok(())
}

/// v28 — the per-request SOURCE WALLET, for the rolling-24h wallet
/// uniqueness rule on every route (`Ledger::wallet_window_blocker_created_at`).
///
/// # Why a column, and why now
///
/// The rule "a source wallet may make at most one bridge attempt per
/// rolling 24 hours" was enforced on two routes before this migration,
/// keyed on two different places: `bridge_requests.requester` (a
/// `SolToGlc` fold's on-chain `WithdrawalObligation.requester`) and
/// `robinhood_deposit_observations.depositor` reached through
/// `folded_request_id` (an `RhnToGlc` fold). The Goldcoin-sourced routes
/// recorded no source identity at all, and the two cross routes recorded
/// one but never consulted it. Six routes, three spellings of "the
/// source wallet", and one query per spelling is exactly the drift this
/// schema keeps designing out — so v28 gives the identity ONE home.
///
/// `source_wallet` holds the wallet the SOURCE chain's own record named
/// as the depositor, spelled the way that chain spells it, and is written
/// by every fold in the same statement as the row it belongs to:
///
/// - Solana-sourced (`SolToGlc`/`SolToRhn`): the 32-byte
///   `WithdrawalObligation.requester` (identical to `requester`, which
///   keeps its existing meaning and readers untouched).
/// - Robinhood-sourced (`RhnToGlc`/`RhnToSol`): the custody contract's
///   20-byte recorded `depositor`.
/// - Goldcoin-sourced (`GlcToSol`/`GlcToRhn`): the address that funded
///   the deposit — the address text of the spent prevout's script when
///   it is a standard P2PKH/P2SH output, the raw script bytes otherwise —
///   traced by `goldcoin::indexer` from the deposit transaction's own
///   inputs at observation time. Before the deposit is seen it holds the
///   address a `POST /transfers` caller DECLARED, if any, so the window
///   is consumed from the moment the request is admitted.
///
/// # Backfill
///
/// Derived from data every existing row already carries, so the rule
/// keeps its history across the upgrade rather than restarting every
/// wallet's window at deploy time: Solana rows copy `requester`;
/// Robinhood rows copy their non-reorged observation's `depositor`.
/// Goldcoin-sourced rows stay NULL — their sender was never recorded and
/// is not re-derived here (that would need chain reads a migration must
/// not make). No `state`, amount, note or reserve figure is touched.
///
/// # Index
///
/// `ix_bridge_requests_source_wallet_window` mirrors v13's
/// `ix_bridge_requests_recipient_window` for the source leg: the window
/// query runs on every fold, every `POST /transfers`, every deposit
/// observation and every resume attempt, and filters on
/// `(direction, source_wallet, created_at)`.
///
/// # Idempotence
///
/// `column_exists`-guarded `ALTER`, `IF NOT EXISTS` index, and a
/// backfill that only ever fills NULLs — a re-run does nothing.
fn apply_v28(conn: &Connection) -> Result<(), LedgerError> {
    if !column_exists(conn, "bridge_requests", "source_wallet")? {
        conn.execute_batch(
            "ALTER TABLE bridge_requests ADD COLUMN source_wallet BLOB
                CHECK (source_wallet IS NULL OR length(source_wallet) > 0);",
        )?;
    }
    conn.execute_batch(
        r#"
        UPDATE bridge_requests
           SET source_wallet = requester
         WHERE source_wallet IS NULL
           AND source_chain = 'solana'
           AND requester IS NOT NULL
           AND length(requester) > 0;

        UPDATE bridge_requests
           SET source_wallet = (
               SELECT o.depositor FROM robinhood_deposit_observations o
                WHERE o.folded_request_id = bridge_requests.id
                  AND o.finality <> 'Reorged'
                  AND length(o.depositor) > 0
                ORDER BY o.id
                LIMIT 1)
         WHERE source_wallet IS NULL
           AND source_chain = 'robinhood';

        CREATE INDEX IF NOT EXISTS ix_bridge_requests_source_wallet_window
            ON bridge_requests(direction, source_wallet, created_at)
            WHERE source_wallet IS NOT NULL;
        "#,
    )?;
    Ok(())
}

/// v29 — a per-request **auto-resume hold** on `bridge_requests`
/// (2026-09-12 incident follow-up).
///
/// `auto_resume_hold_note` (TEXT, NULL = no hold) and
/// `auto_resume_hold_until` (INTEGER unix seconds, informational: the
/// moment the operator intends to act on the row, e.g. refund it).
/// While `auto_resume_hold_note` is set, the daemon's automatic
/// ManualReview recovery pass skips the row and every resume entry
/// point refuses it; refund tooling is unaffected. Set and cleared ONLY
/// by explicit operator command on explicit request ids
/// (`glc-admin manual-review-hold` / `manual-review-hold-release`),
/// never by a fold, a tick or a migration — so a row created after a
/// hold was placed is exactly as it always was (both columns NULL).
///
/// # Why a column and not a note
///
/// `manual_review_note` is the durable key every resume/refund allowlist
/// matches on; rewriting it to "hold" a row would silently disqualify
/// the row from `refund-manual-review`, which is the one thing a held
/// row is being kept FOR. The hold is therefore its own column and its
/// own predicate, and the note keeps meaning what it meant at fold time.
///
/// # Idempotence
///
/// `column_exists`-guarded `ALTER`s; no backfill (every existing row is
/// unheld, which is the correct starting state). A re-run does nothing.
fn apply_v29(conn: &Connection) -> Result<(), LedgerError> {
    if !column_exists(conn, "bridge_requests", "auto_resume_hold_note")? {
        conn.execute_batch(
            "ALTER TABLE bridge_requests ADD COLUMN auto_resume_hold_note TEXT
                CHECK (auto_resume_hold_note IS NULL OR length(auto_resume_hold_note) > 0);",
        )?;
    }
    if !column_exists(conn, "bridge_requests", "auto_resume_hold_until")? {
        conn.execute_batch(
            "ALTER TABLE bridge_requests ADD COLUMN auto_resume_hold_until INTEGER;",
        )?;
    }
    Ok(())
}

/// Whether `ddl` already admits everything `to` would have added: every
/// quoted value in `to` appears in `ddl`. Used only after `from` is known
/// to be absent, so this is asking "did a later migration go past this
/// one", never "should this one run".
fn already_widened(ddl: &str, to: &str) -> bool {
    let values: Vec<&str> = to
        .split('\'')
        .skip(1)
        .step_by(2)
        .filter(|v| !v.is_empty())
        .collect();
    !values.is_empty() && values.iter().all(|v| ddl.contains(&format!("'{v}'")))
}

/// Rebuilds `table` with one exact substring of its DDL replaced —
/// the only way SQLite offers to change a CHECK constraint.
///
/// # Why the DDL is read rather than retyped
///
/// `reserve_ledger` has accumulated nine `ALTER TABLE ADD COLUMN`
/// migrations since v1 and `bridge_requests` thirty-two columns across
/// six. A rebuild that retypes the schema is a rebuild that can silently
/// drop a column an earlier migration added, and the failure would not
/// surface until some unrelated query returned NULL. Reading the real
/// DDL out of `sqlite_master` and copying through a column list read from
/// `PRAGMA table_info` means the new table has exactly the columns the
/// old one had — no more, no fewer, in the same order, with the same
/// types, defaults and constraints.
///
/// # Why exactly one occurrence is required
///
/// `from` must appear once and only once. Zero occurrences means the DDL
/// is not what this migration was written against; two means the
/// replacement is ambiguous. Both are databases this code does not
/// understand, and continuing would rewrite a constraint it cannot
/// predict the effect of.
///
/// # Indexes and triggers
///
/// Everything `sqlite_master` attaches to the table is captured before
/// the drop and replayed after the rename, from that same authoritative
/// source. Auto-created indexes (`sqlite_autoindex_*`, which back
/// `PRIMARY KEY`/`UNIQUE` column constraints) have a NULL `sql` and are
/// skipped: they are recreated by the DDL itself.
///
/// The caller must have foreign keys disabled and must be inside a
/// transaction; both are the caller's job because a rebuild is only ever
/// one step of a larger migration.
fn widen_check_constraint(
    conn: &Connection,
    table: &str,
    from: &str,
    to: &str,
) -> Result<(), LedgerError> {
    widen_check_constraint_labelled(conn, "v23", table, from, to)
}

/// [`widen_check_constraint`] with the migration's own label in its
/// error messages and temp-table name, so a v27 failure reports itself
/// as v27 rather than borrowing v23's name.
fn widen_check_constraint_labelled(
    conn: &Connection,
    label: &str,
    table: &str,
    from: &str,
    to: &str,
) -> Result<(), LedgerError> {
    let ddl: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get(0),
    )?;

    // Already widened — a re-run of a partially applied migration, or a
    // database built fresh from a future baseline. Nothing to do. A LATER
    // migration may have widened the same constraint further (v27 widens
    // v23's direction list again), in which case neither `from` nor `to`
    // appears verbatim but every value `to` admits is still admitted;
    // `widened_by` names a token that proves that.
    if ddl.contains(to) || (!ddl.contains(from) && already_widened(&ddl, to)) {
        return Ok(());
    }
    let occurrences = ddl.matches(from).count();
    if occurrences != 1 {
        return Err(LedgerError::SchemaMigrationFailed(format!(
            "{label} expected exactly one occurrence of {from:?} in {table}'s DDL, found \
             {occurrences} — this database's schema is not the one this migration was written \
             against"
        )));
    }

    let temp = format!("{table}_{label}_rebuild");
    let new_ddl = ddl
        .replacen(from, to, 1)
        // Only the table NAME is renamed, and only its first occurrence:
        // `CREATE TABLE <name> (`. A later mention of the same identifier
        // inside a column name or a REFERENCES clause must survive.
        .replacen(table, &temp, 1);
    if !new_ddl.contains(&temp) {
        return Err(LedgerError::SchemaMigrationFailed(format!(
            "{label} could not rename {table} in its own DDL"
        )));
    }

    // Everything attached to this table, captured from the authoritative
    // source before anything is dropped.
    let attached: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT sql FROM sqlite_master
             WHERE tbl_name = ?1 AND type IN ('index','trigger') AND sql IS NOT NULL",
        )?;
        let rows: Result<Vec<String>, _> = stmt.query_map([table], |r| r.get(0))?.collect();
        rows?
    };

    // The column list, in declared order, from the real table.
    let columns: Vec<String> = {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows: Result<Vec<String>, _> = stmt.query_map([], |r| r.get(1))?.collect();
        rows?
    };
    if columns.is_empty() {
        return Err(LedgerError::SchemaMigrationFailed(format!(
            "{label} found no columns on {table}"
        )));
    }
    let quoted: Vec<String> = columns.iter().map(|c| format!("\"{c}\"")).collect();
    let column_list = quoted.join(", ");

    conn.execute_batch(&new_ddl)?;
    conn.execute_batch(&format!(
        "INSERT INTO \"{temp}\" ({column_list}) SELECT {column_list} FROM \"{table}\";"
    ))?;

    // Belt and braces: the copy must have moved every row. A short count
    // is not something to discover later from a reconciliation alarm.
    let (before, after): (i64, i64) = conn.query_row(
        &format!("SELECT (SELECT COUNT(*) FROM \"{table}\"), (SELECT COUNT(*) FROM \"{temp}\")"),
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if before != after {
        return Err(LedgerError::SchemaMigrationFailed(format!(
            "{label} copied {after} of {before} {table} rows"
        )));
    }

    conn.execute_batch(&format!(
        "DROP TABLE \"{table}\"; ALTER TABLE \"{temp}\" RENAME TO \"{table}\";"
    ))?;
    for index_or_trigger in attached {
        conn.execute_batch(&format!("{index_or_trigger};"))?;
    }
    Ok(())
}

/// v26 — the fourth outbound Robinhood operation: a TREASURY WITHDRAWAL.
///
/// # What changes
///
/// `robinhood_transactions` gains `kind = 'TreasuryWithdraw'` (contract
/// action `0x0C`). Every other outbound operation settles a
/// `bridge_requests` row on one of the two executable routes; a
/// withdrawal settles a `rebalance_requests` row and belongs to no route.
/// So:
///
/// - `request_id` becomes nullable and `rebalance_request_id` is added,
///   with a CHECK that exactly one of the two is set and that which one
///   is decided by `kind`;
/// - `route` becomes nullable, NULL exactly for a withdrawal;
/// - the kind/action, kind/obligation and kind/route CHECKs are restated
///   to admit the new kind without loosening anything for the old three;
/// - `ux_robinhood_tx_rebalance` makes ONE withdrawal operation per
///   rebalance request a database guarantee, the way
///   `ux_robinhood_tx_operation` already does per bridge request.
///
/// And `rebalance_requests.direction` admits `'RobinhoodReserve'`, which
/// v23 deliberately left out because nothing could execute such a
/// request. `GlcRobinhoodBridge.executeTreasuryWithdraw` now can.
///
/// # Why a rebuild
///
/// SQLite cannot alter a CHECK constraint or drop NOT NULL in place. The
/// table is copied into its new shape column for column, the old one is
/// dropped, and the new one takes its name — the v23 recipe, with the
/// same foreign-key handling and the same integrity check before commit.
///
/// The nonce column, the signed bytes, the receipts, the authorization
/// signatures table and every index are carried over unchanged: an
/// in-flight payout survives this migration with its nonce and its raw
/// transaction intact.
fn apply_v26(conn: &Connection) -> Result<(), LedgerError> {
    // Structural idempotence: the real shape decides.
    if !table_exists(conn, "robinhood_transactions")? {
        return Err(LedgerError::SchemaMigrationFailed(
            "v26 expected robinhood_transactions to exist (v23 creates it)".to_string(),
        ));
    }
    if column_exists(conn, "robinhood_transactions", "rebalance_request_id")? {
        // Already rebuilt. The rebalance_requests widening below is
        // idempotent on its own.
        return widen_check_constraint(
            conn,
            "rebalance_requests",
            "direction IN ('GoldcoinReserve','SolanaReserve')",
            "direction IN ('GoldcoinReserve','SolanaReserve','RobinhoodReserve')",
        );
    }

    let foreign_keys_were_on: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = apply_v26_inner(conn);
    if foreign_keys_were_on {
        conn.pragma_update(None, "foreign_keys", "ON")?;
    }
    result
}

fn apply_v26_inner(conn: &Connection) -> Result<(), LedgerError> {
    widen_check_constraint(
        conn,
        "rebalance_requests",
        "direction IN ('GoldcoinReserve','SolanaReserve')",
        "direction IN ('GoldcoinReserve','SolanaReserve','RobinhoodReserve')",
    )?;

    conn.execute_batch(
        r#"
        CREATE TABLE robinhood_transactions_v26 (
            id                   INTEGER PRIMARY KEY,

            -- ---- what this operation is ----
            -- 'Payout'           GlcToRhn: pay reserve GLC to a Robinhood recipient.
            -- 'Settlement'       RhnToGlc: mark an obligation settled AFTER its
            --                    Goldcoin payout confirmed.
            -- 'Refund'           RhnToGlc: return an obligation's exact principal.
            -- 'TreasuryWithdraw' no route: move reserve GLC to the contract's
            --                    immutable TREASURY, settling a rebalance request.
            kind                 TEXT NOT NULL
                                 CHECK (kind IN ('Payout','Settlement','Refund','TreasuryWithdraw')),
            -- Exactly one of these two is set, and the kind says which.
            request_id           INTEGER REFERENCES bridge_requests(id),
            rebalance_request_id INTEGER REFERENCES rebalance_requests(id),
            -- Only the two executable routes; NULL exactly for a withdrawal.
            route                TEXT CHECK (route IS NULL OR route IN ('GlcToRhn','RhnToGlc')),

            -- ---- the contract-side identity this operation was authorized against ----
            bridge_contract      BLOB NOT NULL CHECK (length(bridge_contract) = 20),
            chain_id             INTEGER NOT NULL CHECK (chain_id > 0),
            -- 1 payout, 2 refund, 3 settle, 12 treasury withdraw.
            action               INTEGER NOT NULL CHECK (action IN (1,2,3,12)),
            contract_request_id  BLOB NOT NULL CHECK (length(contract_request_id) = 32),
            obligation_index     INTEGER CHECK (obligation_index IS NULL
                                                OR obligation_index >= 0),
            -- Payout, refund and withdrawal name a recipient and an amount;
            -- a settlement moves nothing and names neither. For a
            -- withdrawal the recipient IS the contract's TREASURY.
            recipient            BLOB CHECK (recipient IS NULL OR length(recipient) = 20),
            amount_robinhood     BLOB CHECK (amount_robinhood IS NULL
                                             OR length(amount_robinhood) = 32),
            signer_epoch         INTEGER NOT NULL CHECK (signer_epoch >= 0),
            expiry               INTEGER NOT NULL CHECK (expiry > 0),
            auth_digest          BLOB NOT NULL CHECK (length(auth_digest) = 32),

            -- ---- the submitter and its nonce ----
            submitter            BLOB CHECK (submitter IS NULL OR length(submitter) = 20),
            nonce                INTEGER CHECK (nonce IS NULL OR nonce >= 0),

            -- ---- the signed transaction, written BEFORE the first broadcast ----
            envelope             TEXT CHECK (envelope IS NULL
                                             OR envelope IN ('legacy','eip1559')),
            gas_limit            INTEGER CHECK (gas_limit IS NULL OR gas_limit > 0),
            fee_summary          TEXT,
            raw_tx               BLOB,
            tx_hash              BLOB CHECK (tx_hash IS NULL OR length(tx_hash) = 32),

            -- ---- lifecycle ----
            state                TEXT NOT NULL CHECK (state IN (
                                     'Authorizing','Authorized','Signed','Broadcast',
                                     'Included','Finalized','Reverted','ManualReview')),
            first_broadcast_at   INTEGER,
            last_broadcast_at    INTEGER,
            broadcast_attempts   INTEGER NOT NULL DEFAULT 0
                                 CHECK (broadcast_attempts >= 0),
            replacement_attempts INTEGER NOT NULL DEFAULT 0
                                 CHECK (replacement_attempts >= 0),
            receipt_status       INTEGER CHECK (receipt_status IS NULL
                                                OR receipt_status IN (0,1)),
            receipt_block_number INTEGER CHECK (receipt_block_number IS NULL
                                                OR receipt_block_number >= 0),
            receipt_block_hash   BLOB CHECK (receipt_block_hash IS NULL
                                             OR length(receipt_block_hash) = 32),
            confirmations        INTEGER NOT NULL DEFAULT 0 CHECK (confirmations >= 0),
            finalized_at         INTEGER,
            failure_reason       TEXT,
            created_at           INTEGER NOT NULL,
            updated_at           INTEGER NOT NULL,

            -- ---- table constraints ----
            -- A withdrawal settles a rebalance request and nothing else;
            -- every other kind settles a bridge request and nothing else.
            CHECK ((kind = 'TreasuryWithdraw') = (request_id IS NULL)),
            CHECK ((kind = 'TreasuryWithdraw') = (rebalance_request_id IS NOT NULL)),
            CHECK ((kind = 'TreasuryWithdraw') = (route IS NULL)),
            -- A settlement or refund names an obligation; a payout and a
            -- withdrawal must not.
            CHECK ((kind IN ('Payout','TreasuryWithdraw')) = (obligation_index IS NULL)),
            CHECK ((kind = 'Settlement') = (recipient IS NULL)),
            CHECK ((recipient IS NULL) = (amount_robinhood IS NULL)),
            -- The action byte and the kind must agree.
            CHECK ((kind = 'Payout')           = (action = 1)),
            CHECK ((kind = 'Refund')           = (action = 2)),
            CHECK ((kind = 'Settlement')       = (action = 3)),
            CHECK ((kind = 'TreasuryWithdraw') = (action = 12)),
            -- A payout is the outbound route; the obligation-closing
            -- operations are the inbound one; a withdrawal has none.
            CHECK (route IS NULL OR ((kind = 'Payout') = (route = 'GlcToRhn'))),
            CHECK ((submitter IS NULL) = (nonce IS NULL)),
            CHECK ((raw_tx IS NULL) = (tx_hash IS NULL)),
            CHECK ((raw_tx IS NULL) = (envelope IS NULL)),
            CHECK (state NOT IN ('Signed','Broadcast','Included','Finalized','Reverted')
                   OR (raw_tx IS NOT NULL AND nonce IS NOT NULL)),
            CHECK (state <> 'Finalized' OR receipt_status = 1),
            CHECK ((finalized_at IS NULL) = (state <> 'Finalized')),
            CHECK ((first_broadcast_at IS NULL) = (broadcast_attempts = 0))
        );

        INSERT INTO robinhood_transactions_v26
            (id, kind, request_id, rebalance_request_id, route, bridge_contract, chain_id,
             action, contract_request_id, obligation_index, recipient, amount_robinhood,
             signer_epoch, expiry, auth_digest, submitter, nonce, envelope, gas_limit,
             fee_summary, raw_tx, tx_hash, state, first_broadcast_at, last_broadcast_at,
             broadcast_attempts, replacement_attempts, receipt_status, receipt_block_number,
             receipt_block_hash, confirmations, finalized_at, failure_reason, created_at,
             updated_at)
        SELECT
             id, kind, request_id, NULL, route, bridge_contract, chain_id,
             action, contract_request_id, obligation_index, recipient, amount_robinhood,
             signer_epoch, expiry, auth_digest, submitter, nonce, envelope, gas_limit,
             fee_summary, raw_tx, tx_hash, state, first_broadcast_at, last_broadcast_at,
             broadcast_attempts, replacement_attempts, receipt_status, receipt_block_number,
             receipt_block_hash, confirmations, finalized_at, failure_reason, created_at,
             updated_at
        FROM robinhood_transactions;

        DROP TABLE robinhood_transactions;
        ALTER TABLE robinhood_transactions_v26 RENAME TO robinhood_transactions;

        -- The indexes, exactly as v23 declared them, plus the one the new
        -- kind needs.
        CREATE UNIQUE INDEX ux_robinhood_tx_operation
            ON robinhood_transactions (kind, request_id);
        CREATE UNIQUE INDEX ux_robinhood_tx_contract_request
            ON robinhood_transactions (chain_id, bridge_contract, action, contract_request_id);
        CREATE UNIQUE INDEX ux_robinhood_tx_nonce
            ON robinhood_transactions (submitter, chain_id, nonce)
            WHERE nonce IS NOT NULL;
        CREATE INDEX ix_robinhood_tx_state ON robinhood_transactions (state, id);
        -- ONE withdrawal operation per rebalance request, ever: a second
        -- attempt cannot be inserted, so "no duplicate withdrawal" is a
        -- constraint violation rather than a second transfer.
        CREATE UNIQUE INDEX ux_robinhood_tx_rebalance
            ON robinhood_transactions (rebalance_request_id)
            WHERE rebalance_request_id IS NOT NULL;
        "#,
    )?;

    let fk_violations: i64 =
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    if fk_violations != 0 {
        return Err(LedgerError::SchemaMigrationFailed(format!(
            "v26 rebuild left {fk_violations} foreign-key violations; rolled back"
        )));
    }
    let integrity: String =
        conn.query_row("SELECT * FROM pragma_integrity_check LIMIT 1", [], |r| {
            r.get(0)
        })?;
    if integrity != "ok" {
        return Err(LedgerError::SchemaMigrationFailed(format!(
            "v26 rebuild failed integrity_check: {integrity}; rolled back"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection at exactly schema v8 -- pre-dating the deposit-address
    /// columns -- to prove the v8 -> v9 upgrade path specifically (not
    /// just a fresh install, which every other test in this crate already
    /// exercises implicitly via `Ledger::open`/`open_in_memory`).
    fn conn_at_v8() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        apply_v1(&conn).unwrap();
        apply_v2(&conn).unwrap();
        apply_v3(&conn).unwrap();
        apply_v4(&conn).unwrap();
        apply_v5(&conn).unwrap();
        apply_v6(&conn).unwrap();
        apply_v7(&conn).unwrap();
        apply_v8(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version (version) VALUES (8);",
        )
        .unwrap();
        conn
    }

    /// Seeds one historic `GlcToSol` row. Deliberately shape-aware: the
    /// same helper is used to plant a row in a PRE-v21 database (which has
    /// no identity columns at all) before a migration runs, and in a
    /// CURRENT one (where `source_chain` is `NOT NULL` with no default)
    /// after it.
    fn insert_minimal_request(conn: &Connection, id: i64) {
        if column_exists(conn, "bridge_requests", "source_chain").unwrap() {
            conn.execute(
                "INSERT INTO bridge_requests
                    (id, direction, state, gross_amount_atomic, recipient, created_at,
                     source_chain)
                 VALUES (?1, 'GlcToSol', 'AwaitingDeposit', 12345, X'ab', 1000, 'goldcoin')",
                [id],
            )
            .unwrap();
        } else {
            conn.execute(
                "INSERT INTO bridge_requests
                    (id, direction, state, gross_amount_atomic, recipient, created_at)
                 VALUES (?1, 'GlcToSol', 'AwaitingDeposit', 12345, X'ab', 1000)",
                [id],
            )
            .unwrap();
        }
    }

    #[test]
    fn fresh_database_reaches_v9_with_deposit_address_columns_present_and_null() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert_eq!(CURRENT_SCHEMA_VERSION, 29);

        insert_minimal_request(&conn, 1);
        let (addr, script, redeem): (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT deposit_address, deposit_script_pubkey_hex, deposit_redeem_script_hex
                 FROM bridge_requests WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(addr.is_none() && script.is_none() && redeem.is_none());
    }

    #[test]
    fn upgrading_from_v8_adds_deposit_address_columns_without_losing_existing_data() {
        let conn = conn_at_v8();
        // Real pre-existing data, inserted BEFORE the v9 migration runs,
        // to prove the ALTER TABLE ADD COLUMN steps never touch it.
        insert_minimal_request(&conn, 1);

        open_and_migrate(&conn).unwrap(); // sees version=8, applies v9..v14

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let (gross, recipient, deposit_address): (i64, Vec<u8>, Option<String>) = conn
            .query_row(
                "SELECT gross_amount_atomic, recipient, deposit_address FROM bridge_requests WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            gross, 12345,
            "pre-existing data must survive the migration untouched"
        );
        assert_eq!(recipient, vec![0xab]);
        assert!(
            deposit_address.is_none(),
            "new column defaults to NULL on existing rows"
        );
    }

    #[test]
    fn upgrading_from_v8_is_idempotent_if_run_twice() {
        let conn = conn_at_v8();
        insert_minimal_request(&conn, 1);
        open_and_migrate(&conn).unwrap();
        open_and_migrate(&conn).unwrap(); // must not error re-adding columns/index
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
    }

    /// Regression for a real production incident: a database already
    /// carried `deposit_address`/`deposit_script_pubkey_hex`/
    /// `deposit_redeem_script_hex` (from an earlier successful rollout of
    /// this exact migration) while its recorded `schema_version` still
    /// read 8 — so `open_and_migrate` decided v9 had not run yet and
    /// re-attempted `ALTER TABLE ... ADD COLUMN`, which failed outright
    /// with `duplicate column name`, and the daemon refused to start. This
    /// builds exactly that mismatched state directly (columns present,
    /// version stuck at 8) rather than relying on `open_and_migrate`
    /// itself to have created it, since the whole point is that some
    /// earlier, different path put the database in this state.
    #[test]
    fn opens_successfully_when_deposit_address_columns_already_exist_but_schema_version_still_reads_8(
    ) {
        let conn = conn_at_v8();
        insert_minimal_request(&conn, 1);
        // Simulates the columns already having been added by an earlier
        // successful run of this migration, WITHOUT going through
        // `open_and_migrate` again here — `schema_version` is deliberately
        // left at 8, reproducing the exact desync production hit.
        conn.execute_batch(
            "ALTER TABLE bridge_requests ADD COLUMN deposit_address TEXT;
             ALTER TABLE bridge_requests ADD COLUMN deposit_script_pubkey_hex TEXT;
             ALTER TABLE bridge_requests ADD COLUMN deposit_redeem_script_hex TEXT;
             CREATE UNIQUE INDEX ux_bridge_requests_deposit_script
                 ON bridge_requests(deposit_script_pubkey_hex)
                 WHERE deposit_script_pubkey_hex IS NOT NULL;
             UPDATE bridge_requests SET deposit_address = 'preexisting' WHERE id = 1;",
        )
        .unwrap();

        open_and_migrate(&conn)
            .expect("must open successfully even though the deposit-address columns already exist");

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        // The pre-existing column value must survive untouched — this is
        // not a recreate-the-column fix, just a skip-if-present one.
        let deposit_address: Option<String> = conn
            .query_row(
                "SELECT deposit_address FROM bridge_requests WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(deposit_address.as_deref(), Some("preexisting"));

        // A second open (the daemon restarting again) must still be a
        // clean no-op.
        open_and_migrate(&conn).unwrap();
    }

    #[test]
    fn deposit_script_pubkey_unique_index_rejects_a_duplicate_assignment() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        insert_minimal_request(&conn, 1);
        insert_minimal_request(&conn, 2);

        conn.execute(
            "UPDATE bridge_requests SET deposit_script_pubkey_hex = 'abc' WHERE id = 1",
            [],
        )
        .unwrap();
        let err = conn
            .execute(
                "UPDATE bridge_requests SET deposit_script_pubkey_hex = 'abc' WHERE id = 2",
                [],
            )
            .unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("unique"),
            "expected a UNIQUE constraint violation from ux_bridge_requests_deposit_script, got: {msg}"
        );
    }

    #[test]
    fn deposit_script_pubkey_null_is_never_constrained_by_the_unique_index() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        // Both left NULL (no deposit address assigned) -- must not collide,
        // since the index is a PARTIAL index (`WHERE ... IS NOT NULL`).
        insert_minimal_request(&conn, 1);
        insert_minimal_request(&conn, 2);
    }

    fn conn_at_v9() -> Connection {
        let conn = conn_at_v8();
        apply_v9(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 9", [])
            .unwrap();
        conn
    }

    #[test]
    fn upgrading_from_v9_creates_the_vault_utxo_splits_table() {
        let conn = conn_at_v9();
        open_and_migrate(&conn).unwrap(); // sees version=9, applies v10..v14

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        conn.execute(
            "INSERT INTO vault_utxo_splits
                (source_txid, source_vout, source_amount_atomic, chunk_count,
                 chunk_target_atomic, fee_atomic, unsigned_tx_hex, state, note, built_at)
             VALUES (X'ab', 0, 1000, 2, 500, 10, 'deadbeef', 'Built', 'test', 1000)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn vault_utxo_splits_source_outpoint_is_unique() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO vault_utxo_splits
                (source_txid, source_vout, source_amount_atomic, chunk_count,
                 chunk_target_atomic, fee_atomic, unsigned_tx_hex, state, note, built_at)
             VALUES (X'ab', 0, 1000, 2, 500, 10, 'deadbeef', 'Built', 'test', 1000)",
            [],
        )
        .unwrap();
        let err = conn
            .execute(
                "INSERT INTO vault_utxo_splits
                    (source_txid, source_vout, source_amount_atomic, chunk_count,
                     chunk_target_atomic, fee_atomic, unsigned_tx_hex, state, note, built_at)
                 VALUES (X'ab', 0, 1000, 2, 500, 10, 'deadbeef', 'Built', 'test again', 2000)",
                [],
            )
            .unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("unique"),
            "expected a UNIQUE constraint violation from ux_vault_utxo_splits_source, got: {msg}"
        );
    }

    #[test]
    fn applying_v10_twice_is_a_safe_no_op() {
        let conn = conn_at_v9();
        apply_v10(&conn).unwrap();
        apply_v10(&conn).unwrap(); // must not error re-creating the table/index
    }

    fn conn_at_v10() -> Connection {
        let conn = conn_at_v9();
        apply_v10(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 10", [])
            .unwrap();
        conn
    }

    #[test]
    fn upgrading_from_v10_adds_admission_columns_defaulting_open() {
        let conn = conn_at_v10();
        // Real pre-existing reserve_ledger data, inserted BEFORE v11 runs,
        // to prove the ALTER TABLE ADD COLUMN steps never touch it.
        conn.execute(
            "INSERT INTO reserve_ledger
                (direction, total_reserve_balance, balance_refreshed_at, protected_minimum,
                 target_reserve, warning_reserve, critical_reserve, paused)
             VALUES ('GoldcoinReserve', 100, 0, 0, 100, 50, 10, 1)",
            [],
        )
        .unwrap();

        open_and_migrate(&conn).unwrap(); // sees version=10, applies v11..v14

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let (paused, admission_closed, admission_reason): (i64, i64, Option<String>) = conn
            .query_row(
                "SELECT paused, admission_closed, admission_reason FROM reserve_ledger
                 WHERE direction = 'GoldcoinReserve'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            paused, 1,
            "pre-existing paused value must survive the migration untouched"
        );
        assert_eq!(
            admission_closed, 0,
            "the new admission column defaults to open (0) on existing rows, \
             independent of the pre-existing paused value"
        );
        assert!(admission_reason.is_none());
    }

    #[test]
    fn applying_v11_twice_is_a_safe_no_op() {
        let conn = conn_at_v10();
        apply_v11(&conn).unwrap();
        apply_v11(&conn).unwrap(); // must not error re-adding the columns
    }

    fn conn_at_v11() -> Connection {
        let conn = conn_at_v10();
        apply_v11(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 11", [])
            .unwrap();
        conn
    }

    #[test]
    fn upgrading_from_v11_adds_change_fanout_table_and_utxo_pool_columns_without_losing_existing_data(
    ) {
        let conn = conn_at_v11();
        // Real pre-existing reserve_ledger data, inserted BEFORE v12 runs,
        // to prove the ALTER TABLE ADD COLUMN steps never touch it — same
        // discipline as `upgrading_from_v10_adds_admission_columns_defaulting_open`.
        conn.execute(
            "INSERT INTO reserve_ledger
                (direction, total_reserve_balance, balance_refreshed_at, protected_minimum,
                 target_reserve, warning_reserve, critical_reserve, paused)
             VALUES ('GoldcoinReserve', 100, 0, 0, 100, 50, 10, 1)",
            [],
        )
        .unwrap();
        // A real payout record, inserted BEFORE v12 runs, whose
        // `change_atomic` must remain exactly as persisted — nothing about
        // the pre-existing single-change-amount column is touched by this
        // purely additive migration.
        conn.execute(
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, created_at)
             VALUES (1, 'SolToGlc', 'SettlementAuthorized', 100, X'AA', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at)
             VALUES (1, X'AB', 90, 9, 1, X'CD', 'Signed', 0)",
            [],
        )
        .unwrap();

        open_and_migrate(&conn).unwrap(); // sees version=11, applies v12..v14

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let (paused, min_available, warning): (i64, i64, i64) = conn
            .query_row(
                "SELECT paused, utxo_pool_min_available_count, utxo_pool_warning_count
                 FROM reserve_ledger WHERE direction = 'GoldcoinReserve'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            paused, 1,
            "pre-existing paused value must survive the migration untouched"
        );
        assert_eq!(
            min_available, 0,
            "the new UTXO-pool columns default to 0 (backpressure disabled) on existing rows"
        );
        assert_eq!(warning, 0);

        let change_atomic: i64 = conn
            .query_row(
                "SELECT change_atomic FROM goldcoin_payouts WHERE request_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            change_atomic, 9,
            "a payout built before fan-out existed keeps its single change_atomic value untouched"
        );
        let change_output_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM goldcoin_payout_change_outputs WHERE request_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            change_output_rows, 0,
            "never backfilled — a legacy payout simply has no itemized breakdown rows"
        );

        // A second open (the daemon restarting again) must still be a
        // clean no-op.
        open_and_migrate(&conn).unwrap();
    }

    #[test]
    fn applying_v12_twice_is_a_safe_no_op() {
        let conn = conn_at_v11();
        apply_v12(&conn).unwrap();
        apply_v12(&conn).unwrap(); // must not error re-adding the table/columns
    }

    fn conn_at_v12() -> Connection {
        let conn = conn_at_v11();
        apply_v12(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 12", [])
            .unwrap();
        conn
    }

    #[test]
    fn upgrading_from_v12_adds_the_recipient_window_index_without_losing_existing_data() {
        let conn = conn_at_v12();
        // Real pre-existing data, inserted BEFORE v13 runs, to prove a
        // purely-additive index creation never touches it.
        conn.execute(
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, created_at)
             VALUES (1, 'SolToGlc', 'SourceFinalized', 100, X'AA', 1000)",
            [],
        )
        .unwrap();

        open_and_migrate(&conn).unwrap(); // sees version=12, applies v13..v14

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let gross: i64 = conn
            .query_row(
                "SELECT gross_amount_atomic FROM bridge_requests WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            gross, 100,
            "pre-existing data must survive an index-only migration untouched"
        );

        let index_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'ix_bridge_requests_recipient_window'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index_exists, 1);

        // A second open (the daemon restarting again) must still be a
        // clean no-op.
        open_and_migrate(&conn).unwrap();
    }

    #[test]
    fn applying_v13_twice_is_a_safe_no_op() {
        let conn = conn_at_v12();
        apply_v13(&conn).unwrap();
        apply_v13(&conn).unwrap(); // must not error re-creating the index
    }

    #[test]
    fn v15_is_idempotent_and_admin_audit_log_enforces_its_checks() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        apply_v15(&conn).unwrap(); // must not error re-creating table/indexes

        // A well-formed row inserts.
        conn.execute(
            "INSERT INTO admin_audit_log (at, actor, action, target, old_value, new_value, note, outcome, error)
             VALUES (1, 'alice', 'pause', 'goldcoin', 'false', 'true', 'incident 42', 'success', NULL)",
            [],
        )
        .unwrap();

        // Empty note, empty actor, and an outcome outside the enum all
        // fail closed at the schema level.
        for bad in [
            "INSERT INTO admin_audit_log (at, actor, action, note, outcome)
             VALUES (1, 'alice', 'pause', '', 'success')",
            "INSERT INTO admin_audit_log (at, actor, action, note, outcome)
             VALUES (1, '', 'pause', 'note', 'success')",
            "INSERT INTO admin_audit_log (at, actor, action, note, outcome)
             VALUES (1, 'alice', 'pause', 'note', 'partial')",
        ] {
            assert!(conn.execute(bad, []).is_err(), "must reject: {bad}");
        }
    }

    #[test]
    fn a_database_newer_than_this_binary_is_refused_not_silently_downgraded() {
        // The rollback scenario: a database written by a FUTURE binary
        // (or, symmetrically, today's v18 database opened by yesterday's
        // v17 binary — same code, same guard). It must refuse, and must
        // NOT rewrite the version marker.
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            [CURRENT_SCHEMA_VERSION + 1],
        )
        .unwrap();

        let err = open_and_migrate(&conn).unwrap_err();
        assert!(
            matches!(err, LedgerError::SchemaTooNew { found, supported }
                if found == CURRENT_SCHEMA_VERSION + 1 && supported == CURRENT_SCHEMA_VERSION),
            "got: {err}"
        );
        // The marker is untouched — no silent downgrade.
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION + 1);
    }

    #[test]
    fn upgrading_to_v18_adds_the_admission_buffer_columns_disabled_and_open() {
        let conn = conn_at_v12();
        // Real pre-existing reserve_ledger data, inserted BEFORE v18 runs
        // — same discipline as the v11/v12 migration tests: purely
        // additive ALTER TABLE ADD COLUMN steps must not touch it.
        conn.execute(
            "INSERT INTO reserve_ledger
                (direction, total_reserve_balance, balance_refreshed_at, protected_minimum,
                 target_reserve, warning_reserve, critical_reserve, paused, admission_closed)
             VALUES ('GoldcoinReserve', 100, 0, 0, 100, 50, 10, 1, 1)",
            [],
        )
        .unwrap();

        open_and_migrate(&conn).unwrap(); // sees version=12, applies v13..v18

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let (paused, admission_closed, buffer, reopen, liquidity_closed, closed_at): (
            i64,
            i64,
            i64,
            i64,
            i64,
            Option<i64>,
        ) = conn
            .query_row(
                "SELECT paused, admission_closed, admission_buffer_atomic,
                        admission_reopen_atomic, liquidity_admission_closed,
                        liquidity_admission_closed_at
                 FROM reserve_ledger WHERE direction = 'GoldcoinReserve'",
                [],
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
            .unwrap();
        assert_eq!(paused, 1, "pre-existing paused must survive untouched");
        assert_eq!(
            admission_closed, 1,
            "the pre-existing OPERATOR admission flag must survive untouched — the new \
             automatic gate is a separate column and never rewrites it"
        );
        assert_eq!(
            (buffer, reopen),
            (0, 0),
            "the buffer defaults to disabled on every existing row: a binary upgrade must \
             never silently start applying admission backpressure a deployment did not \
             configure"
        );
        assert_eq!(liquidity_closed, 0, "the automatic gate defaults to open");
        assert!(closed_at.is_none());
    }

    #[test]
    fn upgrading_from_v18_adds_the_goldcoin_refund_tables_without_losing_data() {
        // A database at v18 exactly as production has it (the full ladder
        // minus v19), with a real pre-existing request row — so the
        // migration is exercised as a genuine upgrade, not a fresh create.
        let conn = conn_at_v8();
        apply_v9(&conn).unwrap();
        apply_v10(&conn).unwrap();
        apply_v11(&conn).unwrap();
        apply_v12(&conn).unwrap();
        apply_v13(&conn).unwrap();
        apply_v14(&conn).unwrap();
        apply_v15(&conn).unwrap();
        apply_v16(&conn).unwrap();
        apply_v17(&conn).unwrap();
        apply_v18(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 18", [])
            .unwrap();
        insert_minimal_request(&conn, 7);

        open_and_migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        // `open_and_migrate` always runs the ladder to the head, so a v18
        // database lands on the CURRENT version, not merely on 19.
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        // The pre-existing row survived.
        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bridge_requests WHERE id = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1);

        // Both new tables exist and start empty.
        for table in ["goldcoin_refunds", "goldcoin_refund_inputs"] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table} must exist and be empty after the upgrade");
        }
    }

    #[test]
    fn upgrading_from_v19_adds_the_durable_amount_witness_without_losing_data() {
        let conn = conn_at_v8();
        apply_v9(&conn).unwrap();
        apply_v10(&conn).unwrap();
        apply_v11(&conn).unwrap();
        apply_v12(&conn).unwrap();
        apply_v13(&conn).unwrap();
        apply_v14(&conn).unwrap();
        apply_v15(&conn).unwrap();
        apply_v16(&conn).unwrap();
        apply_v17(&conn).unwrap();
        apply_v18(&conn).unwrap();
        apply_v19(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 19", [])
            .unwrap();
        insert_minimal_request(&conn, 11);

        open_and_migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        // The historic row survives, and its witness is NULL — never
        // backfilled, because no honest value exists for it.
        let (kept, witness): (i64, Option<i64>) = conn
            .query_row(
                "SELECT COUNT(*), MAX(observed_amount_atomic) FROM bridge_requests WHERE id = 11",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kept, 1);
        assert_eq!(
            witness, None,
            "historic rows must not be backfilled — NULL selects the explicit legacy mode"
        );
    }

    #[test]
    fn the_durable_amount_witness_must_be_positive_when_present() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        insert_minimal_request(&conn, 3);

        // NULL is allowed (legacy rows).
        conn.execute(
            "UPDATE bridge_requests SET observed_amount_atomic = NULL WHERE id = 3",
            [],
        )
        .unwrap();
        // A positive value is allowed.
        conn.execute(
            "UPDATE bridge_requests SET observed_amount_atomic = 1 WHERE id = 3",
            [],
        )
        .unwrap();
        // Zero and negative are not.
        for bad in [0i64, -1] {
            assert!(
                conn.execute(
                    "UPDATE bridge_requests SET observed_amount_atomic = ?1 WHERE id = 3",
                    [bad],
                )
                .is_err(),
                "{bad} must be refused by the CHECK"
            );
        }
    }

    #[test]
    fn applying_v20_twice_is_a_safe_no_op() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        apply_v20(&conn).unwrap(); // must not error re-adding the column
        apply_v20(&conn).unwrap();
    }

    #[test]
    fn applying_v19_twice_is_a_safe_no_op() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        apply_v19(&conn).unwrap(); // must not error re-creating the tables
        apply_v19(&conn).unwrap();
    }

    #[test]
    fn applying_v18_twice_is_a_safe_no_op() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        apply_v18(&conn).unwrap(); // must not error re-adding the columns
        apply_v18(&conn).unwrap();
    }

    #[test]
    fn v17_is_idempotent_and_solana_refunds_enforces_its_constraints() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        apply_v17(&conn).unwrap(); // must not error re-creating table/index

        insert_minimal_request(&conn, 1);
        conn.execute(
            "INSERT INTO solana_refunds
                (request_id, obligation_index, nonce, amount_solana_atomic, requester,
                 destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (1, 7, 9223372036854775807, 500000, X'aa', X'bb', X'cc', X'dd',
                     'admission_closed_at_fold', 'refund 1', 'cli:test', 'Pending', 1000)",
            [],
        )
        .unwrap();

        // One refund lifecycle per request / per obligation / per nonce,
        // structurally.
        insert_minimal_request(&conn, 2);
        for bad in [
            // duplicate request_id
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (1, 8, 100, 1, X'aa', X'bb', X'cc', X'dd', 'r', 'n', 'a', 'Pending', 1)",
            // duplicate obligation_index
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (2, 7, 100, 1, X'aa', X'bb', X'cc', X'dd', 'r', 'n', 'a', 'Pending', 1)",
            // duplicate nonce
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (2, 8, 9223372036854775807, 1, X'aa', X'bb', X'cc', X'dd', 'r', 'n', 'a', 'Pending', 1)",
            // Broadcast without a signature/blockhash
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at, broadcast_at)
             VALUES (2, 8, 100, 1, X'aa', X'bb', X'cc', X'dd', 'r', 'n', 'a', 'Broadcast', 1, 1)",
            // Confirmed without confirmed_at
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at, refund_signature,
                 recent_blockhash, broadcast_at)
             VALUES (2, 8, 100, 1, X'aa', X'bb', X'cc', X'dd', 'r', 'n', 'a', 'Confirmed', 1, 's', 'h', 1)",
            // empty note
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (2, 8, 100, 1, X'aa', X'bb', X'cc', X'dd', 'r', '', 'a', 'Pending', 1)",
            // zero amount
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (2, 8, 100, 0, X'aa', X'bb', X'cc', X'dd', 'r', 'n', 'a', 'Pending', 1)",
        ] {
            assert!(conn.execute(bad, []).is_err(), "must reject: {bad}");
        }
    }

    #[test]
    fn upgrading_from_v16_adds_solana_refunds_without_touching_existing_data() {
        // A database at v16 exactly as production would have it (the full
        // ladder minus v17), with a real pre-existing request row.
        let conn = conn_at_v8();
        apply_v9(&conn).unwrap();
        apply_v10(&conn).unwrap();
        apply_v11(&conn).unwrap();
        apply_v12(&conn).unwrap();
        apply_v13(&conn).unwrap();
        apply_v14(&conn).unwrap();
        apply_v15(&conn).unwrap();
        apply_v16(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 16", [])
            .unwrap();
        insert_minimal_request(&conn, 41);

        open_and_migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        // Pre-existing data untouched; the new table exists and is empty.
        let amount: i64 = conn
            .query_row(
                "SELECT gross_amount_atomic FROM bridge_requests WHERE id = 41",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(amount, 12345);
        let refunds: i64 = conn
            .query_row("SELECT COUNT(*) FROM solana_refunds", [], |r| r.get(0))
            .unwrap();
        assert_eq!(refunds, 0);
    }

    // ------------------------------------------------------------------
    // v21 — chain- and contract-qualified obligation identity
    // ------------------------------------------------------------------

    /// What every HISTORIC Solana obligation row is backfilled with: the
    /// explicit legacy marker, never this binary's program id.
    fn legacy_contract() -> Vec<u8> {
        crate::ledger::LEGACY_SOLANA_SOURCE_CONTRACT.to_vec()
    }

    /// What every row written from v21 onward carries.
    fn current_contract() -> Vec<u8> {
        glc_reserve_bridge_shared::PROGRAM_ID_BYTES.to_vec()
    }

    /// A database at v20 exactly as production has it: the whole ladder,
    /// version marker stamped, nothing from v21 present.
    /// A database at v23 exactly as production has it: the full ladder
    /// minus v24, so `bridge_routes` does not exist yet.
    fn conn_at_v23() -> Connection {
        let conn = conn_at_v20();
        apply_v21(&conn).unwrap();
        apply_v22(&conn).unwrap();
        apply_v23(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 23", [])
            .unwrap();
        assert!(
            !bridge_routes_exists(&conn),
            "the v23 fixture must not already carry the table v24 creates"
        );
        conn
    }

    fn bridge_routes_exists(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = \
             'bridge_routes')",
            [],
            |r| r.get::<_, i64>(0).map(|v| v != 0),
        )
        .unwrap()
    }

    /// `(route_id, source_chain, destination_chain, enabled)` for every
    /// seeded row, ordered by route id.
    fn bridge_route_rows(conn: &Connection) -> Vec<(String, String, String, i64)> {
        conn.prepare(
            "SELECT route_id, source_chain, destination_chain, enabled
               FROM bridge_routes ORDER BY route_id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    /// The one seed this migration is allowed to write, spelled out
    /// literally rather than derived, so a change to `Route::
    /// default_enabled` shows up here as a FAILING TEST instead of
    /// silently rewriting what a migration seeds.
    /// `the_v24_seed_is_exactly_every_routes_default_enabled` (in
    /// `routes::tests`) is the other half: it pins these same values
    /// against the route registry.
    fn expected_seed() -> Vec<(String, String, String, i64)> {
        [
            ("GlcToRhn", "goldcoin", "robinhood", 0),
            ("GlcToSol", "goldcoin", "solana", 1),
            ("RhnToGlc", "robinhood", "goldcoin", 0),
            ("RhnToSol", "robinhood", "solana", 0),
            ("SolToGlc", "solana", "goldcoin", 1),
            ("SolToRhn", "solana", "robinhood", 0),
        ]
        .into_iter()
        .map(|(r, s, d, e)| (r.to_string(), s.to_string(), d.to_string(), e))
        .collect()
    }

    #[test]
    fn a_fresh_database_seeds_one_bridge_route_row_per_route_fail_closed_for_robinhood() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();

        assert_eq!(
            bridge_route_rows(&conn),
            expected_seed(),
            "a fresh ledger must seed all six routes, with ONLY the two legacy routes enabled"
        );
    }

    #[test]
    fn upgrading_from_v23_creates_and_seeds_bridge_routes_without_losing_data() {
        let conn = conn_at_v23();
        insert_minimal_request(&conn, 31);

        open_and_migrate(&conn).unwrap(); // sees version=23, applies v24

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        // The upgrade is a behavioural no-op: every row carries exactly
        // the value `Ledger::route_enabled`'s missing-table fallback
        // already produced, so the legacy routes stay open and all four
        // Robinhood routes stay shut.
        assert_eq!(bridge_route_rows(&conn), expected_seed());

        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bridge_requests WHERE id = 31",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1, "pre-existing data must survive the migration");
    }

    #[test]
    fn re_running_v24_never_reseeds_a_route_an_operator_opened() {
        // The property that makes the migration safe to re-run against a
        // ledger already in production use: an operator's deliberate
        // `enabled = 1` must not be closed by a second pass of the seed.
        let conn = conn_at_v23();
        open_and_migrate(&conn).unwrap();
        conn.execute(
            "UPDATE bridge_routes SET enabled = 1 WHERE route_id = 'GlcToRhn'",
            [],
        )
        .unwrap();

        apply_v24(&conn).unwrap();
        open_and_migrate(&conn).unwrap();

        let enabled: i64 = conn
            .query_row(
                "SELECT enabled FROM bridge_routes WHERE route_id = 'GlcToRhn'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            enabled, 1,
            "re-running the migration must not close a route an operator opened"
        );
        // ...and nothing else moved.
        let others: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bridge_routes WHERE enabled = 1 AND route_id <> 'GlcToRhn'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(others, 2, "only the two legacy routes may also be enabled");
    }

    #[test]
    fn v24_refuses_a_hand_created_two_column_bridge_routes_table_instead_of_seeding_it() {
        // The exact table this repository's older TEST fixtures wrote by
        // hand. `CREATE TABLE IF NOT EXISTS` would no-op against it and
        // the seed would fail with a bare "no such column", so the
        // migration checks for it and refuses with something an operator
        // can act on. Nothing is written, and the version marker does not
        // advance.
        let conn = conn_at_v23();
        conn.execute_batch(
            "CREATE TABLE bridge_routes (
                 route_id TEXT PRIMARY KEY,
                 enabled  INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO bridge_routes (route_id, enabled) VALUES ('GlcToRhn', 1);",
        )
        .unwrap();

        let err = open_and_migrate(&conn).unwrap_err();
        assert!(
            matches!(&err, LedgerError::SchemaMigrationFailed(m) if m.contains("source_chain")),
            "expected an actionable migration refusal, got {err:?}"
        );
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, 23,
            "a refused migration must not advance the version marker"
        );
    }

    fn conn_at_v20() -> Connection {
        let conn = conn_at_v8();
        apply_v9(&conn).unwrap();
        apply_v10(&conn).unwrap();
        apply_v11(&conn).unwrap();
        apply_v12(&conn).unwrap();
        apply_v13(&conn).unwrap();
        apply_v14(&conn).unwrap();
        apply_v15(&conn).unwrap();
        apply_v16(&conn).unwrap();
        apply_v17(&conn).unwrap();
        apply_v18(&conn).unwrap();
        apply_v19(&conn).unwrap();
        apply_v20(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 20", [])
            .unwrap();
        conn
    }

    /// A realistic pre-Phase-B ledger: four `bridge_requests` covering
    /// every category the v21 backfill has to classify, and at least one
    /// row in EVERY table that carries a foreign key to
    /// `bridge_requests` (plus `goldcoin_refund_inputs`, which depends on
    /// one of those dependents in turn).
    ///
    /// Request ids are deliberately sparse and out of order so the
    /// rebuild cannot pass by accidentally renumbering rows.
    fn seed_realistic_v20_ledger(conn: &Connection) {
        // 1) A settled GlcToSol deposit: Goldcoin source outpoint, a
        //    derived deposit address, and v20's durable amount witness.
        conn.execute(
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 reserved_at, reservation_expires_at, source_txid, source_vout,
                 source_block_height, source_block_hash, source_confirmations,
                 source_finalized_at, settlement_claim_hash, destination_txid,
                 destination_confirmations, settled_at, deposit_address,
                 deposit_script_pubkey_hex, deposit_redeem_script_hex, observed_amount_atomic)
             VALUES (4, 'GlcToSol', 'Settled', 1000, 300, 30, 970, 970, X'A1', X'A2', 100,
                     101, 102, X'11', 0, 500, X'12', 200, 150, X'13', X'14', 32, 300,
                     'GaddrOne', 'a914aa87', '5121aa51ae', 1000)",
            [],
        )
        .unwrap();
        // 2) A GlcToSol request that has NOT yet seen a deposit at all:
        //    no txid, no vout, no obligation index. Its source chain is
        //    still decidable — from `direction` alone.
        conn.execute(
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, created_at,
                 reserved_at, reservation_expires_at)
             VALUES (9, 'GlcToSol', 'AwaitingDeposit', 2000, X'B1', 200, 200, 260)",
            [],
        )
        .unwrap();
        // 3) A Solana obligation at index 0 — the exact index a Robinhood
        //    bridge's first deposit will also carry.
        conn.execute(
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 reserved_at, source_obligation_index, source_confirmations,
                 source_finalized_at)
             VALUES (15, 'SolToGlc', 'SourceFinalized', 3000, 300, 90, 2910, 2910,
                     X'C1', X'C2', 300, 300, 0, 1, 300)",
            [],
        )
        .unwrap();
        // 4) A Solana obligation parked in ManualReview and since
        //    refunded on the Solana side.
        conn.execute(
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 reserved_at, source_obligation_index, source_confirmations,
                 source_finalized_at, manual_review_note)
             VALUES (23, 'SolToGlc', 'ManualReview', 4000, 300, 120, 3880, 3880,
                     X'D1', X'D2', 400, 400, 7, 1, 400, 'admission_closed_at_fold')",
            [],
        )
        .unwrap();

        // ---- one row in every foreign-key dependent of bridge_requests ----
        conn.execute(
            "INSERT INTO bridge_request_state_log (id, request_id, from_state, to_state, at, actor)
             VALUES (1, 4, 'AwaitingDeposit', 'Settled', 150, 'system')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO vault_utxos
                (txid, vout, amount_atomic, script_pubkey_hex, confirmations, first_seen_at,
                 state, reserved_by, reserved_at)
             VALUES (X'21', 0, 5000, 'a914bb87', 30, 10, 'Reserved', 23, 401)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at)
             VALUES (15, X'31', 2900, 90, 10, X'32', 'Broadcast', 310)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO goldcoin_payout_inputs (request_id, input_order, txid, vout, amount_atomic)
             VALUES (15, 0, X'41', 1, 3000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO attestation_records
                (id, request_id, action_type, canonical_message, message_hash, created_at)
             VALUES (1, 4, 'release', X'51', X'52', 160)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO goldcoin_payout_change_outputs (request_id, output_order, amount_atomic)
             VALUES (15, 0, 90)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO goldcoin_payout_change_outpoints
                (txid, vout, request_id, amount_atomic, unconfirmed_ancestor_depth)
             VALUES (X'61', 1, 15, 90, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO solana_refunds
                (request_id, obligation_index, nonce, amount_solana_atomic, requester,
                 destination_token_account, reserve_mint, token_program, manual_review_reason,
                 note, created_by, state, created_at)
             VALUES (23, 7, 4242, 4000, X'D2', X'71', X'72', X'73',
                     'admission_closed_at_fold', 'refund 23', 'cli:test', 'Pending', 410)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO goldcoin_refunds
                (request_id, source_txid, source_vout, observed_amount_atomic,
                 source_input_txid, source_input_vout, refund_dest_p2pkh_hash,
                 refund_dest_address, refund_amount_atomic, fee_atomic, state,
                 manual_review_reason, note, created_by, built_at)
             VALUES (9, X'81', 0, 2000, X'82', 1, X'83', 'GaddrRefund', 2000, 0, 'Built',
                     'deposit_amount_mismatch', 'refund 9', 'cli:test', 210)",
            [],
        )
        .unwrap();
        // Second-order dependent: references goldcoin_refunds(request_id),
        // which in turn references bridge_requests(id).
        conn.execute(
            "INSERT INTO goldcoin_refund_inputs
                (request_id, input_order, txid, vout, amount_atomic)
             VALUES (9, 0, X'91', 2, 2500)",
            [],
        )
        .unwrap();
    }

    /// Documents the exact defect v21 closes, against the schema as it
    /// actually was: at v20 the obligation guard is a SINGLE GLOBAL
    /// index, so an obligation index that is already present is rejected
    /// no matter which chain or contract it came from.
    #[test]
    fn at_v20_the_obligation_guard_is_global_and_would_collide_across_chains() {
        let conn = conn_at_v20();
        seed_realistic_v20_ledger(&conn);

        // Obligation 0 already exists (request 15, from Solana). A second
        // deposit carrying index 0 — which is exactly what a Robinhood
        // bridge's FIRST deposit carries — cannot be recorded at all.
        let err = conn
            .execute(
                "INSERT INTO bridge_requests
                    (id, direction, state, gross_amount_atomic, recipient, created_at,
                     source_obligation_index)
                 VALUES (77, 'SolToGlc', 'SourceFinalized', 100, X'E1', 500, 0)",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("UNIQUE"),
            "v20 must reject a second obligation 0 regardless of its origin: {err}"
        );
    }

    #[test]
    fn upgrading_from_v20_qualifies_every_row_by_chain_and_contract_and_keeps_all_dependents() {
        let conn = conn_at_v20();
        seed_realistic_v20_ledger(&conn);

        open_and_migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert_eq!(CURRENT_SCHEMA_VERSION, 29);

        // ---- every row still there, under its ORIGINAL id ----
        let ids: Vec<i64> = conn
            .prepare("SELECT id FROM bridge_requests ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            ids,
            vec![4, 9, 15, 23],
            "row ids are foreign-key targets — they must survive the rebuild exactly, \
             not be renumbered"
        );

        // ---- Goldcoin-sourced rows: chain derived, no contract ----
        for id in [4i64, 9] {
            let (chain, contract): (String, Option<Vec<u8>>) = conn
                .query_row(
                    "SELECT source_chain, source_contract FROM bridge_requests WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(chain, "goldcoin", "request {id}");
            assert_eq!(
                contract, None,
                "request {id}: Goldcoin's source identity is an outpoint — it has no contract"
            );
        }
        // The settled Goldcoin row kept every one of its columns verbatim.
        let (state, gross, fee_bps, net_dest): (String, i64, i64, i64) = conn
            .query_row(
                "SELECT state, gross_amount_atomic, fee_bps, net_destination_atomic
                 FROM bridge_requests WHERE id = 4",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (state.as_str(), gross, fee_bps, net_dest),
            ("Settled", 1000, 300, 970),
            "no column of a pre-existing Goldcoin row may be altered by the rebuild"
        );
        let (txid, vout, block_h, confs): (Vec<u8>, i64, i64, i64) = conn
            .query_row(
                "SELECT source_txid, source_vout, source_block_height, source_confirmations
                 FROM bridge_requests WHERE id = 4",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((txid, vout, block_h, confs), (vec![0x11u8], 0, 500, 200));
        let (addr, script, witness): (String, String, i64) = conn
            .query_row(
                "SELECT deposit_address, deposit_script_pubkey_hex, observed_amount_atomic
                 FROM bridge_requests WHERE id = 4",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (addr.as_str(), script.as_str(), witness),
            ("GaddrOne", "a914aa87", 1000),
            "the v9 deposit-address columns and the v20 amount witness travel with the row"
        );

        // ---- Solana-sourced rows: chain + the program id, index intact ----
        for (id, index) in [(15i64, 0i64), (23, 7)] {
            let (chain, contract, obligation): (String, Option<Vec<u8>>, i64) = conn
                .query_row(
                    "SELECT source_chain, source_contract, source_obligation_index
                     FROM bridge_requests WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(chain, "solana", "request {id}");
            assert_eq!(
                contract,
                Some(legacy_contract()),
                "request {id}: nothing in a pre-v21 ledger records WHICH Solana program \
                 issued this obligation, so the migration must mark it as unrecorded \
                 rather than assert today's program id"
            );
            assert_ne!(
                contract,
                Some(current_contract()),
                "request {id}: the migration must never manufacture a program identity"
            );
            assert_eq!(obligation, index, "request {id}");
        }
        // The ManualReview row kept its park reason.
        let note: String = conn
            .query_row(
                "SELECT manual_review_note FROM bridge_requests WHERE id = 23",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(note, "admission_closed_at_fold");

        // ---- every foreign-key dependent still resolves ----
        for (table, sql) in [
            (
                "bridge_request_state_log",
                "SELECT request_id FROM bridge_request_state_log",
            ),
            ("vault_utxos", "SELECT reserved_by FROM vault_utxos"),
            (
                "goldcoin_payouts",
                "SELECT request_id FROM goldcoin_payouts",
            ),
            (
                "goldcoin_payout_inputs",
                "SELECT request_id FROM goldcoin_payout_inputs",
            ),
            (
                "attestation_records",
                "SELECT request_id FROM attestation_records",
            ),
            (
                "goldcoin_payout_change_outputs",
                "SELECT request_id FROM goldcoin_payout_change_outputs",
            ),
            (
                "goldcoin_payout_change_outpoints",
                "SELECT request_id FROM goldcoin_payout_change_outpoints",
            ),
            ("solana_refunds", "SELECT request_id FROM solana_refunds"),
            (
                "goldcoin_refunds",
                "SELECT request_id FROM goldcoin_refunds",
            ),
            (
                "goldcoin_refund_inputs",
                "SELECT request_id FROM goldcoin_refund_inputs",
            ),
        ] {
            let referenced: i64 = conn.query_row(sql, [], |r| r.get(0)).unwrap();
            let target_exists: i64 = if table == "goldcoin_refund_inputs" {
                conn.query_row(
                    "SELECT COUNT(*) FROM goldcoin_refunds WHERE request_id = ?1",
                    [referenced],
                    |r| r.get(0),
                )
                .unwrap()
            } else {
                conn.query_row(
                    "SELECT COUNT(*) FROM bridge_requests WHERE id = ?1",
                    [referenced],
                    |r| r.get(0),
                )
                .unwrap()
            };
            assert_eq!(
                target_exists, 1,
                "{table} row must still point at an existing parent after the rebuild"
            );
        }

        // ---- the database's own verdict ----
        assert_foreign_keys_and_integrity_clean(&conn);

        // ---- indexes: all preserved, the guard re-keyed ----
        let index_names: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'index' AND tbl_name = 'bridge_requests' AND name IS NOT NULL
                 ORDER BY name",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            index_names,
            vec![
                "ix_bridge_requests_recipient_window".to_string(),
                "ix_bridge_requests_source_wallet_window".to_string(),
                "ix_bridge_requests_state".to_string(),
                "ux_bridge_requests_deposit_script".to_string(),
                "ux_bridge_requests_glc_source".to_string(),
                "ux_bridge_requests_obligation_source".to_string(),
                "ux_bridge_requests_solana_obligation".to_string(),
            ],
            "every pre-v21 index must be back, the global obligation index must be gone, \
             the qualified one must be present, and v28's source-wallet index must be added"
        );

        // A second open (the daemon simply restarting) is a clean no-op.
        open_and_migrate(&conn).unwrap();
        let (rows, version): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM bridge_requests),
                        (SELECT version FROM schema_version)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((rows, version), (4, CURRENT_SCHEMA_VERSION));
    }

    /// The whole point of the migration: which obligation indexes may
    /// coexist, and which may not.
    #[test]
    fn obligation_identity_is_unique_per_chain_and_contract_never_globally() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();

        let solana = current_contract();
        let rhn_v1 = vec![0xAAu8; 20];
        let rhn_v2 = vec![0xBBu8; 20];

        let insert = |id: i64, chain: &str, contract: &[u8], index: i64| {
            conn.execute(
                // `direction` is deliberately untouched by Phase B — no
                // Robinhood direction exists yet — so these rows reuse
                // the existing vocabulary; this test exercises the
                // IDENTITY guard, not routing.
                "INSERT INTO bridge_requests
                    (id, direction, state, gross_amount_atomic, recipient, created_at,
                     source_chain, source_contract, source_obligation_index)
                 VALUES (?1, 'SolToGlc', 'SourceFinalized', 100, X'AA', 0, ?2, ?3, ?4)",
                rusqlite::params![id, chain, contract, index],
            )
        };

        // Solana obligation 0.
        insert(1, "solana", &solana, 0).unwrap();
        // Robinhood v1 obligation 0 — SAME index, different chain AND
        // contract. Under the pre-v21 global index this was silently
        // "already folded"; it must now be accepted as the distinct
        // deposit it is.
        insert(2, "robinhood", &rhn_v1, 0).unwrap();
        // A successor Robinhood contract restarting its counter at 0.
        insert(3, "robinhood", &rhn_v2, 0).unwrap();
        // Higher indexes, same contracts, all fine.
        insert(4, "solana", &solana, 1).unwrap();
        insert(5, "robinhood", &rhn_v1, 1).unwrap();

        // ...and the guard still bites where it must: the SAME obligation
        // under the SAME contract can never be recorded twice.
        for (id, chain, contract, index) in [
            (6i64, "solana", solana.clone(), 0i64),
            (7, "robinhood", rhn_v1.clone(), 0),
            (8, "robinhood", rhn_v2.clone(), 0),
            (9, "robinhood", rhn_v1.clone(), 1),
        ] {
            let err = insert(id, chain, &contract, index).unwrap_err();
            assert!(
                err.to_string().contains("UNIQUE"),
                "{chain} obligation {index} under the same contract must be rejected: {err}"
            );
        }

        assert_foreign_keys_and_integrity_clean(&conn);
    }

    /// The soundness condition the unique index cannot state for itself:
    /// SQL NULLs never compare equal, so an obligation row with no
    /// contract would silently DISABLE the guard instead of tripping it.
    /// A `CHECK` makes that row unrepresentable.
    #[test]
    fn an_obligation_index_can_never_be_recorded_without_a_complete_identity() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();

        for (label, sql) in [
            (
                "obligation index with no contract",
                "INSERT INTO bridge_requests
                    (id, direction, state, gross_amount_atomic, recipient, created_at,
                     source_chain, source_obligation_index)
                 VALUES (1, 'SolToGlc', 'SourceFinalized', 100, X'AA', 0, 'goldcoin', 5)",
            ),
            (
                "contract-bearing chain with no contract",
                "INSERT INTO bridge_requests
                    (id, direction, state, gross_amount_atomic, recipient, created_at,
                     source_chain)
                 VALUES (1, 'SolToGlc', 'SourceFinalized', 100, X'AA', 0, 'solana')",
            ),
            (
                "goldcoin row carrying a contract it cannot have",
                "INSERT INTO bridge_requests
                    (id, direction, state, gross_amount_atomic, recipient, created_at,
                     source_chain, source_contract)
                 VALUES (1, 'GlcToSol', 'AwaitingDeposit', 100, X'AA', 0, 'goldcoin', X'AB')",
            ),
            (
                "empty contract blob is not an identity",
                "INSERT INTO bridge_requests
                    (id, direction, state, gross_amount_atomic, recipient, created_at,
                     source_chain, source_contract, source_obligation_index)
                 VALUES (1, 'SolToGlc', 'SourceFinalized', 100, X'AA', 0, 'solana', X'', 5)",
            ),
            (
                "unknown chain",
                "INSERT INTO bridge_requests
                    (id, direction, state, gross_amount_atomic, recipient, created_at,
                     source_chain, source_contract, source_obligation_index)
                 VALUES (1, 'SolToGlc', 'SourceFinalized', 100, X'AA', 0, 'ethereum', X'AB', 5)",
            ),
            (
                "no source chain at all",
                "INSERT INTO bridge_requests
                    (id, direction, state, gross_amount_atomic, recipient, created_at)
                 VALUES (1, 'GlcToSol', 'AwaitingDeposit', 100, X'AA', 0)",
            ),
        ] {
            assert!(
                conn.execute(sql, []).is_err(),
                "must be refused at the schema level: {label}"
            );
        }
    }

    /// Rows with no obligation index at all are untouched by the new
    /// guard: any number of them coexist, and the Goldcoin outpoint guard
    /// they DO answer to is unchanged.
    #[test]
    fn non_obligation_rows_behave_exactly_as_before() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();

        for id in 1..=5i64 {
            conn.execute(
                "INSERT INTO bridge_requests
                    (id, direction, state, gross_amount_atomic, recipient, created_at,
                     source_chain)
                 VALUES (?1, 'GlcToSol', 'AwaitingDeposit', 100, X'AA', 0, 'goldcoin')",
                [id],
            )
            .unwrap();
        }
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bridge_requests WHERE source_obligation_index IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            n, 5,
            "the qualified index is partial — rows without an obligation index never enter it"
        );

        // The pre-existing outpoint guard is carried over verbatim.
        conn.execute(
            "UPDATE bridge_requests SET source_txid = X'11', source_vout = 0 WHERE id = 1",
            [],
        )
        .unwrap();
        let err = conn
            .execute(
                "UPDATE bridge_requests SET source_txid = X'11', source_vout = 0 WHERE id = 2",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("UNIQUE"),
            "ux_bridge_requests_glc_source must still reject a duplicate deposit outpoint: {err}"
        );
    }

    /// The migration's own pre-commit checks are load-bearing, not
    /// decorative: a ledger that would have been left with a dangling
    /// dependent row aborts and is rolled back completely — the original
    /// table, its rows and its version marker all survive untouched, so
    /// the same binary can be rerun once the cause is understood.
    #[test]
    fn a_rebuild_that_would_orphan_a_dependent_row_is_refused_and_rolled_back() {
        let conn = conn_at_v20();
        seed_realistic_v20_ledger(&conn);

        // Plant a dangling dependent — a payout for a request that does
        // not exist. Only reachable with enforcement off, which is
        // exactly how such a row would come to exist out of band (a hand
        // edit, a partial restore) and why the migration re-checks.
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        conn.execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at)
             VALUES (999, X'31', 1, 0, 0, X'32', 'Built', 0)",
            [],
        )
        .unwrap();

        let err = open_and_migrate(&conn).unwrap_err();
        assert!(
            matches!(&err, LedgerError::SchemaMigrationFailed(detail)
                if detail.contains("foreign-key violations")),
            "got: {err}"
        );

        // Nothing was committed: the table still has its PRE-v21 shape,
        // every row is still there, and the version marker still says 20.
        assert!(
            !column_exists(&conn, "bridge_requests", "source_chain").unwrap(),
            "the rebuild must have been rolled back entirely"
        );
        let (rows, version): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM bridge_requests),
                        (SELECT version FROM schema_version)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((rows, version), (4, 20));
        // The staging table must not have been left behind either.
        let staged: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'bridge_requests_v21'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(staged, 0);

        // Remove the orphan and the very same call now succeeds — the
        // refusal is a transient, operator-resolvable one, not a wedge.
        conn.execute("DELETE FROM goldcoin_payouts WHERE request_id = 999", [])
            .unwrap();
        open_and_migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert_foreign_keys_and_integrity_clean(&conn);
    }

    #[test]
    fn applying_v21_twice_is_a_safe_no_op() {
        let conn = conn_at_v20();
        seed_realistic_v20_ledger(&conn);

        apply_v21(&conn).unwrap();
        // The second call must recognize the work is already done and
        // touch nothing — in particular it must NOT rebuild the table a
        // second time (which would be harmless but wasteful) or fail.
        apply_v21(&conn).unwrap();
        apply_v21(&conn).unwrap();

        let (rows, solana_rows): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM bridge_requests),
                        (SELECT COUNT(*) FROM bridge_requests WHERE source_chain = 'solana')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((rows, solana_rows), (4, 2));
        assert_foreign_keys_and_integrity_clean(&conn);
    }

    /// The migration leaves foreign-key enforcement exactly as it found
    /// it — it is disabled only for the duration of the rebuild, and only
    /// because SQLite's own documented recipe requires it.
    #[test]
    fn the_rebuild_restores_foreign_key_enforcement() {
        let conn = conn_at_v20();
        seed_realistic_v20_ledger(&conn);
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        apply_v21(&conn).unwrap();

        let on: bool = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert!(
            on,
            "foreign key enforcement must be back on after the rebuild"
        );
        // And it is genuinely enforcing.
        assert!(
            conn.execute(
                "INSERT INTO goldcoin_payouts
                    (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                     dest_p2pkh_hash, state, built_at)
                 VALUES (12345, X'31', 1, 0, 0, X'32', 'Built', 0)",
                [],
            )
            .is_err(),
            "a payout for a non-existent request must be refused"
        );
    }

    /// Production's ledger is a WAL-mode file, not an in-memory database,
    /// and the rebuild toggles `PRAGMA foreign_keys` around an explicit
    /// transaction — so the upgrade is exercised once against the real
    /// thing, opened the way `Ledger::open` opens it.
    #[test]
    fn a_wal_mode_file_database_upgrades_from_v20_and_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite3");

        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            apply_v1(&conn).unwrap();
            apply_v2(&conn).unwrap();
            apply_v3(&conn).unwrap();
            apply_v4(&conn).unwrap();
            apply_v5(&conn).unwrap();
            apply_v6(&conn).unwrap();
            apply_v7(&conn).unwrap();
            apply_v8(&conn).unwrap();
            apply_v9(&conn).unwrap();
            apply_v10(&conn).unwrap();
            apply_v11(&conn).unwrap();
            apply_v12(&conn).unwrap();
            apply_v13(&conn).unwrap();
            apply_v14(&conn).unwrap();
            apply_v15(&conn).unwrap();
            apply_v16(&conn).unwrap();
            apply_v17(&conn).unwrap();
            apply_v18(&conn).unwrap();
            apply_v19(&conn).unwrap();
            apply_v20(&conn).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (version INTEGER NOT NULL);
                 INSERT INTO schema_version (version) VALUES (20);",
            )
            .unwrap();
            seed_realistic_v20_ledger(&conn);
        }

        // The upgrade, through the real entry point.
        {
            let conn = Connection::open(&path).unwrap();
            open_and_migrate(&conn).unwrap();
        }

        // A fresh process opening the upgraded file sees the migrated
        // shape, every row, and a clean database.
        let conn = Connection::open(&path).unwrap();
        open_and_migrate(&conn).unwrap();
        let (version, rows, solana_rows): (i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT version FROM schema_version),
                        (SELECT COUNT(*) FROM bridge_requests),
                        (SELECT COUNT(*) FROM bridge_requests
                          WHERE source_chain = 'solana' AND source_contract IS NOT NULL)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((version, rows, solana_rows), (CURRENT_SCHEMA_VERSION, 4, 2));
        assert_foreign_keys_and_integrity_clean(&conn);
    }

    /// The legacy marker must be unmistakable for a real contract
    /// identity — by length alone, before anyone has to compare bytes.
    #[test]
    fn the_legacy_solana_marker_can_never_be_confused_with_a_real_contract() {
        let marker = crate::ledger::LEGACY_SOLANA_SOURCE_CONTRACT;
        assert_ne!(
            marker.len(),
            32,
            "a Solana program id is exactly 32 bytes — the marker must not be that length"
        );
        assert_ne!(
            marker.len(),
            20,
            "an EVM contract address is exactly 20 bytes — the marker must not be that length"
        );
        assert!(!marker.is_empty(), "the empty blob is rejected by a CHECK");
        assert_ne!(marker, &glc_reserve_bridge_shared::PROGRAM_ID_BYTES[..]);
        assert!(
            marker.iter().all(|b| b.is_ascii_graphic()),
            "the marker must read as itself in a plain sqlite3 dump"
        );
        // Pinned: this value is historical row identity, so changing it
        // would rewrite the identity of rows already migrated.
        assert_eq!(marker, b"GLC_LEGACY_SOLANA_PRE_V21");
    }

    /// The regression the legacy marker could otherwise have introduced:
    /// a migrated row's true program is unknown, so re-observing its
    /// obligation index under TODAY's program must still be refused — the
    /// pre-v21 promise, kept exactly.
    #[test]
    fn a_legacy_obligation_index_still_blocks_the_same_index_under_the_current_program() {
        let conn = conn_at_v20();
        seed_realistic_v20_ledger(&conn);
        open_and_migrate(&conn).unwrap();

        // Requests 15 and 23 came across as legacy Solana obligations 0
        // and 7. The same indexes under the CURRENT program id are not a
        // second deposit as far as this ledger can prove, and are refused.
        for index in [0i64, 7] {
            let err = conn
                .execute(
                    "INSERT INTO bridge_requests
                        (id, direction, state, gross_amount_atomic, recipient, created_at,
                         source_chain, source_contract, source_obligation_index)
                     VALUES (NULL, 'SolToGlc', 'SourceFinalized', 100, X'AA', 0,
                             'solana', ?1, ?2)",
                    rusqlite::params![&glc_reserve_bridge_shared::PROGRAM_ID_BYTES[..], index],
                )
                .unwrap_err();
            assert!(
                err.to_string().contains("UNIQUE"),
                "obligation {index} is already held by a legacy row: {err}"
            );
        }

        // A Solana index NOT held by any legacy row is still perfectly
        // acceptable — the guard is not a blanket freeze.
        conn.execute(
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, created_at,
                 source_chain, source_contract, source_obligation_index)
             VALUES (NULL, 'SolToGlc', 'SourceFinalized', 100, X'AA', 0, 'solana', ?1, 99)",
            rusqlite::params![&glc_reserve_bridge_shared::PROGRAM_ID_BYTES[..]],
        )
        .unwrap();

        // And the Solana-scoped guard leaves Robinhood entirely alone:
        // obligation 0 under two different Robinhood contracts, alongside
        // the legacy Solana obligation 0, are three distinct deposits.
        for (contract, id) in [(vec![0xAAu8; 20], 101i64), (vec![0xBBu8; 20], 102)] {
            conn.execute(
                "INSERT INTO bridge_requests
                    (id, direction, state, gross_amount_atomic, recipient, created_at,
                     source_chain, source_contract, source_obligation_index)
                 VALUES (?1, 'SolToGlc', 'SourceFinalized', 100, X'AA', 0, 'robinhood', ?2, 0)",
                rusqlite::params![id, contract],
            )
            .unwrap();
        }
        assert_foreign_keys_and_integrity_clean(&conn);
    }

    fn assert_foreign_keys_and_integrity_clean(conn: &Connection) {
        let violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0, "foreign_key_check must be clean");
        let integrity: String = conn
            .query_row("SELECT * FROM pragma_integrity_check LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(integrity, "ok", "integrity_check must be clean");
    }
    // ------------------------------------------------------------------
    // v25 — route-scoped admission
    // ------------------------------------------------------------------

    /// A database at v24 exactly as production has it: the full ladder
    /// minus v25, so `route_admission` does not exist yet.
    fn conn_at_v24() -> Connection {
        let conn = conn_at_v23();
        apply_v24(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 24", [])
            .unwrap();
        assert!(
            !route_admission_exists(&conn),
            "the v24 fixture must not already carry the table v25 creates"
        );
        conn
    }

    fn route_admission_exists(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = \
             'route_admission')",
            [],
            |r| r.get::<_, i64>(0).map(|v| v != 0),
        )
        .unwrap()
    }

    /// `(route_id, admission_closed)` for every seeded row, ordered.
    fn route_admission_rows(conn: &Connection) -> Vec<(String, i64)> {
        conn.prepare("SELECT route_id, admission_closed FROM route_admission ORDER BY route_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    /// The one seed this migration is allowed to write, spelled out
    /// literally rather than derived — same discipline as
    /// `expected_seed` above, so a change to
    /// `Route::is_admission_settable` shows up here as a FAILING TEST
    /// instead of silently rewriting what a migration seeds.
    fn expected_route_admission_seed() -> Vec<(String, i64)> {
        // v25's two inbound-to-Goldcoin rows plus v27's two cross-route
        // rows, all OPEN. Sorted by route_id, as `route_admission_rows`
        // reads them.
        [
            ("RhnToGlc", 0),
            ("RhnToSol", 0),
            ("SolToGlc", 0),
            ("SolToRhn", 0),
        ]
        .into_iter()
        .map(|(r, c)| (r.to_string(), c))
        .collect()
    }

    #[test]
    fn a_fresh_database_seeds_route_admission_open_for_every_observed_deposit_route() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();

        assert_eq!(
            route_admission_rows(&conn),
            expected_route_admission_seed(),
            "a fresh ledger must seed all four observed-deposit routes, all OPEN"
        );
    }

    // ------------------------------------------------------------- v27 --

    /// The three CHECK literals v27 leaves behind name exactly the values
    /// the Rust enums spell — pinned so a seventh direction, or a fifth
    /// contract route, is a failing test here rather than a row the
    /// database silently refuses.
    #[test]
    fn the_v27_check_literals_match_the_rust_enums() {
        use crate::ledger::Direction;
        use crate::routes::Route;
        let spelled = |list: &str| -> Vec<String> {
            list.split('\'')
                .skip(1)
                .step_by(2)
                .map(str::to_string)
                .collect()
        };
        assert_eq!(
            spelled(V27_DIRECTION_CHECK),
            Direction::ALL
                .iter()
                .map(|d| d.as_str().to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            spelled(V27_ROBINHOOD_TX_ROUTE_CHECK),
            Route::ALL
                .iter()
                .filter(|r| r.contract_route_id().is_some())
                .map(|r| r.as_str().to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            spelled(V27_ROUTE_ADMISSION_CHECK),
            Route::ADMISSION_SETTABLE
                .iter()
                .map(|r| r.as_str().to_string())
                .collect::<Vec<_>>()
        );
    }

    /// A ledger at v26 — carrying rows on every table v27 rebuilds, and
    /// an operator's own state on both axes — upgrades with every row,
    /// every dependent and every operator choice intact, and can then
    /// spell the two cross routes everywhere v27 says it can, while
    /// everything v26 established (a route-less `TreasuryWithdraw`
    /// operation, its one-per-rebalance-request index) still holds.
    #[test]
    fn upgrading_from_v26_widens_the_three_checks_and_keeps_everything() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        // Rewind the MARKER only: the structures are v27's, which is
        // exactly the re-run case every migration must survive; then
        // narrow the three constraints back to their v26 text so the
        // widening genuinely has work to do.
        conn.execute_batch("UPDATE schema_version SET version = 26;")
            .unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
        widen_check_constraint_labelled(
            &conn,
            "rewind",
            "bridge_requests",
            V27_DIRECTION_CHECK,
            "direction IN ('GlcToSol','SolToGlc','GlcToRhn','RhnToGlc')",
        )
        .unwrap();
        widen_check_constraint_labelled(
            &conn,
            "rewind",
            "robinhood_transactions",
            &format!(
                "route                TEXT CHECK (route IS NULL OR {V27_ROBINHOOD_TX_ROUTE_CHECK})"
            ),
            "route                TEXT CHECK (route IS NULL OR route IN ('GlcToRhn','RhnToGlc'))",
        )
        .unwrap();
        widen_check_constraint_labelled(
            &conn,
            "rewind",
            "robinhood_transactions",
            V27_PAYOUT_ROUTE_CHECK,
            "CHECK (route IS NULL OR ((kind = 'Payout') = (route = 'GlcToRhn')))",
        )
        .unwrap();
        conn.execute_batch(
            "DELETE FROM route_admission WHERE route_id IN ('SolToRhn','RhnToSol');",
        )
        .unwrap();
        widen_check_constraint_labelled(
            &conn,
            "rewind",
            "route_admission",
            &format!("CHECK ({V27_ROUTE_ADMISSION_CHECK})"),
            "CHECK (route_id IN ('SolToGlc','RhnToGlc'))",
        )
        .unwrap();
        conn.execute_batch("COMMIT;").unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        // Operator state on both axes, and rows on every rebuilt table.
        insert_minimal_request(&conn, 7);
        conn.execute_batch(
            "UPDATE bridge_routes SET enabled = 1 WHERE route_id = 'RhnToGlc';
             UPDATE route_admission SET admission_closed = 1,
                    admission_closed_reason = 'ops' WHERE route_id = 'SolToGlc';",
        )
        .unwrap();
        let tx_rows_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM robinhood_transactions", [], |r| {
                r.get(0)
            })
            .unwrap();

        open_and_migrate(&conn).unwrap(); // sees version=26, applies v27

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        // Rows and operator state survived.
        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bridge_requests WHERE id = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1);
        let enabled: i64 = conn
            .query_row(
                "SELECT enabled FROM bridge_routes WHERE route_id = 'RhnToGlc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(enabled, 1);
        assert_eq!(
            route_admission_rows(&conn),
            [
                ("RhnToGlc", 0),
                ("RhnToSol", 0),
                ("SolToGlc", 1),
                ("SolToRhn", 0)
            ]
            .into_iter()
            .map(|(r, c)| (r.to_string(), c))
            .collect::<Vec<_>>()
        );
        let tx_rows_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM robinhood_transactions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(tx_rows_before, tx_rows_after);

        // The widened constraints bite in the intended direction.
        for direction in ["SolToRhn", "RhnToSol"] {
            conn.execute(
                "INSERT INTO bridge_requests
                    (direction, state, gross_amount_atomic, recipient, created_at, source_chain,
                     source_contract)
                 VALUES (?1, 'ManualReview', 100, X'00', 1, 'solana', X'01')",
                [direction],
            )
            .unwrap_or_else(|e| panic!("{direction} must be insertable after v27: {e}"));
        }
        assert!(conn
            .execute(
                "INSERT INTO bridge_requests
                    (direction, state, gross_amount_atomic, recipient, created_at, source_chain)
                 VALUES ('GlcToGlc', 'ManualReview', 100, X'00', 1, 'goldcoin')",
                [],
            )
            .is_err());
        for route in ["SolToRhn", "RhnToSol"] {
            conn.execute(
                "INSERT INTO route_admission (route_id, admission_closed, updated_at)
                 VALUES (?1, 1, 1) ON CONFLICT(route_id) DO UPDATE SET admission_closed = 1",
                [route],
            )
            .unwrap();
        }
        assert!(conn
            .execute(
                "INSERT INTO route_admission (route_id, admission_closed, updated_at)
                 VALUES ('GlcToSol', 1, 1)",
                [],
            )
            .is_err());
        // A payout on the second outbound route, and a settlement on the
        // second inbound one, are both recordable now.
        conn.execute(
            "INSERT INTO robinhood_transactions
                (kind, request_id, route, bridge_contract, chain_id, action,
                 contract_request_id, recipient, amount_robinhood, signer_epoch, expiry,
                 auth_digest, state, created_at, updated_at)
             VALUES ('Payout', 7, 'SolToRhn', ?1, 4663, 1, ?2, ?3, ?2, 1, 9, ?2,
                     'Authorizing', 1, 1)",
            rusqlite::params![&[0x11u8; 20][..], &[0x22u8; 32][..], &[0x33u8; 20][..]],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO robinhood_transactions
                (kind, request_id, route, bridge_contract, chain_id, action,
                 contract_request_id, obligation_index, signer_epoch, expiry,
                 auth_digest, state, created_at, updated_at)
             VALUES ('Settlement', 7, 'RhnToSol', ?1, 4663, 3, ?2, 0, 1, 9, ?2,
                     'Authorizing', 1, 1)",
            rusqlite::params![&[0x11u8; 20][..], &[0x44u8; 32][..]],
        )
        .unwrap();
        // ...and a payout on an INBOUND route still is not.
        assert!(conn
            .execute(
                "INSERT INTO robinhood_transactions
                    (kind, request_id, route, bridge_contract, chain_id, action,
                     contract_request_id, recipient, amount_robinhood, signer_epoch, expiry,
                     auth_digest, state, created_at, updated_at)
                 VALUES ('Payout', 7, 'RhnToSol', ?1, 4663, 1, ?2, ?3, ?2, 1, 9, ?2,
                         'Authorizing', 1, 1)",
                rusqlite::params![&[0x11u8; 20][..], &[0x55u8; 32][..], &[0x33u8; 20][..]],
            )
            .is_err());

        // The completion columns exist, and only a 64-byte signature fits.
        assert!(column_exists(
            &conn,
            "robinhood_transactions",
            "onchain_completion_signature"
        )
        .unwrap());
        assert!(column_exists(
            &conn,
            "robinhood_transactions",
            "onchain_completion_submitted_at"
        )
        .unwrap());

        // v26's shape survived the v27 rebuild: a route-less
        // TreasuryWithdraw operation is still recordable, still refused
        // WITH a route, and still unique per rebalance request.
        conn.execute_batch(
            "INSERT INTO rebalance_requests
                (id, direction, kind, amount_atomic, state, reason, requested_by,
                 requested_at, required_approvals)
             VALUES (91, 'RobinhoodReserve', 'Withdraw', 5, 'Approved', 'ops', 'ops', 1, 1);",
        )
        .unwrap();
        let treasury_withdraw = |rebalance_request_id: i64, route: Option<&str>| {
            conn.execute(
                "INSERT INTO robinhood_transactions
                    (kind, request_id, rebalance_request_id, route, bridge_contract, chain_id,
                     action, contract_request_id, recipient, amount_robinhood, signer_epoch,
                     expiry, auth_digest, state, created_at, updated_at)
                 VALUES ('TreasuryWithdraw', NULL, ?1, ?2, ?3, 4663, 12, ?4, ?5, ?4, 1, 9, ?4,
                         'Authorizing', 1, 1)",
                rusqlite::params![
                    rebalance_request_id,
                    route,
                    &[0x11u8; 20][..],
                    &[0x66u8; 32][..],
                    &[0x33u8; 20][..]
                ],
            )
        };
        treasury_withdraw(91, None).expect("a route-less withdrawal is still recordable");
        assert!(
            treasury_withdraw(91, None).is_err(),
            "ux_robinhood_tx_rebalance must survive the rebuild"
        );
        assert!(
            treasury_withdraw(92, Some("SolToRhn")).is_err(),
            "a withdrawal must still carry no route"
        );
        let rebalance_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'ux_robinhood_tx_rebalance'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rebalance_index, 1);

        // And a second run is a no-op.
        open_and_migrate(&conn).unwrap();
        assert_eq!(
            route_admission_rows(&conn)
                .into_iter()
                .filter(|(r, _)| r == "SolToGlc")
                .map(|(_, c)| c)
                .next(),
            Some(1),
            "an operator's closed route must survive a re-run"
        );
    }

    /// The behaviour-preservation guarantee, stated as its own test: the
    /// seed opens nothing and closes nothing.
    #[test]
    fn the_v25_seed_leaves_every_route_admission_open() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();

        let closed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM route_admission WHERE admission_closed <> 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            closed, 0,
            "the migration must never close a route — production behaviour is preserved exactly"
        );
    }

    #[test]
    fn upgrading_from_v24_creates_and_seeds_route_admission_without_losing_data() {
        let conn = conn_at_v24();
        insert_minimal_request(&conn, 41);
        // An operator's pre-existing enablement state on the OTHER axis
        // must be untouched by this migration.
        conn.execute(
            "UPDATE bridge_routes SET enabled = 1 WHERE route_id = 'RhnToGlc'",
            [],
        )
        .unwrap();

        open_and_migrate(&conn).unwrap(); // sees version=24, applies v25

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert_eq!(route_admission_rows(&conn), expected_route_admission_seed());

        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bridge_requests WHERE id = 41",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1, "pre-existing data must survive the migration");

        // The enablement axis is a different table and a different
        // question; v25 must not have touched it.
        let still_enabled: i64 = conn
            .query_row(
                "SELECT enabled FROM bridge_routes WHERE route_id = 'RhnToGlc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            still_enabled, 1,
            "v25 must not disturb bridge_routes — enablement and admission are separate axes"
        );
    }

    #[test]
    fn re_running_v25_never_reopens_a_route_an_operator_closed() {
        // The mirror of `re_running_v24_never_reseeds_a_route_an_operator
        // _opened`, and the direction that matters more here: a
        // re-seeding migration would silently REOPEN a route an operator
        // deliberately closed, which admits deposits nobody authorised.
        let conn = conn_at_v24();
        open_and_migrate(&conn).unwrap();
        conn.execute(
            "UPDATE route_admission SET admission_closed = 1 WHERE route_id = 'RhnToGlc'",
            [],
        )
        .unwrap();

        apply_v25(&conn).unwrap();
        open_and_migrate(&conn).unwrap();

        let closed: i64 = conn
            .query_row(
                "SELECT admission_closed FROM route_admission WHERE route_id = 'RhnToGlc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            closed, 1,
            "re-running the migration must not reopen a route an operator closed"
        );
        // ...and the other route did not move either.
        let other: i64 = conn
            .query_row(
                "SELECT admission_closed FROM route_admission WHERE route_id = 'SolToGlc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(other, 0);
    }

    /// The table's own CHECK is a second, independent backstop under
    /// `Route::is_admission_settable`: no route outside the
    /// inbound-to-Goldcoin pair can be given a row, even by hand.
    #[test]
    fn route_admission_refuses_a_row_for_any_other_route() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();

        for route_id in ["GlcToSol", "GlcToRhn", "SolToRhn", "RhnToSol", "Nonsense"] {
            let err = conn.execute(
                "INSERT INTO route_admission (route_id, admission_closed, updated_at)
                 VALUES (?1, 1, 0)",
                [route_id],
            );
            assert!(
                err.is_err(),
                "the CHECK must refuse a route_admission row for {route_id}"
            );
        }
    }

    /// `admission_closed` is a real flag, not an arbitrary integer.
    #[test]
    fn route_admission_refuses_a_non_boolean_flag() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();

        let err = conn.execute(
            "UPDATE route_admission SET admission_closed = 2 WHERE route_id = 'SolToGlc'",
            [],
        );
        assert!(err.is_err(), "admission_closed must be constrained to 0/1");
    }

    /// A hand-created `route_admission` of the wrong shape is an
    /// actionable migration refusal, not a bare "no such column" — and
    /// the version marker is never advanced past it.
    #[test]
    fn v25_refuses_a_hand_created_route_admission_table_instead_of_seeding_it() {
        let conn = conn_at_v24();
        conn.execute_batch(
            "CREATE TABLE route_admission (route_id TEXT PRIMARY KEY, note TEXT);
             INSERT INTO route_admission (route_id, note) VALUES ('SolToGlc', 'hand-written');",
        )
        .unwrap();

        let err = open_and_migrate(&conn).unwrap_err();
        assert!(
            matches!(&err, LedgerError::SchemaMigrationFailed(m)
                if m.contains("route_admission") && m.contains("admission_closed")),
            "expected an actionable v25 refusal, got: {err}"
        );

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, 24,
            "a refused migration must never advance the version marker"
        );
    }
}

#[cfg(test)]
mod v22_tests {
    use super::*;
    use rusqlite::Connection;

    fn open() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        conn
    }

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn a_fresh_database_reaches_the_current_version_with_the_observation_tables() {
        let conn = open();
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let tables = table_names(&conn);
        for expected in [
            "robinhood_deposit_observations",
            "robinhood_scanned_blocks",
            "robinhood_indexer_state",
            "robinhood_reorg_events",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "{expected} must still exist",
            );
        }
    }

    /// v23 widened this vocabulary to four directions and v27 to six —
    /// exactly the `Direction` enum, and nothing else. The database's
    /// CHECK is an independent copy of the type-level vocabulary: it
    /// widened only when the machinery behind each spelling existed.
    #[test]
    fn the_direction_vocabulary_admits_every_direction_and_no_others() {
        let conn = open();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'bridge_requests'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains(V27_DIRECTION_CHECK),
            "bridge_requests.direction must admit exactly the six settlement directions: {sql}",
        );

        // Not merely a substring check on DDL: prove the constraint bites.
        let insert = |direction: &str| {
            conn.execute(
                "INSERT INTO bridge_requests
                    (direction, state, gross_amount_atomic, recipient, created_at, source_chain)
                 VALUES (?1, 'AwaitingDeposit', 100, X'00', 1, 'goldcoin')",
                [direction],
            )
        };
        for allowed in crate::ledger::Direction::ALL {
            insert(allowed.as_str())
                .unwrap_or_else(|e| panic!("{} must be insertable: {e}", allowed.as_str()));
        }
        for refused in ["GlcToGlc", "SolToSol", ""] {
            assert!(
                insert(refused).is_err(),
                "{refused:?} must be refused by the direction CHECK",
            );
        }
    }

    /// The Robinhood reserve is a THIRD physical reserve, never a
    /// relabelling of one of the existing two.
    #[test]
    fn the_reserve_vocabulary_gains_robinhood_and_keeps_the_other_two() {
        let conn = open();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'reserve_ledger'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("direction IN ('GoldcoinReserve','SolanaReserve','RobinhoodReserve')"),
            "reserve_ledger.direction must admit all three reserves: {sql}",
        );

        // Every column nine ALTER TABLE migrations added must have
        // survived the rebuild — the exact failure a hand-retyped DDL
        // would have caused.
        for column in [
            "accrued_fees_atomic",
            "admission_closed",
            "admission_reason",
            "utxo_pool_min_available_count",
            "utxo_pool_warning_count",
            "admission_buffer_atomic",
            "admission_reopen_atomic",
            "liquidity_admission_closed",
            "liquidity_admission_closed_at",
        ] {
            assert!(
                column_exists(&conn, "reserve_ledger", column).unwrap(),
                "{column} must have survived the v23 rebuild",
            );
        }
    }

    /// v22 pinned `settled` to zero specifically so that settling a
    /// Robinhood deposit would require a reviewable migration. v23 is
    /// that migration; the column is now a real flag, and the link to the
    /// folded request exists.
    #[test]
    fn observations_can_now_be_settled_and_linked_to_a_request() {
        let conn = open();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table'
                 AND name = 'robinhood_deposit_observations'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("CHECK (settled IN (0,1))"),
            "settled must now be a two-valued flag: {sql}",
        );
        assert!(
            !sql.contains("CHECK (settled = 0)"),
            "the v22 pin must be gone: {sql}",
        );
        assert!(
            column_exists(&conn, "robinhood_deposit_observations", "folded_request_id").unwrap()
        );

        // Both v22 identity indexes must have survived the rebuild.
        let indexes: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'index'
                     AND tbl_name = 'robinhood_deposit_observations'",
                )
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        for expected in [
            "ux_robinhood_obligation_source",
            "ux_robinhood_log_identity",
            "ix_robinhood_observations_finality",
            "ux_robinhood_observation_request",
        ] {
            assert!(
                indexes.iter().any(|i| i == expected),
                "{expected} must exist after the v23 rebuild, have: {indexes:?}",
            );
        }
    }

    /// The replay guard is scoped past tombstones on purpose — a reorg
    /// can genuinely reassign an obligation index — and the log identity
    /// is guarded independently of it.
    #[test]
    fn both_identity_indexes_exist_and_skip_tombstones() {
        let conn = open();
        let mut stmt = conn
            .prepare(
                "SELECT name, sql FROM sqlite_master WHERE type = 'index'
                 AND tbl_name = 'robinhood_deposit_observations'",
            )
            .unwrap();
        let indexes: Vec<(String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        for expected in [
            "ux_robinhood_obligation_source",
            "ux_robinhood_log_identity",
        ] {
            let (_, sql) = indexes
                .iter()
                .find(|(name, _)| name == expected)
                .unwrap_or_else(|| panic!("{expected} must exist"));
            let sql = sql.as_deref().expect("a user index has SQL");
            assert!(sql.contains("UNIQUE"), "{expected} must be unique");
            assert!(
                sql.contains("finality <> 'Reorged'"),
                "{expected} must be scoped past tombstones",
            );
        }
    }

    /// Structural idempotence: running the migration again against a
    /// database that already has the tables is a no-op, not a failure.
    #[test]
    fn the_robinhood_migrations_are_idempotent() {
        let conn = open();
        apply_v22(&conn).unwrap();
        apply_v23(&conn).unwrap();
        open_and_migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
    }

    /// Upgrading a pre-v22 database adds the tables and preserves every
    /// existing row.
    #[test]
    fn upgrading_from_v21_adds_the_tables_and_keeps_existing_rows() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, recipient, created_at, source_chain)
             VALUES ('GlcToSol', 'AwaitingDeposit', 100, X'00', 1, 'goldcoin')",
            [],
        )
        .unwrap();

        // Rewind to v21 and drop everything v22/v23 added, then migrate
        // forward through both.
        conn.execute_batch(
            "DROP TABLE robinhood_authorization_signatures;
             DROP TABLE robinhood_transactions;
             DROP TABLE evm_submitter_state;
             DROP TABLE robinhood_deposit_observations;
             DROP TABLE robinhood_scanned_blocks;
             DROP TABLE robinhood_indexer_state;
             DROP TABLE robinhood_reorg_events;
             UPDATE schema_version SET version = 21;",
        )
        .unwrap();
        open_and_migrate(&conn).unwrap();

        let (version, requests): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT version FROM schema_version),
                        (SELECT COUNT(*) FROM bridge_requests)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((version, requests), (CURRENT_SCHEMA_VERSION, 1));
        assert!(table_names(&conn)
            .iter()
            .any(|t| t == "robinhood_deposit_observations"));
    }
}

#[cfg(test)]
mod v28_tests {
    use super::*;
    use rusqlite::Connection;

    fn open() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        conn
    }

    /// Rows shaped exactly as every pre-v28 fold wrote them: a Solana
    /// request carrying `requester`, a Robinhood request whose depositor
    /// lives only on its linked observation, a Goldcoin-sourced request
    /// with no source identity at all — and `source_wallet` NULL on all
    /// three, as an upgrading database has it.
    fn seed_pre_v28_rows(conn: &Connection) {
        conn.execute_batch(
            r#"
            INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, requester, created_at,
                 source_chain, source_contract, source_obligation_index)
            VALUES (1, 'SolToGlc', 'Settled', 100, X'aa', X'1111111111111111111111111111111111111111111111111111111111111111',
                    1000, 'solana', X'2222222222222222222222222222222222222222222222222222222222222222', 7);
            INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, created_at,
                 source_chain, source_contract, source_obligation_index)
            VALUES (2, 'RhnToSol', 'ManualReview', 100, X'bb', 1001, 'robinhood',
                    X'3333333333333333333333333333333333333333', 9);
            INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, created_at, source_chain)
            VALUES (3, 'GlcToSol', 'AwaitingDeposit', 100, X'cc', 1002, 'goldcoin');
            INSERT INTO robinhood_deposit_observations
                (id, source_chain, source_contract, source_obligation_index, contract_route_id,
                 route, depositor, destination, amount_robinhood_atomic, amount_canonical_atomic,
                 tx_hash, log_index, block_number, block_hash, finality, observed_at,
                 finalized_at, folded_request_id)
            VALUES (1, 'robinhood', X'3333333333333333333333333333333333333333', 9, 4, 'RhnToSol',
                    X'4444444444444444444444444444444444444444', X'bb',
                    X'0000000000000000000000000000000000000000000000000000000000000064', 100,
                    X'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 0, 500,
                    X'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'Final',
                    100, 200, 2);
            UPDATE bridge_requests SET source_wallet = NULL;
            "#,
        )
        .unwrap();
    }

    fn source_wallets(conn: &Connection) -> Vec<(i64, Option<Vec<u8>>)> {
        conn.prepare("SELECT id, source_wallet FROM bridge_requests ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn v28_adds_the_source_wallet_column_and_its_index() {
        let conn = open();
        assert!(column_exists(&conn, "bridge_requests", "source_wallet").unwrap());
        let index_exists: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'index'
                    AND name = 'ix_bridge_requests_source_wallet_window')",
                [],
                |r| r.get::<_, i64>(0).map(|v| v != 0),
            )
            .unwrap();
        assert!(index_exists);
        // An empty identity is unrepresentable.
        let err = conn
            .execute(
                "INSERT INTO bridge_requests
                    (direction, state, gross_amount_atomic, recipient, created_at, source_chain,
                     source_wallet)
                 VALUES ('GlcToSol', 'AwaitingDeposit', 1, X'ab', 1, 'goldcoin', X'')",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{err}");
    }

    #[test]
    fn v28_backfills_solana_and_robinhood_rows_from_where_each_route_kept_its_wallet() {
        let conn = open();
        seed_pre_v28_rows(&conn);
        assert_eq!(
            source_wallets(&conn),
            vec![(1, None), (2, None), (3, None)],
            "the seed models an upgrading database"
        );

        // The migration is structurally idempotent, so re-running it on
        // an already-v28 database is exactly the upgrade path's backfill.
        apply_v28(&conn).unwrap();

        assert_eq!(
            source_wallets(&conn),
            vec![
                (1, Some(vec![0x11; 32])),
                (2, Some(vec![0x44; 20])),
                (3, None),
            ],
            "Solana copies `requester`, Robinhood copies its non-reorged observation's \
             `depositor`, a Goldcoin-sourced row stays unknown"
        );
        // Nothing else moved.
        let states: Vec<String> = conn
            .prepare("SELECT state FROM bridge_requests ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(states, vec!["Settled", "ManualReview", "AwaitingDeposit"]);

        // A second run changes nothing, and never overwrites a value.
        conn.execute(
            "UPDATE bridge_requests SET source_wallet = X'55' WHERE id = 1",
            [],
        )
        .unwrap();
        apply_v28(&conn).unwrap();
        assert_eq!(source_wallets(&conn)[0], (1, Some(vec![0x55])));
    }

    #[test]
    fn v28_ignores_a_reorged_observation_when_backfilling() {
        let conn = open();
        seed_pre_v28_rows(&conn);
        conn.execute(
            "UPDATE robinhood_deposit_observations
                SET finality = 'Reorged', reorged_at = 300, finalized_at = NULL
              WHERE id = 1",
            [],
        )
        .unwrap();
        apply_v28(&conn).unwrap();
        assert_eq!(
            source_wallets(&conn)[1],
            (2, None),
            "an orphaned sighting is not evidence of who funded the request"
        );
    }
}
