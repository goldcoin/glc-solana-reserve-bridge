use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::signature::Keypair;
use solana_sdk::transaction::Transaction as SolanaTx;

use super::*;
use crate::goldcoin::coin::VaultUtxo;
use crate::goldcoin::indexer::IndexerConfig;
use crate::goldcoin::rpc::{BlockHeader, DecodedTransaction, RpcError};
use crate::ledger::{CreateRequestOutcome, ResumeManualReviewOutcome, SolFoldOutcome};
use crate::signing::attestation::DevAttestationSigner;
use crate::signing::goldcoin_vault::DevVaultSigner;

// -------------------------------------------------------------- mock RPCs --

#[derive(Default)]
struct FakeGoldcoinChain {
    /// Present once a payout tx has been "mined": txid_hex -> confirmations.
    mined: HashMap<String, i64>,
    broadcasts: Vec<String>,
    /// Matches whatever height/hash a test seeded directly into the shared
    /// ledger via `Ledger::goldcoin_ingest_block` (to fake a chain tip for
    /// payout-confirmation polling) — the goldcoin indexer, sharing that
    /// same on-disk ledger, will otherwise try to verify that tip against
    /// this mock via `find_fork_point` and needs to find it here too.
    known_tip: Option<(i64, String)>,
    /// What `list_unspent` reports — empty unless a test explicitly seeds
    /// it via `set_unspent`, matching the real chain read the orchestrator's
    /// `tick_vault_utxos` phase now performs every tick.
    unspent: Vec<crate::goldcoin::rpc::ListUnspentEntry>,
    /// Full transactions, for the few phases that inspect more than a
    /// confirmation count (GlcToSol refund reconciliation verifies the
    /// confirmed transaction's inputs and outputs against stored
    /// evidence). Takes precedence over `mined`; when a txid is absent
    /// here the mock falls back to the depth-only stub every other phase
    /// relies on.
    raw_txs: HashMap<String, DecodedTransaction>,
}

struct MockGoldcoinRpc {
    chain: Mutex<FakeGoldcoinChain>,
}

impl MockGoldcoinRpc {
    fn new() -> Self {
        MockGoldcoinRpc {
            chain: Mutex::new(FakeGoldcoinChain::default()),
        }
    }

    fn set_confirmations(&self, txid_hex: &str, confirmations: i64) {
        self.chain
            .lock()
            .unwrap()
            .mined
            .insert(txid_hex.to_string(), confirmations);
    }

    fn set_known_tip(&self, height: i64, hash_hex: String) {
        self.chain.lock().unwrap().known_tip = Some((height, hash_hex));
    }

    fn set_unspent(&self, entries: Vec<crate::goldcoin::rpc::ListUnspentEntry>) {
        self.chain.lock().unwrap().unspent = entries;
    }

    /// Serves a complete transaction for `txid_hex`, rather than the
    /// depth-only stub.
    fn set_raw_transaction(&self, txid_hex: &str, tx: DecodedTransaction) {
        self.chain
            .lock()
            .unwrap()
            .raw_txs
            .insert(txid_hex.to_string(), tx);
    }
}

impl GoldcoinRpc for MockGoldcoinRpc {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        Ok(-1) // empty chain: the indexer tick is a harmless no-op
    }
    async fn get_block_hash(&self, height: i64) -> Result<String, RpcError> {
        match &self.chain.lock().unwrap().known_tip {
            Some((h, hash)) if *h == height => Ok(hash.clone()),
            _ => Err(RpcError::Method {
                code: -8,
                message: "height out of range".into(),
            }),
        }
    }
    async fn get_block(&self, _hash: &str) -> Result<BlockHeader, RpcError> {
        unimplemented!("no blocks in this mock chain")
    }
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        let chain = self.chain.lock().unwrap();
        if let Some(tx) = chain.raw_txs.get(txid_hex) {
            return Ok(tx.clone());
        }
        let confirmations = chain.mined.get(txid_hex).copied();
        Ok(DecodedTransaction {
            vin: Vec::new(),
            txid: txid_hex.to_string(),
            vout: Vec::new(),
            confirmations,
        })
    }
    async fn get_tx_out_confirmed(
        &self,
        _txid_hex: &str,
        _vout: u32,
    ) -> Result<Option<crate::goldcoin::rpc::TxOut>, RpcError> {
        unimplemented!("not exercised by orchestrator tests")
    }
    async fn send_raw_transaction(
        &self,
        hex: &str,
    ) -> Result<crate::goldcoin::rpc::BroadcastOutcome, RpcError> {
        self.chain.lock().unwrap().broadcasts.push(hex.to_string());
        Ok(crate::goldcoin::rpc::BroadcastOutcome::Accepted {
            txid: "mock".to_string(),
        })
    }
    async fn list_unspent(
        &self,
        _min_conf: i64,
        _addresses: &[String],
    ) -> Result<Vec<crate::goldcoin::rpc::ListUnspentEntry>, RpcError> {
        Ok(self.chain.lock().unwrap().unspent.clone())
    }
}

impl GoldcoinRpc for Arc<MockGoldcoinRpc> {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        MockGoldcoinRpc::get_block_count(self).await
    }
    async fn get_block_hash(&self, height: i64) -> Result<String, RpcError> {
        MockGoldcoinRpc::get_block_hash(self, height).await
    }
    async fn get_block(&self, hash: &str) -> Result<BlockHeader, RpcError> {
        MockGoldcoinRpc::get_block(self, hash).await
    }
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        MockGoldcoinRpc::get_raw_transaction(self, txid_hex).await
    }
    async fn get_tx_out_confirmed(
        &self,
        txid_hex: &str,
        vout: u32,
    ) -> Result<Option<crate::goldcoin::rpc::TxOut>, RpcError> {
        MockGoldcoinRpc::get_tx_out_confirmed(self, txid_hex, vout).await
    }
    async fn send_raw_transaction(
        &self,
        hex: &str,
    ) -> Result<crate::goldcoin::rpc::BroadcastOutcome, RpcError> {
        MockGoldcoinRpc::send_raw_transaction(self, hex).await
    }
    async fn list_unspent(
        &self,
        min_conf: i64,
        addresses: &[String],
    ) -> Result<Vec<crate::goldcoin::rpc::ListUnspentEntry>, RpcError> {
        MockGoldcoinRpc::list_unspent(self, min_conf, addresses).await
    }
}

#[derive(Default)]
struct MockSolanaRpc {
    accounts: Mutex<HashMap<Pubkey, Vec<u8>>>,
    statuses: Mutex<HashMap<Signature, Result<(), String>>>,
    sent: Mutex<Vec<SolanaTx>>,
    /// How many upcoming `send_transaction` calls fail with a transport
    /// error (the transaction is NOT recorded as sent) — for retry tests.
    fail_sends: Mutex<u32>,
}

impl MockSolanaRpc {
    fn new() -> Self {
        MockSolanaRpc::default()
    }

    fn set_account(&self, pubkey: Pubkey, data: Vec<u8>) {
        self.accounts.lock().unwrap().insert(pubkey, data);
    }

    fn set_status(&self, signature: Signature, status: Result<(), String>) {
        self.statuses.lock().unwrap().insert(signature, status);
    }

    fn fail_next_sends(&self, n: u32) {
        *self.fail_sends.lock().unwrap() = n;
    }
}

impl SolanaRpc for MockSolanaRpc {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        Ok(self
            .accounts
            .lock()
            .unwrap()
            .get(pubkey)
            .cloned()
            .map(|data| Account {
                lamports: 1,
                data,
                owner: accounts::PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            }))
    }
    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        let stored_accounts = self.accounts.lock().unwrap();
        Ok(pubkeys
            .iter()
            .map(|pk| {
                stored_accounts.get(pk).cloned().map(|data| Account {
                    lamports: 1,
                    data,
                    owner: accounts::PROGRAM_ID,
                    executable: false,
                    rent_epoch: 0,
                })
            })
            .collect())
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        Ok(1) // the solana indexer's own progress tracking; not asserted on by these tests
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        Ok(Hash::new_unique())
    }
    async fn send_transaction(&self, tx: &SolanaTx) -> Result<Signature, SolanaRpcError> {
        {
            let mut fail = self.fail_sends.lock().unwrap();
            if *fail > 0 {
                *fail -= 1;
                return Err(SolanaRpcError::Transport("injected send failure".into()));
            }
        }
        let signature = tx.signatures[0];
        self.sent.lock().unwrap().push(tx.clone());
        Ok(signature)
    }
    async fn simulate_transaction(
        &self,
        _tx: &SolanaTx,
    ) -> Result<crate::solana::rpc::SimulationOutcome, SolanaRpcError> {
        unimplemented!("not exercised by orchestrator tests")
    }
    async fn get_signature_status(
        &self,
        signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        Ok(self.statuses.lock().unwrap().get(signature).cloned())
    }
    async fn is_blockhash_valid(&self, _blockhash: &Hash) -> Result<bool, SolanaRpcError> {
        unimplemented!("not exercised by orchestrator tests")
    }
}

impl SolanaRpc for Arc<MockSolanaRpc> {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        MockSolanaRpc::get_account(self, pubkey).await
    }
    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        MockSolanaRpc::get_multiple_accounts(self, pubkeys).await
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        MockSolanaRpc::get_slot(self).await
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        MockSolanaRpc::get_latest_blockhash(self).await
    }
    async fn send_transaction(&self, tx: &SolanaTx) -> Result<Signature, SolanaRpcError> {
        MockSolanaRpc::send_transaction(self, tx).await
    }
    async fn simulate_transaction(
        &self,
        tx: &SolanaTx,
    ) -> Result<crate::solana::rpc::SimulationOutcome, SolanaRpcError> {
        MockSolanaRpc::simulate_transaction(self, tx).await
    }
    async fn get_signature_status(
        &self,
        signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        MockSolanaRpc::get_signature_status(self, signature).await
    }
    async fn is_blockhash_valid(&self, blockhash: &Hash) -> Result<bool, SolanaRpcError> {
        MockSolanaRpc::is_blockhash_valid(self, blockhash).await
    }
}

// -------------------------------------------------------------- fixtures --

fn fake_attestation_key_set_bytes(epoch: u64, threshold: u8, keys: &[Pubkey]) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.extend_from_slice(&epoch.to_le_bytes());
    v.push(threshold);
    v.push(1);
    v.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for k in keys {
        v.extend_from_slice(k.as_ref());
    }
    v.extend_from_slice(&[0u8; 32]);
    v
}

fn fake_bridge_config_bytes(reserve_token_mint: [u8; 32], obligation_count: u64) -> Vec<u8> {
    fake_bridge_config_bytes_with_token_program(reserve_token_mint, obligation_count, spl_token::ID)
}

fn fake_bridge_config_bytes_with_token_program(
    reserve_token_mint: [u8; 32],
    obligation_count: u64,
    reserve_token_program: Pubkey,
) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.push(1); // protocol_version
    v.extend_from_slice(&[0u8; 32]); // admin
    v.push(0); // pending_admin tag (None) — Borsh variable-length: no payload bytes follow
    v.push(0);
    v.push(0);
    v.push(0);
    v.push(7);
    v.extend_from_slice(&reserve_token_mint);
    v.extend_from_slice(reserve_token_program.as_ref()); // reserve_token_program
    v.push(3);
    v.extend_from_slice(&obligation_count.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes());
    v.extend_from_slice(&100u64.to_le_bytes());
    v.extend_from_slice(&1_000_000u64.to_le_bytes());
    v.extend_from_slice(&500u64.to_le_bytes());
    v.extend_from_slice(&2_000_000u64.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes());
    v
}

/// Matches the canonical Solana GLC mint's live decimals (docs/18-token-
/// 2022-support.md); registered as a fake mint account wherever a test
/// exercises a path that now reads decimals live (release building,
/// Goldcoin payout building).
const TEST_SOLANA_DECIMALS: u8 = 6;

/// A minimal, real 82-byte `spl_token::state::Mint`-shaped buffer — see
/// the matching helper in `signing::attestation::tests`.
fn fake_mint_bytes(decimals: u8) -> Vec<u8> {
    let mut v = vec![0u8; 82];
    v[44] = decimals;
    v[45] = 1; // is_initialized
    v
}

fn fake_withdrawal_obligation_bytes(
    index: u64,
    amount: u64,
    requester: &[u8; 32],
    glc_address: &[u8],
) -> Vec<u8> {
    fake_withdrawal_obligation_bytes_with_status(index, amount, requester, glc_address, 0)
}

fn fake_withdrawal_obligation_bytes_with_status(
    index: u64,
    amount: u64,
    requester: &[u8; 32],
    glc_address: &[u8],
    status: u8,
) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.extend_from_slice(&index.to_le_bytes());
    v.extend_from_slice(&amount.to_le_bytes());
    v.extend_from_slice(requester);
    let mut addr = [0u8; 64];
    addr[..glc_address.len()].copy_from_slice(glc_address);
    v.extend_from_slice(&addr);
    v.push(glc_address.len() as u8);
    v.push(status);
    v.extend_from_slice(&11u64.to_le_bytes());
    v.push(1);
    v.push(2);
    v.extend_from_slice(&[0u8; 48]);
    v
}

fn fake_token_account_bytes(amount: u64) -> Vec<u8> {
    let mut v = vec![0u8; 72];
    v[64..72].copy_from_slice(&amount.to_le_bytes());
    v
}

pub(crate) fn attestation_signers() -> Vec<Box<dyn AttestationSigner>> {
    vec![
        Box::new(DevAttestationSigner::generate()),
        Box::new(DevAttestationSigner::generate()),
        Box::new(DevAttestationSigner::generate()),
    ]
}

pub(crate) fn vault_and_signers() -> (MultisigVault, Vec<Box<dyn VaultSigner>>) {
    let signers = vec![
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
    ];
    let vault = MultisigVault::new(
        signers.iter().map(|s| s.pubkey).collect(),
        2,
        Network::Testnet,
    )
    .unwrap();
    let boxed: Vec<Box<dyn VaultSigner>> = signers
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn VaultSigner>)
        .collect();
    (vault, boxed)
}

pub(crate) fn base_config() -> OrchestratorConfig {
    OrchestratorConfig {
        attestation_threshold: 2,
        vault_threshold: 2,
        required_goldcoin_confirmations: 6,
        fee_rate_per_kb: 1000,
        dust_threshold: 1000,
        max_inputs: 10,
        change_fanout_target_atomic: 2_500 * 100_000_000,
        change_fanout_max_outputs: 10,
        zero_conf_change_max_depth: 1,
        zero_conf_change_mode: crate::goldcoin::payout::ZeroConfChangeMode::DepthLimited,
        zero_conf_change_recursive_chain_limit: 20,
        reconciliation_tolerance: 0,
        vault_min_confirmations: 1,
        goldcoin_network: Network::Testnet,
        signer_timeout: std::time::Duration::from_secs(5),
        max_auto_resumes_per_tick: 20,
        // Shaping disabled in the shared base config: these tests pin the
        // exact per-phase behavior of everything else, and must not gain
        // an extra self-transaction mid-scenario. `goldcoin::liquidity`
        // has its own dedicated coverage
        // (service/tests/utxo_liquidity_autoshaping.rs).
        utxo_shaping_enabled: false,
        utxo_shaping_target_available_count: 15,
        utxo_shaping_min_source_atomic: 4 * 2_500 * 100_000_000,
        utxo_shaping_max_outputs_per_split: 25,
    }
}

pub(crate) fn indexer_config() -> IndexerConfig {
    IndexerConfig {
        vault_script_hex: "51".to_string(),
        confirmation_depth: 6,
        max_reorg_depth: 6,
        initial_checkpoint: None,
        network: crate::goldcoin::address::Network::Testnet,
    }
}

/// Opens three independent connections onto the SAME database file — the
/// same "concurrent operators" concurrency model this ledger is designed
/// for (WAL mode + `BEGIN IMMEDIATE` transactions is the real safety
/// boundary, not a single shared in-process handle — see
/// `docs/06-schema.md`), used here in-process so the indexers' writes and
/// the orchestrator's own reads/writes are genuinely visible to each
/// other, exactly as they must be across real, separate processes in
/// production. Using `Ledger::open_in_memory` for more than one of these
/// would silently give each component an isolated database and mask
/// exactly this class of wiring bug.
#[allow(clippy::too_many_arguments)]
fn build_orchestrator(
    db_path: &std::path::Path,
    goldcoin_rpc: Arc<MockGoldcoinRpc>,
    solana_rpc: Arc<MockSolanaRpc>,
    vault: MultisigVault,
    vault_signers: Vec<Box<dyn VaultSigner>>,
    attestation_signers: Vec<Box<dyn AttestationSigner>>,
) -> Orchestrator<Arc<MockGoldcoinRpc>, Arc<MockSolanaRpc>> {
    let goldcoin_indexer = Indexer::new(
        Arc::clone(&goldcoin_rpc),
        Ledger::open(db_path).unwrap(),
        indexer_config(),
    );
    let solana_indexer = SolanaIndexer::new(
        Arc::clone(&solana_rpc),
        Ledger::open(db_path).unwrap(),
        crate::amount_conversion::BRIDGE_FEE_BPS,
    )
    .with_source_minimum_for_tests(crate::amount_conversion::CanonicalAtomic(1));
    let ledger = Ledger::open(db_path).unwrap();
    Orchestrator::new(
        goldcoin_indexer,
        solana_indexer,
        ledger,
        goldcoin_rpc,
        solana_rpc,
        vault,
        vault_signers,
        attestation_signers,
        Keypair::new(),
        base_config(),
        0,
    )
    .with_source_minimum_for_tests(crate::amount_conversion::CanonicalAtomic(1))
}

/// Real gross/fee/net breakdown for a GlcToSol request, matching what
/// `api::create_goldcoin_deposit_transfer` computes (docs/20-bridge-fee.md) —
/// needed here (not the zero-fee shortcut `ledger::tests` uses) because
/// the orchestrator's own attestation path recomputes and strictly
/// verifies the fee against `amount_conversion::BRIDGE_FEE_BPS`.
fn glc_to_sol_amounts(gross: u64, solana_decimals: u8) -> crate::ledger::RequestAmounts {
    let fb =
        crate::amount_conversion::compute_fee(crate::amount_conversion::CanonicalAtomic(gross))
            .unwrap();
    let net_destination = fb.net.to_solana(solana_decimals).unwrap();
    crate::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: net_destination.0,
    }
}

/// [`glc_to_sol_amounts`] at an explicit HISTORICAL fee rate — what a
/// request created under an earlier fee policy actually has persisted
/// (`bridge_requests.fee_bps` snapshot; the fee-policy-snapshot
/// regression tests below).
fn glc_to_sol_amounts_at_bps(
    gross: u64,
    solana_decimals: u8,
    fee_bps: u64,
) -> crate::ledger::RequestAmounts {
    let fb = crate::amount_conversion::compute_fee_at_bps(
        crate::amount_conversion::CanonicalAtomic(gross),
        fee_bps,
    )
    .unwrap();
    let net_destination = fb.net.to_solana(solana_decimals).unwrap();
    crate::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: net_destination.0,
    }
}

/// [`sol_to_glc_amounts`] at an explicit HISTORICAL fee rate — see
/// [`glc_to_sol_amounts_at_bps`].
fn sol_to_glc_amounts_at_bps(
    amount: u64,
    solana_decimals: u8,
    fee_bps: u64,
) -> crate::ledger::RequestAmounts {
    let gross_canonical = crate::amount_conversion::SolanaAtomic(amount)
        .to_canonical(solana_decimals)
        .unwrap();
    let fb = crate::amount_conversion::compute_fee_at_bps(gross_canonical, fee_bps).unwrap();
    crate::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
    }
}

/// Real gross/fee/net breakdown for a SolToGlc obligation, matching what
/// `solana::indexer::tick` computes (docs/20-bridge-fee.md).
fn sol_to_glc_amounts(amount: u64, solana_decimals: u8) -> crate::ledger::RequestAmounts {
    let gross_canonical = crate::amount_conversion::SolanaAtomic(amount)
        .to_canonical(solana_decimals)
        .unwrap();
    let fb = crate::amount_conversion::compute_fee(gross_canonical).unwrap();
    crate::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
    }
}

fn configure_both_reserves(ledger: &mut Ledger) {
    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        ledger
            .configure_reserve(direction, 10_000_000, 0, 5_000_000, 2_000_000, 1_000_000, 0)
            .unwrap();
    }
}

// ----------------------------------------------------------------- tests --

#[tokio::test]
async fn glc_to_sol_release_settles_across_two_ticks() {
    let mint = [7u8; 32];
    let recipient = [9u8; 32];

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(
                Direction::GlcToSol,
                glc_to_sol_amounts(500_000, TEST_SOLANA_DECIMALS),
                &recipient,
                None,
                3600,
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .record_glc_deposit_observed(request_id, [0xAAu8; 32], 2, 500_000, 10, [0u8; 32], 0)
            .unwrap();
        ledger.mark_glc_source_finalized(request_id, 0).unwrap();
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            5,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );

    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    let report = orchestrator.tick(10).await;
    assert_eq!(report.releases_submitted, 1, "errors: {:?}", report.errors);
    assert!(!orchestrator.goldcoin_indexer_status().is_halted());
    assert_eq!(orchestrator.goldcoin_indexer_status().last_tick_unix(), 10);
    assert_eq!(orchestrator.solana_indexer_status().last_tick_unix(), 10);
    let records = orchestrator.ledger().all_attestation_records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].request_id, request_id);
    assert_eq!(records[0].action_type, "release");
    assert_eq!(
        records[0].message_hash,
        Sha256::digest(&records[0].canonical_message).to_vec()
    );
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::DestinationSubmitted
    );

    let destination_txid = orchestrator
        .ledger()
        .get_destination_txid(request_id)
        .unwrap()
        .unwrap();
    let signature = Signature::from(<[u8; 64]>::try_from(destination_txid).unwrap());
    solana_rpc.set_status(signature, Ok(()));

    let report = orchestrator.tick(20).await;
    assert_eq!(report.releases_confirmed, 1, "errors: {:?}", report.errors);
    let req = orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(req.state, RequestState::Settled);
    // Settled liquidity is the NET destination payout, after the 3% bridge
    // fee (docs/20-bridge-fee.md): 500_000 gross - 15_000 fee = 485_000
    // canonical, /100 to the reserve mint's 6-decimal precision = 4_850.
    assert_eq!(
        orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::SolanaReserve)
            .unwrap(),
        4_850
    );
}

#[tokio::test]
async fn sol_to_glc_payout_settles_across_three_ticks() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let mint = [7u8; 32];
    // `fold_sol_deposit`'s 500_000 is Solana-native (6 decimals); the real
    // Goldcoin payout `rederive_plan` builds converts it to Goldcoin-native
    // canonical and takes the 3% bridge fee (docs/20-bridge-fee.md):
    // 500_000 * 100 = 50_000_000 gross, minus 1_500_000 fee = 48_500_000 net.
    // Fund the vault (and configure the GoldcoinReserve balance/
    // reconciliation fixture) with enough real Goldcoin-atomic headroom to
    // cover that net payout.
    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let utxo_amount = goldcoin_payout_atomic + 100_000;
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: utxo_amount,
            script_pubkey_hex: vault.script_pubkey_hex(),
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
            .unwrap();
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                dest_addr.as_bytes(),
                None,
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .goldcoin_ingest_block(100, [1u8; 32], [0u8; 32], 1000, 0)
            .unwrap();
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    goldcoin_rpc.set_known_tip(100, crate::goldcoin::hex::encode(&[1u8; 32]));
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault.script_pubkey_hex(),
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            9,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    solana_rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_withdrawal_obligation_bytes(0, 500_000, &[5u8; 32], dest_addr.as_bytes()),
    );

    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::clone(&goldcoin_rpc),
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    // Tick 1: build, independently sign, and broadcast the payout.
    let report = orchestrator.tick(10).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
    let txid = payout.txid.unwrap();

    // Tick 2: the payout mines with enough confirmations, and — within the
    // same sweep — the now-Confirmed payout's completion is independently
    // attested and submitted to Solana.
    goldcoin_rpc.set_confirmations(&crate::goldcoin::hex::encode(&txid), 6);
    let report = orchestrator.tick(20).await;
    assert_eq!(report.payouts_confirmed, 1, "errors: {:?}", report.errors);
    assert_eq!(
        report.completions_submitted, 1,
        "errors: {:?}",
        report.errors
    );
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Confirmed");
    let records = orchestrator.ledger().all_attestation_records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].action_type, "completion");
    assert_eq!(payout.mined_height, Some(95)); // tip 100 - confirmations 6 + 1
    let completion_sig = Signature::from(payout.onchain_completion_signature.unwrap());
    solana_rpc.set_status(completion_sig, Ok(()));

    // Tick 3: the completion confirms and the request settles.
    let report = orchestrator.tick(30).await;
    assert_eq!(
        report.completions_confirmed, 1,
        "errors: {:?}",
        report.errors
    );
    let req = orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(req.state, RequestState::Settled);
    // Settled liquidity is the NET Goldcoin-native destination payout,
    // after the 3% bridge fee (docs/20-bridge-fee.md).
    assert_eq!(
        orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        goldcoin_payout_atomic
    );
}

// --------------------------------- recipient-ATA provisioning regressions --
//
// Production bug: `bridge_requests.recipient` stores the recipient's
// OWNER pubkey; the release moves funds to that owner's canonical ATA,
// which the on-chain program requires to ALREADY exist (deliberately no
// `init_if_needed`). Nothing service-side ever created it, so a
// recipient without an ATA failed every release attempt with Anchor 3012
// AccountNotInitialized, forever. The fix prepends the Associated Token
// Program's idempotent canonical-ATA creation to the SAME release
// transaction: atomic with the release, a no-op when the ATA already
// exists, fail-closed on a non-canonical occupant, and retry-safe by
// construction.

/// Shared GlcToSol release fixture, parameterized on the reserve's token
/// program (legacy SPL Token vs Token-2022) so the ATA-derivation tests
/// exercise the real program-id-aware path.
async fn release_fixture(
    token_program: Pubkey,
) -> (
    Arc<MockSolanaRpc>,
    Orchestrator<Arc<MockGoldcoinRpc>, Arc<MockSolanaRpc>>,
    i64,
    Pubkey,
    Pubkey,
    tempfile::TempDir,
) {
    let mint = [7u8; 32];
    let recipient = Pubkey::new_unique();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(
                Direction::GlcToSol,
                glc_to_sol_amounts(500_000, TEST_SOLANA_DECIMALS),
                &recipient.to_bytes(),
                None,
                3600,
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .record_glc_deposit_observed(request_id, [0xAAu8; 32], 2, 500_000, 10, [0u8; 32], 0)
            .unwrap();
        ledger.mark_glc_source_finalized(request_id, 0).unwrap();
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            5,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes_with_token_program(mint, 0, token_program),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );

    let (vault, vault_signers) = vault_and_signers();
    let orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );
    (
        solana_rpc,
        orchestrator,
        request_id,
        recipient,
        Pubkey::new_from_array(mint),
        dir,
    )
}

/// Resolves a sent transaction's instructions to `(program_id, accounts)`
/// so tests can assert on real composition rather than counting bytes.
fn decompiled(tx: &SolanaTx) -> Vec<(Pubkey, Vec<Pubkey>)> {
    let keys = &tx.message.account_keys;
    tx.message
        .instructions
        .iter()
        .map(|ix| {
            (
                keys[ix.program_id_index as usize],
                ix.accounts.iter().map(|&i| keys[i as usize]).collect(),
            )
        })
        .collect()
}

/// The release transaction must be exactly: (0) idempotent canonical-ATA
/// creation via the Associated Token Program, (1) the ed25519 proof, (2)
/// `release_from_reserve` — the proof still immediately precedes the
/// release (the program checks relative -1), and BOTH the creation and
/// the release reference the SAME canonically derived ATA, never an
/// arbitrary token account. Because the creation is idempotent, this one
/// composition covers both the missing-ATA recipient (account gets
/// created, atomically with the funds landing) and the existing-ATA
/// recipient (no-op; the release proceeds exactly as before the fix —
/// also proven by the unchanged pre-existing happy-path test above).
#[tokio::test]
async fn release_transaction_provisions_the_canonical_recipient_ata_idempotently() {
    let (solana_rpc, mut orchestrator, request_id, recipient, mint, _dir) =
        release_fixture(spl_token::ID).await;

    let report = orchestrator.tick(10).await;
    assert_eq!(report.releases_submitted, 1, "errors: {:?}", report.errors);

    {
        let sent = solana_rpc.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let ixs = decompiled(&sent[0]);
        assert_eq!(ixs.len(), 3, "create-ATA + proof + release, nothing else");

        let canonical_ata = accounts::associated_token_address(&recipient, &mint, &spl_token::ID);
        // (0) idempotent creation of exactly the canonical ATA, rent
        // funded by the submitter (the transaction fee payer).
        assert_eq!(ixs[0].0, spl_associated_token_account::ID);
        assert_eq!(
            ixs[0].1[0], sent[0].message.account_keys[0],
            "the submitter/fee payer funds the rent"
        );
        assert_eq!(ixs[0].1[1], canonical_ata);
        assert_eq!(ixs[0].1[2], recipient);
        assert_eq!(ixs[0].1[3], mint);
        // (1) the proof immediately precedes (2) the release — the
        // on-chain relative(-1) adjacency the program verifies is
        // preserved.
        assert_eq!(ixs[1].0, solana_sdk::ed25519_program::ID);
        assert_eq!(ixs[2].0, accounts::PROGRAM_ID);
        assert!(
            ixs[2].1.contains(&canonical_ata),
            "the release must pay into the SAME canonical ATA the creation provisioned"
        );
    }

    // And the request still settles through the unchanged path.
    let destination_txid = orchestrator
        .ledger()
        .get_destination_txid(request_id)
        .unwrap()
        .unwrap();
    let signature = Signature::from(<[u8; 64]>::try_from(destination_txid).unwrap());
    solana_rpc.set_status(signature, Ok(()));
    let report = orchestrator.tick(20).await;
    assert_eq!(report.releases_confirmed, 1, "errors: {:?}", report.errors);
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::Settled
    );
}

/// A Token-2022 reserve must derive the recipient ATA with the
/// program-id-aware derivation — the Token-2022 canonical address, which
/// genuinely differs from the legacy SPL Token derivation for the same
/// (owner, mint).
#[tokio::test]
async fn release_with_a_token_2022_reserve_derives_the_ata_with_the_token_2022_program() {
    let (solana_rpc, mut orchestrator, _request_id, recipient, mint, _dir) =
        release_fixture(spl_token_2022::ID).await;

    let report = orchestrator.tick(10).await;
    assert_eq!(report.releases_submitted, 1, "errors: {:?}", report.errors);

    let sent = solana_rpc.sent.lock().unwrap();
    let ixs = decompiled(&sent[0]);
    let token_2022_ata = accounts::associated_token_address(&recipient, &mint, &spl_token_2022::ID);
    let legacy_ata = accounts::associated_token_address(&recipient, &mint, &spl_token::ID);
    assert_ne!(
        token_2022_ata, legacy_ata,
        "the two derivations must genuinely differ for this to prove anything"
    );
    assert_eq!(ixs[0].0, spl_associated_token_account::ID);
    assert_eq!(ixs[0].1[1], token_2022_ata);
    assert!(
        ixs[0].1.contains(&spl_token_2022::ID),
        "the creation must run under the configured Token-2022 program"
    );
    assert!(ixs[2].1.contains(&token_2022_ata));
    assert!(
        !ixs[2].1.contains(&legacy_ata),
        "the legacy-derived address must appear nowhere"
    );
}

/// Retry safety: a failed submission leaves NO partial state (the ATA
/// creation is in the same atomic transaction), the next tick retries
/// the identical composition, and once one submission succeeds the
/// request leaves the queue — the release is never submitted again.
#[tokio::test]
async fn release_retry_after_a_failed_send_reprovisions_the_ata_and_submits_once() {
    let (solana_rpc, mut orchestrator, request_id, recipient, mint, _dir) =
        release_fixture(spl_token::ID).await;

    // Tick 1: the send fails; nothing was recorded, nothing advanced.
    solana_rpc.fail_next_sends(1);
    let report = orchestrator.tick(10).await;
    assert_eq!(report.releases_submitted, 0);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("injected send failure")),
        "the failure must be surfaced: {:?}",
        report.errors
    );
    assert_eq!(solana_rpc.sent.lock().unwrap().len(), 0);
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::SourceFinalized,
        "a failed submission must leave the request exactly where it was"
    );

    // Tick 2: the retry carries the same idempotent ATA creation (safe
    // whether or not the previous attempt reached the chain) and submits.
    let report = orchestrator.tick(20).await;
    assert_eq!(report.releases_submitted, 1, "errors: {:?}", report.errors);
    {
        let sent = solana_rpc.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let ixs = decompiled(&sent[0]);
        assert_eq!(ixs[0].0, spl_associated_token_account::ID);
        assert_eq!(
            ixs[0].1[1],
            accounts::associated_token_address(&recipient, &mint, &spl_token::ID)
        );
    }

    // Tick 3: confirms and settles.
    let destination_txid = orchestrator
        .ledger()
        .get_destination_txid(request_id)
        .unwrap()
        .unwrap();
    let signature = Signature::from(<[u8; 64]>::try_from(destination_txid).unwrap());
    solana_rpc.set_status(signature, Ok(()));
    let report = orchestrator.tick(30).await;
    assert_eq!(report.releases_confirmed, 1, "errors: {:?}", report.errors);

    // Tick 4: settled requests never resubmit — exactly one release
    // transaction ever left this daemon.
    orchestrator.tick(40).await;
    assert_eq!(solana_rpc.sent.lock().unwrap().len(), 1);
}

// ------------------------------------- fee-policy-snapshot regressions --
//
// Production bug (request #818): after BRIDGE_FEE_BPS changed 600 -> 300,
// every settlement/attestation path re-judged already-existing requests
// against the NEW compiled-in rate and refused their (perfectly
// consistent) 6%-era fee/net records. The fix validates and settles an
// existing request at ITS OWN `fee_bps` snapshot; the compiled-in rate
// prices new requests only. These tests run the REAL orchestrator ticks
// under the current 300-bps binary against requests persisted with
// 600-bps-era figures — exactly the production situation. New-request
// coverage at the current rate is the two happy-path tests above.

/// GlcToSol: a request created under the 6% fee policy (fee_bps=600
/// snapshot, 6%-consistent fee/net) must still attest, release, and
/// settle after the binary's rate became 3% — at its ORIGINAL 6% net.
#[tokio::test]
async fn glc_to_sol_request_created_at_600_bps_settles_under_the_300_bps_binary() {
    let mint = [7u8; 32];
    let recipient = [9u8; 32];

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
        // 500_000 canonical gross at the HISTORICAL 600 bps: fee 30_000,
        // net 470_000 (destination: 4_700 at 6 decimals) — versus 15_000 /
        // 485_000 at today's rate. The stored snapshot must govern.
        let amounts = glc_to_sol_amounts_at_bps(500_000, TEST_SOLANA_DECIMALS, 600);
        assert_eq!(amounts.fee_bps, 600);
        assert_eq!(amounts.fee_atomic, 30_000);
        assert_eq!(amounts.net_atomic, 470_000);
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(Direction::GlcToSol, amounts, &recipient, None, 3600, 0)
            .unwrap()
        else {
            panic!()
        };
        ledger
            .record_glc_deposit_observed(request_id, [0xAAu8; 32], 2, 500_000, 10, [0u8; 32], 0)
            .unwrap();
        ledger.mark_glc_source_finalized(request_id, 0).unwrap();
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            5,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );

    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    let report = orchestrator.tick(10).await;
    assert_eq!(
        report.releases_submitted, 1,
        "a 600-bps-era request must still attest and submit under the 300-bps binary; errors: {:?}",
        report.errors
    );
    let destination_txid = orchestrator
        .ledger()
        .get_destination_txid(request_id)
        .unwrap()
        .unwrap();
    let signature = Signature::from(<[u8; 64]>::try_from(destination_txid).unwrap());
    solana_rpc.set_status(signature, Ok(()));

    let report = orchestrator.tick(20).await;
    assert_eq!(report.releases_confirmed, 1, "errors: {:?}", report.errors);
    let req = orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(req.state, RequestState::Settled);
    // Settled at the ORIGINAL 6% net (4_700 destination units), never
    // re-priced at today's 3% (which would be 4_850).
    assert_eq!(
        req.fee_bps, 600,
        "the snapshot is immutable historical accounting"
    );
    assert_eq!(
        orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::SolanaReserve)
            .unwrap(),
        4_700
    );
}

/// SolToGlc: a 6%-era request must still build/sign/broadcast its payout,
/// pass completion attestation (which re-derives the expected payout from
/// the ON-CHAIN gross at the request's snapshot rate), and settle — at
/// its ORIGINAL 6% net.
#[tokio::test]
async fn sol_to_glc_request_created_at_600_bps_settles_under_the_300_bps_binary() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let mint = [7u8; 32];
    // 500_000 Solana-native gross -> 50_000_000 canonical; at the
    // HISTORICAL 600 bps: fee 3_000_000, net 47_000_000 (today's rate
    // would give 1_500_000 / 48_500_000).
    let amounts = sol_to_glc_amounts_at_bps(500_000, TEST_SOLANA_DECIMALS, 600);
    assert_eq!(amounts.fee_bps, 600);
    assert_eq!(amounts.fee_atomic, 3_000_000);
    let goldcoin_payout_atomic = amounts.net_destination_atomic;
    assert_eq!(goldcoin_payout_atomic, 47_000_000);
    let utxo_amount = goldcoin_payout_atomic + 100_000;
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: utxo_amount,
            script_pubkey_hex: vault.script_pubkey_hex(),
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
            .unwrap();
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(0, amounts, [1u8; 32], dest_addr.as_bytes(), None, 0)
            .unwrap()
        else {
            panic!()
        };
        ledger
            .goldcoin_ingest_block(100, [1u8; 32], [0u8; 32], 1000, 0)
            .unwrap();
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    goldcoin_rpc.set_known_tip(100, crate::goldcoin::hex::encode(&[1u8; 32]));
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault.script_pubkey_hex(),
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            9,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    solana_rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_withdrawal_obligation_bytes(0, 500_000, &[5u8; 32], dest_addr.as_bytes()),
    );

    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::clone(&goldcoin_rpc),
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    // Tick 1: payout builds and broadcasts at the 6%-era net — the vault
    // signers' independent re-derivation validates against the snapshot.
    let report = orchestrator.tick(10).await;
    assert_eq!(
        report.payouts_built, 1,
        "a 600-bps-era request's payout must still build under the 300-bps binary; errors: {:?}",
        report.errors
    );
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        payout.payout_atomic, 47_000_000,
        "the ORIGINAL 6% net, never re-priced"
    );
    let txid = payout.txid.unwrap();

    // Tick 2: payout confirms; completion attests against the ON-CHAIN
    // gross at the request's own snapshot rate and submits.
    goldcoin_rpc.set_confirmations(&crate::goldcoin::hex::encode(&txid), 6);
    let report = orchestrator.tick(20).await;
    assert_eq!(report.payouts_confirmed, 1, "errors: {:?}", report.errors);
    assert_eq!(
        report.completions_submitted, 1,
        "completion attestation must validate at the stored 600 bps, not today's 300; errors: {:?}",
        report.errors
    );
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    let completion_sig = Signature::from(payout.onchain_completion_signature.unwrap());
    solana_rpc.set_status(completion_sig, Ok(()));

    // Tick 3: settles, moving exactly the 6%-era net.
    let report = orchestrator.tick(30).await;
    assert_eq!(
        report.completions_confirmed, 1,
        "errors: {:?}",
        report.errors
    );
    let req = orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(req.state, RequestState::Settled);
    assert_eq!(
        req.fee_bps, 600,
        "the snapshot is immutable historical accounting"
    );
    assert_eq!(
        orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        47_000_000
    );
}

/// Honoring the snapshot must NOT weaken fail-closed validation: stored
/// fee/net that do not reconcile against the stored snapshot rate — and a
/// snapshot rate outside the arithmetically valid range — both keep being
/// refused, in both directions, and the requests never advance.
///
/// The third fixture used to be a 0-bps row, refused for being a rate the
/// protocol had never charged. 0 bps is an ordinary configurable rate now
/// (a free route), so the row that must fail closed is one whose rate is
/// genuinely impossible: above 100%, where the net entitlement would be
/// negative. The reconciliation check — the one that actually catches a
/// tampered row — is unchanged and is what the first two fixtures pin.
#[tokio::test]
async fn corrupted_or_impossible_fee_snapshots_still_fail_closed_in_both_directions() {
    let mint = [7u8; 32];
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let (glc_to_sol_id, sol_to_glc_id, out_of_range_bps_id) = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: 60_000_000,
            script_pubkey_hex: vault.script_pubkey_hex(),
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
            .unwrap();

        // GlcToSol: claims the genuine 600 bps snapshot, but fee/net are
        // NOT the 600-bps breakdown of gross (real: 30_000 / 470_000).
        let corrupted_glc_to_sol = crate::ledger::RequestAmounts {
            gross_atomic: 500_000,
            fee_bps: 600,
            fee_atomic: 20_000,
            net_atomic: 480_000,
            net_destination_atomic: 4_800,
        };
        let CreateRequestOutcome::Reserved { request_id: a } = ledger
            .create_request(
                Direction::GlcToSol,
                corrupted_glc_to_sol,
                &[9u8; 32],
                None,
                3600,
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .record_glc_deposit_observed(a, [0xAAu8; 32], 2, 500_000, 10, [0u8; 32], 0)
            .unwrap();
        ledger.mark_glc_source_finalized(a, 0).unwrap();

        // SolToGlc: same corruption shape (real 600-bps breakdown of
        // this gross: fee 300_000 / net 4_700_000).
        let corrupted_sol_to_glc = crate::ledger::RequestAmounts {
            gross_atomic: 5_000_000,
            fee_bps: 600,
            fee_atomic: 100_000,
            net_atomic: 4_900_000,
            net_destination_atomic: 4_900_000,
        };
        let SolFoldOutcome::FoldedFinalized { request_id: b } = ledger
            .fold_sol_deposit(
                0,
                corrupted_sol_to_glc,
                [1u8; 32],
                dest_addr.as_bytes(),
                None,
                0,
            )
            .unwrap()
        else {
            panic!()
        };

        // GlcToSol: a rate that is not a rate — 100.01%. No fee/net pair
        // can reconcile against it, because the fee would exceed the
        // gross and the net entitlement would be negative. Refused on the
        // rate itself, before any reconciliation is attempted.
        let impossible_rate = crate::ledger::RequestAmounts {
            gross_atomic: 500_000,
            fee_bps: 10_001,
            fee_atomic: 500_050,
            net_atomic: 0,
            net_destination_atomic: 0,
        };
        let CreateRequestOutcome::Reserved { request_id: c } = ledger
            .create_request(
                Direction::GlcToSol,
                impossible_rate,
                &[8u8; 32],
                None,
                3600,
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .record_glc_deposit_observed(c, [0xBBu8; 32], 2, 500_000, 10, [0u8; 32], 0)
            .unwrap();
        ledger.mark_glc_source_finalized(c, 0).unwrap();
        ledger
            .goldcoin_ingest_block(100, [1u8; 32], [0u8; 32], 1000, 0)
            .unwrap();
        (a, b, c)
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    goldcoin_rpc.set_known_tip(100, crate::goldcoin::hex::encode(&[1u8; 32]));
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            5,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    solana_rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_withdrawal_obligation_bytes(0, 500_000, &[5u8; 32], dest_addr.as_bytes()),
    );

    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        solana_rpc,
        vault,
        vault_signers,
        attestation_signers,
    );

    let report = orchestrator.tick(10).await;
    assert_eq!(report.releases_submitted, 0);
    assert_eq!(report.payouts_built, 0);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("disagrees with the stored ledger record")),
        "the corrupted rows must be refused loudly: {:?}",
        report.errors
    );
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("above 10000 basis points")),
        "the out-of-range snapshot must be refused on the rate itself: {:?}",
        report.errors
    );
    for id in [glc_to_sol_id, out_of_range_bps_id] {
        assert_eq!(
            orchestrator
                .ledger()
                .get_request(id)
                .unwrap()
                .unwrap()
                .state,
            RequestState::SourceFinalized,
            "refused GlcToSol request {id} must not advance"
        );
    }
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(sol_to_glc_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::SourceFinalized,
        "refused SolToGlc request must not advance"
    );
    assert!(orchestrator
        .ledger()
        .get_goldcoin_payout(sol_to_glc_id)
        .unwrap()
        .is_none());
}

/// Shared fixture for the `DestinationConfirmed`-stage regression tests
/// below (the production incident's stuck stage): drives a real SolToGlc
/// request through tick 1 (payout built + broadcast) and tick 2 at the
/// required depth (payout `Confirmed`, request `DestinationConfirmed`,
/// completion independently attested and submitted to Solana), then hands
/// the caller everything needed to exercise what happens AFTER that point.
struct DestinationConfirmedFixture {
    _dir: tempfile::TempDir,
    goldcoin_rpc: Arc<MockGoldcoinRpc>,
    solana_rpc: Arc<MockSolanaRpc>,
    orchestrator: Orchestrator<Arc<MockGoldcoinRpc>, Arc<MockSolanaRpc>>,
    request_id: i64,
    payout_txid_hex: String,
    goldcoin_payout_atomic: u64,
    first_completion_signature: Signature,
    dest_addr: &'static str,
}

async fn destination_confirmed_fixture() -> DestinationConfirmedFixture {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let mint = [7u8; 32];
    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let utxo_amount = goldcoin_payout_atomic + 100_000;
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: utxo_amount,
            script_pubkey_hex: vault.script_pubkey_hex(),
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
            .unwrap();
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                dest_addr.as_bytes(),
                None,
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .goldcoin_ingest_block(100, [1u8; 32], [0u8; 32], 1000, 0)
            .unwrap();
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    goldcoin_rpc.set_known_tip(100, crate::goldcoin::hex::encode(&[1u8; 32]));
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault.script_pubkey_hex(),
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            9,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    solana_rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_withdrawal_obligation_bytes(0, 500_000, &[5u8; 32], dest_addr.as_bytes()),
    );

    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::clone(&goldcoin_rpc),
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    // Tick 1: build, sign, and broadcast the payout.
    let report = orchestrator.tick(10).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    let payout_txid_hex = crate::goldcoin::hex::encode(&payout.txid.unwrap());

    // Tick 2: required depth reached — payout Confirmed, request
    // DestinationConfirmed, completion submitted to Solana.
    goldcoin_rpc.set_confirmations(&payout_txid_hex, 6);
    let report = orchestrator.tick(20).await;
    assert_eq!(report.payouts_confirmed, 1, "errors: {:?}", report.errors);
    assert_eq!(
        report.completions_submitted, 1,
        "errors: {:?}",
        report.errors
    );
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Confirmed");
    let request = orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(request.state, RequestState::DestinationConfirmed);
    let first_completion_signature = Signature::from(payout.onchain_completion_signature.unwrap());

    DestinationConfirmedFixture {
        _dir: dir,
        goldcoin_rpc,
        solana_rpc,
        orchestrator,
        request_id,
        payout_txid_hex,
        goldcoin_payout_atomic,
        first_completion_signature,
        dest_addr,
    }
}

/// Regression for the production incident's observability half: once a
/// SolToGlc request reached `DestinationConfirmed`, the payout
/// confirmation tracker (which polled only `Broadcast` payouts) stopped
/// consulting Goldcoin RPC for it entirely, so its recorded confirmation
/// depth froze at the threshold and `bridge_requests.
/// destination_confirmations` — which no code path wrote at all — sat at
/// 0 forever while the chain moved on. The tracker must keep refreshing
/// both counters until the request actually settles, without re-counting
/// the payout as newly confirmed each tick.
#[tokio::test]
async fn destination_confirmed_requests_keep_refreshing_confirmations_until_settled() {
    let mut fx = destination_confirmed_fixture().await;

    // The chain deepens to 30 confirmations while the completion is still
    // pending on Solana (its signature status is not observable yet).
    fx.goldcoin_rpc.set_confirmations(&fx.payout_txid_hex, 30);
    let report = fx.orchestrator.tick(30).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(
        report.payouts_confirmed, 0,
        "an already-Confirmed payout must not be re-counted as newly confirmed"
    );
    let payout = fx
        .orchestrator
        .ledger()
        .get_goldcoin_payout(fx.request_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        payout.confirmations, 30,
        "goldcoin_payouts.confirmations must keep tracking the chain after DestinationConfirmed"
    );
    assert_eq!(
        fx.orchestrator
            .ledger()
            .destination_confirmations(fx.request_id)
            .unwrap(),
        30,
        "bridge_requests.destination_confirmations must mirror the live depth"
    );

    // The completion then confirms and the request settles normally.
    fx.solana_rpc
        .set_status(fx.first_completion_signature, Ok(()));
    let report = fx.orchestrator.tick(40).await;
    assert_eq!(
        report.completions_confirmed, 1,
        "errors: {:?}",
        report.errors
    );
    let request = fx
        .orchestrator
        .ledger()
        .get_request(fx.request_id)
        .unwrap()
        .unwrap();
    assert_eq!(request.state, RequestState::Settled);

    // Once Settled the payout is Completed and drops out of the tracker:
    // further chain growth no longer touches the frozen final record.
    fx.goldcoin_rpc.set_confirmations(&fx.payout_txid_hex, 40);
    fx.orchestrator.tick(50).await;
    assert_eq!(
        fx.orchestrator
            .ledger()
            .destination_confirmations(fx.request_id)
            .unwrap(),
        30
    );
}

/// Regression for the production incident's stall half, dropped-transaction
/// flavor: the one `record_goldcoin_completion` submission never lands
/// (blockhash expired / dropped from the mempool), its signature status
/// answers `None` on every poll, and — before the fix — the recorded
/// signature suppressed any re-submission, leaving the request in
/// `DestinationConfirmed` forever with no error reported. The orchestrator
/// must wait out the grace window, verify via the obligation's terminal
/// on-chain status that the completion truly has not happened, re-attest
/// and re-submit with a fresh blockhash, and then settle normally — exactly
/// once.
#[tokio::test]
async fn dropped_completion_transaction_is_resubmitted_and_settles_exactly_once() {
    let mut fx = destination_confirmed_fixture().await;

    // Within the grace window an unobserved signature is just in-flight:
    // no re-submission yet.
    let report = fx.orchestrator.tick(100).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.completions_submitted, 0);
    let payout = fx
        .orchestrator
        .ledger()
        .get_goldcoin_payout(fx.request_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        Signature::from(payout.onchain_completion_signature.unwrap()),
        fx.first_completion_signature,
        "the tracked signature must not change while the original could still land"
    );

    // Past the grace window (submitted at t=20, resubmit after 300s) with
    // the obligation still Pending on-chain: re-submit with a fresh
    // blockhash, replacing the tracked signature.
    let report = fx.orchestrator.tick(321).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(
        report.completions_submitted, 1,
        "a demonstrably dead completion submission must be re-sent"
    );
    let payout = fx
        .orchestrator
        .ledger()
        .get_goldcoin_payout(fx.request_id)
        .unwrap()
        .unwrap();
    let second_signature = Signature::from(payout.onchain_completion_signature.unwrap());
    assert_ne!(second_signature, fx.first_completion_signature);
    assert_eq!(fx.solana_rpc.sent.lock().unwrap().len(), 2);

    // The re-submission lands; the request settles through the normal
    // path, with the accounting moved exactly once.
    fx.solana_rpc.set_status(second_signature, Ok(()));
    let report = fx.orchestrator.tick(330).await;
    assert_eq!(
        report.completions_confirmed, 1,
        "errors: {:?}",
        report.errors
    );
    let request = fx
        .orchestrator
        .ledger()
        .get_request(fx.request_id)
        .unwrap()
        .unwrap();
    assert_eq!(request.state, RequestState::Settled);
    assert_eq!(
        fx.orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        fx.goldcoin_payout_atomic
    );
}

/// Regression for the production incident's stall half, aged-out flavor:
/// the completion transaction DID land, but its signature fell out of the
/// node's recent-status cache before this service observed it (daemon
/// restart / RPC outage), so `get_signature_status` answers `None` forever
/// even though the obligation is terminally `Completed` on-chain. The
/// orchestrator must settle from that on-chain ground truth — never
/// re-submit a completion the chain says already happened — and stay
/// idempotent on later ticks.
#[tokio::test]
async fn completion_that_landed_but_left_the_status_cache_settles_from_obligation_status() {
    let mut fx = destination_confirmed_fixture().await;

    // The obligation reached its terminal Completed status on-chain, but
    // the tracked signature is never observable via the status cache.
    fx.solana_rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_withdrawal_obligation_bytes_with_status(
            0,
            500_000,
            &[5u8; 32],
            fx.dest_addr.as_bytes(),
            2,
        ),
    );

    let sent_before = fx.solana_rpc.sent.lock().unwrap().len();
    let report = fx.orchestrator.tick(400).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(
        report.completions_confirmed, 1,
        "a completion the chain records as done must settle from that record"
    );
    assert_eq!(
        report.completions_submitted, 0,
        "an already-completed obligation must never be re-submitted"
    );
    assert_eq!(fx.solana_rpc.sent.lock().unwrap().len(), sent_before);
    let request = fx
        .orchestrator
        .ledger()
        .get_request(fx.request_id)
        .unwrap()
        .unwrap();
    assert_eq!(request.state, RequestState::Settled);
    let payout = fx
        .orchestrator
        .ledger()
        .get_goldcoin_payout(fx.request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Completed");
    assert_eq!(
        fx.orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        fx.goldcoin_payout_atomic
    );

    // A later tick must not settle, submit, or move the accounting again.
    let report = fx.orchestrator.tick(410).await;
    assert_eq!(report.completions_confirmed, 0);
    assert_eq!(report.completions_submitted, 0);
    assert_eq!(
        fx.orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        fx.goldcoin_payout_atomic
    );
}

/// Proves coin selection fails closed, at the real orchestrator-tick level,
/// when the vault has no real spendable UTXO yet — the request must park
/// safely (stay `SourceFinalized`, no `goldcoin_payouts` row, no panic, no
/// stuck tick) and then settle automatically, with no operator action, the
/// very next tick after a real UTXO actually becomes available.
#[tokio::test]
async fn sol_to_glc_payout_parks_safely_and_later_succeeds_once_the_vault_has_mature_funds() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let vault_script_pubkey_hex = vault.script_pubkey_hex();
    let mint = [7u8; 32];
    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let utxo_amount = goldcoin_payout_atomic + 100_000;
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // Capacity accounting believes this much is backed (matching the
        // real admission-time cached balance) even though no real spendable
        // UTXO exists yet — exactly the gap automatic coin selection must
        // fail closed against rather than fabricate.
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                dest_addr.as_bytes(),
                None,
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        request_id
    };

    // No `set_unspent` call: the vault genuinely has zero spendable UTXOs.
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            9,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );

    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::clone(&goldcoin_rpc),
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    // Tick 1: coin selection has nothing to select from — must fail
    // closed, park the request, and keep the tick itself healthy.
    let report = orchestrator.tick(10).await;
    assert_eq!(report.payouts_built, 0);
    assert!(
        !report.errors.is_empty(),
        "insufficient liquidity must be recorded, not silently swallowed"
    );
    assert!(
        orchestrator
            .ledger()
            .get_goldcoin_payout(request_id)
            .unwrap()
            .is_none(),
        "no payout row may exist until a real spendable UTXO is actually available"
    );
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::SourceFinalized,
        "the request must stay parked for automatic retry, never move to a stuck or wrong state"
    );

    // The vault now genuinely receives (and matures) a real UTXO — no
    // operator command of any kind.
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault_script_pubkey_hex,
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);

    // Tick 2: the same still-`SourceFinalized` request is retried
    // automatically and now succeeds.
    let report = orchestrator.tick(20).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
}

/// Proves `glc-admin resume-manual-review` genuinely restores normal
/// processing end to end: a request parked in `ManualReview` by
/// `fold_sol_deposit` (admission closed at the time) is resumed via
/// `Ledger::resume_manual_review_sol_to_glc`, and the real `Orchestrator`
/// tick loop then builds, signs, and broadcasts its Goldcoin payout exactly
/// as it would have for a normal `SourceFinalized` request — with admission
/// left closed throughout, since resuming never re-admits anything new.
#[tokio::test]
async fn resumed_manual_review_request_processes_normally_with_admission_still_closed() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let mint = [7u8; 32];
    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let utxo_amount = goldcoin_payout_atomic + 100_000;
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: utxo_amount,
            script_pubkey_hex: vault.script_pubkey_hex(),
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
            .unwrap();

        ledger
            .set_admission(
                ReserveDirection::GoldcoinReserve,
                true,
                Some("closing before the deposit arrives"),
            )
            .unwrap();

        let SolFoldOutcome::FoldedManualReview { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                dest_addr.as_bytes(),
                None,
                0,
            )
            .unwrap()
        else {
            panic!("expected admission-closed to route to ManualReview")
        };
        ledger
            .goldcoin_ingest_block(100, [1u8; 32], [0u8; 32], 1000, 0)
            .unwrap();

        let outcome = ledger
            .resume_manual_review_sol_to_glc(request_id, "verified, safe to resume", "operator", 0)
            .unwrap();
        assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
        assert_eq!(
            ledger.get_request(request_id).unwrap().unwrap().state,
            RequestState::SourceFinalized
        );
        // Admission stays closed — resuming never re-admits anything new.
        assert!(ledger
            .is_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap());

        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    goldcoin_rpc.set_known_tip(100, crate::goldcoin::hex::encode(&[1u8; 32]));
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault.script_pubkey_hex(),
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            9,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    solana_rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_withdrawal_obligation_bytes(0, 500_000, &[5u8; 32], dest_addr.as_bytes()),
    );

    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::clone(&goldcoin_rpc),
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    let report = orchestrator.tick(10).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
    assert!(orchestrator
        .ledger()
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

/// Proves the two admission-control requirements end to end through the
/// real `Orchestrator` tick loop (docs/09-runbook.md "Admission control
/// (Solana->Goldcoin)"): (1) a request already `SourceFinalized` BEFORE
/// admission was closed still gets built, signed, and broadcast normally —
/// payout processing has never been gated by either `paused` or
/// `admission_closed`, and closing admission must not change that; (2) a
/// brand-new fold attempted WHILE admission is closed is routed to
/// `ManualReview` instead, even though the reserve is not paused and has
/// ample capacity.
#[tokio::test]
async fn admission_closed_blocks_new_folds_but_never_already_accepted_processing() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let mint = [7u8; 32];
    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let utxo_amount = goldcoin_payout_atomic + 100_000;
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: utxo_amount,
            script_pubkey_hex: vault.script_pubkey_hex(),
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
            .unwrap();

        // Accepted BEFORE admission is closed.
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                dest_addr.as_bytes(),
                None,
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .goldcoin_ingest_block(100, [1u8; 32], [0u8; 32], 1000, 0)
            .unwrap();

        // Close admission — the exact operator action this feature adds.
        ledger
            .set_admission(
                ReserveDirection::GoldcoinReserve,
                true,
                Some("draining backlog, not accepting new transfers"),
            )
            .unwrap();

        // A brand-new obligation observed WHILE admission is closed must
        // be refused (routed to ManualReview), even though the reserve
        // is not paused and there is no capacity shortfall.
        assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
        let new_outcome = ledger
            .fold_sol_deposit(
                1,
                sol_to_glc_amounts(1_000, TEST_SOLANA_DECIMALS),
                [2u8; 32],
                dest_addr.as_bytes(),
                None,
                0,
            )
            .unwrap();
        assert!(
            matches!(new_outcome, SolFoldOutcome::FoldedManualReview { .. }),
            "expected a new fold while admission is closed to land in ManualReview, got {new_outcome:?}"
        );

        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    goldcoin_rpc.set_known_tip(100, crate::goldcoin::hex::encode(&[1u8; 32]));
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault.script_pubkey_hex(),
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            9,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    solana_rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_withdrawal_obligation_bytes(0, 500_000, &[5u8; 32], dest_addr.as_bytes()),
    );

    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::clone(&goldcoin_rpc),
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    // The already-accepted request still gets built, signed, and broadcast
    // this tick — payout processing is unaffected by admission being
    // closed.
    let report = orchestrator.tick(10).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");

    // Admission remains closed — nothing in the tick loop reopens it.
    assert!(orchestrator
        .ledger()
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

/// The confirmed-liquidity twin of the test above (docs/09-runbook.md's
/// "Confirmed-liquidity admission safety buffer"): when the AUTOMATIC gate
/// closes, a brand-new obligation parks — and the obligation accepted
/// before it still gets built, signed and broadcast by the very same tick.
/// This is the property that makes the buffer safe to run in production at
/// all: it holds back new demand without ever interrupting, cancelling, or
/// even touching demand already taken on.
#[tokio::test]
async fn liquidity_admission_gate_closes_without_stopping_an_already_accepted_payout() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let mint = [7u8; 32];
    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let utxo_amount = goldcoin_payout_atomic + 100_000;
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: utxo_amount,
            script_pubkey_hex: vault.script_pubkey_hex(),
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
            .unwrap();

        // Accepted while headroom was ample and no buffer was configured.
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                dest_addr.as_bytes(),
                None,
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .goldcoin_ingest_block(100, [1u8; 32], [0u8; 32], 1000, 0)
            .unwrap();

        // The deployment now configures a safety buffer well above what
        // is left (100_000 atomic units of confirmed headroom) — the same
        // thing a daemon restart does after an operator raises
        // `goldcoin.admission_safety_buffer_atomic`.
        ledger
            .set_admission_liquidity_thresholds(
                ReserveDirection::GoldcoinReserve,
                1_000_000,
                2_000_000,
            )
            .unwrap();

        // Neither the operator flag nor `paused` is set: the ONLY thing
        // holding new obligations back here is confirmed liquidity.
        assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
        assert!(!ledger
            .is_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap());
        // A DIFFERENT recipient and a different source wallet, so neither
        // rolling-24h rate limit can be what parks this — the liquidity
        // gate has to be the reason, and the asserted note proves it.
        let new_outcome = ledger
            .fold_sol_deposit(
                1,
                sol_to_glc_amounts(1_000, TEST_SOLANA_DECIMALS),
                [2u8; 32],
                b"mnQ7kA1SLJTLxTbCPjbe6R2Q3tgTiDLZLd",
                None,
                0,
            )
            .unwrap();
        let SolFoldOutcome::FoldedManualReview { request_id: parked } = new_outcome else {
            panic!("expected the liquidity gate to park a new fold, got {new_outcome:?}")
        };
        assert_eq!(
            ledger
                .get_request(parked)
                .unwrap()
                .unwrap()
                .manual_review_note
                .as_deref(),
            Some("liquidity_buffer_low_at_fold")
        );
        assert!(ledger
            .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap());

        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    goldcoin_rpc.set_known_tip(100, crate::goldcoin::hex::encode(&[1u8; 32]));
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault.script_pubkey_hex(),
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            9,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    solana_rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_withdrawal_obligation_bytes(0, 500_000, &[5u8; 32], dest_addr.as_bytes()),
    );

    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::clone(&goldcoin_rpc),
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    let report = orchestrator.tick(10).await;
    // The accepted obligation is built, signed and broadcast on this very
    // tick — a closed liquidity gate never reaches past admission.
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::DestinationSubmitted
    );

    // The tick evaluated and reported the gate, and left it closed —
    // nothing in the loop reopens it below the reopen threshold.
    let gate = report
        .goldcoin_admission_liquidity_gate
        .expect("every tick evaluates the gate")
        .expect("evaluation must not error");
    assert!(gate.closed);
    assert_eq!(gate.buffer_atomic, 1_000_000);
    assert_eq!(gate.reopen_atomic, 2_000_000);
    assert!(orchestrator
        .ledger()
        .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
    // The operator-only flag was never touched by any of this.
    assert!(!orchestrator
        .ledger()
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

/// Step 4 end-to-end: a SolToGlc payout is built, signed, and broadcast by
/// the real `Orchestrator` tick loop spending a UTXO that lives at a
/// per-request DERIVED deposit address (never the legacy static vault) —
/// exercising the full production wiring (`Ledger::available_vault_utxos`,
/// `signing::goldcoin_vault::rederive_plan`'s per-input resolution, and
/// `Orchestrator::build_and_broadcast_payout`'s per-input assemble loop),
/// not just the signing module in isolation.
#[tokio::test]
async fn watched_goldcoin_addresses_includes_the_root_vault_and_every_derived_deposit_address() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
        for tag in 0..2u8 {
            let CreateRequestOutcome::Reserved { request_id } = ledger
                .create_request(
                    Direction::GlcToSol,
                    crate::ledger::RequestAmounts {
                        gross_atomic: 1,
                        fee_bps: 0,
                        fee_atomic: 0,
                        net_atomic: 1,
                        net_destination_atomic: 1,
                    },
                    // One recipient per request — the rolling-24h
                    // destination window refuses a repeat.
                    &[0xAB + tag; 32],
                    None,
                    100_000,
                    0,
                )
                .unwrap()
            else {
                panic!("reservation should succeed")
            };
            let derived = crate::goldcoin::derivation::derive_request_vault(
                &vault,
                request_id,
                Network::Testnet,
            )
            .unwrap();
            ledger
                .set_goldcoin_deposit_address(
                    request_id,
                    derived.address(),
                    &derived.script_pubkey_hex(),
                    &derived.redeem_script_hex(),
                )
                .unwrap();
        }
    }

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        solana_rpc,
        vault.clone(),
        vault_signers,
        attestation_signers(),
    );

    let addresses = orchestrator.watched_goldcoin_addresses().unwrap();
    assert_eq!(
        addresses.len(),
        3,
        "the root vault plus both derived deposit addresses: {addresses:?}"
    );
    assert!(addresses.contains(&vault.address().to_string()));
    let all_deposit_addresses = orchestrator
        .ledger()
        .all_goldcoin_deposit_addresses()
        .unwrap();
    assert_eq!(all_deposit_addresses.len(), 2);
    for addr in &all_deposit_addresses {
        assert!(addresses.contains(addr));
    }
}

#[tokio::test]
async fn sol_to_glc_payout_spends_a_derived_address_utxo_end_to_end() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let mint = [7u8; 32];
    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let utxo_amount = goldcoin_payout_atomic + 100_000;

    let (request_id, derived_script_pubkey_hex) = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();

        // An ordinary GlcToSol reservation gets a unique derived deposit
        // address — exactly what `api::BridgeApi::create_glc_to_sol_
        // transfer` does in production — and its address receives the
        // UTXO the payout below will spend. This is a DIFFERENT request
        // from (and, realistically, a different direction than) the
        // SolToGlc `request_id` whose payout is being settled.
        let CreateRequestOutcome::Reserved {
            request_id: funding_request_id,
        } = ledger
            .create_request(
                Direction::GlcToSol,
                crate::ledger::RequestAmounts {
                    gross_atomic: 1,
                    fee_bps: 0,
                    fee_atomic: 0,
                    net_atomic: 1,
                    net_destination_atomic: 1,
                },
                &[0xABu8; 32],
                None,
                100_000,
                0,
            )
            .unwrap()
        else {
            panic!("reservation should succeed")
        };
        let derived = crate::goldcoin::derivation::derive_request_vault(
            &vault,
            funding_request_id,
            Network::Testnet,
        )
        .unwrap();
        ledger
            .set_goldcoin_deposit_address(
                funding_request_id,
                derived.address(),
                &derived.script_pubkey_hex(),
                &derived.redeem_script_hex(),
            )
            .unwrap();

        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: utxo_amount,
            script_pubkey_hex: derived.script_pubkey_hex(),
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, derived.script_pubkey_hex())], 1, 0)
            .unwrap();

        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                dest_addr.as_bytes(),
                None,
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .goldcoin_ingest_block(100, [1u8; 32], [0u8; 32], 1000, 0)
            .unwrap();
        (request_id, derived.script_pubkey_hex())
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    goldcoin_rpc.set_known_tip(100, crate::goldcoin::hex::encode(&[1u8; 32]));
    // The mocked node reports this UTXO at the DERIVED address's
    // scriptPubKey — never the legacy vault's.
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: derived_script_pubkey_hex,
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            9,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    solana_rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_withdrawal_obligation_bytes(0, 500_000, &[5u8; 32], dest_addr.as_bytes()),
    );

    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::clone(&goldcoin_rpc),
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    let report = orchestrator.tick(10).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
}

#[tokio::test]
async fn reconciliation_breach_pauses_the_solana_reserve_without_aborting_the_tick() {
    let mint = [7u8; 32];
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
    }
    // protected_minimum for SolanaReserve is 1_000_000 (see configure_both_reserves);
    // an observed balance of 0 is a hard invariant breach.

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    let reserve_authority = accounts::reserve_authority_pda();
    let ata = accounts::associated_token_address(
        &reserve_authority,
        &Pubkey::new_from_array(mint),
        &spl_token::ID,
    );
    solana_rpc.set_account(ata, fake_token_account_bytes(0));

    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        solana_rpc,
        vault,
        vault_signers,
        attestation_signers(),
    );

    let report = orchestrator.tick(10).await;
    let reconciliation = report.solana_reconciliation.unwrap().unwrap();
    assert_eq!(
        reconciliation.classification,
        crate::reconciliation::Classification::Breach
    );
    assert!(reconciliation.auto_paused);
    assert!(orchestrator
        .ledger()
        .is_paused(ReserveDirection::SolanaReserve)
        .unwrap());
    // The tick did not abort: reservation expiry still ran (no error recorded for it).
    assert!(
        !report
            .errors
            .iter()
            .any(|e| e.contains("expire_reservations")),
        "a reconciliation breach must not stop the rest of the tick from running: {:?}",
        report.errors
    );
}

#[tokio::test]
async fn reconciliation_breach_pauses_the_goldcoin_reserve_without_aborting_the_tick() {
    // Regression coverage for the gap found in the post-Phase-6 audit:
    // `tick_reconciliation` used to cover SolanaReserve only, so a
    // discrepancy between the vault's real Goldcoin balance and the
    // ledger's bookkeeping would never trigger a pause. GoldcoinReserve
    // must get exactly the same automatic breach detection as
    // SolanaReserve — this mirrors
    // `reconciliation_breach_pauses_the_solana_reserve_without_aborting_the_tick`
    // with the two directions' roles swapped, keeping the Solana side
    // healthy so this test isolates the Goldcoin path.
    let mint = [7u8; 32];
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
    }
    // GoldcoinReserve's configured total_reserve_balance is 10_000_000
    // (see configure_both_reserves); the mock's `list_unspent` reports no
    // UTXOs at all by default, an unexplained drop to 0.

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    let reserve_authority = accounts::reserve_authority_pda();
    let ata = accounts::associated_token_address(
        &reserve_authority,
        &Pubkey::new_from_array(mint),
        &spl_token::ID,
    );
    // Solana side matches its configured balance exactly, so only the
    // Goldcoin side is under test here.
    solana_rpc.set_account(ata, fake_token_account_bytes(10_000_000));

    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        solana_rpc,
        vault,
        vault_signers,
        attestation_signers(),
    );

    let report = orchestrator.tick(10).await;
    let solana_reconciliation = report.solana_reconciliation.unwrap().unwrap();
    assert_eq!(
        solana_reconciliation.classification,
        crate::reconciliation::Classification::WithinTolerance,
        "Solana side must stay healthy so this test isolates the Goldcoin path"
    );
    // Caught by the EARLIER, admission-gating reconciliation pass
    // (`Orchestrator::tick`'s own comment explains why it runs first) —
    // by the time the pre-existing end-of-tick pass below runs again, its
    // own "before" baseline has already been refreshed to match the still
    // point at 0.0 GLC (with this test's protected_minimum=0 and no
    // pending obligations, the hard invariant trivially holds too, so
    // that second pass reports WithinTolerance on its own turn — expected,
    // not a bug: the breach was already found and paused, and nothing
    // silently un-pauses it (reconciliation never auto-unpauses).
    let goldcoin_pre_admission_reconciliation = report
        .goldcoin_pre_admission_reconciliation
        .unwrap()
        .unwrap();
    assert_eq!(
        goldcoin_pre_admission_reconciliation.classification,
        crate::reconciliation::Classification::Breach
    );
    assert!(goldcoin_pre_admission_reconciliation.auto_paused);
    assert!(orchestrator
        .ledger()
        .is_paused(ReserveDirection::GoldcoinReserve)
        .unwrap());
    assert!(
        !orchestrator
            .ledger()
            .is_paused(ReserveDirection::SolanaReserve)
            .unwrap(),
        "a Goldcoin-side breach must not pause the unrelated Solana direction"
    );
    // The tick did not abort: reservation expiry still ran (no error recorded for it).
    assert!(
        !report
            .errors
            .iter()
            .any(|e| e.contains("expire_reservations")),
        "a reconciliation breach must not stop the rest of the tick from running: {:?}",
        report.errors
    );
}

/// Production-shaped regression for the admission-freshness incident:
/// several near-10,000-GLC-gross SolToGlc obligations arrive together
/// (all newly observed in the same `solana_indexer.tick()` batch), against
/// a `GoldcoinReserve` whose CACHED `total_reserve_balance` is stale
/// relative to the live, freshly-read mature balance (69,942.41205717
/// GLC — the exact production figure from the incident this fixes) by
/// exactly the value of an already-`Broadcast` (not yet settled) Goldcoin
/// payout — the same mechanism the real incident involved: a payout's own
/// change output goes immature the moment it broadcasts, mechanically
/// dropping observed mature balance by more than the cached figure yet
/// reflects. That gap is fully explained by
/// `pending_destination_settlement_amount`'s in-flight accounting
/// (`Classification::InFlightExplained`, not a breach), so the pre-
/// admission pass refreshes the balance and moves on without pausing
/// anything — protected_minimum is 20,000 GLC, so:
///
///   - against the STALE cached balance (89,942.41205717 GLC), available
///     looks like 69,942.41205717 GLC — comfortably enough for all six
///     9,700 GLC net obligations (6 * 9,700 = 58,200 <=
///     69,942.41205717): every one would have been admitted.
///   - against the FRESH balance this fix surfaces before admission runs
///     (69,942.41205717 GLC), available is only 49,942.41205717 GLC —
///     enough for exactly five (5 * 9,700 = 48,500 <= 49,942.41205717)
///     but not six (6 * 9,700 = 58,200 > 49,942.41205717).
///
/// Desired behavior per the incident follow-up: park/reject only the one
/// request that would cross the floor, never admit it and then discover
/// the breach only after the fact via auto-pause of the whole direction.
#[tokio::test]
async fn several_near_10k_requests_arriving_together_park_only_the_one_that_does_not_fit() {
    let mint = [7u8; 32];
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");

    const PROTECTED_MINIMUM: u64 = 20_000 * 100_000_000;
    // The exact production figure from the incident this fixes.
    const FRESH_LIVE_BALANCE: u64 = 6_994_241_205_717; // 69,942.41205717 GLC
                                                       // An already-broadcast payout's full input value (never settled yet),
                                                       // explaining the entire gap between the stale cache and live reality.
    const IN_FLIGHT_BROADCAST_VALUE: u64 = 20_000 * 100_000_000;
    const STALE_CACHED_BALANCE: u64 = FRESH_LIVE_BALANCE + IN_FLIGHT_BROADCAST_VALUE;
    const NUM_REQUESTS: u64 = 6;
    // 10,000 GLC gross (Solana-native, 6 decimals) -> 9,700 GLC net after
    // the 3% bridge fee.
    const GROSS_SOLANA_ATOMIC: u64 = 10_000_000_000;

    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                STALE_CACHED_BALANCE,
                PROTECTED_MINIMUM,
                90_000 * 100_000_000,
                50_000 * 100_000_000,
                30_000 * 100_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();

        // A phantom, already-settling SolToGlc request (id 1) whose
        // Goldcoin payout has already broadcast but not yet settled —
        // the real-world cause of the stale-vs-live gap above. Inserted
        // directly: this is a fixed, pre-existing fact this test starts
        // from, not something exercised through the normal build/sign/
        // broadcast pipeline.
        let conn = ledger.raw();
        conn.execute(
            // `source_chain`/`source_contract` are named because the v21
            // table requires a complete source identity on every row; the
            // contract bytes are a fixture stand-in, this test never reads
            // them back.
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, created_at,
                 source_chain, source_contract)
             VALUES (1, 'SolToGlc', 'SettlementAuthorized', ?1, X'AA', 0, 'solana', X'AB')",
            [IN_FLIGHT_BROADCAST_VALUE as i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at, broadcast_at)
             VALUES (1, X'AB', ?1, 0, 0, X'CD', 'Broadcast', 0, 0)",
            [IN_FLIGHT_BROADCAST_VALUE as i64],
        )
        .unwrap();
    }

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let (vault, _vault_signers) = vault_and_signers();
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault.script_pubkey_hex(),
        amount: FRESH_LIVE_BALANCE as f64 / 100_000_000.0,
        confirmations: 20,
        solvable: true,
    }]);

    let solana_rpc = Arc::new(MockSolanaRpc::new());
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, NUM_REQUESTS),
    );
    let reserve_authority = accounts::reserve_authority_pda();
    let ata = accounts::associated_token_address(
        &reserve_authority,
        &Pubkey::new_from_array(mint),
        &spl_token::ID,
    );
    solana_rpc.set_account(ata, fake_token_account_bytes(10_000_000));
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    // Distinct recipients AND distinct source wallets: six real obligations
    // arriving in the same tick from the same wallet, or to the same
    // recipient, would now also trip one of the (unrelated) SolToGlc rate
    // limits, which is not what this test targets — it isolates the
    // pre-admission-reconciliation/capacity mechanic only.
    for i in 0..NUM_REQUESTS {
        solana_rpc.set_account(
            accounts::withdrawal_obligation_pda(i),
            fake_withdrawal_obligation_bytes(
                i,
                GROSS_SOLANA_ATOMIC,
                &distinct_test_wallet(i),
                &distinct_test_recipient(i),
            ),
        );
    }

    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        solana_rpc,
        vault,
        vault_signers,
        attestation_signers(),
    );

    let report = orchestrator.tick(10).await;

    // The pre-admission pass sees a real drop (stale cache vs. live
    // reality) but it's fully explained by the phantom request's
    // already-broadcast payout, so it refreshes total_reserve_balance to
    // the fresh, lower figure — which is what admission then sees — and
    // reports InFlightExplained, not a breach.
    let pre_admission = report
        .goldcoin_pre_admission_reconciliation
        .unwrap()
        .unwrap();
    assert_eq!(
        pre_admission.classification,
        crate::reconciliation::Classification::InFlightExplained
    );

    // The six real obligations folded this tick get ids 2..=7 (id 1 is the
    // phantom inserted above).
    let mut finalized = 0u32;
    let mut manual_review = 0u32;
    for i in 2..=(NUM_REQUESTS as i64 + 1) {
        match orchestrator.ledger().get_request(i).unwrap().unwrap().state {
            RequestState::SourceFinalized => finalized += 1,
            RequestState::ManualReview => manual_review += 1,
            other => panic!("request {i}: unexpected state {other:?}"),
        }
    }
    assert_eq!(
        finalized, 5,
        "exactly five of the six should fit against the fresh balance"
    );
    assert_eq!(
        manual_review, 1,
        "exactly one should be parked, not silently admitted"
    );

    // The whole direction must NOT be paused — only the one oversized
    // request was parked, matching "park/reject only that new request
    // instead of admitting it and then auto-pausing the whole direction."
    assert!(
        !orchestrator
            .ledger()
            .is_paused(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        "admitting only what fits must never itself trigger an auto-pause"
    );

    // The invariant genuinely holds for what was actually admitted.
    orchestrator
        .ledger()
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(
        orchestrator
            .ledger()
            .reserve_snapshot(ReserveDirection::GoldcoinReserve)
            .unwrap()
            .2, // reserved_liquidity
        5 * 970_000_000_000,
        "five admissions at 9,700 GLC net each"
    );
}

#[tokio::test]
async fn goldcoin_reconciliation_pause_survives_a_simulated_crash_and_restart() {
    // Crash/restart companion to the breach test above: a pause set by one
    // orchestrator instance must still be in force after that instance is
    // dropped (simulated crash) and a fresh one is rebuilt against the
    // same on-disk ledger — pauses are never auto-cleared, including
    // across a restart (docs/09-runbook.md).
    let mint = [7u8; 32];
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
    }

    let solana_rpc = Arc::new(MockSolanaRpc::new());
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    let reserve_authority = accounts::reserve_authority_pda();
    let ata = accounts::associated_token_address(
        &reserve_authority,
        &Pubkey::new_from_array(mint),
        &spl_token::ID,
    );
    solana_rpc.set_account(ata, fake_token_account_bytes(10_000_000));

    // A fresh vault/signer set per orchestrator instance is fine here:
    // this test only exercises reconciliation/pause bookkeeping, never
    // vault-signed payout construction, so the two instances do not need
    // to share the same vault identity.
    let (vault1, vault_signers1) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::new(MockGoldcoinRpc::new()), // empty list_unspent -> unexplained drop
        Arc::clone(&solana_rpc),
        vault1,
        vault_signers1,
        attestation_signers(),
    );
    orchestrator.tick(10).await;
    assert!(orchestrator
        .ledger()
        .is_paused(ReserveDirection::GoldcoinReserve)
        .unwrap());
    drop(orchestrator); // simulated crash

    let (vault2, vault_signers2) = vault_and_signers();
    let mut restarted = build_orchestrator(
        &db_path,
        Arc::new(MockGoldcoinRpc::new()),
        solana_rpc,
        vault2,
        vault_signers2,
        attestation_signers(),
    );
    assert!(
        restarted
            .ledger()
            .is_paused(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        "pause must survive a restart"
    );
    let outcome = Ledger::open(&db_path)
        .unwrap()
        .create_request(
            Direction::SolToGlc,
            crate::ledger::RequestAmounts {
                gross_atomic: 1_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 1_000,
                net_destination_atomic: 1_000,
            },
            &[1u8; 32],
            None,
            3600,
            20,
        )
        .unwrap();
    assert_eq!(
        outcome,
        CreateRequestOutcome::Paused,
        "a restarted orchestrator must still fail closed on the paused direction, \
         never silently resuming settlement"
    );

    // A further tick observes the same (still-zero) balance it just cached,
    // so this tick's own delta is now zero — no *new* breach — but the
    // pause from the first tick is never lifted regardless: reconciliation
    // has no code path that clears a pause, only operator action does.
    let report = restarted.tick(20).await;
    assert_eq!(
        report
            .goldcoin_reconciliation
            .unwrap()
            .unwrap()
            .classification,
        crate::reconciliation::Classification::WithinTolerance,
        "the cache converged to the observed balance, so this tick sees no new drop"
    );
    assert!(restarted
        .ledger()
        .is_paused(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

// ------------------------------------- automatic UTXO-liquidity backlog recovery --

/// A distinct, VALID Goldcoin testnet P2PKH address per obligation index —
/// must decode successfully, since a resumed request's payout eventually
/// gets built and signed against it for real (`signing::goldcoin_vault`).
/// Several tests in this file fold multiple independent obligations in
/// close succession — since `Ledger::MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED`'s
/// 24h window would otherwise treat every one of them as the SAME
/// recipient re-depositing inside the window (an unrelated, newer
/// mechanic unless a test is deliberately exercising it), each obligation
/// gets its own synthetic recipient here so those tests continue to
/// isolate whichever mechanic they actually target.
fn distinct_test_recipient(obligation_index: u64) -> Vec<u8> {
    let mut hash = [0u8; 20];
    hash[..8].copy_from_slice(&obligation_index.to_be_bytes());
    crate::goldcoin::address::encode_p2pkh(&hash, crate::goldcoin::address::Network::Testnet)
        .into_bytes()
}

/// The source-wallet twin of `distinct_test_recipient`: a distinct 32-byte
/// "requester" per `obligation_index`, so tests that need many independent
/// obligations to isolate an UNRELATED mechanic (UTXO liquidity, capacity)
/// don't accidentally trip the source-wallet rate limit against each
/// other, the same way `distinct_test_recipient` already keeps them from
/// tripping the recipient rate limit.
fn distinct_test_wallet(obligation_index: u64) -> [u8; 32] {
    let mut wallet = [0u8; 32];
    wallet[..8].copy_from_slice(&obligation_index.to_be_bytes());
    wallet
}

/// A zero-protected-minimum GoldcoinReserve/SolanaReserve pair, with the
/// given UTXO-liquidity floor — isolates every test below to the
/// COUNT-based mechanic specifically, never the value-based invariant.
/// `initial_balance` MUST match the total value of whatever mature UTXOs
/// the test seeds immediately after calling this — otherwise the very
/// first reconciliation pass (every tick, pre-admission AND post) sees a
/// huge apparent "unexplained drop" between this cached figure and the
/// real observed mature balance, and auto-pauses before the test's actual
/// scenario ever gets a chance to run.
fn configure_auto_resume_reserve(ledger: &mut Ledger, floor: u32, initial_balance: u64) {
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            initial_balance,
            0,
            initial_balance.max(1),
            initial_balance.max(1),
            initial_balance.max(1),
            0,
        )
        .unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            10_000_000,
            0,
            5_000_000,
            2_000_000,
            1_000_000,
            0,
        )
        .unwrap();
    ledger
        .set_utxo_pool_thresholds(ReserveDirection::GoldcoinReserve, floor, floor + 5)
        .unwrap();
}

/// Seeds `count` mature vault UTXOs of `amount_atomic` each, distinct
/// txids, via a direct full-snapshot `sync_vault_utxos` call — a full
/// snapshot in its own right (see `utxo_liquidity_incident.rs`'s module
/// docs on why `sync_vault_utxos` must always be called with the complete
/// currently-true set).
fn seed_mature_vault_utxos(
    ledger: &mut Ledger,
    vault: &MultisigVault,
    count: u8,
    amount_atomic: u64,
) {
    let entries: Vec<_> = (0..count)
        .map(|i| {
            let mut txid = [0xE0u8; 32];
            txid[1] = i;
            (
                VaultUtxo {
                    txid,
                    vout: 0,
                    amount_atomic,
                    script_pubkey_hex: vault.script_pubkey_hex(),
                },
                10,
                vault.script_pubkey_hex(),
            )
        })
        .collect();
    ledger.sync_vault_utxos(&entries, 1, 0).unwrap();
}

/// Folds `count` distinct SolToGlc obligations (indices starting at
/// `first_obligation_index`) against whatever the pool currently looks
/// like, asserting every single one parks specifically for
/// `utxo_liquidity_low_at_fold` — never any other reason, or the test
/// setup itself is wrong. Returns the parked request ids in fold order
/// (== creation order == oldest-first).
fn park_utxo_liquidity_requests(
    ledger: &mut Ledger,
    first_obligation_index: u64,
    count: u64,
) -> Vec<i64> {
    (0..count)
        .map(|i| {
            let obligation_index = first_obligation_index + i;
            let outcome = ledger
                .fold_sol_deposit(
                    obligation_index,
                    sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                    distinct_test_wallet(obligation_index),
                    &distinct_test_recipient(obligation_index),
                    None,
                    obligation_index as i64,
                )
                .unwrap();
            let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
                panic!("obligation {obligation_index}: expected ManualReview, got {outcome:?}")
            };
            assert_eq!(
                ledger
                    .get_request(request_id)
                    .unwrap()
                    .unwrap()
                    .manual_review_note
                    .as_deref(),
                Some("utxo_liquidity_low_at_fold"),
                "obligation {obligation_index}: must park for liquidity specifically, or this \
                 test's setup is wrong"
            );
            request_id
        })
        .collect()
}

/// Mirrors every currently-`Available` `vault_utxos` row into the mock
/// RPC's own `list_unspent` results. Required after any test seeds vault
/// UTXOs directly via `sync_vault_utxos` (bypassing the mock entirely):
/// without this, the very first `tick_vault_utxos` full-snapshot resync
/// would see NOTHING observed via `list_unspent` and mark every
/// directly-seeded UTXO 'Spent' (`Ledger::sync_vault_utxos`'s full-snapshot
/// contract), collapsing the pool to zero and triggering a spurious
/// reconciliation breach before the test's actual scenario ever runs.
fn sync_mock_unspent_from_ledger(goldcoin_rpc: &MockGoldcoinRpc, db_path: &std::path::Path) {
    let ledger = Ledger::open(db_path).unwrap();
    let entries = ledger
        .available_vault_utxos()
        .unwrap()
        .into_iter()
        .map(|u| crate::goldcoin::rpc::ListUnspentEntry {
            txid: crate::goldcoin::hex::encode(&u.txid),
            vout: u.vout,
            script_pub_key: u.script_pubkey_hex,
            amount: u.amount_atomic as f64 / 100_000_000.0,
            confirmations: 10,
            solvable: true,
        })
        .collect();
    goldcoin_rpc.set_unspent(entries);
}

fn bare_orchestrator(
    db_path: &std::path::Path,
    goldcoin_rpc: Arc<MockGoldcoinRpc>,
    vault: MultisigVault,
    vault_signers: Vec<Box<dyn VaultSigner>>,
) -> Orchestrator<Arc<MockGoldcoinRpc>, Arc<MockSolanaRpc>> {
    // No Solana accounts configured at all: these tests are entirely about
    // the Goldcoin-side ManualReview backlog and never fold anything via a
    // real Solana obligation scan, so `solana_indexer.tick()` and
    // `tick_rolling_volume_quota` see nothing and record harmless,
    // per-phase-isolated errors in unrelated `TickReport` fields — never
    // touching `paused` (a missing `bridge_config` account short-circuits
    // `tick_rolling_volume_quota` before it ever reaches
    // `enforce_rolling_volume_quota`).
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    build_orchestrator(
        db_path,
        goldcoin_rpc,
        solana_rpc,
        vault,
        vault_signers,
        attestation_signers(),
    )
}

/// Like [`bare_orchestrator`], with a `max_auto_resumes_per_tick` override
/// instead of `base_config()`'s default of 20 — used to prove oldest-first
/// draining is bounded by this cap even when the mature pool itself has
/// ample room for more.
fn bare_orchestrator_with_max_auto_resumes(
    db_path: &std::path::Path,
    goldcoin_rpc: Arc<MockGoldcoinRpc>,
    vault: MultisigVault,
    vault_signers: Vec<Box<dyn VaultSigner>>,
    max_auto_resumes_per_tick: usize,
) -> Orchestrator<Arc<MockGoldcoinRpc>, Arc<MockSolanaRpc>> {
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let goldcoin_indexer = Indexer::new(
        Arc::clone(&goldcoin_rpc),
        Ledger::open(db_path).unwrap(),
        indexer_config(),
    );
    let solana_indexer = SolanaIndexer::new(
        Arc::clone(&solana_rpc),
        Ledger::open(db_path).unwrap(),
        crate::amount_conversion::BRIDGE_FEE_BPS,
    )
    .with_source_minimum_for_tests(crate::amount_conversion::CanonicalAtomic(1));
    let ledger = Ledger::open(db_path).unwrap();
    let mut config = base_config();
    config.max_auto_resumes_per_tick = max_auto_resumes_per_tick;
    Orchestrator::new(
        goldcoin_indexer,
        solana_indexer,
        ledger,
        goldcoin_rpc,
        solana_rpc,
        vault,
        vault_signers,
        attestation_signers(),
        Keypair::new(),
        config,
        0,
    )
    .with_source_minimum_for_tests(crate::amount_conversion::CanonicalAtomic(1))
}

/// Test 1 (automatic recovery after change matures): a real payout is
/// built and broadcast for one obligation, consuming one of two mature
/// chunks and leaving the pool exactly at the configured floor. A second
/// obligation then genuinely parks for `utxo_liquidity_low_at_fold`. While
/// the first payout's own change is still immature, auto-resume correctly
/// does nothing. Once that change matures past `vault_min_confirmations`
/// (simulated via the mock's `list_unspent` results, exactly like a real
/// chain scan a few blocks later), the very next tick resumes the second
/// request automatically — no `glc-admin resume-manual-review` call
/// anywhere in this test — and its own payout then builds normally on the
/// tick after that, proving the full loop closes end to end.
#[tokio::test]
async fn auto_resume_recovers_after_the_triggering_payouts_change_matures() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let vault_script = vault.script_pubkey_hex();

    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    // Comfortably more than the payout needs, so a real, non-dust change
    // output is created rather than folded entirely into the fee — and
    // large enough that, once matured, the remaining chunk plus the
    // matured change together still cover BOTH obligations'
    // `reserved_liquidity` (request0's net destination stays counted
    // there until full cross-chain settlement, not just broadcast — this
    // is correct, pre-existing accounting behavior this test must budget
    // for, not something to work around).
    let chunk_amount = goldcoin_payout_atomic + 30_000_000;

    let request0 = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 1, 2 * chunk_amount);
        seed_mature_vault_utxos(&mut ledger, &vault, 2, chunk_amount);
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                &distinct_test_recipient(0),
                None,
                0,
            )
            .unwrap()
        else {
            panic!("2 mature chunks against floor 1 must admit the first obligation normally")
        };
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator =
        bare_orchestrator(&db_path, Arc::clone(&goldcoin_rpc), vault, vault_signers);

    // Tick 1: request0's payout builds and broadcasts, consuming one
    // chunk. Available count drops from 2 to 1 == floor.
    let report = orchestrator.tick(10).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request0)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
    let payout_txid = payout.txid.unwrap();
    let payout_full = orchestrator
        .ledger()
        .get_goldcoin_payout_full(request0)
        .unwrap()
        .unwrap();
    assert_eq!(
        payout_full.change_outputs.len(),
        1,
        "this scenario is only realistic with a single change output"
    );
    let change_amount = payout_full.change_outputs[0];

    // The untouched chunk's REAL identity, read back from the ledger —
    // never hand-typed, since `coin::select`'s deterministic tie-break
    // (smallest txid first, both chunks being equal amounts) decides
    // which of the two original chunks was actually consumed, not this
    // test.
    let remaining_chunk = orchestrator.ledger().available_vault_utxos().unwrap();
    assert_eq!(
        remaining_chunk.len(),
        1,
        "exactly one chunk must remain untouched"
    );
    let remaining_chunk_entry = crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&remaining_chunk[0].txid),
        vout: remaining_chunk[0].vout,
        script_pub_key: remaining_chunk[0].script_pubkey_hex.clone(),
        amount: remaining_chunk[0].amount_atomic as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    };

    // Obligation 1 now genuinely parks: available count is 1, not > floor 1.
    let request1 = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        park_utxo_liquidity_requests(&mut ledger, 1, 1)[0]
    };

    // Tick 2: the change output is observed on-chain but still immature
    // (0 confirmations < vault_min_confirmations 1). Auto-resume must do
    // nothing yet.
    goldcoin_rpc.set_unspent(vec![
        remaining_chunk_entry.clone(),
        crate::goldcoin::rpc::ListUnspentEntry {
            txid: crate::goldcoin::hex::encode(&payout_txid),
            vout: 1,
            script_pub_key: vault_script.clone(),
            amount: change_amount as f64 / 100_000_000.0,
            confirmations: 0,
            solvable: true,
        },
    ]);
    let report = orchestrator.tick(20).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.resumed, 0, "change is still immature");
    assert_eq!(
        ledger_state(&orchestrator, request1),
        RequestState::ManualReview,
        "must remain parked while genuinely immature"
    );

    // Tick 3: the change matures to 6 confirmations — a real chain scan a
    // few blocks later. Auto-resume now succeeds, with no operator action
    // of any kind.
    goldcoin_rpc.set_unspent(vec![
        remaining_chunk_entry,
        crate::goldcoin::rpc::ListUnspentEntry {
            txid: crate::goldcoin::hex::encode(&payout_txid),
            vout: 1,
            script_pub_key: vault_script.clone(),
            amount: change_amount as f64 / 100_000_000.0,
            confirmations: 6,
            solvable: true,
        },
    ]);
    let report = orchestrator.tick(30).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.resumed, 1, "errors: {:?}", report.errors);
    assert_eq!(
        ledger_state(&orchestrator, request1),
        RequestState::SourceFinalized
    );

    // Tick 4: the resumed request's own payout now builds normally too —
    // the loop closes end to end, exactly like a fresh admission would.
    let report = orchestrator.tick(40).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout1 = orchestrator
        .ledger()
        .get_goldcoin_payout(request1)
        .unwrap()
        .unwrap();
    assert_eq!(payout1.state, "Broadcast");
}

fn ledger_state(
    orchestrator: &Orchestrator<Arc<MockGoldcoinRpc>, Arc<MockSolanaRpc>>,
    request_id: i64,
) -> RequestState {
    orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap()
        .state
}

/// Test 2 (multiple parked requests drained oldest-first): four requests
/// park for liquidity; the pool then recovers fully (ample mature UTXOs
/// for all four — resuming alone never consumes a UTXO, only an actual
/// payout build does, so the count-based check alone cannot distinguish
/// candidates within one batch). Draining is bounded instead by
/// `max_auto_resumes_per_tick = 2`, so auto-resume must drain the two
/// OLDEST first, leaving the two newest still parked.
#[tokio::test]
async fn auto_resume_drains_multiple_parked_requests_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let request_ids = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 5, 5 * 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 5, 100_000_000_000);
        park_utxo_liquidity_requests(&mut ledger, 0, 4)
    };
    assert_eq!(request_ids, {
        let mut sorted = request_ids.clone();
        sorted.sort();
        sorted
    });

    // Liquidity recovers fully (20 mature UTXOs, comfortably above the
    // floor of 5) — the mature pool itself has ample room for all 4.
    // Draining is bounded instead by `max_auto_resumes_per_tick = 2`, so
    // this test proves genuine oldest-first ordering under that cap,
    // rather than "drain everything that fits."
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        seed_mature_vault_utxos(&mut ledger, &vault, 20, 100_000_000_000);
    }

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator =
        bare_orchestrator_with_max_auto_resumes(&db_path, goldcoin_rpc, vault, vault_signers, 2);
    let report = orchestrator.tick(10).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.resumed, 2, "errors: {:?}", report.errors);

    assert_eq!(
        ledger_state(&orchestrator, request_ids[0]),
        RequestState::SourceFinalized,
        "the oldest must be resumed first"
    );
    assert_eq!(
        ledger_state(&orchestrator, request_ids[1]),
        RequestState::SourceFinalized,
        "the second-oldest must be resumed next"
    );
    assert_eq!(
        ledger_state(&orchestrator, request_ids[2]),
        RequestState::ManualReview,
        "the third request must remain parked — draining stops once max_auto_resumes_per_tick is reached"
    );
    assert_eq!(
        ledger_state(&orchestrator, request_ids[3]),
        RequestState::ManualReview,
        "the newest request must remain parked"
    );
}

/// Test 3 (stop at UTXO floor): liquidity never recovers at all — the pool
/// stays exactly at the configured floor. Auto-resume must attempt (and
/// fail) exactly the oldest candidate, then stop immediately, leaving
/// every other parked request untouched.
#[tokio::test]
async fn auto_resume_stops_immediately_at_the_utxo_floor() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let request_ids = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 5, 5 * 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 5, 100_000_000_000);
        park_utxo_liquidity_requests(&mut ledger, 0, 3)
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);
    let report = orchestrator.tick(10).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(
        auto_resume.attempted, 1,
        "must attempt exactly the oldest, then stop"
    );
    assert_eq!(auto_resume.resumed, 0);
    assert!(
        auto_resume
            .stopped_reason
            .as_deref()
            .unwrap()
            .contains("utxo_liquidity_low"),
        "stopped_reason={:?}",
        auto_resume.stopped_reason
    );
    for id in request_ids {
        assert_eq!(ledger_state(&orchestrator, id), RequestState::ManualReview);
    }
}

/// Test 4 (stop on quota exhaustion): `GoldcoinReserve` is already
/// `paused` — exactly what `quota::enforce_rolling_volume_quota` does on
/// exhaustion (the same flag a hard-invariant breach uses). Auto-resume
/// must not attempt anything at all, even though the mature pool itself
/// looks perfectly healthy.
#[tokio::test]
async fn auto_resume_stops_immediately_when_the_reserve_is_paused() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let request_ids = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 1, 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 1, 100_000_000_000);
        let ids = park_utxo_liquidity_requests(&mut ledger, 0, 1);
        // Liquidity recovers fully...
        seed_mature_vault_utxos(&mut ledger, &vault, 10, 100_000_000_000);
        // ...but the reserve is paused for an unrelated reason (mirrors
        // what `quota::enforce_rolling_volume_quota` does on exhaustion,
        // or what `reconciliation::reconcile` does on a hard-invariant
        // breach — auto-resume must treat both identically).
        ledger
            .set_paused(
                ReserveDirection::GoldcoinReserve,
                true,
                Some("simulated rolling-volume quota exhaustion"),
            )
            .unwrap();
        ids
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);
    let report = orchestrator.tick(10).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.attempted, 0, "must not even try while paused");
    assert_eq!(auto_resume.resumed, 0);
    assert!(auto_resume
        .stopped_reason
        .as_deref()
        .unwrap()
        .contains("paused"));
    assert_eq!(
        ledger_state(&orchestrator, request_ids[0]),
        RequestState::ManualReview
    );
}

/// Test 5 (never resumes unrelated ManualReview reasons): a request
/// parked for `insufficient_capacity_at_fold` — a totally different
/// reason, with plenty of mature UTXO liquidity available — must never be
/// touched, even though liquidity itself is perfectly healthy.
#[tokio::test]
async fn auto_resume_never_touches_unrelated_manual_review_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let unrelated_request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // Plenty of mature UTXOs (backpressure disabled): liquidity is
        // never the constraint here. `goldcoin_payout_atomic` is what
        // `sol_to_glc_amounts(500_000, ..)` will need as
        // `net_destination_atomic` — sizing the reserve to exactly one
        // such obligation's worth (protected_minimum 0) means the FIRST
        // one exhausts all accounting capacity and the SECOND genuinely
        // parks on `insufficient_capacity_at_fold`, never liquidity.
        let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
            crate::amount_conversion::SolanaAtomic(500_000)
                .to_canonical(TEST_SOLANA_DECIMALS)
                .unwrap(),
        )
        .unwrap()
        .net
        .0;
        configure_auto_resume_reserve(&mut ledger, 0, goldcoin_payout_atomic);
        seed_mature_vault_utxos(&mut ledger, &vault, 5, 100_000_000_000);
        let SolFoldOutcome::FoldedFinalized { .. } = ledger
            .fold_sol_deposit(
                100,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                &distinct_test_recipient(100),
                None,
                0,
            )
            .unwrap()
        else {
            panic!("the first obligation must exhaust capacity, not park")
        };
        let outcome = ledger
            .fold_sol_deposit(
                101,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                distinct_test_wallet(101),
                &distinct_test_recipient(101),
                None,
                1,
            )
            .unwrap();
        let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
            panic!("expected the second obligation to park on insufficient capacity")
        };
        assert_eq!(
            ledger
                .get_request(request_id)
                .unwrap()
                .unwrap()
                .manual_review_note
                .as_deref(),
            Some("insufficient_capacity_at_fold"),
            "this test's setup must genuinely exercise an UNRELATED reason"
        );
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);
    let report = orchestrator.tick(10).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(
        auto_resume.attempted, 0,
        "an unrelated reason must never even be attempted"
    );
    assert_eq!(
        ledger_state(&orchestrator, unrelated_request_id),
        RequestState::ManualReview
    );
}

/// Mirrors an arbitrary set of mature vault UTXOs into the mock's
/// `list_unspent`, WITHOUT touching the ledger — the outside world
/// changing under a running daemon, which is how a real reserve top-up
/// reaches the service (reconciliation observes it and refreshes
/// `total_reserve_balance`; `tick_vault_utxos` then syncs the pool).
fn set_mock_unspent(
    goldcoin_rpc: &MockGoldcoinRpc,
    vault: &MultisigVault,
    count: u8,
    amount_atomic: u64,
) {
    let entries = (0..count)
        .map(|i| {
            let mut txid = [0xB0u8; 32];
            txid[1] = i;
            crate::goldcoin::rpc::ListUnspentEntry {
                txid: crate::goldcoin::hex::encode(&txid),
                vout: 0,
                script_pub_key: vault.script_pubkey_hex(),
                amount: amount_atomic as f64 / 100_000_000.0,
                confirmations: 10,
                solvable: true,
            }
        })
        .collect();
    goldcoin_rpc.set_unspent(entries);
}

/// `liquidity_buffer_low_at_fold` auto-resume, both halves (added
/// 2026-09-04): a request the confirmed-liquidity safety buffer parked is
/// NOT retried while the direction-wide gate is still closed, and IS
/// retried, automatically, once headroom genuinely recovers to the reopen
/// threshold and the gate opens.
///
/// Before this, the reason was excluded from the auto-resume filter
/// outright, so such a request sat in `ManualReview` until a human ran
/// `glc-admin resume-manual-review` — even after the reserve had fully
/// recovered. The hysteresis it was excluded to protect is exactly what
/// now gates it: the gate reopens only at `admission_reopen_atomic`,
/// never on a single reading back over the close threshold.
#[tokio::test]
async fn liquidity_buffer_parked_request_auto_resumes_only_once_the_gate_reopens() {
    const UTXO_ATOMIC: u64 = 100_000_000;
    const BUFFER: u64 = 150_000_000;
    const REOPEN: u64 = 200_000_000;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let parked = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // UTXO backpressure disabled (floor 0) so the count-based gate can
        // never be what parks or blocks anything here — this test is about
        // the value-based buffer alone.
        configure_auto_resume_reserve(&mut ledger, 0, UTXO_ATOMIC);
        seed_mature_vault_utxos(&mut ledger, &vault, 1, UTXO_ATOMIC);
        ledger
            .set_admission_liquidity_thresholds(ReserveDirection::GoldcoinReserve, BUFFER, REOPEN)
            .unwrap();

        let outcome = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                distinct_test_wallet(0),
                &distinct_test_recipient(0),
                None,
                0,
            )
            .unwrap();
        let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
            panic!("the closed liquidity gate must park this fold, got {outcome:?}")
        };
        assert_eq!(
            ledger
                .get_request(request_id)
                .unwrap()
                .unwrap()
                .manual_review_note
                .as_deref(),
            Some("liquidity_buffer_low_at_fold"),
            "this test's setup must genuinely exercise the buffer reason"
        );
        assert!(ledger
            .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap());
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    set_mock_unspent(&goldcoin_rpc, &vault, 1, UTXO_ATOMIC);
    let mut orchestrator = bare_orchestrator(
        &db_path,
        Arc::clone(&goldcoin_rpc),
        vault.clone(),
        vault_signers,
    );

    // Tick 1 — headroom is still below the reopen threshold, so the gate
    // stays closed and the request is not even attempted. Retrying here is
    // what would defeat the hysteresis.
    let report = orchestrator.tick(10).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(
        auto_resume.attempted, 0,
        "a buffer-parked request must not be retried while the gate is closed"
    );
    assert_eq!(
        ledger_state(&orchestrator, parked),
        RequestState::ManualReview
    );
    assert!(orchestrator
        .ledger()
        .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    // The reserve is topped up in the outside world, to comfortably above
    // the reopen threshold.
    set_mock_unspent(&goldcoin_rpc, &vault, 3, UTXO_ATOMIC);

    // Tick 2 — reconciliation observes the new balance, the gate reopens
    // on a genuine recovery, and the same pass that has always drained the
    // other self-clearing reasons now drains this one too. No operator.
    let report = orchestrator.tick(20).await;
    let gate = report
        .goldcoin_admission_liquidity_gate
        .clone()
        .unwrap()
        .unwrap();
    assert!(!gate.closed, "headroom recovered past the reopen threshold");
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(
        auto_resume.resumed, 1,
        "errors: {:?}, auto_resume: {auto_resume:?}",
        report.errors
    );
    assert_eq!(
        ledger_state(&orchestrator, parked),
        RequestState::SourceFinalized
    );

    // Re-admission reserved capacity through the normal mechanism — the
    // buffer was honoured, not bypassed.
    let (balance, protected, reserved, _pending) = orchestrator
        .ledger()
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    let net = orchestrator
        .ledger()
        .get_request(parked)
        .unwrap()
        .unwrap()
        .net_destination_atomic;
    assert_eq!(reserved, net, "the resume applied the normal reservation");
    assert!(
        balance - protected - reserved >= BUFFER,
        "headroom left after re-admission must still clear the safety buffer"
    );
}

/// A buffer refusal is per-request, not a stop condition: an oversized
/// buffer-parked request that still cannot be re-admitted is SKIPPED, and
/// the eligible candidate behind it is resumed in the same pass. Treating
/// it as a batch-stopping error instead would let one request that never
/// fits stall automatic recovery for every other reason, every tick.
#[tokio::test]
async fn a_buffer_blocked_candidate_is_skipped_without_stalling_the_batch() {
    const UTXO_ATOMIC: u64 = 100_000_000;
    const BUFFER: u64 = 60_000_000;
    const REOPEN: u64 = 70_000_000;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let (too_big, eligible) = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 0, UTXO_ATOMIC);
        seed_mature_vault_utxos(&mut ledger, &vault, 1, UTXO_ATOMIC);
        ledger
            .set_admission_liquidity_thresholds(ReserveDirection::GoldcoinReserve, BUFFER, REOPEN)
            .unwrap();

        // Headroom (100_000_000) is above the close threshold, so the
        // direction-wide gate stays OPEN — this request parks on the
        // PER-REQUEST half of the buffer rule: it alone would eat into it.
        let outcome = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                distinct_test_wallet(0),
                &distinct_test_recipient(0),
                None,
                0,
            )
            .unwrap();
        let SolFoldOutcome::FoldedManualReview { request_id: big } = outcome else {
            panic!("expected the per-request buffer check to park this fold, got {outcome:?}")
        };
        assert_eq!(
            ledger
                .get_request(big)
                .unwrap()
                .unwrap()
                .manual_review_note
                .as_deref(),
            Some("liquidity_buffer_low_at_fold")
        );
        assert!(
            !ledger
                .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
                .unwrap(),
            "the direction-wide gate must stay open, or this is the wrong scenario"
        );

        // A second, smaller request parked for an UNRELATED self-clearing
        // reason (the count-based UTXO floor), which then clears. It sits
        // strictly behind the oversized one in oldest-first order.
        ledger
            .set_utxo_pool_thresholds(ReserveDirection::GoldcoinReserve, 5, 10)
            .unwrap();
        let outcome = ledger
            .fold_sol_deposit(
                1,
                sol_to_glc_amounts(300_000, TEST_SOLANA_DECIMALS),
                distinct_test_wallet(1),
                &distinct_test_recipient(1),
                None,
                1,
            )
            .unwrap();
        let SolFoldOutcome::FoldedManualReview { request_id: small } = outcome else {
            panic!("expected the UTXO floor to park this fold, got {outcome:?}")
        };
        assert_eq!(
            ledger
                .get_request(small)
                .unwrap()
                .unwrap()
                .manual_review_note
                .as_deref(),
            Some("utxo_liquidity_low_at_fold")
        );
        ledger
            .set_utxo_pool_thresholds(ReserveDirection::GoldcoinReserve, 0, 0)
            .unwrap();
        (big, small)
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    set_mock_unspent(&goldcoin_rpc, &vault, 1, UTXO_ATOMIC);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);

    let report = orchestrator.tick(10).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(
        auto_resume.skipped, 1,
        "the buffer-blocked candidate must be skipped: {auto_resume:?}"
    );
    assert_eq!(
        auto_resume.resumed, 1,
        "the eligible candidate behind it must still be resumed: {auto_resume:?}, errors: {:?}",
        report.errors
    );
    assert!(
        auto_resume.stopped_reason.is_none(),
        "a per-request buffer refusal must never stop the batch: {auto_resume:?}"
    );
    assert_eq!(
        ledger_state(&orchestrator, too_big),
        RequestState::ManualReview,
        "the request that still does not fit above the buffer stays parked — no bypass"
    );
    assert_eq!(
        ledger_state(&orchestrator, eligible),
        RequestState::SourceFinalized
    );
}

/// Test 6 (restart/idempotency): running the auto-resume pass, then
/// simulating a full daemon restart (a fresh `Orchestrator` built from the
/// same on-disk ledger), then running it again, must never double-resume,
/// error, or otherwise misbehave — the next tick simply continues from
/// whatever is still genuinely in `ManualReview`.
#[tokio::test]
async fn auto_resume_is_idempotent_across_a_simulated_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, _unused_signers) = vault_and_signers();

    let request_ids = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 1, 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 1, 100_000_000_000);
        let ids = park_utxo_liquidity_requests(&mut ledger, 0, 2);
        // Liquidity recovers enough for both.
        seed_mature_vault_utxos(&mut ledger, &vault, 10, 100_000_000_000);
        ids
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, Arc::clone(&goldcoin_rpc), vault.clone(), {
        let (_, signers) = vault_and_signers();
        signers
    });
    let report = orchestrator.tick(10).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.resumed, 2, "errors: {:?}", report.errors);
    for id in &request_ids {
        assert_eq!(
            ledger_state(&orchestrator, *id),
            RequestState::SourceFinalized
        );
    }

    // Simulate a full restart: a brand-new Orchestrator over the SAME
    // on-disk ledger (the outside world — `goldcoin_rpc` — persists
    // across a real restart too, so the same mock is reused).
    drop(orchestrator);
    let (_, restarted_vault_signers) = vault_and_signers();
    let mut restarted = bare_orchestrator(&db_path, goldcoin_rpc, vault, restarted_vault_signers);
    let report = restarted.tick(20).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(
        auto_resume.attempted, 0,
        "nothing is left in ManualReview for this reason — must be a safe no-op, not an error"
    );
    assert!(report.errors.is_empty() || report.errors.iter().all(|e| !e.contains("auto_resume")));
    for id in &request_ids {
        assert_eq!(
            ledger_state(&restarted, *id),
            RequestState::SourceFinalized,
            "must not have moved backwards or duplicated anything across the restart"
        );
    }
}

/// Test 7 (no duplicate payout construction/broadcast): an auto-resumed
/// request's payout builds exactly once across several further ticks —
/// `tick_goldcoin_payouts`'s own pre-existing guard (skip if a
/// `goldcoin_payouts` row already exists) is never bypassed by the
/// auto-resume path.
#[tokio::test]
async fn auto_resume_never_causes_a_duplicate_payout() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 1, 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 1, 100_000_000_000);
        let id = park_utxo_liquidity_requests(&mut ledger, 0, 1)[0];
        seed_mature_vault_utxos(&mut ledger, &vault, 10, 100_000_000_000);
        id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);

    // Tick 1: resumed by auto-resume (last phase); not yet built (payout
    // building already happened earlier in this SAME tick).
    let report = orchestrator.tick(10).await;
    assert_eq!(
        report
            .goldcoin_utxo_liquidity_auto_resume
            .clone()
            .unwrap()
            .resumed,
        1
    );
    assert!(orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .is_none());

    // Tick 2: now SourceFinalized, the payout builds for the first time.
    let report = orchestrator.tick(20).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);

    // Ticks 3-5: repeated ticks must never attempt a second payout for
    // the same request — `tick_goldcoin_payouts`'s existing
    // `get_goldcoin_payout(id) -> skip if Some` guard, untouched by this
    // feature, keeps doing its job.
    for now in [30, 40, 50] {
        let report = orchestrator.tick(now).await;
        assert_eq!(
            report.payouts_built, 0,
            "no second payout may ever be built for the same request"
        );
    }
    let all_payouts_for_request = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap();
    assert!(
        all_payouts_for_request.is_some(),
        "exactly one payout must exist"
    );
}

/// Test 8 (recipient rate limit auto-resume): a request parked
/// `recipient_rate_limited` must not auto-resume before its window
/// elapses, and must auto-resume normally — no operator action — once it
/// does. The same mechanism Tests 1-7 exercise for
/// `utxo_liquidity_low_at_fold`, now proven for the newer reason too
/// (docs/09-runbook.md's "SolToGlc recipient rate limit").
#[tokio::test]
async fn auto_resume_drains_a_recipient_rate_limited_request_once_its_window_clears() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let recipient = distinct_test_recipient(0);

    let parked_request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // UTXO backpressure disabled (floor 0) — isolates the recipient
        // rate limit specifically, never the liquidity mechanic.
        configure_auto_resume_reserve(&mut ledger, 0, 5 * 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 5, 100_000_000_000);
        let SolFoldOutcome::FoldedFinalized { .. } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                &recipient,
                None,
                1_000,
            )
            .unwrap()
        else {
            panic!("the first obligation to a fresh recipient must fold straight through")
        };
        let SolFoldOutcome::FoldedManualReview { request_id: parked } = ledger
            .fold_sol_deposit(
                1,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [2u8; 32],
                &recipient,
                None,
                1_000 + 10,
            )
            .unwrap()
        else {
            panic!("a second obligation to the SAME recipient inside the window must park")
        };
        assert_eq!(
            ledger
                .get_request(parked)
                .unwrap()
                .unwrap()
                .manual_review_note
                .as_deref(),
            Some("wallet_destination_24h_limit")
        );
        parked
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);

    // Still well inside the blocking request's 24h window (created_at
    // 1_000 + 86_400 has not elapsed) — must not resume, but must also
    // not be reported as a batch stop: this is a per-recipient, skip-only
    // condition.
    let report = orchestrator.tick(1_000 + 100).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.resumed, 0);
    assert_eq!(auto_resume.skipped, 1);
    assert_eq!(
        ledger_state(&orchestrator, parked_request_id),
        RequestState::ManualReview
    );

    // The window has now elapsed: automatic recovery, no operator action.
    let report = orchestrator.tick(1_000 + 86_400 + 1).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.resumed, 1, "errors: {:?}", report.errors);
    assert_eq!(auto_resume.skipped, 0);
    assert_eq!(
        ledger_state(&orchestrator, parked_request_id),
        RequestState::SourceFinalized
    );
}

/// Test 9 (independent recipients in one batch, skip vs. stop): two
/// DIFFERENT recipients each have their own `recipient_rate_limited`
/// candidate. One recipient's window has already cleared by tick time;
/// the other's has not. The still-blocked one must be SKIPPED — never a
/// batch stop — so the other, older-or-not, unrelated candidate still
/// drains in the SAME tick. This is what actually makes "oldest first"
/// true across a backlog of independent per-recipient conditions, not
/// just within a single recipient's own history.
///
/// UTXO liquidity is deliberately disabled (floor 0) throughout, isolating
/// the recipient-rate-limit skip-vs-stop behavior specifically — Tests
/// 1-7 above already cover the liquidity mechanic's own stop-the-batch
/// behavior in isolation.
#[tokio::test]
async fn auto_resume_skips_a_still_rate_limited_candidate_and_drains_the_next_eligible_one() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    const V: u64 = 100_000_000_000;

    // Recipient A's blocking deposit is old enough that its window has
    // already cleared by `tick_now` below; recipient B's is recent enough
    // that it has not.
    const TICK_NOW: i64 = 90_000;
    const RECIPIENT_A_BLOCKING_CREATED_AT: i64 = 0; // window ends 86_400 < TICK_NOW
    const RECIPIENT_B_BLOCKING_CREATED_AT: i64 = 89_000; // window ends 175_400 > TICK_NOW

    let (still_rate_limited_id, window_cleared_id) = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // UTXO backpressure disabled (floor 0): isolates the recipient
        // rate limit specifically.
        configure_auto_resume_reserve(&mut ledger, 0, V);
        seed_mature_vault_utxos(&mut ledger, &vault, 1, V);

        let recipient_a = distinct_test_recipient(1_000);
        ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                &recipient_a,
                None,
                RECIPIENT_A_BLOCKING_CREATED_AT,
            )
            .unwrap();
        let SolFoldOutcome::FoldedManualReview {
            request_id: window_cleared,
        } = ledger
            .fold_sol_deposit(
                1,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [2u8; 32],
                &recipient_a,
                None,
                RECIPIENT_A_BLOCKING_CREATED_AT + 10,
            )
            .unwrap()
        else {
            panic!("expected the second deposit to recipient A to park")
        };

        let recipient_b = distinct_test_recipient(2_000);
        ledger
            .fold_sol_deposit(
                2,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [3u8; 32],
                &recipient_b,
                None,
                RECIPIENT_B_BLOCKING_CREATED_AT,
            )
            .unwrap();
        let SolFoldOutcome::FoldedManualReview {
            request_id: still_rate_limited,
        } = ledger
            .fold_sol_deposit(
                3,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [4u8; 32],
                &recipient_b,
                None,
                RECIPIENT_B_BLOCKING_CREATED_AT + 10,
            )
            .unwrap()
        else {
            panic!("expected the second deposit to recipient B to park")
        };

        for id in [window_cleared, still_rate_limited] {
            assert_eq!(
                ledger
                    .get_request(id)
                    .unwrap()
                    .unwrap()
                    .manual_review_note
                    .as_deref(),
                Some("wallet_destination_24h_limit")
            );
        }
        assert!(
            window_cleared < still_rate_limited,
            "recipient A's candidate must be OLDER, so oldest-first ordering \
             actually exercises the skip-then-continue path"
        );

        (still_rate_limited, window_cleared)
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);

    let report = orchestrator.tick(TICK_NOW).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.attempted, 2, "both candidates must be tried");
    assert_eq!(
        auto_resume.skipped, 1,
        "the still-rate-limited candidate must be skipped, not stop the batch"
    );
    assert_eq!(
        auto_resume.resumed, 1,
        "errors: {:?} — the unrelated, eligible candidate behind it must still drain \
         in the SAME tick",
        report.errors
    );
    assert_eq!(
        auto_resume.stopped_reason, None,
        "a skip must never be reported as a batch stop"
    );
    assert_eq!(
        ledger_state(&orchestrator, still_rate_limited_id),
        RequestState::ManualReview
    );
    assert_eq!(
        ledger_state(&orchestrator, window_cleared_id),
        RequestState::SourceFinalized
    );
}

/// Test 10 (source-wallet rate limit auto-resume): the source-wallet twin
/// of Test 8 — a request parked `source_wallet_rate_limited` (the SAME
/// wallet depositing to a DIFFERENT recipient inside the window, the
/// exact bypass this dual limit closes) must not auto-resume before its
/// window elapses, and must auto-resume normally once it does.
#[tokio::test]
async fn auto_resume_drains_a_source_wallet_rate_limited_request_once_its_window_clears() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let wallet = [5u8; 32];

    let parked_request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 0, 5 * 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 5, 100_000_000_000);
        let SolFoldOutcome::FoldedFinalized { .. } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                wallet,
                &distinct_test_recipient(0),
                None,
                1_000,
            )
            .unwrap()
        else {
            panic!("the first obligation from a fresh wallet must fold straight through")
        };
        // A DIFFERENT recipient this time — only the source-wallet limit
        // can be why this parks.
        let SolFoldOutcome::FoldedManualReview { request_id: parked } = ledger
            .fold_sol_deposit(
                1,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                wallet,
                &distinct_test_recipient(1),
                None,
                1_000 + 10,
            )
            .unwrap()
        else {
            panic!("a second obligation from the SAME wallet inside the window must park")
        };
        assert_eq!(
            ledger
                .get_request(parked)
                .unwrap()
                .unwrap()
                .manual_review_note
                .as_deref(),
            Some("wallet_source_24h_limit")
        );
        parked
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);

    let report = orchestrator.tick(1_000 + 100).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.resumed, 0);
    assert_eq!(auto_resume.skipped, 1);
    assert_eq!(
        ledger_state(&orchestrator, parked_request_id),
        RequestState::ManualReview
    );

    let report = orchestrator.tick(1_000 + 86_400 + 1).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.resumed, 1, "errors: {:?}", report.errors);
    assert_eq!(auto_resume.skipped, 0);
    assert_eq!(
        ledger_state(&orchestrator, parked_request_id),
        RequestState::SourceFinalized
    );
}

/// Auto-resume must NEVER resume a request with a refund lifecycle, and a
/// refunded request in the backlog must not stall the drain of the others
/// (docs/09-runbook.md "ManualReview refunds"). Two independent layers
/// are exercised here: the candidate query (a refund moves the request
/// out of `ManualReview`, so it is never selected) and, for the
/// out-of-band-edit case, the ledger's own refusal.
#[tokio::test]
async fn auto_resume_never_resumes_a_request_with_a_refund_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let request_ids = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 5, 5 * 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 5, 100_000_000_000);
        let ids = park_utxo_liquidity_requests(&mut ledger, 0, 2);

        // Refund the FIRST (oldest) parked request, all the way to
        // terminal `Refunded`.
        let request = ledger.get_request(ids[0]).unwrap().unwrap();
        let verified = crate::ledger::VerifiedRefundInputs {
            obligation_index: request.source_obligation_index.unwrap(),
            amount_solana_atomic: 500_000,
            gross_canonical_atomic: request.gross_amount_atomic,
            requester: request.requester.unwrap(),
            destination_token_account: [0xDD; 32],
            reserve_mint: [0xEE; 32],
            token_program: [0xFF; 32],
        };
        ledger
            .begin_solana_refund(
                ids[0],
                &verified,
                "refunding the backlog head",
                "cli:test",
                5,
            )
            .unwrap();
        ledger
            .record_solana_refund_broadcast(ids[0], "sig", "hash", 0, 6)
            .unwrap();
        ledger.mark_solana_refund_confirmed(ids[0], 7).unwrap();

        // Liquidity recovers fully, so nothing but the refund itself can
        // hold the oldest request back.
        seed_mature_vault_utxos(&mut ledger, &vault, 20, 100_000_000_000);
        ids
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);
    let report = orchestrator.tick(10).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();

    assert_eq!(
        ledger_state(&orchestrator, request_ids[0]),
        RequestState::Refunded,
        "a refunded request must stay Refunded — auto-resume may never revive it"
    );
    assert_eq!(
        ledger_state(&orchestrator, request_ids[1]),
        RequestState::SourceFinalized,
        "the refunded request must not stall the rest of the backlog: errors {:?}",
        report.errors
    );
    assert_eq!(
        auto_resume.resumed, 1,
        "exactly the one non-refunded request may resume"
    );
}

/// Defense in depth for the same rule: with the request's STATE shoved
/// back to `ManualReview` out of band (a corrupted restore, a manual SQL
/// edit), the refund ROW alone must still make auto-resume refuse — and
/// skip, not stall the batch.
#[tokio::test]
async fn auto_resume_refuses_a_refund_row_even_if_the_state_was_edited_back() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let request_ids = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 5, 5 * 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 5, 100_000_000_000);
        let ids = park_utxo_liquidity_requests(&mut ledger, 0, 2);
        let request = ledger.get_request(ids[0]).unwrap().unwrap();
        let verified = crate::ledger::VerifiedRefundInputs {
            obligation_index: request.source_obligation_index.unwrap(),
            amount_solana_atomic: 500_000,
            gross_canonical_atomic: request.gross_amount_atomic,
            requester: request.requester.unwrap(),
            destination_token_account: [0xDD; 32],
            reserve_mint: [0xEE; 32],
            token_program: [0xFF; 32],
        };
        ledger
            .begin_solana_refund(ids[0], &verified, "refunding", "cli:test", 5)
            .unwrap();
        // The out-of-band edit: state says ManualReview again, but the
        // refund row is still there.
        ledger
            .raw()
            .execute(
                "UPDATE bridge_requests SET state = 'ManualReview' WHERE id = ?1",
                [ids[0]],
            )
            .unwrap();
        seed_mature_vault_utxos(&mut ledger, &vault, 20, 100_000_000_000);
        ids
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);
    let report = orchestrator.tick(10).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();

    assert_eq!(
        ledger_state(&orchestrator, request_ids[0]),
        RequestState::ManualReview,
        "the refund row alone must block the resume, regardless of the edited state"
    );
    assert_eq!(
        auto_resume.skipped, 1,
        "it must be SKIPPED, not stall the batch"
    );
    assert_eq!(
        ledger_state(&orchestrator, request_ids[1]),
        RequestState::SourceFinalized,
        "the rest of the backlog must still drain: errors {:?}",
        report.errors
    );
}

// ------------------------ GlcToSol refund confirmation reconciliation --

/// End-to-end through the real tick loop: a GlcToSol refund that was
/// broadcast long ago, whose transaction is already confirmed on Goldcoin,
/// is advanced to `Refunded` by the daemon — no operator, no second
/// transaction, and the stranded SolanaReserve reservation released once.
///
/// This is request #2477's shape. Before the reconciliation phase existed,
/// `Ledger::record_goldcoin_refund_confirmed` had no production caller at
/// all, so this row stayed `RefundBroadcast` forever however deep its
/// transaction got.
#[tokio::test]
async fn a_confirmed_glc_refund_is_reconciled_to_refunded_by_the_daemon_tick() {
    use crate::goldcoin::coin::VaultUtxo as CoinUtxo;
    use crate::goldcoin::rpc::{DecodedScriptPubKey, DecodedVin, DecodedVout};
    use crate::ledger::GoldcoinRefundState;

    const GROSS: u64 = 5_000_000;
    const OBSERVED: u64 = 4_900_000;
    const SENDER_HASH: [u8; 20] = [0x5A; 20];
    const REFUND_TXID: [u8; 32] = [0xF7; 32];
    const REFUND_INPUT_TXID: [u8; 32] = [0xC1; 32];
    const FEE: u64 = 50_000;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(
                Direction::GlcToSol,
                crate::ledger::RequestAmounts {
                    gross_atomic: GROSS,
                    fee_bps: 0,
                    fee_atomic: 0,
                    net_atomic: GROSS,
                    net_destination_atomic: GROSS,
                },
                &[0xABu8; 32],
                None,
                100_000,
                0,
            )
            .unwrap()
        else {
            panic!("expected a reservation")
        };
        // Parked on an amount mismatch, exactly as the indexer does.
        ledger
            .record_glc_deposit_observed(request_id, [0xAA; 32], 1, OBSERVED, 10, [0xBB; 32], 1)
            .unwrap();
        ledger
            .conn_for_tests()
            .execute(
                "INSERT INTO vault_utxos (txid, vout, amount_atomic, script_pubkey_hex,
                                          confirmations, first_seen_at, state)
                 VALUES (?1, 0, ?2, ?3, 50, 1, 'Available')",
                rusqlite::params![
                    REFUND_INPUT_TXID.as_slice(),
                    6_000_000i64,
                    vault.script_pubkey_hex()
                ],
            )
            .unwrap();
        ledger
            .begin_goldcoin_refund(
                request_id,
                OBSERVED,
                [0xEA; 32],
                0,
                SENDER_HASH,
                "mfTestSenderAddress1111111111111111",
                FEE,
                &[CoinUtxo {
                    txid: REFUND_INPUT_TXID,
                    vout: 0,
                    amount_atomic: 6_000_000,
                    script_pubkey_hex: vault.script_pubkey_hex(),
                }],
                "00",
                "refunding the mismatched deposit",
                "operator",
                2,
            )
            .unwrap();
        ledger
            .record_goldcoin_refund_signed(request_id, "00", 3)
            .unwrap();
        ledger
            .record_goldcoin_refund_broadcast(request_id, REFUND_TXID, 4)
            .unwrap();
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    // The chain's view: the refund transaction, already deep.
    goldcoin_rpc.set_raw_transaction(
        &crate::goldcoin::hex::encode(&REFUND_TXID),
        DecodedTransaction {
            txid: crate::goldcoin::hex::encode(&REFUND_TXID),
            vin: vec![DecodedVin {
                txid: Some(crate::goldcoin::hex::encode(&REFUND_INPUT_TXID)),
                vout: Some(0),
                coinbase: None,
            }],
            vout: vec![
                DecodedVout {
                    value: OBSERVED as f64 / 100_000_000.0,
                    n: 0,
                    script_pub_key: DecodedScriptPubKey {
                        hex: crate::goldcoin::address::p2pkh_script_hex(&SENDER_HASH),
                        kind: "pubkeyhash".to_string(),
                    },
                },
                DecodedVout {
                    value: (6_000_000u64 - OBSERVED - FEE) as f64 / 100_000_000.0,
                    n: 1,
                    script_pub_key: DecodedScriptPubKey {
                        hex: vault.script_pubkey_hex(),
                        kind: "scripthash".to_string(),
                    },
                },
            ],
            confirmations: Some(50),
        },
    );

    let mut orchestrator =
        bare_orchestrator(&db_path, Arc::clone(&goldcoin_rpc), vault, vault_signers);
    let report = orchestrator.tick(10).await;

    let pass = report
        .glc_refund_reconciliation
        .clone()
        .expect("every tick runs the refund reconciliation pass");
    assert_eq!(pass.checked, 1, "errors: {:?}", report.errors);
    assert_eq!(pass.confirmed, 1, "errors: {:?}", report.errors);
    assert_eq!(pass.mismatched, 0);

    let row = orchestrator
        .ledger()
        .get_goldcoin_refund(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, GoldcoinRefundState::Refunded);
    assert_eq!(
        row.txid,
        Some(REFUND_TXID),
        "the stored txid is authoritative and must survive reconciliation"
    );
    assert!(row.reservation_released);
    assert_eq!(
        ledger_state(&orchestrator, request_id),
        RequestState::Refunded
    );
    // The daemon never sent anything to reconcile a refund.
    assert!(
        goldcoin_rpc.chain.lock().unwrap().broadcasts.is_empty(),
        "reconciliation must never broadcast"
    );

    // A second tick is a clean no-op: the row has left the Broadcast query.
    let report = orchestrator.tick(20).await;
    let pass = report.glc_refund_reconciliation.clone().unwrap();
    assert_eq!(pass.checked, 0);
    assert_eq!(pass.confirmed, 0);
}

/// The same daemon path, one confirmation short of the configured payout
/// depth: the row is left exactly where it is, and no reservation is
/// released. Depth alone decides, and it decides inside the ledger.
#[tokio::test]
async fn a_shallow_glc_refund_is_left_alone_by_the_daemon_tick() {
    use crate::goldcoin::coin::VaultUtxo as CoinUtxo;
    use crate::goldcoin::rpc::{DecodedScriptPubKey, DecodedVin, DecodedVout};
    use crate::ledger::GoldcoinRefundState;

    const GROSS: u64 = 5_000_000;
    const OBSERVED: u64 = 4_900_000;
    const SENDER_HASH: [u8; 20] = [0x5A; 20];
    const REFUND_TXID: [u8; 32] = [0xF8; 32];
    const REFUND_INPUT_TXID: [u8; 32] = [0xC2; 32];
    const FEE: u64 = 50_000;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(
                Direction::GlcToSol,
                crate::ledger::RequestAmounts {
                    gross_atomic: GROSS,
                    fee_bps: 0,
                    fee_atomic: 0,
                    net_atomic: GROSS,
                    net_destination_atomic: GROSS,
                },
                &[0xABu8; 32],
                None,
                100_000,
                0,
            )
            .unwrap()
        else {
            panic!("expected a reservation")
        };
        ledger
            .record_glc_deposit_observed(request_id, [0xAA; 32], 1, OBSERVED, 10, [0xBB; 32], 1)
            .unwrap();
        ledger
            .conn_for_tests()
            .execute(
                "INSERT INTO vault_utxos (txid, vout, amount_atomic, script_pubkey_hex,
                                          confirmations, first_seen_at, state)
                 VALUES (?1, 0, ?2, ?3, 50, 1, 'Available')",
                rusqlite::params![
                    REFUND_INPUT_TXID.as_slice(),
                    6_000_000i64,
                    vault.script_pubkey_hex()
                ],
            )
            .unwrap();
        ledger
            .begin_goldcoin_refund(
                request_id,
                OBSERVED,
                [0xEA; 32],
                0,
                SENDER_HASH,
                "mfTestSenderAddress1111111111111111",
                FEE,
                &[CoinUtxo {
                    txid: REFUND_INPUT_TXID,
                    vout: 0,
                    amount_atomic: 6_000_000,
                    script_pubkey_hex: vault.script_pubkey_hex(),
                }],
                "00",
                "refunding the mismatched deposit",
                "operator",
                2,
            )
            .unwrap();
        ledger
            .record_goldcoin_refund_signed(request_id, "00", 3)
            .unwrap();
        ledger
            .record_goldcoin_refund_broadcast(request_id, REFUND_TXID, 4)
            .unwrap();
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let required = base_config().required_goldcoin_confirmations;
    goldcoin_rpc.set_raw_transaction(
        &crate::goldcoin::hex::encode(&REFUND_TXID),
        DecodedTransaction {
            txid: crate::goldcoin::hex::encode(&REFUND_TXID),
            vin: vec![DecodedVin {
                txid: Some(crate::goldcoin::hex::encode(&REFUND_INPUT_TXID)),
                vout: Some(0),
                coinbase: None,
            }],
            vout: vec![
                DecodedVout {
                    value: OBSERVED as f64 / 100_000_000.0,
                    n: 0,
                    script_pub_key: DecodedScriptPubKey {
                        hex: crate::goldcoin::address::p2pkh_script_hex(&SENDER_HASH),
                        kind: "pubkeyhash".to_string(),
                    },
                },
                DecodedVout {
                    value: (6_000_000u64 - OBSERVED - FEE) as f64 / 100_000_000.0,
                    n: 1,
                    script_pub_key: DecodedScriptPubKey {
                        hex: vault.script_pubkey_hex(),
                        kind: "scripthash".to_string(),
                    },
                },
            ],
            confirmations: Some(required - 1),
        },
    );

    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);
    let report = orchestrator.tick(10).await;
    let pass = report.glc_refund_reconciliation.clone().unwrap();
    assert_eq!(pass.checked, 1);
    assert_eq!(pass.confirmed, 0);
    assert_eq!(pass.pending, 1);

    let row = orchestrator
        .ledger()
        .get_goldcoin_refund(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, GoldcoinRefundState::Broadcast);
    assert_eq!(
        row.confirmations,
        required - 1,
        "the depth is still recorded"
    );
    assert!(!row.reservation_released);
    assert_eq!(
        ledger_state(&orchestrator, request_id),
        RequestState::RefundBroadcast
    );
}
// ---------------------------------------------------------------------------
// Test 10 (RhnToGlc auto-resume): the automatic-recovery pass covers BOTH
// inbound-to-Goldcoin routes, not just Solana.
//
// This is the piece that makes the Robinhood rate limits semantically equal
// to the Solana ones rather than merely stricter: a park must be a
// self-clearing 24-hour hold, not a hold until a human notices. Without the
// pass covering `RhnToGlc`, a rate-limited Robinhood deposit would sit in
// `ManualReview` indefinitely with only a refund as an exit.
// ---------------------------------------------------------------------------

const RHN_CANONICAL_SCALE: u128 = 10_000_000_000;

/// Stores a FINAL `RhnToGlc` observation and folds it through the real
/// fold path, returning the outcome.
fn fold_rhn_deposit(
    ledger: &mut Ledger,
    index: u64,
    destination: &[u8],
    depositor: [u8; 20],
    canonical: u64,
    now: i64,
) -> crate::robinhood::fold::FoldOutcome {
    let robinhood = u128::from(canonical) * RHN_CANONICAL_SCALE;
    let row = crate::ledger::RobinhoodObservationRow {
        id: index as i64 + 1,
        observation: crate::ledger::RobinhoodDepositObservation {
            source_contract: crate::robinhood::testkit::BRIDGE.to_bytes(),
            obligation_index: index,
            route: crate::routes::Route::RhnToGlc,
            depositor,
            destination: destination.to_vec(),
            amount_robinhood_atomic: crate::evm::EvmU256::from_u128(robinhood).to_be_bytes(),
            amount_canonical_atomic: canonical,
            tx_hash: {
                let mut h = [0xaa; 32];
                h[0] = index as u8;
                h
            },
            log_index: 0,
            block_number: 500,
            block_hash: [0xbb; 32],
        },
        finality: crate::ledger::RobinhoodFinality::Final,
        observed_at: 100,
        finalized_at: Some(200),
        reorged_at: None,
    };
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_deposit_observations
                (id, source_chain, source_contract, source_obligation_index, contract_route_id,
                 route, depositor, destination, amount_robinhood_atomic,
                 amount_canonical_atomic, tx_hash, log_index, block_number, block_hash,
                 finality, observed_at, finalized_at)
             VALUES (?1, 'robinhood', ?2, ?3, 2, 'RhnToGlc', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                     'Final', 100, 200)",
            rusqlite::params![
                row.id,
                &row.observation.source_contract[..],
                row.observation.obligation_index as i64,
                &row.observation.depositor[..],
                row.observation.destination,
                &row.observation.amount_robinhood_atomic[..],
                row.observation.amount_canonical_atomic as i64,
                &row.observation.tx_hash[..],
                row.observation.log_index as i64,
                row.observation.block_number as i64,
                &row.observation.block_hash[..],
            ],
        )
        .unwrap();
    crate::robinhood::fold::fold_observation(
        ledger,
        &row,
        crate::goldcoin::address::Network::Testnet,
        crate::amount_conversion::BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        true,
        now,
    )
    .unwrap()
}

#[tokio::test]
async fn auto_resume_drains_an_rhn_to_glc_recipient_rate_limited_request_once_its_window_clears() {
    use crate::robinhood::fold::FoldOutcome;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let recipient = distinct_test_recipient(0);

    let parked_request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 0, 5 * 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 5, 100_000_000_000);

        let FoldOutcome::FoldedFinalized { .. } =
            fold_rhn_deposit(&mut ledger, 0, &recipient, [0x01; 20], 500_000, 1_000)
        else {
            panic!("the first deposit to a fresh recipient must fold straight through")
        };
        // A DIFFERENT depositor, so the recipient rule alone is doing the
        // parking here.
        let FoldOutcome::FoldedManualReview { request_id: parked } =
            fold_rhn_deposit(&mut ledger, 1, &recipient, [0x02; 20], 500_000, 1_000 + 10)
        else {
            panic!("a second deposit to the SAME recipient inside the window must park")
        };
        assert_eq!(
            ledger
                .get_request(parked)
                .unwrap()
                .unwrap()
                .manual_review_note
                .as_deref(),
            Some("wallet_destination_24h_limit")
        );
        parked
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);

    // Inside the window: skipped, never a batch stop.
    let report = orchestrator.tick(1_000 + 100).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.resumed, 0);
    assert_eq!(auto_resume.skipped, 1);
    assert_eq!(
        ledger_state(&orchestrator, parked_request_id),
        RequestState::ManualReview
    );

    // Window elapsed: automatic recovery, no operator action — exactly the
    // SolToGlc behaviour.
    let report = orchestrator.tick(1_000 + 86_400 + 1).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.resumed, 1, "errors: {:?}", report.errors);
    assert_eq!(auto_resume.skipped, 0);
    assert_eq!(
        ledger_state(&orchestrator, parked_request_id),
        RequestState::SourceFinalized
    );
}

#[tokio::test]
async fn auto_resume_drains_an_rhn_to_glc_source_wallet_rate_limited_request() {
    use crate::robinhood::fold::FoldOutcome;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let wallet = [0x77; 20];

    let parked_request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 0, 5 * 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 5, 100_000_000_000);

        let FoldOutcome::FoldedFinalized { .. } = fold_rhn_deposit(
            &mut ledger,
            0,
            &distinct_test_recipient(0),
            wallet,
            500_000,
            1_000,
        ) else {
            panic!()
        };
        // Same wallet, DIFFERENT recipient — the bypass the source-wallet
        // limit closes.
        let FoldOutcome::FoldedManualReview { request_id: parked } = fold_rhn_deposit(
            &mut ledger,
            1,
            &distinct_test_recipient(1),
            wallet,
            500_000,
            1_000 + 10,
        ) else {
            panic!("a second deposit from the SAME wallet inside the window must park")
        };
        assert_eq!(
            ledger
                .get_request(parked)
                .unwrap()
                .unwrap()
                .manual_review_note
                .as_deref(),
            Some("wallet_source_24h_limit")
        );
        parked
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);

    let report = orchestrator.tick(1_000 + 100).await;
    assert_eq!(
        report
            .goldcoin_utxo_liquidity_auto_resume
            .clone()
            .unwrap()
            .skipped,
        1
    );
    let report = orchestrator.tick(1_000 + 86_400 + 1).await;
    assert_eq!(
        report
            .goldcoin_utxo_liquidity_auto_resume
            .clone()
            .unwrap()
            .resumed,
        1,
        "errors: {:?}",
        report.errors
    );
    assert_eq!(
        ledger_state(&orchestrator, parked_request_id),
        RequestState::SourceFinalized
    );
}

/// A mixed batch spanning BOTH routes, drained in one global oldest-first
/// order. The cross-route destination window means these are one queue for
/// one address, so the pass must not treat them as two independent lists.
#[tokio::test]
async fn auto_resume_drains_a_mixed_sol_and_rhn_backlog_in_one_global_order() {
    use crate::robinhood::fold::FoldOutcome;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let recipient = distinct_test_recipient(0);

    let (sol_parked, rhn_parked) = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 0, 5 * 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 5, 100_000_000_000);

        // A (Robinhood, t=1_000) takes the address's window.
        let FoldOutcome::FoldedFinalized { .. } =
            fold_rhn_deposit(&mut ledger, 0, &recipient, [0x01; 20], 500_000, 1_000)
        else {
            panic!()
        };
        // B (Solana, t=1_010) parks behind it — cross-route.
        let SolFoldOutcome::FoldedManualReview { request_id: b } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [2u8; 32],
                &recipient,
                None,
                1_010,
            )
            .unwrap()
        else {
            panic!("a Solana deposit must park behind a Robinhood payout to the same address")
        };
        // C (Robinhood, t=1_020) parks behind B.
        let FoldOutcome::FoldedManualReview { request_id: c } =
            fold_rhn_deposit(&mut ledger, 1, &recipient, [0x03; 20], 500_000, 1_020)
        else {
            panic!()
        };
        (b, c)
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator(&db_path, goldcoin_rpc, vault, vault_signers);

    // At A's boundary only B — the OLDEST parked row, whichever route it
    // is on — becomes eligible. C is skipped, because its own predecessor
    // B is still inside its window.
    let report = orchestrator.tick(1_000 + 86_400).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.resumed, 1, "errors: {:?}", report.errors);
    assert_eq!(auto_resume.skipped, 1);
    assert_eq!(
        ledger_state(&orchestrator, sol_parked),
        RequestState::SourceFinalized
    );
    assert_eq!(
        ledger_state(&orchestrator, rhn_parked),
        RequestState::ManualReview
    );

    // C drains once B's own window clears.
    let report = orchestrator.tick(1_010 + 86_400).await;
    assert_eq!(
        report
            .goldcoin_utxo_liquidity_auto_resume
            .clone()
            .unwrap()
            .resumed,
        1,
        "errors: {:?}",
        report.errors
    );
    assert_eq!(
        ledger_state(&orchestrator, rhn_parked),
        RequestState::SourceFinalized
    );
}

/// An operator hold (schema v29) removes a parked request from the
/// auto-resume sweep entirely: it is not attempted, it does not consume
/// the per-tick budget, and the unheld requests behind it drain as if it
/// were not there. A request folded AFTER the hold is placed is unheld
/// and resumes normally — the hold is per-row and never inherited.
#[tokio::test]
async fn a_held_request_is_skipped_by_auto_resume_and_new_folds_are_unaffected() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let request_ids = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 5, 5 * 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 5, 100_000_000_000);
        let ids = park_utxo_liquidity_requests(&mut ledger, 0, 3);
        // Hold the two OLDEST — exactly the ones the sweep would take
        // first — so the test can tell "skipped" from "not reached".
        for id in &ids[..2] {
            ledger
                .set_manual_review_hold(*id, Some(10 + 72 * 3600), "72h freeze", "cli:test", 10)
                .unwrap();
        }
        // A fold placed after the hold: unheld by construction.
        let later = park_utxo_liquidity_requests(&mut ledger, 10, 1);
        let later_row = ledger.get_request(later[0]).unwrap().unwrap();
        assert_eq!(later_row.auto_resume_hold_note, None);
        assert_eq!(later_row.auto_resume_hold_until, None);
        let mut all = ids;
        all.extend(later);
        all
    };

    // Liquidity recovers fully; budget of 2 per tick.
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        seed_mature_vault_utxos(&mut ledger, &vault, 20, 100_000_000_000);
    }
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator =
        bare_orchestrator_with_max_auto_resumes(&db_path, goldcoin_rpc, vault, vault_signers, 2);
    let report = orchestrator.tick(20).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(
        auto_resume.attempted, 2,
        "held rows must not even be attempted — errors: {:?}",
        report.errors
    );
    assert_eq!(auto_resume.resumed, 2, "errors: {:?}", report.errors);

    assert_eq!(
        ledger_state(&orchestrator, request_ids[0]),
        RequestState::ManualReview
    );
    assert_eq!(
        ledger_state(&orchestrator, request_ids[1]),
        RequestState::ManualReview
    );
    assert_eq!(
        ledger_state(&orchestrator, request_ids[2]),
        RequestState::SourceFinalized,
        "the oldest UNHELD request drains first"
    );
    assert_eq!(
        ledger_state(&orchestrator, request_ids[3]),
        RequestState::SourceFinalized,
        "a request folded after the hold is unheld and drains normally"
    );

    // Another tick: still nothing happens to the held rows, however
    // much room there is.
    let report = orchestrator.tick(30).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(auto_resume.attempted, 0);
    assert_eq!(
        ledger_state(&orchestrator, request_ids[0]),
        RequestState::ManualReview
    );
    assert_eq!(
        ledger_state(&orchestrator, request_ids[1]),
        RequestState::ManualReview
    );
}

/// A rapid-burst hold (schema v30) is never a candidate for the
/// auto-resume sweep — not when liquidity recovers, not when the route
/// reopens, not across a daemon restart (a fresh orchestrator over the
/// same ledger), and not once `review_after` has passed. The ordinary
/// parks around it, including one folded LATER by an unrelated wallet,
/// drain exactly as before; the held row never leaves `ManualReview`,
/// never enters a refund lifecycle, and never has anything reserved.
#[tokio::test]
async fn a_rapid_burst_hold_is_never_auto_resumed_whatever_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();

    let (held_id, normal_ids) = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_auto_resume_reserve(&mut ledger, 5, 5 * 100_000_000_000);
        seed_mature_vault_utxos(&mut ledger, &vault, 5, 100_000_000_000);
        ledger
            .set_rapid_burst_policy(
                &crate::ledger::RapidBurstPolicy {
                    enabled: true,
                    window_secs: 600,
                    max_per_source_wallet: 2,
                    max_per_destination_wallet: 2,
                    max_per_pair: 1,
                    minimum_review_hold_secs: 72 * 3600,
                },
                0,
            )
            .unwrap();
        // Two ordinary liquidity parks from unrelated wallets.
        let normal = park_utxo_liquidity_requests(&mut ledger, 0, 2);
        // A burst: the same pair twice, 10 s apart. The first is an
        // ordinary liquidity park; the second is HELD.
        let burst_wallet = distinct_test_wallet(900);
        let burst_recipient = distinct_test_recipient(900);
        let mut burst = Vec::new();
        for (i, at) in [(100u64, 100i64), (101, 110)] {
            let outcome = ledger
                .fold_sol_deposit(
                    i,
                    sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                    burst_wallet,
                    &burst_recipient,
                    None,
                    at,
                )
                .unwrap();
            let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
                panic!("{outcome:?}")
            };
            burst.push(request_id);
        }
        let held = ledger.get_request(burst[1]).unwrap().unwrap();
        assert_eq!(
            held.manual_review_disposition,
            crate::ledger::ManualReviewDisposition::RapidBurstHold
        );
        assert!(held.is_held());
        let first = ledger.get_request(burst[0]).unwrap().unwrap();
        assert_eq!(
            first.manual_review_disposition,
            crate::ledger::ManualReviewDisposition::Normal
        );
        // A fold placed after the hold, unrelated wallet: unheld.
        let later = park_utxo_liquidity_requests(&mut ledger, 10, 1);
        let mut normal_all = normal;
        normal_all.push(burst[0]);
        normal_all.extend(later);
        (burst[1], normal_all)
    };
    let review_after = Ledger::open(&db_path)
        .unwrap()
        .get_request(held_id)
        .unwrap()
        .unwrap()
        .review_after
        .unwrap();

    // Liquidity recovers fully; generous budget.
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        seed_mature_vault_utxos(&mut ledger, &vault, 30, 100_000_000_000);
    }
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    sync_mock_unspent_from_ledger(&goldcoin_rpc, &db_path);
    let mut orchestrator = bare_orchestrator_with_max_auto_resumes(
        &db_path,
        goldcoin_rpc.clone(),
        vault.clone(),
        vault_signers,
        20,
    );
    let report = orchestrator.tick(200).await;
    let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
    assert_eq!(
        auto_resume.attempted,
        normal_ids.len() as u32,
        "the held row must not be attempted — errors: {:?}",
        report.errors
    );
    for id in &normal_ids {
        assert_eq!(
            ledger_state(&orchestrator, *id),
            RequestState::SourceFinalized,
            "ordinary park {id} drains"
        );
    }
    assert_eq!(
        ledger_state(&orchestrator, held_id),
        RequestState::ManualReview
    );

    // "Restart": a fresh orchestrator over the same ledger, ticking well
    // past `review_after` — still nothing.
    drop(orchestrator);
    let (_, vault_signers) = vault_and_signers();
    let mut restarted =
        bare_orchestrator_with_max_auto_resumes(&db_path, goldcoin_rpc, vault, vault_signers, 20);
    for at in [review_after - 1, review_after, review_after + 30 * 86_400] {
        let report = restarted.tick(at).await;
        let auto_resume = report.goldcoin_utxo_liquidity_auto_resume.clone().unwrap();
        assert_eq!(auto_resume.attempted, 0, "at {at}: {:?}", report.errors);
        assert_eq!(
            ledger_state(&restarted, held_id),
            RequestState::ManualReview
        );
    }
    let ledger = Ledger::open(&db_path).unwrap();
    let row = ledger.get_request(held_id).unwrap().unwrap();
    assert!(row.is_held());
    assert_eq!(row.operator_decision, None);
    assert!(row.review_available(review_after + 30 * 86_400));
    assert!(
        ledger.get_solana_refund(held_id).unwrap().is_none(),
        "never auto-refunded"
    );
    assert_eq!(
        ledger.state_log(held_id).unwrap().last().unwrap().1,
        RequestState::ManualReview
    );
}

mod cross_route;
