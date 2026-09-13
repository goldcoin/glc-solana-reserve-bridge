//! The Goldcoin indexer tick loop: forward indexing, reorg detection and
//! rollback, request-binding matching, and confirmation-depth promotion.
//! Adapted from the old bridge's `glc::indexer` (docs/01-reuse-inventory.md:
//! "REUSE with modification" — the reorg-walk/confirmation-depth mechanics
//! are chain-mechanics and directly reusable; the deposit-candidate ledger
//! is replaced by direct `bridge_requests` mutation through [`Ledger`], and
//! `ReadyForSignature`'s mint-claim-message construction is replaced by the
//! plain `SourceFinalized` transition — settlement authorization is a later
//! phase's concern (attestation signing clients), not indexing).
//!
//! [`GoldcoinRpc`] is a trait covering the exact RPC surface the indexer
//! needs, implemented for the real [`RpcClient`] and for a test-only mock,
//! so tick/reorg logic is unit-testable without a live node (real-node
//! testing against Goldcoin v0.17.0-beta1 regtest is Phase 6 per
//! docs/11-testing-plan.md, not this pass — no `goldcoind` binary is
//! available in this environment; see IMPLEMENTATION_LOG.md).

use std::future::Future;

use thiserror::Error;

use crate::ledger::{GlcObservationOutcome, Ledger, LedgerError};

use super::deposit::{extract_request_binding, glc_to_atomic, vault_output_candidates};
use super::hex;
use super::rpc::{BlockHeader, BroadcastOutcome, DecodedTransaction, RpcClient, RpcError, TxOut};

#[derive(Debug, Error)]
pub enum IndexerError {
    #[error("Goldcoin node unavailable: {0}")]
    NodeUnavailable(RpcError),
    #[error("Goldcoin RPC method error: {0}")]
    Rpc(RpcError),
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
    /// A configured [`InitialCheckpoint`] is structurally invalid — bad hex,
    /// a negative height, or the required operator acknowledgement not set.
    /// Never falls back to height 0; the whole tick fails instead
    /// (constraint: never silently start indexing from an unintended
    /// point).
    #[error("initial Goldcoin checkpoint config is malformed: {0}")]
    InvalidCheckpointConfig(String),
    /// The live node's `getblockhash(height)` did not exactly match the
    /// configured checkpoint hash — never guessed, never overridden.
    #[error(
        "initial Goldcoin checkpoint at height {height} does not match the live chain: \
         configured hash {configured}, live hash {live}"
    )]
    CheckpointHashMismatch {
        height: i64,
        configured: String,
        live: String,
    },
    /// The configured checkpoint height is above the live chain tip —
    /// refusing to fabricate a checkpoint for a block that does not exist
    /// yet.
    #[error(
        "initial Goldcoin checkpoint height {height} is above the live chain tip {tip} — \
         refusing to fabricate a future checkpoint"
    )]
    CheckpointAboveTip { height: i64, tip: i64 },
}

/// A verified starting point for a brand-new Goldcoin indexer, used ONLY
/// when the ledger has no indexed blocks yet (`Ledger::goldcoin_chain_tip`
/// returns `None`) — see [`Indexer::bootstrap_from_checkpoint_or_genesis`].
/// Means exactly this: **every Goldcoin deposit before `height` is
/// intentionally outside the bridge's supported history** — this vault
/// must not have received any bridge deposit at or before this height
/// (enforced by `operator_acknowledged_no_prior_deposits`, not inferred:
/// Goldcoin 0.15 has no `scantxoutset`, so this service cannot itself scan
/// pre-checkpoint history to check that claim — see this field's own
/// docs).
///
/// Exists purely to skip the otherwise many-hours-long full resync a
/// brand-new ledger would need against an already-tall production chain
/// (docs/09-runbook.md "Goldcoin indexer initial checkpoint"). Once
/// ledger has ANY indexed block, this is never consulted again — the
/// normal persisted cursor/reorg logic always wins from then on (see
/// [`Indexer::tick`]'s `Some((local_height, local_hash))` branch, entirely
/// unchanged by this struct's existence).
#[derive(Debug, Clone)]
pub struct InitialCheckpoint {
    /// Must be `>= 0` and `<=` the live chain tip at bootstrap time —
    /// checked against a live `getblockhash`/`getblock` read, never
    /// trusted on its own.
    pub height: i64,
    /// Lowercase or uppercase 64-character hex — the exact block hash
    /// `getblockhash(height)` must return on the live node, byte-for-byte,
    /// or this checkpoint is rejected outright (never guessed, never
    /// partially matched).
    pub hash: String,
    /// Required explicit operator confirmation that this vault address
    /// received no bridge deposit at or before `height` — this service
    /// cannot verify that claim itself (Goldcoin 0.15 has no
    /// `scantxoutset` to scan pre-checkpoint history), so it refuses to
    /// even attempt to infer it. `false` (including the zero-value
    /// default from an unset config field) fails the whole checkpoint
    /// closed, exactly like a malformed hash or an out-of-range height —
    /// never silently ignored, never silently downgraded to "start at 0"
    /// instead.
    pub operator_acknowledged_no_prior_deposits: bool,
}

#[derive(Debug, Clone)]
pub struct ReorgSummary {
    pub fork_height: i64,
    pub old_tip_height: i64,
    pub orphaned_count: i64,
}

#[derive(Debug, Clone)]
pub enum TickOutcome {
    Progressed {
        blocks_indexed: i64,
        reorg: Option<ReorgSummary>,
    },
    /// A reorg deeper than `max_reorg_depth` was detected. No database
    /// writes were made this tick, and every future tick reports `Halted`
    /// again without touching the database or the network until the
    /// process is restarted with a wider `max_reorg_depth` or manual
    /// operator intervention (fail closed: never guess a fork point).
    Halted { attempted_depth: i64 },
    /// A reorg was found within `max_reorg_depth`, but its fork point is
    /// at or below the source block of at least one Goldcoin-sourced request
    /// already told its deposit was final — a genuine incident
    /// (docs/10-threat-model.md's "post-finality reorg", never routine).
    /// Neither the normal rollback nor forward indexing ran this tick;
    /// both reserve directions were paused
    /// ([`Ledger::record_post_finality_reorg`]) and every future tick
    /// reports this outcome again without touching the database or the
    /// network until an operator investigates and explicitly unpauses
    /// (the persisted pause, not this in-memory halt flag, is what
    /// actually survives a process restart — see that function's docs).
    PostFinalityReorgHalted {
        fork_height: i64,
        old_tip_height: i64,
        affected_request_ids: Vec<i64>,
    },
}

pub trait GoldcoinRpc {
    fn get_block_count(&self) -> impl Future<Output = Result<i64, RpcError>> + Send;
    fn get_block_hash(&self, height: i64) -> impl Future<Output = Result<String, RpcError>> + Send;
    fn get_block(&self, hash: &str) -> impl Future<Output = Result<BlockHeader, RpcError>> + Send;
    fn get_raw_transaction(
        &self,
        txid_hex: &str,
    ) -> impl Future<Output = Result<DecodedTransaction, RpcError>> + Send;
    fn get_tx_out_confirmed(
        &self,
        txid_hex: &str,
        vout: u32,
    ) -> impl Future<Output = Result<Option<TxOut>, RpcError>> + Send;
    /// Broadcasts a fully-signed payout transaction
    /// ([`crate::orchestrator`], Solana->Goldcoin leg). See
    /// [`RpcClient::send_raw_transaction`] for the exact accepted/
    /// already-in-chain/missing-inputs contract.
    fn send_raw_transaction(
        &self,
        hex: &str,
    ) -> impl Future<Output = Result<BroadcastOutcome, RpcError>> + Send;
    /// Live vault UTXO discovery ([`crate::orchestrator`]'s vault-UTXO
    /// sync phase, and eventually Goldcoin-reserve reconciliation). See
    /// [`RpcClient::list_unspent`] for the `solvable`-not-`spendable`
    /// filter rationale.
    fn list_unspent(
        &self,
        min_conf: i64,
        addresses: &[String],
    ) -> impl Future<Output = Result<Vec<super::rpc::ListUnspentEntry>, RpcError>> + Send;
}

impl GoldcoinRpc for RpcClient {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        RpcClient::get_block_count(self).await
    }
    async fn get_block_hash(&self, height: i64) -> Result<String, RpcError> {
        RpcClient::get_block_hash(self, height).await
    }
    async fn get_block(&self, hash: &str) -> Result<BlockHeader, RpcError> {
        RpcClient::get_block(self, hash).await
    }
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        RpcClient::get_raw_transaction(self, txid_hex).await
    }
    async fn get_tx_out_confirmed(
        &self,
        txid_hex: &str,
        vout: u32,
    ) -> Result<Option<TxOut>, RpcError> {
        RpcClient::get_tx_out_confirmed(self, txid_hex, vout).await
    }
    async fn send_raw_transaction(&self, hex: &str) -> Result<BroadcastOutcome, RpcError> {
        RpcClient::send_raw_transaction(self, hex).await
    }
    async fn list_unspent(
        &self,
        min_conf: i64,
        addresses: &[String],
    ) -> Result<Vec<super::rpc::ListUnspentEntry>, RpcError> {
        RpcClient::list_unspent(self, min_conf, addresses).await
    }
}

const INNER_RETRY_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone)]
pub struct IndexerConfig {
    pub vault_script_hex: String,
    pub confirmation_depth: u32,
    pub max_reorg_depth: u32,
    /// The network whose address encoding a deposit's FUNDING wallets
    /// are spelled in (`Indexer::trace_funding_wallets`) — the key the
    /// rolling-24h source-wallet window is consumed under, and therefore
    /// the spelling `GET /routes/{route}/eligibility?source=` must
    /// canonicalize a caller's address to. Same value as the daemon's
    /// `config.goldcoin.network`.
    pub network: super::address::Network,
    /// `None` (the default) preserves exactly the pre-existing dev/test
    /// behavior: a brand-new ledger starts indexing at height 0. `Some`
    /// is consulted ONLY when the ledger has no indexed blocks yet — see
    /// [`InitialCheckpoint`]'s own docs.
    pub initial_checkpoint: Option<InitialCheckpoint>,
}

pub struct Indexer<R: GoldcoinRpc> {
    rpc: R,
    ledger: Ledger,
    config: IndexerConfig,
    halted: bool,
    /// Set once this process's own tick loop has detected and recorded a
    /// post-finality reorg, so subsequent ticks in the SAME process
    /// continue reporting the specific `PostFinalityReorgHalted` details
    /// rather than blurring into the generic `Halted` message. The
    /// persisted reserve pause (`Ledger::record_post_finality_reorg`), not
    /// this in-memory flag, is what actually survives a restart — see
    /// that function's docs.
    post_finality_halt: Option<(i64, i64, Vec<i64>)>,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl<R: GoldcoinRpc> Indexer<R> {
    pub fn new(rpc: R, ledger: Ledger, config: IndexerConfig) -> Self {
        Indexer {
            rpc,
            ledger,
            config,
            halted: false,
            post_finality_halt: None,
        }
    }

    async fn call<T, F, Fut>(f: F) -> Result<T, IndexerError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, RpcError>>,
    {
        super::rpc::call_with_retry(INNER_RETRY_ATTEMPTS, f)
            .await
            .map_err(|e| {
                if e.is_retriable() {
                    IndexerError::NodeUnavailable(e)
                } else {
                    IndexerError::Rpc(e)
                }
            })
    }

    /// Runs one indexer tick: reorg detection/rollback (if needed), forward
    /// indexing to the live chain tip, and confirmation-depth promotion.
    pub async fn tick(&mut self) -> Result<TickOutcome, IndexerError> {
        if let Some((fork_height, old_tip_height, affected_request_ids)) = &self.post_finality_halt
        {
            return Ok(TickOutcome::PostFinalityReorgHalted {
                fork_height: *fork_height,
                old_tip_height: *old_tip_height,
                affected_request_ids: affected_request_ids.clone(),
            });
        }
        if self.halted {
            return Ok(TickOutcome::Halted {
                attempted_depth: self.config.max_reorg_depth as i64 + 1,
            });
        }

        let live_tip_height = Self::call(|| self.rpc.get_block_count()).await?;

        let (start_height, reorg) = match self.ledger.goldcoin_chain_tip()? {
            None => (
                self.bootstrap_from_checkpoint_or_genesis(live_tip_height)
                    .await?,
                None,
            ),
            Some((local_height, local_hash)) => match self.find_fork_point(local_height).await? {
                None => {
                    self.halted = true;
                    return Ok(TickOutcome::Halted {
                        attempted_depth: self.config.max_reorg_depth as i64 + 1,
                    });
                }
                Some(fork_height) if fork_height == local_height => (local_height + 1, None),
                Some(fork_height) => {
                    // Checked BEFORE any rollback write: does this fork
                    // point reach back far enough to orphan the source
                    // block of a request already told its deposit was
                    // final? `goldcoin_rollback_reorg` below deliberately
                    // only ever touches pre-finality requests, so this is
                    // the one and only place that gap would otherwise go
                    // undetected (docs/22-production-readiness-review.md).
                    let post_finality_affected =
                        self.ledger.detect_post_finality_reorg(fork_height)?;
                    if !post_finality_affected.is_empty() {
                        self.ledger.record_post_finality_reorg(
                            fork_height,
                            local_height,
                            &post_finality_affected,
                            now_unix(),
                        )?;
                        self.post_finality_halt =
                            Some((fork_height, local_height, post_finality_affected.clone()));
                        return Ok(TickOutcome::PostFinalityReorgHalted {
                            fork_height,
                            old_tip_height: local_height,
                            affected_request_ids: post_finality_affected,
                        });
                    }
                    let fork_hash_hex = Self::call(|| self.rpc.get_block_hash(fork_height)).await?;
                    let fork_hash: [u8; 32] = hex::decode_exact(&fork_hash_hex)
                        .map_err(|e| IndexerError::Rpc(RpcError::Malformed(e.to_string())))?;
                    let orphaned_count = self.ledger.goldcoin_rollback_reorg(
                        fork_height,
                        fork_hash,
                        local_height,
                        local_hash,
                        now_unix(),
                    )?;
                    (
                        fork_height + 1,
                        Some(ReorgSummary {
                            fork_height,
                            old_tip_height: local_height,
                            orphaned_count,
                        }),
                    )
                }
            },
        };

        let mut blocks_indexed = 0i64;
        for height in start_height..=live_tip_height {
            self.index_block(height).await?;
            blocks_indexed += 1;
        }

        self.promote_confirming(live_tip_height).await?;

        Ok(TickOutcome::Progressed {
            blocks_indexed,
            reorg,
        })
    }

    /// Called ONLY when [`Ledger::goldcoin_chain_tip`] is `None` — a
    /// brand-new ledger with no indexed blocks. Returns the height the
    /// caller's normal forward-indexing loop (`index_block` for every
    /// height from here through the live tip, in this same tick) should
    /// start from.
    ///
    /// With no [`InitialCheckpoint`] configured, preserves the exact
    /// pre-existing behavior: start at height 0 (dev/test compatibility —
    /// requirement kept unconditionally, never gated behind any flag).
    ///
    /// With one configured, fails closed on every malformed or
    /// unverifiable input — never guesses a hash, never proceeds on a
    /// partial match, and never falls back to height 0 instead of
    /// erroring:
    /// 1. `height < 0` -> [`IndexerError::InvalidCheckpointConfig`].
    /// 2. `hash` not exactly 64 hex chars -> `InvalidCheckpointConfig`.
    /// 3. `operator_acknowledged_no_prior_deposits == false` ->
    ///    `InvalidCheckpointConfig` (this service cannot itself verify the
    ///    vault had no prior bridge deposits — Goldcoin 0.15 has no
    ///    `scantxoutset` — so it refuses to proceed without the operator
    ///    saying so explicitly).
    /// 4. `height > live_tip_height` -> [`IndexerError::CheckpointAboveTip`].
    /// 5. Live `getblockhash(height)` byte-compared against the configured
    ///    hash -> [`IndexerError::CheckpointHashMismatch`] on any
    ///    disagreement, including case-only differences (compared as
    ///    decoded bytes, not as strings).
    ///
    /// Deliberately does NOT itself ingest anything into the ledger: once
    /// verification passes, this returns `height` (not `height + 1`) so
    /// the ordinary `index_block` loop processes the checkpoint block
    /// exactly like any other block — including scanning its own
    /// transactions for vault deposits — matching this checkpoint's
    /// stated meaning precisely: everything **before** `height` is
    /// intentionally outside the bridge's supported history, not
    /// `height` itself. `index_block` is what actually writes the
    /// `goldcoin_indexed_blocks` row (with the real header's `prev_hash`/
    /// `time`, never placeholder values), so every existing
    /// reorg-detection code path (`find_fork_point`,
    /// `goldcoin_rollback_reorg`) treats it exactly like any other
    /// indexed block from the very next tick onward.
    async fn bootstrap_from_checkpoint_or_genesis(
        &mut self,
        live_tip_height: i64,
    ) -> Result<i64, IndexerError> {
        let Some(checkpoint) = self.config.initial_checkpoint.clone() else {
            return Ok(0);
        };

        if checkpoint.height < 0 {
            return Err(IndexerError::InvalidCheckpointConfig(format!(
                "initial_checkpoint_height {} is negative",
                checkpoint.height
            )));
        }
        let configured_hash: [u8; 32] = hex::decode_exact(&checkpoint.hash).map_err(|e| {
            IndexerError::InvalidCheckpointConfig(format!(
                "initial_checkpoint_hash {:?} is not exactly 32 bytes of hex: {e}",
                checkpoint.hash
            ))
        })?;
        if !checkpoint.operator_acknowledged_no_prior_deposits {
            return Err(IndexerError::InvalidCheckpointConfig(
                "initial_checkpoint_operator_acknowledged_no_prior_deposits is not set — this \
                 service cannot verify on its own that the configured vault received no bridge \
                 deposit before the checkpoint (Goldcoin 0.15 has no scantxoutset); an operator \
                 must explicitly confirm this before the checkpoint can be used"
                    .to_string(),
            ));
        }
        if checkpoint.height > live_tip_height {
            return Err(IndexerError::CheckpointAboveTip {
                height: checkpoint.height,
                tip: live_tip_height,
            });
        }

        let live_hash_hex = Self::call(|| self.rpc.get_block_hash(checkpoint.height)).await?;
        let live_hash: [u8; 32] = hex::decode_exact(&live_hash_hex)
            .map_err(|e| IndexerError::Rpc(RpcError::Malformed(e.to_string())))?;
        if live_hash != configured_hash {
            return Err(IndexerError::CheckpointHashMismatch {
                height: checkpoint.height,
                configured: checkpoint.hash.clone(),
                live: live_hash_hex,
            });
        }

        tracing::warn!(
            height = checkpoint.height,
            hash_hex = live_hash_hex,
            "Goldcoin indexer verified an operator-configured initial checkpoint — every \
             deposit before this height is intentionally outside the bridge's supported \
             history; indexing starts here"
        );

        Ok(checkpoint.height)
    }

    /// Walks backward from `from_height` comparing the locally stored hash
    /// against the live chain, returning the fork point, or `None` if no
    /// agreement is found within `max_reorg_depth` (caller must halt, never
    /// guess).
    async fn find_fork_point(&mut self, from_height: i64) -> Result<Option<i64>, IndexerError> {
        let mut h = from_height;
        loop {
            if h < 0 {
                return Ok(None);
            }
            let live_hash_hex = Self::call(|| self.rpc.get_block_hash(h)).await?;
            let live_hash: [u8; 32] = hex::decode_exact(&live_hash_hex)
                .map_err(|e| IndexerError::Rpc(RpcError::Malformed(e.to_string())))?;
            let local_hash = self.ledger.goldcoin_block_hash_at(h)?;
            if local_hash == Some(live_hash) {
                return Ok(Some(h));
            }
            let depth = from_height - (h - 1);
            if depth > self.config.max_reorg_depth as i64 {
                return Ok(None);
            }
            h -= 1;
        }
    }

    async fn index_block(&mut self, height: i64) -> Result<(), IndexerError> {
        let hash_hex = Self::call(|| self.rpc.get_block_hash(height)).await?;
        let hash: [u8; 32] = hex::decode_exact(&hash_hex)
            .map_err(|e| IndexerError::Rpc(RpcError::Malformed(e.to_string())))?;
        let header = Self::call(|| self.rpc.get_block(&hash_hex)).await?;
        let prev_hash: [u8; 32] = match &header.previousblockhash {
            Some(p) => hex::decode_exact(p)
                .map_err(|e| IndexerError::Rpc(RpcError::Malformed(e.to_string())))?,
            None => [0u8; 32],
        };

        let now = now_unix();
        // The genesis coinbase is permanently unfetchable even with
        // -txindex=1 (verified empirically by the old bridge against a real
        // node, docs/goldcoin-rpc-notes.md) — height 0 is skipped entirely.
        let is_genesis = height == 0;
        if !is_genesis {
            for txid_hex in &header.tx {
                let decoded = Self::call(|| self.rpc.get_raw_transaction(txid_hex)).await?;

                // Legacy path (old requests only): the static shared vault
                // script, attributed by decoding an OP_RETURN request-id
                // binding — unchanged from before per-request addresses
                // existed.
                let vault_outputs =
                    vault_output_candidates(&decoded, &self.config.vault_script_hex);

                // New path: per-request derived deposit addresses,
                // attributed purely by destination scriptPubKey -> request
                // mapping (`Ledger::find_goldcoin_deposit_request_by_
                // script`) — no OP_RETURN, no amount-based attribution.
                // Looked up per-output against the live ledger (not a
                // cached snapshot) so a request created mid-tick is still
                // matched, and so a rescan/restart sees exactly the same
                // mapping every time (idempotent).
                //
                // The lookup spans both Goldcoin-sourced directions, and
                // the ROUTE is a property of the row it lands on, never
                // of anything read off the chain: a deposit cannot pick
                // its own route, because the only thing it can address is
                // a script that already belongs to one specific request.
                let mut direct_matches: Vec<(u32, u64, i64)> = Vec::new();
                for v in &decoded.vout {
                    let script_hex_lower = v.script_pub_key.hex.to_lowercase();
                    if let Some((request_id, _direction)) = self
                        .ledger
                        .find_goldcoin_deposit_request_by_script(&script_hex_lower)?
                    {
                        direct_matches.push((v.n, glc_to_atomic(v.value), request_id));
                    }
                }

                if vault_outputs.is_empty() && direct_matches.is_empty() {
                    continue;
                }
                let txid: [u8; 32] = hex::decode_exact(txid_hex)
                    .map_err(|e| IndexerError::Rpc(RpcError::Malformed(e.to_string())))?;
                // The wallets this deposit was ACTUALLY funded from —
                // traced once per matched transaction, before either
                // attribution path records it, so the ledger's
                // source-wallet window is checked against chain evidence
                // and never against anything a client declared.
                let funding_wallets = self.trace_funding_wallets(&decoded).await?;

                if !vault_outputs.is_empty() {
                    let binding = extract_request_binding(&decoded);
                    for out in &vault_outputs {
                        match &binding {
                            Err(e) => {
                                if self
                                    .ledger
                                    .get_broadcast_vault_utxo_split(txid)?
                                    .is_some_and(|split| {
                                        crate::goldcoin::split::matches_expected_split_output(
                                            split.source_amount_atomic,
                                            split.fee_atomic,
                                            split.chunk_count as u64,
                                            out.vout,
                                            out.amount_atomic,
                                        )
                                    })
                                {
                                    // An internal vault-split output
                                    // (`glc-admin split-vault-utxo`), not an
                                    // unexplained deposit: every split
                                    // output pays the vault's own script
                                    // with no OP_RETURN by construction, so
                                    // it always fails the request-binding
                                    // check above — recognized here by an
                                    // EXACT match against the persisted
                                    // split plan, never a broad "this txid
                                    // is a known split" acceptance.
                                    // `vault_utxos`/reserve capacity are
                                    // already correctly populated for it by
                                    // `Orchestrator::tick_vault_utxos`
                                    // (`list_unspent`-based, independent of
                                    // this per-block scan) — nothing more
                                    // to do here than not raise a false
                                    // alarm.
                                    tracing::info!(
                                        txid_hex,
                                        vout = out.vout,
                                        "internal vault split output recognized — not recorded as unmatched"
                                    );
                                } else {
                                    tracing::warn!(txid_hex, vout = out.vout, reason = e.reason_code(), "vault payment with unusable request binding — recorded, not ignored");
                                    self.ledger.record_unmatched_goldcoin_deposit(
                                        txid,
                                        out.vout,
                                        out.amount_atomic,
                                        height,
                                        e.reason_code(),
                                        now,
                                    )?;
                                }
                            }
                            Ok(request_id) => {
                                self.observe_glc_deposit(
                                    *request_id,
                                    txid,
                                    txid_hex,
                                    out.vout,
                                    out.amount_atomic,
                                    height,
                                    hash,
                                    &funding_wallets,
                                    now,
                                )?;
                            }
                        }
                    }
                }

                for (vout, amount_atomic, request_id) in direct_matches {
                    self.observe_glc_deposit(
                        request_id,
                        txid,
                        txid_hex,
                        vout,
                        amount_atomic,
                        height,
                        hash,
                        &funding_wallets,
                        now,
                    )?;
                }
            }
        }

        self.ledger
            .goldcoin_ingest_block(height, hash, prev_hash, header.time, now)?;
        Ok(())
    }

    /// The distinct wallets a deposit transaction was funded from, in
    /// input order: for each input that spends a real previous output,
    /// the prevout's scriptPubKey is fetched (one `getrawtransaction` per
    /// input) and spelled as the address text it pays on this network
    /// when it is a standard P2PKH/P2SH script, or kept as the raw script
    /// bytes when it is not — so every input still keys a window on
    /// itself, and the key a user can be asked about is the key a
    /// standard-address deposit consumes. A coinbase input, or one the
    /// node did not fully report, contributes nothing (a miner funding a
    /// deposit straight from a coinbase has no wallet to trace); an RPC
    /// failure fails the tick, exactly as every other read in
    /// [`Indexer::index_block`] does, so a deposit is never recorded with
    /// a source the node could not confirm.
    ///
    /// This is the enforcing source identity for the rolling-24h wallet
    /// window on the Goldcoin-sourced routes
    /// (`Ledger::record_glc_deposit_observed_from`): chain evidence,
    /// never a client claim. Deliberately NOT the refund path's
    /// single-input rule — a refund must name ONE unambiguous sender, but
    /// a window check must see EVERY wallet that took part.
    async fn trace_funding_wallets(
        &self,
        deposit: &DecodedTransaction,
    ) -> Result<Vec<Vec<u8>>, IndexerError> {
        let mut wallets: Vec<Vec<u8>> = Vec::new();
        for input in &deposit.vin {
            let Some((prev_txid_hex, prev_vout)) = input.prevout() else {
                continue;
            };
            let prev = Self::call(|| self.rpc.get_raw_transaction(prev_txid_hex)).await?;
            if prev.txid != prev_txid_hex {
                return Err(IndexerError::Rpc(RpcError::Malformed(format!(
                    "asked for prevout parent {prev_txid_hex}, node returned {}",
                    prev.txid
                ))));
            }
            let Some(prev_out) = prev.vout.iter().find(|v| v.n == prev_vout) else {
                return Err(IndexerError::Rpc(RpcError::Malformed(format!(
                    "prevout parent {prev_txid_hex} has no output {prev_vout}"
                ))));
            };
            let script_hex = &prev_out.script_pub_key.hex;
            let wallet =
                match super::address::address_from_script_hex(script_hex, self.config.network) {
                    Some(address) => address.into_bytes(),
                    None => hex::decode_vec(script_hex)
                        .map_err(|e| IndexerError::Rpc(RpcError::Malformed(e.to_string())))?,
                };
            if !wallet.is_empty() && !wallets.contains(&wallet) {
                wallets.push(wallet);
            }
        }
        Ok(wallets)
    }

    /// Shared by both attribution paths in [`Indexer::index_block`] — the
    /// legacy static-vault + OP_RETURN path and the per-request
    /// derived-address path — once each has independently resolved a
    /// `request_id` for a candidate output. From here on the two paths are
    /// identical: [`Ledger::record_glc_deposit_observed_from`] is
    /// completely agnostic to how `request_id` was resolved.
    #[allow(clippy::too_many_arguments)]
    fn observe_glc_deposit(
        &mut self,
        request_id: i64,
        txid: [u8; 32],
        txid_hex: &str,
        vout: u32,
        amount_atomic: u64,
        height: i64,
        hash: [u8; 32],
        funding_wallets: &[Vec<u8>],
        now: i64,
    ) -> Result<(), IndexerError> {
        let outcome = self.ledger.record_glc_deposit_observed_from(
            request_id,
            txid,
            vout,
            amount_atomic,
            height,
            hash,
            funding_wallets,
            now,
        )?;
        match outcome {
            GlcObservationOutcome::Recorded => {
                tracing::info!(
                    request_id,
                    txid_hex,
                    vout,
                    "deposit observed, now confirming"
                );
            }
            GlcObservationOutcome::AlreadyRecorded => {}
            GlcObservationOutcome::LateDepositRecreated => {
                tracing::warn!(
                    request_id,
                    txid_hex,
                    vout,
                    "deposit observed against an Expired reservation — capacity was available, reservation auto-recreated, now confirming"
                );
            }
            GlcObservationOutcome::LateDepositNoCapacity => {
                tracing::error!(
                    request_id,
                    txid_hex,
                    vout,
                    "deposit observed against an Expired reservation with no capacity remaining to re-reserve — routed to ManualReview"
                );
            }
            GlcObservationOutcome::AmountMismatch { expected, observed } => {
                tracing::warn!(
                    request_id,
                    expected,
                    observed,
                    "deposit amount mismatch — routed to ManualReview"
                );
            }
            GlcObservationOutcome::WalletLimited {
                reason,
                retry_after,
            } => {
                tracing::warn!(
                    request_id,
                    txid_hex,
                    vout,
                    reason,
                    retry_after,
                    "deposit funded from, or destined for, a wallet still inside its rolling \
                     24-hour window — recorded and routed to ManualReview, no payout"
                );
            }
            GlcObservationOutcome::RapidBurstHeld { rule, review_after } => {
                tracing::warn!(
                    request_id,
                    txid_hex,
                    vout,
                    rule = rule.as_str(),
                    review_after,
                    "deposit matched the rapid-burst rule — recorded and HELD in ManualReview \
                     for an explicit operator decision, no payout"
                );
            }
            GlcObservationOutcome::NoMatchingRequest => {
                tracing::warn!(
                    request_id,
                    txid_hex,
                    vout,
                    "no matching AwaitingDeposit request — recorded as unmatched"
                );
                self.ledger.record_unmatched_goldcoin_deposit(
                    txid,
                    vout,
                    amount_atomic,
                    height,
                    "no_matching_request",
                    now,
                )?;
            }
        }
        Ok(())
    }

    /// Re-evaluates every `Confirming` request against the current tip:
    /// promotes to `SourceFinalized` once depth is reached and the vault
    /// output is confirmed still unspent, or leaves it in `Confirming` for
    /// a later tick.
    ///
    /// Sweeps every Goldcoin-SOURCED direction, not just `GlcToSol`. The
    /// confirmation rule is a property of the Goldcoin chain, not of
    /// where the value is going: `self.config.confirmation_depth`, the
    /// live `get_tx_out_confirmed` re-check, and the fail-closed
    /// `ManualReview` route for an output spent before finality are the
    /// SAME for a `GlcToRhn` deposit as for a `GlcToSol` one, because it
    /// is the same deposit sitting at the same kind of derived address.
    /// What differs afterwards — which reserve is drawn down, which
    /// engine settles it — is decided by the request's own direction,
    /// downstream of here.
    async fn promote_confirming(&mut self, tip_height: i64) -> Result<(), IndexerError> {
        let mut confirming = Vec::new();
        for direction in crate::ledger::Direction::ALL
            .into_iter()
            .filter(|d| d.source_is_goldcoin())
        {
            confirming.extend(
                self.ledger
                    .requests_by_state(direction, crate::ledger::RequestState::Confirming)?,
            );
        }
        for row in confirming {
            let Some(block_height) = row.source_block_height else {
                continue;
            };
            let depth = tip_height - block_height + 1;
            self.ledger.update_glc_confirmations(row.id, depth)?;
            if depth < self.config.confirmation_depth as i64 {
                continue;
            }

            let (Some(txid), Some(vout)) = (row.source_txid, row.source_vout) else {
                continue;
            };
            let txid_hex = hex::encode(&txid);
            let still_unspent = Self::call(|| self.rpc.get_tx_out_confirmed(&txid_hex, vout))
                .await?
                .is_some();
            if !still_unspent {
                // Anomalous (not a routine reorg — that path is
                // find_fork_point/rollback): fail closed to ManualReview
                // rather than warning and leaving the request stranded in
                // `Confirming` forever with no operator-visible terminal
                // state and its reservation held indefinitely.
                self.ledger
                    .mark_glc_deposit_spent_before_finalized(row.id, now_unix())?;
                tracing::warn!(
                    request_id = row.id,
                    txid_hex,
                    vout,
                    "vault output spent before reaching SourceFinalized — routed to ManualReview"
                );
                continue;
            }

            self.ledger.mark_glc_source_finalized(row.id, now_unix())?;
            tracing::info!(
                request_id = row.id,
                direction = row.direction.as_str(),
                txid_hex,
                vout,
                gross_amount = row.gross_amount_atomic,
                net_amount = row.net_amount_atomic,
                "source finalized"
            );
        }
        Ok(())
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }
}

#[cfg(test)]
mod tests;
