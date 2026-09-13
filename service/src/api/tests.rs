use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction as SolanaTx;

use super::*;
use crate::ledger::ReserveDirection;
use crate::solana::rpc::SolanaRpcError;

struct FakeSolanaRpc {
    bridge_config: Vec<u8>,
    /// `(release/GlcToSol, deposit/SolToGlc)` `RollingVolumeWindow`
    /// account bytes — defaults to a fresh, unused (`window_total: 0`)
    /// window for each in [`build`], so existing tests that don't care
    /// about quota state see full remaining capacity, same as before this
    /// field existed.
    rolling_volume_windows: (Vec<u8>, Vec<u8>),
}

/// Mirrors `solana::accounts::tests::fake_rolling_volume_window_bytes`.
fn fake_rolling_volume_window_bytes(
    direction: u8,
    window_start: i64,
    window_total: u64,
) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.push(direction);
    v.extend_from_slice(&window_start.to_le_bytes());
    v.extend_from_slice(&window_total.to_le_bytes());
    v.push(4); // bump
    v.extend_from_slice(&[0u8; 16]); // reserved
    v
}

/// Matches the canonical Solana GLC mint's live decimals (docs/18-token-
/// 2022-support.md); `fake_bridge_config_bytes`'s `reserve_token_mint` is
/// always `[9u8; 32]`, so `FakeSolanaRpc` serves a fake mint account there
/// for `fetch_reserve_mint_decimals`'s live read (docs/20-bridge-fee.md).
const TEST_SOLANA_DECIMALS: u8 = 6;

/// A minimal, real 82-byte `spl_token::state::Mint`-shaped buffer — see
/// the matching helper in `signing::attestation::tests`.
fn fake_mint_bytes(decimals: u8) -> Vec<u8> {
    let mut v = vec![0u8; 82];
    v[44] = decimals;
    v[45] = 1; // is_initialized
    v
}

impl SolanaRpc for FakeSolanaRpc {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        if *pubkey == accounts::bridge_config_pda() {
            return Ok(Some(Account {
                lamports: 1,
                data: self.bridge_config.clone(),
                owner: accounts::PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            }));
        }
        if *pubkey == Pubkey::new_from_array([9u8; 32]) {
            return Ok(Some(Account {
                lamports: 1,
                data: fake_mint_bytes(TEST_SOLANA_DECIMALS),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            }));
        }
        if *pubkey == accounts::rolling_volume_window_pda(0) {
            return Ok(Some(Account {
                lamports: 1,
                data: self.rolling_volume_windows.0.clone(),
                owner: accounts::PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            }));
        }
        if *pubkey == accounts::rolling_volume_window_pda(1) {
            return Ok(Some(Account {
                lamports: 1,
                data: self.rolling_volume_windows.1.clone(),
                owner: accounts::PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            }));
        }
        Ok(None)
    }
    async fn get_multiple_accounts(
        &self,
        _pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        unimplemented!()
    }
    async fn send_transaction(&self, _tx: &SolanaTx) -> Result<Signature, SolanaRpcError> {
        unimplemented!()
    }
    async fn simulate_transaction(
        &self,
        _tx: &SolanaTx,
    ) -> Result<crate::solana::rpc::SimulationOutcome, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_signature_status(
        &self,
        _signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        unimplemented!()
    }
    async fn is_blockhash_valid(&self, _blockhash: &Hash) -> Result<bool, SolanaRpcError> {
        unimplemented!()
    }
}

/// Mirrors `solana::accounts::tests::fake_bridge_config_bytes`'s layout —
/// duplicated here (small, self-contained) rather than reused across a
/// private-module boundary.
/// `rolling_volume_limit` deliberately far above every capacity/amount
/// figure any existing (non-quota-specific) test in this module uses, so
/// it never becomes the binding constraint by accident — quota
/// exhaustion is exercised only by tests that explicitly configure a
/// tight `rolling_volume_limit`/`rolling_volume_windows` fixture via
/// [`fake_bridge_config_bytes_with_rolling_limit`].
const TEST_DEFAULT_ROLLING_VOLUME_LIMIT: u64 = 1_000_000_000_000;

/// The `per_transfer_limit` (reserve-mint units, 6 decimals) every
/// non-limit-specific fixture in this module serves — 0.05 GLC, i.e.
/// 5_000_000 canonical.
///
/// `SolToGlc`'s public availability is evaluated at THIS size (see
/// `api::AdmissionProbe`): the route reads `available` only if a deposit
/// of the program's full `per_transfer_limit` would be admitted. The
/// fixture reserves seed 10_000_000 canonical of headroom, so the probe
/// nets to 4_850_000 at the 300 bps test rate and fits comfortably —
/// every existing "healthy deployment is available" expectation holds
/// unchanged — while a test that wants the probe to be the binding
/// constraint sets the buffer or headroom explicitly and says so.
///
/// It used to be 1_000_000 (100_000_000 canonical), which no fixture
/// reserve could ever have admitted; that only went unnoticed because
/// availability was probed at one atomic unit, which is the bug this
/// module's `liquidity_buffer_probe` tests pin closed.
const TEST_PER_TRANSFER_LIMIT: u64 = 50_000;

fn fake_bridge_config_bytes(
    obligation_count: u64,
    min_transfer: u64,
    per_transfer: u64,
) -> Vec<u8> {
    fake_bridge_config_bytes_with_rolling_limit(
        obligation_count,
        min_transfer,
        per_transfer,
        TEST_DEFAULT_ROLLING_VOLUME_LIMIT,
    )
}

fn fake_bridge_config_bytes_with_rolling_limit(
    obligation_count: u64,
    min_transfer: u64,
    per_transfer: u64,
    rolling_volume_limit: u64,
) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.push(1); // protocol_version
    v.extend_from_slice(&[0u8; 32]); // admin
    v.push(0); // pending_admin: None
    v.push(0); // paused
    v.push(0); // release_paused
    v.push(0); // deposit_paused
    v.push(7); // bump
    v.extend_from_slice(&[9u8; 32]); // reserve_token_mint
    v.extend_from_slice(spl_token::ID.as_ref()); // reserve_token_program
    v.push(3); // reserve_authority_bump
    v.extend_from_slice(&obligation_count.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes()); // governance_timelock_seconds
    v.extend_from_slice(&min_transfer.to_le_bytes());
    v.extend_from_slice(&per_transfer.to_le_bytes());
    v.extend_from_slice(&500u64.to_le_bytes()); // protected_minimum
    v.extend_from_slice(&rolling_volume_limit.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes()); // rolling_window_seconds
    v
}

/// A real, node-verified 2-of-3 redeem script (same vector as
/// `goldcoin::vault::tests::REAL_REDEEM_SCRIPT`) — used here only to build
/// a `MultisigVault` for `BridgeApi::new`'s `root_vault` parameter; these
/// tests don't exercise custody/signing, just address derivation wiring.
const TEST_ROOT_REDEEM_SCRIPT: &str = "5221028e7147e643d67093dc8ca6a8fb888f1a452dddc62de991c7ed72080d65a421e42102f1c88ca7176c3ffee952ee6fae697991b257b6d53c3bc88e81cfe99adbcdbee5210256220bb7865197a40c4590ac80f12ef18e9063eac2eff92c4476ec27034042f953ae";

fn test_root_vault() -> crate::goldcoin::vault::MultisigVault {
    crate::goldcoin::vault::MultisigVault::from_redeem_script_hex(
        TEST_ROOT_REDEEM_SCRIPT,
        crate::goldcoin::address::Network::Testnet,
    )
    .unwrap()
}

/// The per-route fee table every API test builds against.
///
/// Deliberately gives the two Solana routes the compiled-in rate these
/// tests were written under (so every pre-existing amount expectation is
/// unchanged) and the two Robinhood routes a DIFFERENT one — because the
/// property most of these tests now have to be able to catch is a
/// Robinhood price leaking into a Solana quote, and a table where every
/// route charges the same thing cannot catch it.
fn test_route_fees() -> crate::fees::RouteFees {
    let mut fees = crate::fees::RouteFees::new();
    fees.insert(
        crate::routes::Route::GlcToSol,
        crate::amount_conversion::BRIDGE_FEE_BPS,
    )
    .unwrap();
    fees.insert(
        crate::routes::Route::SolToGlc,
        crate::amount_conversion::BRIDGE_FEE_BPS,
    )
    .unwrap();
    fees.insert(crate::routes::Route::GlcToRhn, 600).unwrap();
    fees.insert(crate::routes::Route::RhnToGlc, 600).unwrap();
    fees
}

/// The floor every constructor in this harness opts down to.
///
/// **This is a TEST opt-down, not the policy.** Production admits against
/// exactly 100 GLC (`crate::min_transfer::SOURCE_MINIMUM_CANONICAL`), set
/// unconditionally by `BridgeApi::new` and reachable by no config key —
/// which `the_production_default_source_minimum_is_one_hundred_glc` and
/// the boundary cases beside it prove on the DEFAULT path, with no opt-down
/// anywhere near them.
///
/// It exists because this file's fixtures predate the source minimum by a
/// long way: [`QUOTE_GROSS`] is 0.005 GLC and the reserves these tests seed
/// are 0.1 GLC. Those figures are load-bearing for what each test is
/// actually about — cursor pagination, fee rounding to the exact atomic
/// unit, reserve capacity arithmetic, route gating — and none of it is
/// about the source minimum. Scaling every amount, reserve, quota and
/// pinned expectation in the suite by four orders of magnitude to satisfy
/// a rule they do not exercise is how a genuine regression gets quietly
/// rewritten into agreement with a bug.
///
/// One atomic unit, so the only amount it still refuses is zero — which
/// the `amount_atomic must be > 0` checks own and several tests pin.
const TEST_SOURCE_MINIMUM: crate::amount_conversion::CanonicalAtomic =
    crate::amount_conversion::CanonicalAtomic(1);

/// Applies [`TEST_SOURCE_MINIMUM`]. Every constructor below ends in this
/// call, so "which harness bypasses the policy" has one answer and one
/// grep.
fn opt_down<R: SolanaRpc>(api: BridgeApi<R>) -> BridgeApi<R> {
    api.with_source_minimum_for_tests(TEST_SOURCE_MINIMUM)
}

fn build(db_path: &std::path::Path, obligation_count: u64) -> BridgeApi<FakeSolanaRpc> {
    opt_down(BridgeApi::new(
        db_path.to_path_buf(),
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes(obligation_count, 100, TEST_PER_TRANSFER_LIMIT),
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        test_root_vault(),
        crate::goldcoin::address::Network::Testnet,
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::routes::RouteGate::legacy_only()),
        test_route_fees(),
    ))
}

/// Like [`build`], but with an explicit `rolling_volume_limit` and each
/// direction's current `window_total` — for exercising quota-exhaustion
/// behavior deliberately, never by accident from an unrelated test's
/// capacity/amount figures.
fn build_with_rolling_volume(
    db_path: &std::path::Path,
    rolling_volume_limit: u64,
    release_window_total: u64,
    deposit_window_total: u64,
) -> BridgeApi<FakeSolanaRpc> {
    // `window_start` must be recent (close to real wall-clock `now_unix`),
    // never `0` — a `0` start would make every real bucket_age check
    // (`now - window_start`) enormous next to a 3_600s window, so
    // `rolling_volume_remaining` would always see it as an already-
    // expired/reset bucket and report full capacity regardless of
    // `window_total`, silently defeating the whole test.
    let window_start = now_unix() - 10;
    opt_down(BridgeApi::new(
        db_path.to_path_buf(),
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes_with_rolling_limit(
                0,
                100,
                TEST_PER_TRANSFER_LIMIT,
                rolling_volume_limit,
            ),
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, window_start, release_window_total),
                fake_rolling_volume_window_bytes(1, window_start, deposit_window_total),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        test_root_vault(),
        crate::goldcoin::address::Network::Testnet,
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::routes::RouteGate::legacy_only()),
        test_route_fees(),
    ))
}

fn configure(dir: &std::path::Path) -> std::path::PathBuf {
    let db_path = dir.join("ledger.sqlite3");
    let mut ledger = Ledger::open(&db_path).unwrap();
    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        ledger
            .configure_reserve(direction, 10_000_000, 0, 5_000_000, 2_000_000, 1_000_000, 0)
            .unwrap();
    }
    db_path
}

#[tokio::test]
async fn status_reports_pause_state_and_next_obligation_index() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 42);

    let status = api.status().await.unwrap();
    assert!(!status.goldcoin_paused);
    assert!(!status.solana_paused);
    assert_eq!(status.next_solana_obligation_index, 42);
    assert_eq!(status.vault_address, "REGTESTVAULTADDRESSXXXXXXXXXXXXX");
}

#[tokio::test]
async fn status_reflects_a_paused_direction() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_paused(ReserveDirection::GoldcoinReserve, true, Some("test"))
            .unwrap();
    }
    let api = build(&db_path, 0);
    let status = api.status().await.unwrap();
    assert!(status.goldcoin_paused);
    assert!(!status.solana_paused);
}

#[tokio::test]
async fn limits_reflects_the_live_bridge_config() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let limits = api.limits().await.unwrap();
    assert_eq!(limits.min_transfer_amount.0, 100);
    assert_eq!(limits.per_transfer_limit.0, TEST_PER_TRANSFER_LIMIT);
    assert_eq!(
        limits.bridge_fee_bps,
        amount_conversion::BRIDGE_FEE_BPS,
        "the fee rate must be the fixed protocol constant, discoverable without a quote"
    );
}

/// Same pass-through as `limits_reflects_the_live_bridge_config`, but at
/// the REAL production values of the 2026-08-29 update
/// (docs/22-production-readiness-review.md): `per_transfer_limit` =
/// 20,000 GLC = 20_000_000_000 (6-decimal mint units),
/// `min_transfer_amount` = 99 GLC = 99_000_000 (unchanged — the NET-side
/// floor; the UI derives its 102.061856 GLC gross entry minimum from
/// this figure plus `bridge_fee_bps`), `bridge_fee_bps` = 300 — pinned
/// literally, not via the constant, so an unintended constant change
/// fails a test instead of silently flowing to the public API.
#[tokio::test]
async fn limits_reports_the_production_values() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = BridgeApi::new(
        db_path,
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes(0, 99_000_000, 20_000_000_000),
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        test_root_vault(),
        crate::goldcoin::address::Network::Testnet,
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::routes::RouteGate::legacy_only()),
        test_route_fees(),
    );
    let limits = api.limits().await.unwrap();
    assert_eq!(limits.min_transfer_amount.0, 99_000_000);
    assert_eq!(limits.per_transfer_limit.0, 20_000_000_000);
    assert_eq!(limits.bridge_fee_bps, 300);
}

#[tokio::test]
async fn status_reports_direction_availability_reflecting_pause_and_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let status = api.status().await.unwrap();
    assert!(status.glc_to_sol_available);
    assert!(status.sol_to_glc_available);

    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // GlcToSol's destination is the Solana reserve.
        ledger
            .set_paused(ReserveDirection::SolanaReserve, true, Some("test"))
            .unwrap();
    }
    let status = api.status().await.unwrap();
    assert!(
        !status.glc_to_sol_available,
        "pausing the destination reserve must mark that direction unavailable"
    );
    assert!(
        status.sol_to_glc_available,
        "the other direction's destination reserve is untouched"
    );
}

#[tokio::test]
async fn status_reports_a_direction_unavailable_when_destination_capacity_is_exhausted() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for direction in [
            ReserveDirection::GoldcoinReserve,
            ReserveDirection::SolanaReserve,
        ] {
            // balance == protected_minimum: zero available capacity, but
            // not paused. critical_reserve must still exceed
            // protected_minimum (docs/05-reserve-accounting.md).
            ledger
                .configure_reserve(direction, 1_000, 1_000, 5_000, 2_000, 1_001, 0)
                .unwrap();
        }
    }
    let api = build(&db_path, 0);
    let status = api.status().await.unwrap();
    assert!(!status.goldcoin_paused);
    assert!(!status.solana_paused);
    assert!(
        !status.glc_to_sol_available,
        "zero available capacity must mark the direction unavailable even though nothing is paused"
    );
    assert!(!status.sol_to_glc_available);
}

/// Items 1/3/4 of the quota-exhausted -> operator-pause -> refill ->
/// manual-unpause workflow report: quota exhaustion is a distinct,
/// independently-reported state from pause and from reserve-capacity
/// constraint, and it blocks ONLY the affected direction — the opposite
/// direction, whose own window is untouched, must remain fully reported
/// as available.
#[tokio::test]
async fn status_reports_quota_exhausted_independently_per_direction() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    // release/GlcToSol window fully used against a 2_000_000 limit;
    // deposit/SolToGlc window untouched.
    let api = build_with_rolling_volume(&db_path, 2_000_000, 2_000_000, 0);
    let status = api.status().await.unwrap();

    assert!(!status.goldcoin_paused);
    assert!(!status.solana_paused);
    assert!(
        status.glc_to_sol_quota_exhausted,
        "GlcToSol's release window is fully used"
    );
    assert!(
        !status.sol_to_glc_quota_exhausted,
        "SolToGlc's own deposit window was never touched"
    );
    assert_eq!(status.glc_to_sol_rolling_volume_remaining.0, 0);
    assert_eq!(status.sol_to_glc_rolling_volume_remaining.0, 2_000_000);
    assert!(
        !status.glc_to_sol_available,
        "quota exhaustion alone (nothing paused, capacity otherwise fine) must still mark \
         the direction unavailable"
    );
    assert!(
        status.sol_to_glc_available,
        "the opposite direction, whose quota was never touched, must remain operational — \
         quota exhaustion blocks only the affected direction"
    );
}

/// Below the exhaustion threshold (`remaining >= min_transfer_amount`),
/// the direction must still report available — the check is "no legal
/// transfer fits", not "any volume has ever been used".
#[tokio::test]
async fn status_does_not_report_quota_exhausted_while_headroom_remains() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build_with_rolling_volume(&db_path, 2_000_000, 1_000_000, 500_000);
    let status = api.status().await.unwrap();

    assert!(!status.glc_to_sol_quota_exhausted);
    assert!(!status.sol_to_glc_quota_exhausted);
    assert_eq!(status.glc_to_sol_rolling_volume_remaining.0, 1_000_000);
    assert_eq!(status.sol_to_glc_rolling_volume_remaining.0, 1_500_000);
    assert!(status.glc_to_sol_available);
    assert!(status.sol_to_glc_available);
}

#[tokio::test]
async fn health_reports_healthy_when_nothing_is_wrong() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let health = api.health().await.unwrap();
    assert!(health.healthy);
    assert!(!health.goldcoin_indexer_halted);
    assert_eq!(health.manual_review_backlog, 0);
    assert_eq!(health.post_finality_reorg_events, 0);
}

#[tokio::test]
async fn health_reports_unhealthy_when_the_goldcoin_indexer_is_halted() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let indexer_status = Arc::new(crate::ops::indexer_status::IndexerStatus::new(0));
    indexer_status.record_halt(7);
    let api = BridgeApi::new(
        db_path,
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes(0, 100, TEST_PER_TRANSFER_LIMIT),
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        test_root_vault(),
        crate::goldcoin::address::Network::Testnet,
        3600,
        6,
        indexer_status,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::routes::RouteGate::legacy_only()),
        test_route_fees(),
    );
    let health = api.health().await.unwrap();
    assert!(!health.healthy);
    assert!(health.goldcoin_indexer_halted);
}

#[tokio::test]
async fn health_reports_unhealthy_after_a_post_finality_reorg_event() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .record_post_finality_reorg(5, 12, &[1, 2], 1_000)
            .unwrap();
    }
    let api = build(&db_path, 0);
    let health = api.health().await.unwrap();
    assert!(!health.healthy);
    assert_eq!(health.post_finality_reorg_events, 1);
    // Non-sensitive: the affected request ids and fork/tip heights are
    // never part of the public response, only the count.
}

// --------------------------------------------------------------- /stats --

#[tokio::test]
async fn stats_on_a_freshly_configured_ledger_reports_zero_counts_not_missing_fields() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let stats = api.stats().await.unwrap();
    assert!(!stats.goldcoin_paused);
    assert!(!stats.solana_paused);
    assert!(stats.glc_to_sol_available);
    assert!(stats.sol_to_glc_available);
    assert_eq!(stats.bridge_fee_bps, amount_conversion::BRIDGE_FEE_BPS);
    assert_eq!(stats.glc_to_sol.total_requests, 0);
    assert_eq!(stats.sol_to_glc.total_requests, 0);
    assert_eq!(stats.goldcoin_reserve.settled_volume_atomic.0, 0);
    assert_eq!(stats.solana_reserve.settled_volume_atomic.0, 0);
    assert!(!stats.goldcoin_indexer_halted);
    assert_eq!(stats.post_finality_reorg_events, 0);
}

#[tokio::test]
async fn stats_reflects_real_request_counts_by_direction_and_state() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    for _ in 0..3 {
        api.create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();
    }
    let stats = api.stats().await.unwrap();
    assert_eq!(stats.glc_to_sol.total_requests, 3);
    assert_eq!(
        stats.glc_to_sol.in_progress_requests, 3,
        "a freshly created request is AwaitingDeposit, an active state"
    );
    assert_eq!(stats.glc_to_sol.settled_requests, 0);
    assert_eq!(stats.glc_to_sol.manual_review_requests, 0);
    assert_eq!(stats.sol_to_glc.total_requests, 0);
}

// ----------------------------------------------------- /reserves/history --

#[tokio::test]
async fn reserves_history_on_an_empty_ledger_returns_an_empty_page_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let page = api.reserves_history(None, None, 50).await.unwrap();
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn reserves_history_returns_real_reconciliation_ticks_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for (i, balance) in [10_000_000u64, 10_050_000, 10_100_000]
            .into_iter()
            .enumerate()
        {
            crate::reconciliation::reconcile(
                &mut ledger,
                ReserveDirection::SolanaReserve,
                balance,
                1_000,
                1_000 + i as i64,
            )
            .unwrap();
        }
    }
    let api = build(&db_path, 0);
    let page = api.reserves_history(None, None, 50).await.unwrap();
    assert_eq!(page.items.len(), 3);
    // Newest first: the last reconcile() call (balance 10_100_000) leads.
    assert_eq!(page.items[0].observed_atomic.0, 10_100_000);
    assert_eq!(page.items[1].observed_atomic.0, 10_050_000);
    assert_eq!(page.items[2].observed_atomic.0, 10_000_000);
    assert!(
        page.items[0].id > page.items[1].id && page.items[1].id > page.items[2].id,
        "ids must be strictly descending"
    );
    assert!(page.next_cursor.is_none(), "fewer than `limit` rows exist");
    for item in &page.items {
        assert_eq!(item.direction, "SolanaReserve");
        assert_eq!(item.classification, "WITHIN_TOLERANCE");
        assert!(!item.auto_paused);
    }
}

#[tokio::test]
async fn reserves_history_filters_by_direction() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        crate::reconciliation::reconcile(
            &mut ledger,
            ReserveDirection::GoldcoinReserve,
            10_000_000,
            1_000,
            1_000,
        )
        .unwrap();
        crate::reconciliation::reconcile(
            &mut ledger,
            ReserveDirection::SolanaReserve,
            10_000_000,
            1_000,
            1_001,
        )
        .unwrap();
    }
    let api = build(&db_path, 0);
    let page = api
        .reserves_history(Some(ReserveDirection::GoldcoinReserve), None, 50)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].direction, "GoldcoinReserve");
}

#[tokio::test]
async fn reserves_history_cursor_pagination_walks_the_full_history_without_gaps_or_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for i in 0..5u64 {
            crate::reconciliation::reconcile(
                &mut ledger,
                ReserveDirection::SolanaReserve,
                10_000_000 + i * 1_000,
                1_000,
                1_000 + i as i64,
            )
            .unwrap();
        }
    }
    let api = build(&db_path, 0);
    let mut seen_ids = Vec::new();
    let mut cursor: Option<i64> = None;
    loop {
        let page = api.reserves_history(None, cursor, 2).await.unwrap();
        assert!(
            page.items.len() <= 2,
            "must never exceed the requested limit"
        );
        for item in &page.items {
            seen_ids.push(item.id);
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c.parse().unwrap()),
            None => break,
        }
    }
    assert_eq!(seen_ids.len(), 5, "every row must be visited exactly once");
    let mut sorted = seen_ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 5, "no id may repeat across pages");
    let mut descending = seen_ids.clone();
    descending.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        seen_ids, descending,
        "pages must compose into one strictly-descending sequence"
    );
}

#[tokio::test]
async fn reserves_history_limit_is_clamped_to_the_maximum() {
    // Clamping is an HTTP query-parsing concern (`parse_page_params`),
    // not something `ApiSource::reserves_history` itself re-enforces —
    // exercised here through the real HTTP server, the actual path a
    // client hits.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for i in 0..(MAX_PAGE_LIMIT + 5) {
            crate::reconciliation::reconcile(
                &mut ledger,
                ReserveDirection::SolanaReserve,
                10_000_000,
                1_000,
                1_000 + i as i64,
            )
            .unwrap();
        }
    }
    let (base, _tx) = spawn_real_server(&db_path, 0).await;
    let page: Page<ReserveHistoryEntry> =
        reqwest::get(format!("{base}/reserves/history?limit=1000000"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(
        page.items.len() as u32,
        MAX_PAGE_LIMIT,
        "a limit far beyond the maximum must be clamped, not rejected or taken literally"
    );
}

#[tokio::test]
async fn reserves_history_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        crate::reconciliation::reconcile(
            &mut ledger,
            ReserveDirection::SolanaReserve,
            10_000_000,
            1_000,
            1_000,
        )
        .unwrap();
    }
    // A fresh `BridgeApi` (and thus a fresh `Ledger::open` per call) is
    // exactly what a process restart looks like from this API's point of
    // view — there is no separate in-memory cache to lose.
    let api = build(&db_path, 0);
    let page = api.reserves_history(None, None, 50).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].observed_atomic.0, 10_000_000);
}

// ------------------------------------------------------- /explorer/events --

#[tokio::test]
async fn explorer_events_on_an_empty_ledger_returns_an_empty_page_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let page = api.explorer_events(None, None, None, 50).await.unwrap();
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn explorer_events_returns_real_state_transitions_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    // Each created transfer logs two real transitions: None->LiquidityReserved,
    // then LiquidityReserved->AwaitingDeposit (`Ledger::create_request`).
    let created = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();

    let page = api.explorer_events(None, None, None, 50).await.unwrap();
    assert_eq!(page.items.len(), 2);
    // Newest first: AwaitingDeposit was logged after LiquidityReserved.
    assert_eq!(page.items[0].to_state, "AwaitingDeposit");
    assert_eq!(
        page.items[0].from_state.as_deref(),
        Some("LiquidityReserved")
    );
    assert_eq!(page.items[1].to_state, "LiquidityReserved");
    assert_eq!(page.items[1].from_state, None);
    for item in &page.items {
        assert_eq!(item.request_id, created.request_id);
        assert_eq!(item.direction, "GlcToSol");
    }
    assert!(page.items[0].id > page.items[1].id);
}

#[tokio::test]
async fn explorer_events_filters_by_direction_and_state() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    api.create_goldcoin_deposit_transfer(CreateTransferInput {
        amount_atomic: AtomicU64(500_000),
        recipient: Keypair::new().pubkey().to_string(),
        route: None,
        source_address: None,
    })
    .await
    .unwrap();

    let by_state = api
        .explorer_events(None, Some(RequestState::AwaitingDeposit), None, 50)
        .await
        .unwrap();
    assert_eq!(by_state.items.len(), 1);
    assert_eq!(by_state.items[0].to_state, "AwaitingDeposit");

    let by_direction = api
        .explorer_events(Some(Direction::SolToGlc), None, None, 50)
        .await
        .unwrap();
    assert!(
        by_direction.items.is_empty(),
        "no SolToGlc requests exist yet"
    );

    let no_match_state = api
        .explorer_events(None, Some(RequestState::Settled), None, 50)
        .await
        .unwrap();
    assert!(no_match_state.items.is_empty());
}

#[tokio::test]
async fn explorer_events_cursor_pagination_walks_without_gaps_or_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    for _ in 0..3 {
        api.create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();
    }
    // 3 requests * 2 log rows each = 6 total rows.
    let mut seen_ids = Vec::new();
    let mut cursor: Option<i64> = None;
    loop {
        let page = api.explorer_events(None, None, cursor, 2).await.unwrap();
        assert!(page.items.len() <= 2);
        for item in &page.items {
            seen_ids.push(item.id);
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c.parse().unwrap()),
            None => break,
        }
    }
    assert_eq!(seen_ids.len(), 6);
    let mut sorted = seen_ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 6, "no id may repeat across pages");
}

#[tokio::test]
async fn explorer_events_limit_is_clamped_to_the_maximum() {
    // Same HTTP-boundary clamping property as
    // `reserves_history_limit_is_clamped_to_the_maximum`, exercised
    // through the real server rather than `ApiSource` directly.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for direction in [
            ReserveDirection::GoldcoinReserve,
            ReserveDirection::SolanaReserve,
        ] {
            ledger
                .configure_reserve(
                    direction,
                    1_000_000_000,
                    0,
                    5_000_000,
                    2_000_000,
                    1_000_000,
                    0,
                )
                .unwrap();
        }
    }
    let (base, _tx) = spawn_real_server(&db_path, 0).await;
    let client = reqwest::Client::new();
    // Each transfer logs 2 rows; comfortably exceed MAX_PAGE_LIMIT.
    for _ in 0..(MAX_PAGE_LIMIT / 2 + 5) {
        let resp = client
            .post(format!("{base}/transfers"))
            .json(&CreateTransferInput {
                amount_atomic: AtomicU64(500_000),
                recipient: Keypair::new().pubkey().to_string(),
                route: None,
                source_address: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    }
    let page: Page<ExplorerEvent> = reqwest::get(format!("{base}/explorer/events?limit=1000000"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len() as u32, MAX_PAGE_LIMIT);
}

#[tokio::test]
async fn explorer_events_never_exposes_recipient_or_operator_identity() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    api.create_goldcoin_deposit_transfer(CreateTransferInput {
        amount_atomic: AtomicU64(500_000),
        recipient: Keypair::new().pubkey().to_string(),
        route: None,
        source_address: None,
    })
    .await
    .unwrap();
    let page = api.explorer_events(None, None, None, 50).await.unwrap();
    let raw = serde_json::to_string(&page).unwrap();
    assert!(!raw.contains("recipient"));
    assert!(!raw.contains("requester"));
}

#[tokio::test]
async fn reserve_reports_available_capacity_per_direction() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let reserve = api.reserve().await.unwrap();
    // balance(10_000_000) - protected_minimum(0) - reserved(0)
    assert_eq!(reserve.goldcoin_available_capacity.0, 10_000_000);
    assert_eq!(reserve.solana_available_capacity.0, 10_000_000);
}

#[tokio::test]
async fn create_transfer_reserves_capacity_and_returns_deposit_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let recipient = Keypair::new().pubkey();
    let output = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: recipient.to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();
    assert!(output.request_id > 0);
    let expected_vault = crate::goldcoin::derivation::derive_request_vault(
        &test_root_vault(),
        output.request_id,
        crate::goldcoin::address::Network::Testnet,
    )
    .unwrap();
    assert_eq!(output.deposit_address, expected_vault.address());
    // The per-request address must differ from the static root vault
    // address — that's the whole point of this feature.
    assert_ne!(output.deposit_address, "REGTESTVAULTADDRESSXXXXXXXXXXXXX");

    let reserve = api.reserve().await.unwrap();
    // Capacity is reserved on the NET destination payout, in the
    // destination's own decimals (docs/20-bridge-fee.md): 500_000 gross -
    // 3% fee = 485_000 net canonical (8 decimals), /100 to the mint's
    // 6-decimal precision = 4_850.
    assert_eq!(reserve.solana_available_capacity.0, 10_000_000 - 4_850);
}

#[tokio::test]
async fn two_transfer_requests_get_different_deposit_addresses() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    // Two recipients: the rolling-24h destination window
    // (`ledger::wallet_window`) refuses a second request to one pubkey
    // inside a day, and this test is about the deposit addresses.
    let first = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();
    let second = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(300_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();

    assert_ne!(first.request_id, second.request_id);
    assert_ne!(first.deposit_address, second.deposit_address);
}

#[tokio::test]
async fn api_returned_deposit_address_matches_what_is_persisted_in_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let recipient = Keypair::new().pubkey();
    let output = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: recipient.to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();

    let ledger = Ledger::open(&db_path).unwrap();
    let persisted_address: String = ledger
        .raw()
        .query_row(
            "SELECT deposit_address FROM bridge_requests WHERE id = ?1",
            [output.request_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(output.deposit_address, persisted_address);
}

#[tokio::test]
async fn create_transfer_rejects_an_invalid_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let err = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: "not-a-valid-pubkey".to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::BadRequest(_)));
}

#[tokio::test]
async fn create_transfer_rejects_a_zero_amount() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let err = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(0),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::BadRequest(_)));
}

#[tokio::test]
async fn create_transfer_reports_insufficient_liquidity_never_creates_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let err = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            // Even after the bridge fee and the 8->6 decimal shrink
            // (docs/20-bridge-fee.md), this remains far beyond the
            // configured 10_000_000 available capacity.
            amount_atomic: AtomicU64(2_000_000_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::InsufficientLiquidity { .. }));
    assert_eq!(
        err.to_string(),
        DIRECTION_UNAVAILABLE_MESSAGE,
        "the raw available-capacity number must never reach the end user — same generic \
         copy as every other direction-unavailable cause"
    );
    // No capacity was touched: a fresh request must still see it all.
    assert_eq!(
        api.reserve().await.unwrap().solana_available_capacity.0,
        10_000_000
    );
}

#[tokio::test]
async fn create_transfer_fails_closed_on_a_paused_reserve() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_paused(ReserveDirection::SolanaReserve, true, Some("test"))
            .unwrap();
    }
    let api = build(&db_path, 0);

    let err = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Paused));
    assert_eq!(err.to_string(), DIRECTION_UNAVAILABLE_MESSAGE);
}

/// Item 6 of the quota-exhausted -> operator-pause -> refill -> manual-
/// unpause workflow report: `GlcToSol`'s rolling-24h-volume quota being
/// exhausted must reject a new transfer proactively — with the exact
/// approved user-facing copy, no reference to any midnight reset or
/// automatic reopening — and must never touch off-chain reserved
/// capacity, exactly like the insufficient-liquidity and paused cases
/// above.
#[tokio::test]
async fn create_transfer_reports_quota_exhausted_with_the_exact_message_never_creates_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    // release/GlcToSol window already at its 2_000_000 limit; deposit/
    // SolToGlc window untouched — only the affected direction should be
    // rejected (asserted separately below via `/status`).
    let api = build_with_rolling_volume(&db_path, 2_000_000, 2_000_000, 0);

    let err = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::QuotaExhausted));
    assert_eq!(
        err.to_string(),
        "Bridge capacity reached for this direction.\nTransfers are temporarily paused while reserves are replenished.\nPlease check the official Telegram for reopening updates."
    );
    assert_eq!(err.to_string(), DIRECTION_UNAVAILABLE_MESSAGE);
    assert!(
        !err.to_string().to_lowercase().contains("midnight"),
        "must never claim an automatic midnight reset"
    );
    assert!(
        !err.to_string().to_lowercase().contains("automatic"),
        "must never claim automatic reopening"
    );
    // No off-chain capacity was touched: a fresh request must still see
    // it all, exactly as the insufficient-liquidity/paused cases do.
    assert_eq!(
        api.reserve().await.unwrap().solana_available_capacity.0,
        10_000_000
    );
}

/// A transfer that fits within remaining quota must still succeed — the
/// proactive check must reject only when it would genuinely be rejected
/// on-chain, never more conservatively than that.
#[tokio::test]
async fn create_transfer_succeeds_when_amount_fits_within_remaining_quota() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build_with_rolling_volume(&db_path, 2_000_000, 1_000_000, 0);

    let out = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();
    assert_eq!(out.request_id, 1);
}

#[tokio::test]
async fn get_transfer_returns_none_for_an_unknown_id() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    assert!(api.get_transfer(999).await.unwrap().is_none());
}

#[tokio::test]
async fn get_transfer_reflects_a_just_created_request() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let recipient = Keypair::new().pubkey();
    let created = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: recipient.to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();

    let view = api.get_transfer(created.request_id).await.unwrap().unwrap();
    assert_eq!(view.id, created.request_id);
    assert_eq!(view.direction, "GlcToSol");
    assert_eq!(view.state, "AwaitingDeposit");
    assert_eq!(view.gross_amount_atomic.0, 500_000);
    assert_eq!(view.fee_bps, amount_conversion::BRIDGE_FEE_BPS);
    assert_eq!(view.fee_amount_atomic.0, 15_000);
    assert_eq!(view.net_amount_atomic.0, 485_000);
    assert!(view.source_txid.is_none());
    assert!(view.destination_txid.is_none());
    assert!(view.failure_reason.is_none());
    assert_eq!(
        view.required_source_confirmations,
        Some(6),
        "GlcToSol progress must be renderable against the configured confirmation depth"
    );
}

// -------------------------------------------------------------- /transfers (list) --

#[tokio::test]
async fn list_transfers_on_an_empty_ledger_returns_an_empty_page_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let page = api.list_transfers(None, None, None, 50).await.unwrap();
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn list_transfers_filters_by_address_matching_either_recipient_or_requester() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let mine = Keypair::new().pubkey();
    let someone_else = Keypair::new().pubkey();

    api.create_goldcoin_deposit_transfer(CreateTransferInput {
        amount_atomic: AtomicU64(500_000),
        recipient: mine.to_string(),
        route: None,
        source_address: None,
    })
    .await
    .unwrap();
    api.create_goldcoin_deposit_transfer(CreateTransferInput {
        amount_atomic: AtomicU64(500_000),
        recipient: someone_else.to_string(),
        route: None,
        source_address: None,
    })
    .await
    .unwrap();

    let page = api
        .list_transfers(
            Some(TransferAddressFilter::Solana(mine.to_bytes())),
            None,
            None,
            50,
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].direction, "GlcToSol");
}

#[tokio::test]
async fn list_transfers_filters_by_state() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    api.create_goldcoin_deposit_transfer(CreateTransferInput {
        amount_atomic: AtomicU64(500_000),
        recipient: Keypair::new().pubkey().to_string(),
        route: None,
        source_address: None,
    })
    .await
    .unwrap();

    let matching = api
        .list_transfers(None, Some(RequestState::AwaitingDeposit), None, 50)
        .await
        .unwrap();
    assert_eq!(matching.items.len(), 1);

    let non_matching = api
        .list_transfers(None, Some(RequestState::Settled), None, 50)
        .await
        .unwrap();
    assert!(non_matching.items.is_empty());
}

#[tokio::test]
async fn list_transfers_newest_first_and_cursor_pagination_has_no_gaps_or_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let mut created_ids = Vec::new();
    for _ in 0..5 {
        let created = api
            .create_goldcoin_deposit_transfer(CreateTransferInput {
                amount_atomic: AtomicU64(500_000),
                recipient: Keypair::new().pubkey().to_string(),
                route: None,
                source_address: None,
            })
            .await
            .unwrap();
        created_ids.push(created.request_id);
    }

    let mut seen_ids = Vec::new();
    let mut cursor: Option<i64> = None;
    loop {
        let page = api.list_transfers(None, None, cursor, 2).await.unwrap();
        assert!(page.items.len() <= 2);
        for item in &page.items {
            seen_ids.push(item.id);
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c.parse().unwrap()),
            None => break,
        }
    }
    let mut expected = created_ids.clone();
    expected.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        seen_ids, expected,
        "must visit every created transfer exactly once, newest first"
    );
}

#[tokio::test]
async fn get_transfers_list_route_returns_200_and_rejects_an_invalid_address() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let (base, _tx) = spawn_real_server(&db_path, 0).await;
    let resp = reqwest::get(format!("{base}/transfers")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let resp = reqwest::get(format!("{base}/transfers?address=not-a-pubkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn client_supplied_fee_fields_in_the_request_body_are_silently_ignored() {
    // `CreateTransferInput` has no fee/net field at all — there is nothing
    // for a client to submit that could bypass or alter the fee
    // (docs/20-bridge-fee.md: "never trust gross, fee or net calculations
    // supplied by the UI"). This proves it holds at the real HTTP/JSON
    // boundary too, not just at the Rust type level: a raw JSON body
    // smuggling `fee_bps`/`fee_amount_atomic`/`net_amount_atomic` fields
    // alongside the real ones is silently ignored by serde (no
    // `deny_unknown_fields`), and the server computes the real 3% fee
    // regardless of what the client tried to claim.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let (base, _tx) = spawn_real_server(&db_path, 0).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/transfers"))
        .json(&serde_json::json!({
            "amount_atomic": 500_000,
            "recipient": Keypair::new().pubkey().to_string(),
            // Attempted client-side fee bypass/manipulation:
            "fee_bps": 0,
            "fee_amount_atomic": 0,
            "net_amount_atomic": 500_000,
            "gross_amount_atomic": 1,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let created: CreateTransferOutput = resp.json().await.unwrap();

    let view: TransferView = reqwest::get(format!("{base}/transfers/{}", created.request_id))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        view.gross_amount_atomic.0, 500_000,
        "gross must be exactly what the server itself received, never a client-claimed value"
    );
    assert_eq!(
        view.fee_bps,
        amount_conversion::BRIDGE_FEE_BPS,
        "fee_bps must always be the real protocol rate, never the client-submitted 0"
    );
    assert_eq!(
        view.fee_amount_atomic.0, 15_000,
        "the real 3% fee must be charged regardless of a client-submitted fee_amount_atomic of 0"
    );
    assert_eq!(
        view.net_amount_atomic.0, 485_000,
        "net must reflect the real fee, never the client-submitted (unreduced) net"
    );
}

async fn spawn_real_server(
    db_path: &std::path::Path,
    obligation_count: u64,
) -> (String, tokio::sync::watch::Sender<bool>) {
    let (listener, port) = bound_listener().await;
    let (tx, rx) = tokio::sync::watch::channel(false);
    let api = Arc::new(build(db_path, obligation_count));
    tokio::spawn(async move {
        // `serve_on` cannot fail to bind — the listener is already ours.
        let _ = serve_on(listener, api, rx).await;
    });
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if reqwest::get(format!("{base}/status")).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (base, tx)
}

#[tokio::test]
async fn concurrent_post_transfers_never_oversubscribe_capacity() {
    // The same concurrency property `adversarial.rs`'s
    // `ten_concurrent_shaped_reservations_never_oversubscribe_capacity`
    // proves at the `Ledger` level, exercised here through the real HTTP
    // API — SQLite's own `BEGIN IMMEDIATE` transactions are what actually
    // make this safe (see `Ledger::create_request`), and this confirms
    // that guarantee survives being reached over the network with many
    // real concurrent connections rather than in-process calls.
    // A gross of 1_000_000 canonical costs 30_000 in fee (exact, no
    // rounding: 1_000_000 is a multiple of 10_000, see
    // `glc_to_sol_amounts`-style derivations elsewhere in this crate),
    // leaving 970_000 net canonical, which converts exactly to 9_700 at
    // the (6-decimal) reserve mint's precision (docs/20-bridge-fee.md).
    // Configure capacity to exactly 10 * 9_700 so the "exactly N succeed,
    // capacity fully and exactly consumed" property still holds under the
    // real fee math, not just the pre-fee 1:1 numbers.
    const NET_DESTINATION_PER_REQUEST: u64 = 9_700;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for direction in [
            ReserveDirection::GoldcoinReserve,
            ReserveDirection::SolanaReserve,
        ] {
            ledger
                .configure_reserve(
                    direction,
                    NET_DESTINATION_PER_REQUEST * 10,
                    0,
                    5_000_000,
                    2_000_000,
                    1_000_000,
                    0,
                )
                .unwrap();
        }
    }
    let (base, _tx) = spawn_real_server(&db_path, 0).await;

    let client = reqwest::Client::new();
    let mut handles = Vec::new();
    for _ in 0..20 {
        let client = client.clone();
        let base = base.clone();
        handles.push(tokio::spawn(async move {
            client
                .post(format!("{base}/transfers"))
                .json(&CreateTransferInput {
                    amount_atomic: AtomicU64(1_000_000),
                    recipient: Keypair::new().pubkey().to_string(),
                    route: None,
                    source_address: None,
                })
                .send()
                .await
                .unwrap()
                .status()
        }));
    }
    let mut created = 0;
    let mut rejected = 0;
    for h in handles {
        match h.await.unwrap() {
            reqwest::StatusCode::CREATED => created += 1,
            reqwest::StatusCode::CONFLICT => rejected += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(created, 10, "exactly capacity/amount requests must succeed");
    assert_eq!(
        rejected, 10,
        "the rest must be cleanly rejected, never oversubscribed"
    );

    let reserve: ReserveAvailability = reqwest::get(format!("{base}/reserve"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reserve.solana_available_capacity.0, 0,
        "capacity must be fully and exactly accounted for, no double-reservation and no leakage"
    );
}

// ---- SolToGlc recipient eligibility (pre-transaction rate-limit read) ----

/// A syntactically valid Testnet p2pkh address — [`build`] configures the
/// API with `Network::Testnet`, so this passes the same `decode_p2pkh`
/// validation the payout path applies.
fn test_glc_address(seed: u8) -> String {
    crate::goldcoin::address::encode_p2pkh(&[seed; 20], crate::goldcoin::address::Network::Testnet)
}

/// A distinct 32-byte "Solana wallet" for eligibility tests, matching
/// `test_glc_address`'s seed-a-fixed-pattern shape.
fn test_wallet(seed: u8) -> [u8; 32] {
    [seed; 32]
}

/// Folds one SolToGlc obligation for `address`/`requester` directly into
/// the ledger at `created_at`, the same way the Solana indexer does — the
/// eligibility endpoint must then answer from this authoritative state.
fn fold_payout_for(
    db_path: &std::path::Path,
    index: u64,
    address: &str,
    requester: [u8; 32],
    created_at: i64,
) {
    let mut ledger = Ledger::open(db_path).unwrap();
    let outcome = ledger
        .fold_sol_deposit(
            index,
            crate::ledger::RequestAmounts {
                gross_atomic: 50_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 50_000,
                net_destination_atomic: 50_000,
            },
            requester,
            address.as_bytes(),
            None,
            created_at,
        )
        .unwrap();
    assert!(
        matches!(
            outcome,
            crate::ledger::SolFoldOutcome::FoldedFinalized { .. }
        ),
        "test setup expected a clean fold, got {outcome:?}"
    );
}

#[tokio::test]
async fn recipient_eligibility_reports_an_unused_address_as_eligible() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let out = api
        .sol_to_glc_recipient_eligibility(test_glc_address(7), None)
        .await
        .unwrap();
    assert!(out.eligible);
    assert_eq!(out.retry_after, None);
    assert_eq!(out.retry_after_seconds, None);
    assert_eq!(out.window_seconds, 86_400);
    assert_eq!(out.direction, "SolToGlc");
}

#[tokio::test]
async fn recipient_eligibility_blocks_a_recently_paid_address_with_the_exact_retry_after() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let address = test_glc_address(7);
    let folded_at = now_unix() - 100;
    fold_payout_for(&db_path, 0, &address, test_wallet(1), folded_at);

    let api = build(&db_path, 1);
    let out = api
        .sol_to_glc_recipient_eligibility(address.clone(), None)
        .await
        .unwrap();
    assert!(!out.eligible);
    assert_eq!(
        out.retry_after,
        Some(folded_at + 86_400),
        "retry_after must be the blocking payout's created_at plus the window"
    );
    let remaining = out.retry_after_seconds.unwrap();
    // now_unix() advances between fold and check; allow a small margin.
    assert!(
        (86_290..=86_300).contains(&remaining),
        "retry_after_seconds must be the remaining window, got {remaining}"
    );
    assert_eq!(out.address, address, "echoes the address it answered for");
}

#[tokio::test]
async fn recipient_eligibility_clears_once_the_window_has_expired() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let address = test_glc_address(7);
    fold_payout_for(&db_path, 0, &address, test_wallet(1), now_unix() - 86_401);

    let api = build(&db_path, 1);
    let out = api
        .sol_to_glc_recipient_eligibility(address, None)
        .await
        .unwrap();
    assert!(
        out.eligible,
        "a payout older than the rolling 24h window must not block"
    );
    assert_eq!(out.retry_after, None);
}

#[tokio::test]
async fn recipient_eligibility_is_per_address_a_different_recipient_stays_eligible() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    fold_payout_for(
        &db_path,
        0,
        &test_glc_address(7),
        test_wallet(1),
        now_unix() - 100,
    );

    let api = build(&db_path, 1);
    let out = api
        .sol_to_glc_recipient_eligibility(test_glc_address(8), None)
        .await
        .unwrap();
    assert!(
        out.eligible,
        "one recipient's payout must never rate-limit a different address"
    );
}

#[tokio::test]
async fn recipient_eligibility_trims_surrounding_whitespace_like_the_ui_does() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let address = test_glc_address(7);
    fold_payout_for(&db_path, 0, &address, test_wallet(1), now_unix() - 100);

    let api = build(&db_path, 1);
    let out = api
        .sol_to_glc_recipient_eligibility(format!("  {address} "), None)
        .await
        .unwrap();
    assert!(
        !out.eligible,
        "padding must not make the same recipient look fresh"
    );
    assert_eq!(out.address, address);
}

#[tokio::test]
async fn recipient_eligibility_rejects_a_malformed_address() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let err = api
        .sol_to_glc_recipient_eligibility("not-a-goldcoin-address".to_string(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::BadRequest(_)));
}

// ---- SolToGlc source-wallet eligibility (dual rate-limit key) ----

#[tokio::test]
async fn eligibility_blocks_on_source_wallet_even_with_a_fresh_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let wallet = test_wallet(9);
    let folded_at = now_unix() - 100;
    // Wallet 9 already deposited to recipient 7, inside the window.
    fold_payout_for(&db_path, 0, &test_glc_address(7), wallet, folded_at);

    let api = build(&db_path, 1);
    // Same wallet, but a BRAND NEW recipient — the recipient leg alone
    // would report eligible; the wallet leg must still block it.
    let out = api
        .sol_to_glc_recipient_eligibility(test_glc_address(8), Some(wallet))
        .await
        .unwrap();
    assert!(
        !out.eligible,
        "the source wallet's own limit must block a new obligation even to a fresh recipient"
    );
    assert_eq!(
        out.blocked_reason.as_deref(),
        Some(BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED)
    );
    assert_eq!(
        out.retry_after,
        Some(folded_at + 86_400),
        "retry_after must be the blocking deposit's created_at plus the window"
    );
}

#[tokio::test]
async fn eligibility_reports_recipient_reason_when_only_the_recipient_is_limited() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let address = test_glc_address(7);
    // A DIFFERENT wallet already paid this recipient.
    fold_payout_for(&db_path, 0, &address, test_wallet(1), now_unix() - 100);

    let api = build(&db_path, 1);
    let out = api
        .sol_to_glc_recipient_eligibility(address, Some(test_wallet(2)))
        .await
        .unwrap();
    assert!(!out.eligible);
    assert_eq!(
        out.blocked_reason.as_deref(),
        Some(BLOCKED_REASON_RECIPIENT_RATE_LIMITED),
        "wallet 2 has no history of its own — only the recipient leg should block"
    );
}

#[tokio::test]
async fn eligibility_prefers_the_source_wallet_reason_when_both_are_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let address = test_glc_address(7);
    let wallet = test_wallet(9);
    fold_payout_for(&db_path, 0, &address, wallet, now_unix() - 100);

    let api = build(&db_path, 1);
    // Same wallet AND same recipient as the existing payout: both limits
    // independently apply, but only one reason is surfaced.
    let out = api
        .sol_to_glc_recipient_eligibility(address, Some(wallet))
        .await
        .unwrap();
    assert!(!out.eligible);
    assert_eq!(
        out.blocked_reason.as_deref(),
        Some(BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED)
    );
}

#[tokio::test]
async fn eligibility_with_a_fresh_wallet_and_fresh_recipient_is_eligible() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let out = api
        .sol_to_glc_recipient_eligibility(test_glc_address(7), Some(test_wallet(9)))
        .await
        .unwrap();
    assert!(out.eligible);
    assert_eq!(out.blocked_reason, None);
    assert_eq!(
        out.wallet.as_deref(),
        Some(Pubkey::new_from_array(test_wallet(9)).to_string().as_str())
    );
}

#[tokio::test]
async fn eligibility_echoes_none_wallet_when_not_provided() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let out = api
        .sol_to_glc_recipient_eligibility(test_glc_address(7), None)
        .await
        .unwrap();
    assert_eq!(
        out.wallet, None,
        "omitting ?wallet= must mean the source-wallet leg was never evaluated"
    );
}

// ---- RhnToGlc eligibility (pre-transaction rate-limit read) ----
//
// The exact twin of the SolToGlc block above. Everything these assert is
// asserted there too, plus the two things unique to this route: the wallet
// leg is keyed on a 20-byte EVM depositor, and the recipient leg is
// route-global, so a prior SolToGlc payout blocks here as surely as a
// prior RhnToGlc one.

/// A distinct 20-byte "Robinhood wallet", matching `test_wallet`'s shape.
fn test_evm_wallet(seed: u8) -> [u8; 20] {
    [seed; 20]
}

/// The `0x`-prefixed spelling a caller would put in `?wallet=`.
fn evm_wallet_param(seed: u8) -> String {
    crate::evm::address::EvmAddress::from_bytes(test_evm_wallet(seed)).to_string()
}

/// Folds one `RhnToGlc` deposit for `address`/`depositor` at `created_at`,
/// the way the Robinhood indexer + fold phase would — the eligibility
/// endpoint must then answer from this authoritative state.
///
/// Deliberately goes through the REAL `fold_observation`, not a hand-built
/// row: the endpoint's whole claim is that it agrees with what admission
/// actually did, so the setup has to be what admission actually does.
fn fold_rhn_payout_for(
    db_path: &std::path::Path,
    obligation_index: u64,
    address: &str,
    depositor: [u8; 20],
    created_at: i64,
) {
    use crate::ledger::{RobinhoodDepositObservation, RobinhoodFinality, RobinhoodObservationRow};

    let canonical: u64 = 50_000;
    let row = RobinhoodObservationRow {
        id: obligation_index as i64 + 1,
        observation: RobinhoodDepositObservation {
            source_contract: crate::robinhood::testkit::BRIDGE.to_bytes(),
            obligation_index,
            route: crate::routes::Route::RhnToGlc,
            depositor,
            destination: address.as_bytes().to_vec(),
            amount_robinhood_atomic: crate::evm::EvmU256::from_u128(
                u128::from(canonical) * 10_000_000_000,
            )
            .to_be_bytes(),
            amount_canonical_atomic: canonical,
            tx_hash: {
                let mut h = [0xaa; 32];
                h[0] = obligation_index as u8;
                h
            },
            log_index: 0,
            block_number: 500,
            block_hash: [0xbb; 32],
        },
        finality: RobinhoodFinality::Final,
        observed_at: created_at,
        finalized_at: Some(created_at),
        reorged_at: None,
    };
    let mut ledger = Ledger::open(db_path).unwrap();
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_deposit_observations
                (id, source_chain, source_contract, source_obligation_index, contract_route_id,
                 route, depositor, destination, amount_robinhood_atomic,
                 amount_canonical_atomic, tx_hash, log_index, block_number, block_hash,
                 finality, observed_at, finalized_at)
             VALUES (?1, 'robinhood', ?2, ?3, 2, 'RhnToGlc', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                     'Final', ?12, ?12)",
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
                created_at,
            ],
        )
        .unwrap();
    let outcome = crate::robinhood::fold::fold_observation(
        &mut ledger,
        &row,
        crate::goldcoin::address::Network::Testnet,
        crate::amount_conversion::BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        true,
        created_at,
    )
    .expect("the observation folds");
    assert!(
        matches!(
            outcome,
            crate::robinhood::fold::FoldOutcome::FoldedFinalized { .. }
        ),
        "test setup expected a clean fold, got {outcome:?}"
    );
}

#[tokio::test]
async fn rhn_eligibility_reports_a_fresh_wallet_and_address_as_eligible() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let out = api
        .rhn_to_glc_recipient_eligibility(test_glc_address(7), Some(test_evm_wallet(9)))
        .await
        .unwrap();
    assert!(out.eligible);
    assert_eq!(out.direction, "RhnToGlc");
    assert_eq!(out.blocked_reason, None);
    assert!(out.blocked_reasons.is_empty());
    assert_eq!(out.retry_after, None);
    assert_eq!(out.retry_after_seconds, None);
    assert_eq!(out.source_wallet_retry_after, None);
    assert_eq!(out.recipient_retry_after, None);
    assert_eq!(
        out.window_seconds, 86_400,
        "the same window constant the enforcing folds use"
    );
    assert_eq!(out.wallet.as_deref(), Some(evm_wallet_param(9).as_str()));
}

#[tokio::test]
async fn rhn_eligibility_blocks_on_the_source_wallet_even_with_a_fresh_address() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let wallet = test_evm_wallet(9);
    let folded_at = now_unix() - 100;
    // The wallet deposited to a DIFFERENT address, so only the
    // source-wallet leg can be doing the blocking.
    fold_rhn_payout_for(&db_path, 0, &test_glc_address(7), wallet, folded_at);

    let api = build(&db_path, 1);
    let out = api
        .rhn_to_glc_recipient_eligibility(test_glc_address(8), Some(wallet))
        .await
        .unwrap();
    assert!(!out.eligible);
    assert_eq!(
        out.blocked_reason.as_deref(),
        Some(BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED)
    );
    assert_eq!(
        out.blocked_reasons,
        vec![BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED.to_string()]
    );
    assert_eq!(out.source_wallet_retry_after, Some(folded_at + 86_400));
    assert_eq!(out.recipient_retry_after, None);
    assert_eq!(out.retry_after, Some(folded_at + 86_400));
}

#[tokio::test]
async fn rhn_eligibility_blocks_an_address_paid_by_a_prior_rhn_to_glc_payout() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let address = test_glc_address(7);
    let folded_at = now_unix() - 100;
    fold_rhn_payout_for(&db_path, 0, &address, test_evm_wallet(1), folded_at);

    let api = build(&db_path, 1);
    // A DIFFERENT wallet, so only the recipient leg can be blocking.
    let out = api
        .rhn_to_glc_recipient_eligibility(address.clone(), Some(test_evm_wallet(2)))
        .await
        .unwrap();
    assert!(!out.eligible);
    assert_eq!(
        out.blocked_reason.as_deref(),
        Some(BLOCKED_REASON_RECIPIENT_RATE_LIMITED)
    );
    assert_eq!(
        out.blocked_reasons,
        vec![BLOCKED_REASON_RECIPIENT_RATE_LIMITED.to_string()]
    );
    assert_eq!(out.recipient_retry_after, Some(folded_at + 86_400));
    assert_eq!(out.source_wallet_retry_after, None);
    assert_eq!(out.address, address);
}

/// The cross-route half, on the read side: the destination window is one
/// window per Goldcoin address across every inbound route, so a SolToGlc
/// payout must make this endpoint report the address as ineligible.
#[tokio::test]
async fn rhn_eligibility_blocks_an_address_paid_by_a_prior_sol_to_glc_payout() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let address = test_glc_address(7);
    let folded_at = now_unix() - 100;
    fold_payout_for(&db_path, 0, &address, test_wallet(1), folded_at);

    let api = build(&db_path, 1);
    let out = api
        .rhn_to_glc_recipient_eligibility(address, Some(test_evm_wallet(9)))
        .await
        .unwrap();
    assert!(
        !out.eligible,
        "a Solana-funded payout must block the same Goldcoin address here"
    );
    assert_eq!(
        out.blocked_reason.as_deref(),
        Some(BLOCKED_REASON_RECIPIENT_RATE_LIMITED)
    );
    assert_eq!(out.recipient_retry_after, Some(folded_at + 86_400));
    assert_eq!(
        out.source_wallet_retry_after, None,
        "a Solana wallet's window must never be charged to an EVM wallet"
    );
}

#[tokio::test]
async fn rhn_eligibility_reports_both_reasons_when_both_limits_apply() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let address = test_glc_address(7);
    let wallet = test_evm_wallet(9);
    let folded_at = now_unix() - 100;
    // Same wallet AND same address: both limits independently apply.
    fold_rhn_payout_for(&db_path, 0, &address, wallet, folded_at);

    let api = build(&db_path, 1);
    let out = api
        .rhn_to_glc_recipient_eligibility(address, Some(wallet))
        .await
        .unwrap();
    assert!(!out.eligible);
    // `blocked_reason` still names exactly one, wallet-first — the same
    // reason a real fold would have recorded as its manual_review_note.
    assert_eq!(
        out.blocked_reason.as_deref(),
        Some(BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED)
    );
    // ...and `blocked_reasons` names both, in the same precedence order.
    assert_eq!(
        out.blocked_reasons,
        vec![
            BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED.to_string(),
            BLOCKED_REASON_RECIPIENT_RATE_LIMITED.to_string(),
        ]
    );
    assert_eq!(out.source_wallet_retry_after, Some(folded_at + 86_400));
    assert_eq!(out.recipient_retry_after, Some(folded_at + 86_400));
}

#[tokio::test]
async fn rhn_eligibility_honours_the_exact_86_400_second_boundary() {
    let address = test_glc_address(7);
    let wallet = test_evm_wallet(9);

    // One second inside the window: still blocked, on BOTH legs.
    {
        let dir = tempfile::tempdir().unwrap();
        let db_path = configure(dir.path());
        fold_rhn_payout_for(&db_path, 0, &address, wallet, now_unix() - 86_399);
        let api = build(&db_path, 1);
        let out = api
            .rhn_to_glc_recipient_eligibility(address.clone(), Some(wallet))
            .await
            .unwrap();
        assert!(!out.eligible, "at window-1 the payout must still block");
    }

    // At exactly `created_at + 86_400` the blocker has aged out —
    // `retry_after` is the FIRST eligible second, not the last blocked
    // one. `- 86_401` rather than `- 86_400` because `now_unix()` advances
    // between the fold and the read; the assertion under test is the
    // boundary's direction, and one second of slack keeps it from being a
    // clock race.
    {
        let dir = tempfile::tempdir().unwrap();
        let db_path = configure(dir.path());
        fold_rhn_payout_for(&db_path, 0, &address, wallet, now_unix() - 86_401);
        let api = build(&db_path, 1);
        let out = api
            .rhn_to_glc_recipient_eligibility(address.clone(), Some(wallet))
            .await
            .unwrap();
        assert!(
            out.eligible,
            "a payout older than the rolling 24h window must not block either leg"
        );
        assert_eq!(out.retry_after, None);
        assert_eq!(out.source_wallet_retry_after, None);
        assert_eq!(out.recipient_retry_after, None);
    }
}

/// The endpoint is advisory and READ-ONLY. This is the test that says so
/// in the only way that counts: snapshot every table the rate limits and
/// the reserve live in, hammer the endpoint across eligible and blocked
/// inputs, and assert the database is byte-for-byte unchanged.
#[tokio::test]
async fn rhn_eligibility_is_read_only_and_consumes_no_cooldown() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let address = test_glc_address(7);
    let wallet = test_evm_wallet(9);
    fold_rhn_payout_for(&db_path, 0, &address, wallet, now_unix() - 100);

    /// Everything an admission decision could have touched.
    fn snapshot(db_path: &std::path::Path) -> Vec<String> {
        let ledger = Ledger::open(db_path).unwrap();
        let conn = ledger.conn_for_tests();
        let mut out = Vec::new();
        for sql in [
            "SELECT id, direction, state, manual_review_note, recipient, created_at,
                    net_destination_atomic FROM bridge_requests ORDER BY id",
            "SELECT direction, total_reserve_balance, protected_minimum, reserved_liquidity,
                    pending_obligations, paused, admission_closed FROM reserve_ledger
             ORDER BY direction",
            "SELECT id, folded_request_id, finality, depositor FROM
             robinhood_deposit_observations ORDER BY id",
            "SELECT id, request_id, from_state, to_state FROM bridge_request_state_log
             ORDER BY id",
        ] {
            let mut stmt = conn.prepare(sql).unwrap();
            let cols = stmt.column_count();
            let rows = stmt
                .query_map([], |r| {
                    let mut line = String::new();
                    for i in 0..cols {
                        let v: rusqlite::types::Value = r.get(i)?;
                        line.push_str(&format!("{v:?}|"));
                    }
                    Ok(line)
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            out.extend(rows);
            out.push("--".to_string());
        }
        out
    }

    let before = snapshot(&db_path);
    let api = build(&db_path, 1);

    // Blocked on both legs, blocked on one leg, and fully eligible —
    // every branch of the handler, several times over.
    for _ in 0..3 {
        let blocked = api
            .rhn_to_glc_recipient_eligibility(address.clone(), Some(wallet))
            .await
            .unwrap();
        assert!(!blocked.eligible);
        let recipient_only = api
            .rhn_to_glc_recipient_eligibility(address.clone(), Some(test_evm_wallet(1)))
            .await
            .unwrap();
        assert!(!recipient_only.eligible);
        let fresh = api
            .rhn_to_glc_recipient_eligibility(test_glc_address(8), Some(test_evm_wallet(2)))
            .await
            .unwrap();
        assert!(fresh.eligible);
        let no_wallet = api
            .rhn_to_glc_recipient_eligibility(test_glc_address(8), None)
            .await
            .unwrap();
        assert!(no_wallet.eligible);
    }

    assert_eq!(
        snapshot(&db_path),
        before,
        "an advisory read must never mutate a request, a reservation, a state log row, or an \
         observation — and must never consume the cooldown it reports on"
    );

    // The cooldown it reported is still there afterwards, unchanged: the
    // reads did not spend it.
    let after = api
        .rhn_to_glc_recipient_eligibility(address, Some(wallet))
        .await
        .unwrap();
    assert!(!after.eligible);
}

#[tokio::test]
async fn rhn_eligibility_echoes_none_wallet_when_not_provided() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let wallet = test_evm_wallet(9);
    fold_rhn_payout_for(&db_path, 0, &test_glc_address(7), wallet, now_unix() - 100);

    let api = build(&db_path, 1);
    let out = api
        .rhn_to_glc_recipient_eligibility(test_glc_address(8), None)
        .await
        .unwrap();
    assert_eq!(
        out.wallet, None,
        "omitting ?wallet= must mean the source-wallet leg was never evaluated"
    );
    assert!(
        out.eligible,
        "an unevaluated wallet leg must never be reported as blocking"
    );
    assert_eq!(out.source_wallet_retry_after, None);
}

#[tokio::test]
async fn rhn_eligibility_trims_whitespace_and_rejects_a_malformed_address() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let address = test_glc_address(7);
    fold_rhn_payout_for(&db_path, 0, &address, test_evm_wallet(1), now_unix() - 100);

    let api = build(&db_path, 1);
    let out = api
        .rhn_to_glc_recipient_eligibility(format!("  {address} "), None)
        .await
        .unwrap();
    assert!(
        !out.eligible,
        "padding must not make the same recipient look fresh"
    );
    assert_eq!(out.address, address);

    let err = api
        .rhn_to_glc_recipient_eligibility("not-a-goldcoin-address".to_string(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::BadRequest(_)));
}

#[tokio::test]
async fn rhn_eligibility_is_per_address_a_different_recipient_stays_eligible() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    fold_rhn_payout_for(
        &db_path,
        0,
        &test_glc_address(7),
        test_evm_wallet(1),
        now_unix() - 100,
    );

    let api = build(&db_path, 1);
    let out = api
        .rhn_to_glc_recipient_eligibility(test_glc_address(8), Some(test_evm_wallet(2)))
        .await
        .unwrap();
    assert!(
        out.eligible,
        "one recipient's payout must never rate-limit a different address"
    );
}

/// A tiny, fully in-memory [`ApiSource`] for exercising `handle`'s routing
/// and status-code mapping without a real ledger/RPC.
struct StubSource;

impl ApiSource for StubSource {
    fn chains(&self) -> BoxFut<'_, Result<ChainsView, ApiError>> {
        // Mirrors a default deployment: legacy routes open, Robinhood
        // routes closed and flagged unimplemented.
        Box::pin(async {
            Ok(ChainsView {
                chains: crate::routes::Chain::ALL
                    .iter()
                    .map(|c| ChainView {
                        id: c.as_str().to_string(),
                        display_name: c.display_name().to_string(),
                    })
                    .collect(),
                routes: crate::routes::Route::ALL
                    .iter()
                    .map(|r| RouteView {
                        id: r.as_str().to_string(),
                        source_chain: r.source_chain().as_str().to_string(),
                        destination_chain: r.destination_chain().as_str().to_string(),
                        enabled: r.default_enabled(),
                        disabled_reason: (!r.default_enabled()).then(|| {
                            crate::routes::RouteGateError::UNAVAILABLE_MESSAGE.to_string()
                        }),
                        implemented: r.as_direction().is_some(),
                        // A healthy stub deployment: whatever is enabled
                        // is also available, which is the shape a client
                        // exercising `handle`'s routing should see.
                        available: r.default_enabled(),
                        min_transfer_atomic: AtomicU64(crate::min_transfer::source_minimum(*r).0),
                        unavailable_reason: (!r.default_enabled()).then(|| {
                            crate::routes::RouteGateError::UNAVAILABLE_MESSAGE.to_string()
                        }),
                        availability_reason: (!r.default_enabled())
                            .then(|| AVAILABILITY_REASON_ROUTE_DISABLED.to_string()),
                        capacity: None,
                    })
                    .collect(),
                as_of: 0,
            })
        })
    }
    fn status(&self) -> BoxFut<'_, Result<BridgeStatus, ApiError>> {
        Box::pin(async {
            Ok(BridgeStatus {
                goldcoin_paused: false,
                solana_paused: false,
                vault_address: "V".into(),
                next_solana_obligation_index: 0,
                glc_to_sol_available: true,
                sol_to_glc_available: true,
                glc_to_sol_quota_exhausted: false,
                sol_to_glc_quota_exhausted: false,
                glc_to_sol_rolling_volume_remaining: AtomicU64(100_000_000),
                sol_to_glc_rolling_volume_remaining: AtomicU64(100_000_000),
                sol_to_glc_admission_open: true,
                goldcoin_destination_admission_open: true,
                sol_to_glc_availability_reason: None,
                sol_to_glc_capacity: None,
            })
        })
    }
    fn limits(&self) -> BoxFut<'_, Result<TransferLimits, ApiError>> {
        Box::pin(async {
            Ok(TransferLimits {
                min_transfer_amount: AtomicU64(1),
                per_transfer_limit: AtomicU64(2),
                bridge_fee_bps: amount_conversion::BRIDGE_FEE_BPS,
            })
        })
    }
    fn health(&self) -> BoxFut<'_, Result<PublicHealth, ApiError>> {
        Box::pin(async {
            Ok(PublicHealth {
                healthy: true,
                goldcoin_indexer_halted: false,
                manual_review_backlog: 0,
                post_finality_reorg_events: 0,
            })
        })
    }
    fn reserve(&self) -> BoxFut<'_, Result<ReserveAvailability, ApiError>> {
        Box::pin(async {
            Ok(ReserveAvailability {
                goldcoin_available_capacity: AtomicI64(1),
                solana_available_capacity: AtomicI64(2),
            })
        })
    }
    fn create_goldcoin_deposit_transfer(
        &self,
        input: CreateTransferInput,
    ) -> BoxFut<'_, Result<CreateTransferOutput, ApiError>> {
        Box::pin(async move {
            if input.amount_atomic.0 == 0 {
                return Err(ApiError::BadRequest("amount_atomic must be > 0".into()));
            }
            Ok(CreateTransferOutput {
                request_id: 7,
                deposit_address: "V".into(),
            })
        })
    }
    fn get_transfer(&self, id: i64) -> BoxFut<'_, Result<Option<TransferView>, ApiError>> {
        Box::pin(async move {
            if id == 7 {
                Ok(Some(TransferView {
                    id: 7,
                    direction: "GlcToSol".to_string(),
                    state: "AwaitingDeposit".to_string(),
                    gross_amount_atomic: AtomicU64(500_000),
                    fee_bps: amount_conversion::BRIDGE_FEE_BPS,
                    fee_amount_atomic: AtomicU64(15_000),
                    net_amount_atomic: AtomicU64(485_000),
                    created_at: 0,
                    source_txid: None,
                    source_confirmations: 0,
                    required_source_confirmations: Some(6),
                    destination_txid: None,
                    failure_reason: None,
                    refund: None,
                }))
            } else {
                Ok(None)
            }
        })
    }
    fn list_transfers(
        &self,
        _address: Option<TransferAddressFilter>,
        _state: Option<RequestState>,
        _cursor: Option<i64>,
        _limit: u32,
    ) -> BoxFut<'_, Result<Page<TransferView>, ApiError>> {
        Box::pin(async {
            Ok(Page {
                items: vec![],
                next_cursor: None,
                as_of: 0,
            })
        })
    }
    fn stats(&self) -> BoxFut<'_, Result<BridgeStats, ApiError>> {
        Box::pin(async {
            Ok(BridgeStats {
                goldcoin_paused: false,
                solana_paused: false,
                glc_to_sol_available: true,
                sol_to_glc_available: true,
                sol_to_glc_availability_reason: None,
                glc_to_sol_quota_exhausted: false,
                sol_to_glc_quota_exhausted: false,
                glc_to_sol_rolling_volume_remaining: AtomicU64(100_000_000),
                sol_to_glc_rolling_volume_remaining: AtomicU64(100_000_000),
                bridge_fee_bps: amount_conversion::BRIDGE_FEE_BPS,
                route_fees: Vec::new(),
                glc_to_sol: DirectionStats {
                    total_requests: 1,
                    in_progress_requests: 0,
                    settled_requests: 1,
                    manual_review_requests: 0,
                },
                sol_to_glc: DirectionStats {
                    total_requests: 0,
                    in_progress_requests: 0,
                    settled_requests: 0,
                    manual_review_requests: 0,
                },
                goldcoin_reserve: ReserveStats {
                    paused: false,
                    available_capacity: AtomicI64(1),
                    settled_volume_atomic: AtomicU64(0),
                    accrued_fees_atomic: AtomicU64(0),
                },
                solana_reserve: ReserveStats {
                    paused: false,
                    available_capacity: AtomicI64(2),
                    settled_volume_atomic: AtomicU64(485_000),
                    accrued_fees_atomic: AtomicU64(15_000),
                },
                // The stub serves the not-configured arm, matching what
                // every production deployment returns today.
                robinhood_reserve: RobinhoodReserveStats {
                    ledger_availability: crate::robinhood::public::AVAILABILITY_NOT_CONFIGURED
                        .to_string(),
                    paused: None,
                    available_capacity: None,
                    settled_volume_atomic: None,
                    accrued_fees_atomic: None,
                },
                goldcoin_indexer_halted: false,
                goldcoin_indexer_seconds_since_tick: 0,
                solana_indexer_seconds_since_tick: 0,
                post_finality_reorg_events: 0,
                as_of: 0,
            })
        })
    }
    fn reserves_history(
        &self,
        _direction: Option<ReserveDirection>,
        _cursor: Option<i64>,
        _limit: u32,
    ) -> BoxFut<'_, Result<Page<ReserveHistoryEntry>, ApiError>> {
        Box::pin(async {
            Ok(Page {
                items: vec![],
                next_cursor: None,
                as_of: 0,
            })
        })
    }
    fn sol_to_glc_recipient_eligibility(
        &self,
        address: String,
        wallet: Option<[u8; 32]>,
    ) -> BoxFut<'_, Result<RecipientEligibility, ApiError>> {
        Box::pin(async move {
            Ok(RecipientEligibility {
                direction: "SolToGlc".into(),
                address,
                wallet: wallet.map(|w| Pubkey::new_from_array(w).to_string()),
                eligible: true,
                blocked_reason: None,
                blocked_reasons: Vec::new(),
                retry_after: None,
                retry_after_seconds: None,
                source_wallet_retry_after: None,
                recipient_retry_after: None,
                window_seconds: 86_400,
            })
        })
    }
    fn rhn_to_glc_recipient_eligibility(
        &self,
        address: String,
        wallet: Option<[u8; 20]>,
    ) -> BoxFut<'_, Result<RecipientEligibility, ApiError>> {
        Box::pin(async move {
            Ok(RecipientEligibility {
                direction: "RhnToGlc".into(),
                address,
                wallet: wallet.map(|w| crate::evm::address::EvmAddress::from_bytes(w).to_string()),
                eligible: true,
                blocked_reason: None,
                blocked_reasons: Vec::new(),
                retry_after: None,
                retry_after_seconds: None,
                source_wallet_retry_after: None,
                recipient_retry_after: None,
                window_seconds: 86_400,
            })
        })
    }
    fn route_wallet_eligibility(
        &self,
        route: crate::routes::Route,
        source: Option<String>,
        destination: Option<String>,
    ) -> BoxFut<'_, Result<RouteWalletEligibilityView, ApiError>> {
        Box::pin(async move {
            let leg = |address: String| WalletLegView {
                address,
                eligible: true,
                reason: None,
                retry_after: None,
                retry_after_seconds: None,
            };
            Ok(RouteWalletEligibilityView {
                route: route.as_str().to_string(),
                source: source.map(leg),
                destination: destination.map(leg),
                eligible: true,
                blocked_reason: None,
                blocked_reasons: Vec::new(),
                retry_after: None,
                retry_after_seconds: None,
                window_seconds: 86_400,
                as_of: 0,
            })
        })
    }
    fn robinhood_reserve(&self) -> BoxFut<'_, Result<RobinhoodReserveView, ApiError>> {
        Box::pin(async {
            Ok(RobinhoodReserveView {
                ledger_availability: crate::robinhood::public::AVAILABILITY_NOT_CONFIGURED
                    .to_string(),
                balance_atomic: None,
                protected_minimum_atomic: None,
                reserved_liquidity_atomic: None,
                pending_obligations_atomic: None,
                available_capacity_atomic: None,
                accrued_fees_atomic: None,
                paused: None,
                onchain: RobinhoodOnchainView {
                    availability: crate::robinhood::public::AVAILABILITY_NOT_CONFIGURED.to_string(),
                    encumbered_reserve_atomic: None,
                    protected_min_reserve_atomic: None,
                    deposits_paused: None,
                    payouts_paused: None,
                    inbound_window: None,
                    outbound_window: None,
                    window_seconds: None,
                },
                routes: vec![],
                indexer: RobinhoodIndexerView {
                    configured: false,
                    connected: false,
                    lag_blocks: None,
                    last_success_at: None,
                    halted: false,
                },
                as_of: 0,
            })
        })
    }
    fn robinhood_limits(&self) -> BoxFut<'_, Result<RobinhoodLimitsView, ApiError>> {
        Box::pin(async {
            Ok(RobinhoodLimitsView {
                availability: crate::robinhood::public::AVAILABILITY_NOT_CONFIGURED.to_string(),
                inbound_min_atomic: None,
                inbound_max_atomic: None,
                inbound_rolling_limit_atomic: None,
                outbound_min_atomic: None,
                outbound_max_atomic: None,
                outbound_rolling_limit_atomic: None,
                protected_min_reserve_atomic: None,
                rolling_window_seconds: None,
                rhn_to_glc_rolling_window: None,
                glc_to_rhn_rolling_window: None,
                bridge_fee_bps: 600,
                glc_to_rhn_fee_bps: 600,
                rhn_to_glc_fee_bps: 600,
                as_of: 0,
            })
        })
    }
    fn explorer_events(
        &self,
        _direction: Option<Direction>,
        _state: Option<RequestState>,
        _cursor: Option<i64>,
        _limit: u32,
    ) -> BoxFut<'_, Result<Page<ExplorerEvent>, ApiError>> {
        Box::pin(async {
            Ok(Page {
                items: vec![],
                next_cursor: None,
                as_of: 0,
            })
        })
    }
    fn quote(&self, input: QuoteInput) -> BoxFut<'_, Result<QuoteOutput, ApiError>> {
        Box::pin(async move {
            if input.gross_amount.0 == 0 {
                return Err(ApiError::BadRequest("gross_amount must be > 0".into()));
            }
            Ok(QuoteOutput {
                direction: input.direction,
                gross_amount: input.gross_amount,
                gross_display_amount: "0.00500000".to_string(),
                fee_bps: amount_conversion::BRIDGE_FEE_BPS,
                fee_amount: AtomicU64(15_000),
                fee_display_amount: "0.00030000".to_string(),
                net_amount: AtomicU64(485_000),
                net_display_amount: "0.00470000".to_string(),
                source_decimals: 8,
                destination_decimals: 6,
                source_asset: "GLC (Goldcoin)".to_string(),
                destination_asset: "GLC (Solana)".to_string(),
            })
        })
    }
}

// ------------------------------------------------------------- HTTP routing --
//
// Routing/status-code behavior is exercised against a real server on a
// real (ephemeral) localhost port — `hyper::body::Incoming` isn't
// user-constructible, so a raw-`Request` unit test isn't an option; this
// is the same "spawn the real thing, hit it over HTTP" approach
// tests/daemon_smoke.rs uses for the whole process, just in-process and
// fast here since only this one server needs to run.

/// A listener on an ephemeral loopback port, handed to the server still
/// bound — see `admin_api::tests::bound_listener` for why the port is
/// never released between being chosen and being served on. This harness
/// had the identical race, and the two collide with each other: whichever
/// one lost the port produced a server that never came up.
async fn bound_listener() -> (tokio::net::TcpListener, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

async fn spawn_stub_server() -> (String, tokio::sync::watch::Sender<bool>) {
    let (listener, port) = bound_listener().await;
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        // `serve_on` cannot fail to bind — the listener is already ours.
        let _ = serve_on(listener, Arc::new(StubSource), rx).await;
    });
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if reqwest::get(format!("{base}/status")).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (base, tx)
}

#[tokio::test]
async fn unknown_path_is_404() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/nope")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_status_returns_200_and_json() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/status")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    let body: BridgeStatus = resp.json().await.unwrap();
    assert!(!body.goldcoin_paused);
}

#[tokio::test]
async fn get_limits_and_reserve_return_200() {
    let (base, _tx) = spawn_stub_server().await;
    assert_eq!(
        reqwest::get(format!("{base}/limits"))
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        reqwest::get(format!("{base}/reserve"))
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
}

#[tokio::test]
async fn get_health_returns_200() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: PublicHealth = resp.json().await.unwrap();
    assert!(body.healthy);
}

#[tokio::test]
async fn get_recipient_eligibility_routes_with_an_address() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!(
        "{base}/recipients/sol-to-glc/eligibility?address=mfWxJ45yp2SFn7UciZyNpvDKrzbhyfKrY8"
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: RecipientEligibility = resp.json().await.unwrap();
    assert!(body.eligible);
    assert_eq!(body.direction, "SolToGlc");
}

#[tokio::test]
async fn get_recipient_eligibility_without_an_address_is_400() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/recipients/sol-to-glc/eligibility"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_recipient_eligibility_routes_with_a_wallet_too() {
    let (base, _tx) = spawn_stub_server().await;
    let wallet = Pubkey::new_unique();
    let resp = reqwest::get(format!(
        "{base}/recipients/sol-to-glc/eligibility?address=mfWxJ45yp2SFn7UciZyNpvDKrzbhyfKrY8&wallet={wallet}"
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: RecipientEligibility = resp.json().await.unwrap();
    assert_eq!(body.wallet.as_deref(), Some(wallet.to_string().as_str()));
}

#[tokio::test]
async fn get_recipient_eligibility_with_a_malformed_wallet_is_400() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!(
        "{base}/recipients/sol-to-glc/eligibility?address=mfWxJ45yp2SFn7UciZyNpvDKrzbhyfKrY8&wallet=not-a-pubkey"
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_rhn_recipient_eligibility_routes_with_an_address() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!(
        "{base}/recipients/rhn-to-glc/eligibility?address=mfWxJ45yp2SFn7UciZyNpvDKrzbhyfKrY8"
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: RecipientEligibility = resp.json().await.unwrap();
    assert!(body.eligible);
    assert_eq!(body.direction, "RhnToGlc");
    assert_eq!(body.wallet, None);
}

#[tokio::test]
async fn get_rhn_recipient_eligibility_without_an_address_is_400() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/recipients/rhn-to-glc/eligibility"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_rhn_recipient_eligibility_routes_an_evm_wallet_end_to_end() {
    let (base, _tx) = spawn_stub_server().await;
    let wallet = crate::evm::address::EvmAddress::from_bytes([0xAB; 20]).to_string();
    let resp = reqwest::get(format!(
        "{base}/recipients/rhn-to-glc/eligibility?address=mfWxJ45yp2SFn7UciZyNpvDKrzbhyfKrY8&wallet={wallet}"
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: RecipientEligibility = resp.json().await.unwrap();
    assert_eq!(body.wallet.as_deref(), Some(wallet.as_str()));
}

/// The wallet leg is parsed by `EvmAddress`'s own strict `FromStr`. A
/// malformed wallet must be a 400, never a zero-padded or truncated blob
/// that would then be asked about someone else's rate-limit window — and
/// never a silent fallthrough to the Solana pubkey parser.
#[tokio::test]
async fn get_rhn_recipient_eligibility_with_a_malformed_wallet_is_400() {
    let (base, _tx) = spawn_stub_server().await;
    for bad in [
        "not-a-wallet",
        // A base58 Solana pubkey: valid on the OTHER endpoint, never here.
        "11111111111111111111111111111111",
        // Right shape, wrong length.
        "0xabab",
        // `0X` is refused; only `0x` is a prefix.
        "0XABABABABABABABABABABABABABABABABABABABAB",
        // Mixed case that fails its EIP-55 checksum.
        "0xAbAbabababababababababababababababababAb",
    ] {
        let resp = reqwest::get(format!(
            "{base}/recipients/rhn-to-glc/eligibility?address=mfWxJ45yp2SFn7UciZyNpvDKrzbhyfKrY8&wallet={bad}"
        ))
        .await
        .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "wallet {bad:?} must be refused"
        );
    }
}

#[tokio::test]
async fn post_transfers_with_malformed_body_is_400() {
    let (base, _tx) = spawn_stub_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/transfers"))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_transfers_with_a_business_rule_violation_maps_to_400() {
    let (base, _tx) = spawn_stub_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/transfers"))
        .json(&CreateTransferInput {
            amount_atomic: AtomicU64(0),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_transfers_with_a_valid_body_is_201() {
    let (base, _tx) = spawn_stub_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/transfers"))
        .json(&CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let body: CreateTransferOutput = resp.json().await.unwrap();
    assert_eq!(body.request_id, 7);
}

#[tokio::test]
async fn get_transfers_by_id_round_trips() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/transfers/7")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: TransferView = resp.json().await.unwrap();
    assert_eq!(body.id, 7);
}

#[tokio::test]
async fn get_transfers_by_unknown_id_is_404() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/transfers/9999"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_transfers_with_a_non_numeric_id_is_400() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/transfers/not-a-number"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn shutdown_signal_stops_the_server() {
    let (base, tx) = spawn_stub_server().await;
    tx.send(true).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        reqwest::get(format!("{base}/status")).await.is_err(),
        "the server must stop accepting connections after shutdown"
    );
}

// -------------------------------------------------------- pagination/validation --

#[tokio::test]
async fn get_stats_returns_200_and_json() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/stats")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: BridgeStats = resp.json().await.unwrap();
    assert_eq!(body.bridge_fee_bps, amount_conversion::BRIDGE_FEE_BPS);
}

#[tokio::test]
async fn stats_json_schema_has_the_documented_top_level_fields() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/stats")).await.unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    for field in [
        "goldcoin_paused",
        "solana_paused",
        "glc_to_sol_available",
        "sol_to_glc_available",
        "glc_to_sol_quota_exhausted",
        "sol_to_glc_quota_exhausted",
        "glc_to_sol_rolling_volume_remaining",
        "sol_to_glc_rolling_volume_remaining",
        "bridge_fee_bps",
        "glc_to_sol",
        "sol_to_glc",
        "goldcoin_reserve",
        "solana_reserve",
        "robinhood_reserve",
        "goldcoin_indexer_halted",
        "goldcoin_indexer_seconds_since_tick",
        "solana_indexer_seconds_since_tick",
        "post_finality_reorg_events",
        "as_of",
    ] {
        assert!(
            body.get(field).is_some(),
            "GET /stats must always carry a stable {field:?} field"
        );
    }
}

#[tokio::test]
async fn get_reserves_history_and_explorer_events_return_200() {
    let (base, _tx) = spawn_stub_server().await;
    assert_eq!(
        reqwest::get(format!("{base}/reserves/history"))
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        reqwest::get(format!("{base}/explorer/events"))
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
}

#[tokio::test]
async fn reserves_history_rejects_a_non_numeric_cursor() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/reserves/history?cursor=not-a-number"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reserves_history_rejects_a_zero_limit() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/reserves/history?limit=0"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reserves_history_rejects_a_non_numeric_limit() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/reserves/history?limit=abc"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reserves_history_rejects_an_unknown_direction() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/reserves/history?direction=bogus"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reserves_history_accepts_valid_direction_values() {
    let (base, _tx) = spawn_stub_server().await;
    for direction in ["goldcoin", "solana"] {
        let resp = reqwest::get(format!("{base}/reserves/history?direction={direction}"))
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }
}

#[tokio::test]
async fn explorer_events_rejects_a_non_numeric_cursor() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/explorer/events?cursor=not-a-number"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn explorer_events_rejects_a_zero_limit() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/explorer/events?limit=0"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn explorer_events_rejects_an_unknown_direction() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/explorer/events?direction=bogus"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn explorer_events_rejects_an_unknown_state() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/explorer/events?state=NotARealState"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn explorer_events_accepts_valid_direction_and_state_values() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!(
        "{base}/explorer/events?direction=GlcToSol&state=AwaitingDeposit"
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn pagination_empty_query_string_values_fall_back_to_defaults() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/reserves/history?cursor=&limit=&direction="))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

// ------------------------------- atomic amounts are strings on the wire --

/// The exact `GET /stats` payload production served when the Reserves page
/// broke, with the real `settled_volume_atomic = 9408405829927559`.
///
/// This is the live-payload regression fixture: it pins the FULL response
/// shape, not just one field, so a future field added as a bare number is
/// caught here rather than in a browser. Kept byte-exact deliberately —
/// the UI's own fixture mirrors this same JSON.
fn production_stats() -> BridgeStats {
    BridgeStats {
        goldcoin_paused: false,
        solana_paused: false,
        glc_to_sol_available: true,
        sol_to_glc_available: true,
        sol_to_glc_availability_reason: None,
        glc_to_sol_quota_exhausted: false,
        sol_to_glc_quota_exhausted: false,
        glc_to_sol_rolling_volume_remaining: AtomicU64(17_500_000_000),
        sol_to_glc_rolling_volume_remaining: AtomicU64(100_000_000_000),
        bridge_fee_bps: 300,
        route_fees: Vec::new(),
        glc_to_sol: DirectionStats {
            total_requests: 41,
            in_progress_requests: 2,
            settled_requests: 36,
            manual_review_requests: 3,
        },
        sol_to_glc: DirectionStats {
            total_requests: 18,
            in_progress_requests: 1,
            settled_requests: 14,
            manual_review_requests: 3,
        },
        goldcoin_reserve: ReserveStats {
            paused: false,
            available_capacity: AtomicI64(425_000_000_000_000),
            // The value that broke the page.
            settled_volume_atomic: AtomicU64(9_408_405_829_927_559),
            accrued_fees_atomic: AtomicU64(290_982_654_018),
        },
        solana_reserve: ReserveStats {
            paused: false,
            available_capacity: AtomicI64(-1),
            settled_volume_atomic: AtomicU64(1_284_902_004_551),
            accrued_fees_atomic: AtomicU64(39_739_237),
        },
        // A CONFIGURED Robinhood reserve, so this fixture exercises the
        // arm where the atomic fields carry real values and the
        // string-encoding guard has something to check. The
        // not-configured arm is covered separately by
        // `not_configured_robinhood_stats`, which must serialize nulls.
        //
        // `settled_volume_atomic` is deliberately past 2^53 here too: it
        // is the field that broke the Reserves page on the Goldcoin
        // reserve (docs/31), and a THIRD reserve carrying the same
        // counter must not reintroduce the defect.
        robinhood_reserve: RobinhoodReserveStats {
            ledger_availability: crate::robinhood::public::AVAILABILITY_AVAILABLE.to_string(),
            paused: Some(false),
            available_capacity: Some(AtomicI64(77_500_000_000_000)),
            settled_volume_atomic: Some(AtomicU64(9_007_199_254_740_993)),
            accrued_fees_atomic: Some(AtomicU64(4_182_119_004)),
        },
        goldcoin_indexer_halted: false,
        goldcoin_indexer_seconds_since_tick: 4,
        solana_indexer_seconds_since_tick: 3,
        post_finality_reorg_events: 0,
        as_of: 1_788_600_000,
    }
}

#[test]
fn the_production_stats_payload_serializes_every_atomic_amount_as_a_string() {
    let json = serde_json::to_string(&production_stats()).unwrap();

    // The exact digits must appear, quoted. Before this change the field
    // was a bare number and a JavaScript client read 9408405829927560.
    assert!(
        json.contains("\"settled_volume_atomic\":\"9408405829927559\""),
        "{json}"
    );
    assert!(
        !json.contains("9408405829927560"),
        "the corrupted value must appear nowhere: {json}"
    );

    // Every atomic field, on both reserves, quoted.
    for needle in [
        "\"available_capacity\":\"425000000000000\"",
        "\"accrued_fees_atomic\":\"290982654018\"",
        "\"available_capacity\":\"-1\"",
        "\"settled_volume_atomic\":\"1284902004551\"",
        "\"accrued_fees_atomic\":\"39739237\"",
        "\"glc_to_sol_rolling_volume_remaining\":\"17500000000\"",
        "\"sol_to_glc_rolling_volume_remaining\":\"100000000000\"",
        // The third reserve, held to exactly the same contract. The
        // settled-volume value here is 2^53 + 1 — the smallest integer a
        // JavaScript double CANNOT represent — so if this field ever
        // regressed to a bare number, a client would read 9007199254740992
        // and this assertion would fail.
        "\"available_capacity\":\"77500000000000\"",
        "\"settled_volume_atomic\":\"9007199254740993\"",
        "\"accrued_fees_atomic\":\"4182119004\"",
    ] {
        assert!(json.contains(needle), "missing {needle} in {json}");
    }
    assert!(
        !json.contains("9007199254740992"),
        "the double-rounded value must appear nowhere: {json}"
    );

    // Bounded fields stay plain numbers — a string there would be churn
    // for every client with nothing gained.
    for needle in [
        "\"bridge_fee_bps\":300",
        "\"total_requests\":41",
        "\"post_finality_reorg_events\":0",
        "\"as_of\":1788600000",
        "\"goldcoin_indexer_seconds_since_tick\":4",
    ] {
        assert!(json.contains(needle), "missing {needle} in {json}");
    }

    // And it round-trips back to the identical value.
    let back: BridgeStats = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back.goldcoin_reserve.settled_volume_atomic.0,
        9_408_405_829_927_559
    );
    assert_eq!(back.solana_reserve.available_capacity.0, -1);
    assert_eq!(
        back.robinhood_reserve
            .settled_volume_atomic
            .expect("configured")
            .0,
        9_007_199_254_740_993,
        "the third reserve's counter round-trips exactly, past 2^53"
    );
}

/// Contract guard across EVERY public DTO carrying an atomic amount: the
/// field must be a JSON string. Walks the serialized value rather than
/// asserting field by field, so a new atomic field on any of these is
/// caught the moment it is added as a number.
#[test]
fn every_atomic_field_on_every_public_dto_is_a_json_string() {
    /// Field names whose values must be JSON strings wherever they appear.
    const ATOMIC_FIELDS: [&str; 19] = [
        "settled_volume_atomic",
        "accrued_fees_atomic",
        "available_capacity",
        "goldcoin_available_capacity",
        "solana_available_capacity",
        "glc_to_sol_rolling_volume_remaining",
        "sol_to_glc_rolling_volume_remaining",
        "min_transfer_amount",
        "per_transfer_limit",
        "expected_atomic",
        "observed_atomic",
        "delta_atomic",
        "gross_amount_atomic",
        "fee_amount_atomic",
        "net_amount_atomic",
        "amount_atomic",
        "observed_amount_atomic",
        "refund_amount_atomic",
        "fee_charged_atomic",
    ];

    /// `null` is accepted alongside a string, and ONLY those two.
    ///
    /// The guard's subject is the failure in docs/31: an atomic amount
    /// arriving as a JSON NUMBER, which a JavaScript client silently
    /// rounds past 2^53. `null` cannot be misparsed into a wrong balance —
    /// it is the honest encoding for `robinhood_reserve`'s fields on a
    /// deployment with no `[reserve.robinhood]` section, where the
    /// alternative would be `0` claiming an empty reserve exists.
    ///
    /// This costs the guard nothing on the non-optional fields: a
    /// `ReserveStats` figure is an `AtomicU64`/`AtomicI64`, which can
    /// never serialize as null, so for those the assertion is still
    /// exactly "must be a string".
    fn assert_atomics_are_strings(value: &serde_json::Value, fields: &[&str], where_: &str) {
        match value {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    if fields.contains(&k.as_str()) {
                        assert!(
                            v.is_string() || v.is_null(),
                            "{where_}.{k} must serialize as a JSON string (or null where the \
                             figure is genuinely absent), got {v}"
                        );
                    }
                    assert_atomics_are_strings(v, fields, &format!("{where_}.{k}"));
                }
            }
            serde_json::Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    assert_atomics_are_strings(v, fields, &format!("{where_}[{i}]"));
                }
            }
            _ => {}
        }
    }

    let payloads: Vec<(&str, serde_json::Value)> = vec![
        ("/stats", serde_json::to_value(production_stats()).unwrap()),
        // The same endpoint on a deployment with no Robinhood reserve —
        // the shape EVERY production deployment serves today. Walked
        // through the identical guard so the not-configured arm can never
        // start emitting a bare number either.
        (
            "/stats (robinhood not configured)",
            serde_json::to_value(not_configured_robinhood_stats()).unwrap(),
        ),
        (
            "/reserve",
            serde_json::to_value(ReserveAvailability {
                goldcoin_available_capacity: AtomicI64(9_408_405_829_927_559),
                solana_available_capacity: AtomicI64(-9_408_405_829_927_559),
            })
            .unwrap(),
        ),
        (
            "/limits",
            serde_json::to_value(TransferLimits {
                min_transfer_amount: AtomicU64(100_000_000),
                per_transfer_limit: AtomicU64(9_408_405_829_927_559),
                bridge_fee_bps: 300,
            })
            .unwrap(),
        ),
        (
            "/status",
            serde_json::to_value(BridgeStatus {
                goldcoin_paused: false,
                solana_paused: false,
                vault_address: "vault".to_string(),
                next_solana_obligation_index: 7,
                glc_to_sol_available: true,
                sol_to_glc_available: true,
                glc_to_sol_quota_exhausted: false,
                sol_to_glc_quota_exhausted: false,
                glc_to_sol_rolling_volume_remaining: AtomicU64(9_408_405_829_927_559),
                sol_to_glc_rolling_volume_remaining: AtomicU64(0),
                sol_to_glc_admission_open: true,
                goldcoin_destination_admission_open: true,
                sol_to_glc_availability_reason: None,
                sol_to_glc_capacity: None,
            })
            .unwrap(),
        ),
        (
            "/reserves/history",
            serde_json::to_value(Page {
                items: vec![ReserveHistoryEntry {
                    id: 1,
                    direction: "GoldcoinReserve".to_string(),
                    detected_at: 1_788_600_000,
                    expected_atomic: AtomicI64(9_408_405_829_927_559),
                    observed_atomic: AtomicI64(9_408_405_829_927_558),
                    delta_atomic: AtomicI64(-1),
                    classification: "OK".to_string(),
                    auto_paused: false,
                }],
                next_cursor: None,
                as_of: 1_788_600_000,
            })
            .unwrap(),
        ),
        (
            "/transfers",
            serde_json::to_value(TransferView {
                id: 1,
                direction: "GlcToSol".to_string(),
                state: "Settled".to_string(),
                gross_amount_atomic: AtomicU64(9_408_405_829_927_559),
                fee_bps: 300,
                fee_amount_atomic: AtomicU64(282_252_174_897_826),
                net_amount_atomic: AtomicU64(9_126_153_655_029_733),
                created_at: 1_788_600_000,
                source_txid: None,
                source_confirmations: 6,
                required_source_confirmations: Some(6),
                destination_txid: None,
                failure_reason: None,
                // A refunded transfer's amounts sit one level deeper; the
                // guard recurses, so they are held to the same string
                // contract as the flat ones.
                refund: Some(RefundView {
                    state: "Refunded".to_string(),
                    observed_amount_atomic: AtomicU64(9_408_405_829_927_559),
                    refund_amount_atomic: AtomicU64(9_408_405_829_927_559),
                    fee_charged_atomic: AtomicU64(0),
                    refund_txid: Some("ff".repeat(32)),
                    broadcast_at: Some(1_788_600_100),
                    refunded_at: Some(1_788_600_500),
                }),
            })
            .unwrap(),
        ),
        (
            "/quote",
            serde_json::to_value(QuoteOutput {
                direction: "GlcToSol".to_string(),
                gross_amount: AtomicU64(9_408_405_829_927_559),
                gross_display_amount: "94084058.29927559".to_string(),
                fee_bps: 300,
                fee_amount: AtomicU64(282_252_174_897_826),
                fee_display_amount: "2822521.74897826".to_string(),
                net_amount: AtomicU64(9_126_153_655_029_733),
                net_display_amount: "91261536.55029733".to_string(),
                source_decimals: 8,
                destination_decimals: 6,
                source_asset: "GLC (Goldcoin)".to_string(),
                destination_asset: "GLC (Solana)".to_string(),
            })
            .unwrap(),
        ),
    ];

    for (endpoint, payload) in payloads {
        assert_atomics_are_strings(&payload, &ATOMIC_FIELDS, endpoint);
    }

    // The guard is not vacuous: a numeric atomic field — the exact shape
    // production served — must fail it.
    let regressed = serde_json::json!({
        "goldcoin_reserve": { "settled_volume_atomic": 9_408_405_829_927_559u64 }
    });
    let caught = std::panic::catch_unwind(|| {
        assert_atomics_are_strings(&regressed, &ATOMIC_FIELDS, "regressed");
    });
    assert!(
        caught.is_err(),
        "the contract guard must reject an atomic field serialized as a number"
    );
}

/// `production_stats()`'s twin for the arm every production deployment is
/// actually in: no `[reserve.robinhood]` section, so no `reserve_ledger`
/// row, so every Robinhood figure is `null`.
fn not_configured_robinhood_stats() -> BridgeStats {
    BridgeStats {
        robinhood_reserve: RobinhoodReserveStats {
            ledger_availability: crate::robinhood::public::AVAILABILITY_NOT_CONFIGURED.to_string(),
            paused: None,
            available_capacity: None,
            settled_volume_atomic: None,
            accrued_fees_atomic: None,
        },
        ..production_stats()
    }
}

// ------------------------------------------- /stats: the Robinhood reserve --
//
// `GET /stats` published `goldcoin_reserve` and `solana_reserve` but not
// the third physical reserve, even though the authoritative
// `RobinhoodReserve` state was already there and already served to
// operators by `glc-admin robinhood-status` / `robinhood-reserve`.
//
// The reason it could not simply be a third `ReserveStats` is the subject
// of the first two tests below: the Robinhood reserve may not EXIST, and
// the field had to be added without turning the whole endpoint into a 500
// on the deployments that were working.

/// The production shape today: Goldcoin and Solana configured, Robinhood
/// not. `GET /stats` must still succeed, and must say "not configured"
/// rather than inventing zeroes.
///
/// This is the test that would have caught a naive
/// `robinhood_reserve: ReserveStats` — `ledger.is_paused(RobinhoodReserve)`
/// raises `ReserveNotInitialized` here, and a `?` on it would fail the
/// entire request.
#[tokio::test]
async fn stats_succeeds_and_reports_not_configured_when_no_robinhood_reserve_exists() {
    let dir = tempfile::tempdir().unwrap();
    // `configure` deliberately sets up ONLY Goldcoin and Solana — the
    // production posture.
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let stats = api
        .stats()
        .await
        .expect("an unconfigured Robinhood reserve must not fail the whole endpoint");

    assert_eq!(
        stats.robinhood_reserve.ledger_availability,
        "not_configured"
    );
    assert_eq!(stats.robinhood_reserve.paused, None);
    assert!(stats.robinhood_reserve.available_capacity.is_none());
    assert!(stats.robinhood_reserve.settled_volume_atomic.is_none());
    assert!(stats.robinhood_reserve.accrued_fees_atomic.is_none());
}

/// Absent is not zero, on the wire. A `0` would claim an empty Robinhood
/// reserve exists; `null` says the deployment has none.
#[tokio::test]
async fn an_unconfigured_robinhood_reserve_serializes_as_null_never_zero() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let stats = api.stats().await.unwrap();

    let json = serde_json::to_value(&stats).unwrap();
    let rh = &json["robinhood_reserve"];
    for field in [
        "paused",
        "available_capacity",
        "settled_volume_atomic",
        "accrued_fees_atomic",
    ] {
        assert!(
            rh[field].is_null(),
            "robinhood_reserve.{field} must be null, not {}",
            rh[field]
        );
    }
    assert_eq!(rh["ledger_availability"], "not_configured");
}

/// A CONFIGURED Robinhood reserve is read from the ledger, not defaulted.
#[tokio::test]
async fn stats_publishes_a_configured_robinhood_reserve_from_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::RobinhoodReserve,
                10_000_000, // balance
                2_000_000,  // protected minimum
                8_000_000,
                5_000_000,
                3_000_000,
                0,
            )
            .unwrap();
    }
    let api = build(&db_path, 0);
    let stats = api.stats().await.unwrap();

    assert_eq!(stats.robinhood_reserve.ledger_availability, "available");
    assert_eq!(stats.robinhood_reserve.paused, Some(false));
    assert_eq!(
        stats.robinhood_reserve.available_capacity.map(|c| c.0),
        Some(8_000_000),
        "balance - protected_minimum - reserved_liquidity, the same \
         formula the other two reserves report"
    );
    assert_eq!(
        stats.robinhood_reserve.settled_volume_atomic.map(|v| v.0),
        Some(0),
        "a real counter reading zero — distinct from the null a \
         nonexistent reserve reports"
    );
    assert_eq!(
        stats.robinhood_reserve.accrued_fees_atomic.map(|v| v.0),
        Some(0)
    );
}

/// The settled counter, proved NON-zero by a real settled `GlcToRhn`
/// payout rather than by a hand-written row: create the request through
/// the API, take its Goldcoin deposit through the real observation /
/// confirmation / finality transitions, then settle it with
/// `mark_robinhood_payout_settled` — the one function that advances
/// `settled_liquidity_total` on this reserve. `/stats` must report that
/// amount, exactly, and not a figure summed from request history.
///
/// It also pins where a `GlcToRhn` FEE lands. The fee is withheld on the
/// source side, Goldcoin, so this reserve's `accrued_fees_atomic` stays
/// `0` while `goldcoin_reserve.accrued_fees_atomic` rises — a zero that
/// is correct, not a missing read.
#[tokio::test]
async fn stats_reports_a_real_settled_robinhood_payout_from_the_authoritative_counter() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);

    let created = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: TEST_EVM_RECIPIENT.to_string(),
            route: Some("GlcToRhn".to_string()),
            source_address: None,
        })
        .await
        .unwrap();
    let net_settled = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        let net = ledger
            .get_request(created.request_id)
            .unwrap()
            .unwrap()
            .net_destination_atomic;
        ledger
            .record_glc_deposit_observed(
                created.request_id,
                [0xD1; 32],
                0,
                500_000,
                10,
                [0xB1; 32],
                1_000,
            )
            .unwrap();
        ledger
            .update_glc_confirmations(created.request_id, 6)
            .unwrap();
        ledger
            .mark_glc_source_finalized(created.request_id, 1_100)
            .unwrap();
        ledger
            .mark_robinhood_payout_settled(created.request_id, 1_200)
            .unwrap();
        net
    };
    assert!(net_settled > 0, "the fixture must settle a real amount");

    let stats = api.stats().await.unwrap();
    let ledger = Ledger::open(&db_path).unwrap();

    assert_eq!(
        stats.robinhood_reserve.settled_volume_atomic.map(|v| v.0),
        Some(net_settled),
        "the real amount that just settled out of this reserve"
    );
    assert_eq!(
        stats.robinhood_reserve.settled_volume_atomic.map(|v| v.0),
        Some(
            ledger
                .settled_liquidity(ReserveDirection::RobinhoodReserve)
                .unwrap()
        ),
        "read from `reserve_ledger.settled_liquidity_total`, nowhere else"
    );
    assert_eq!(
        stats.robinhood_reserve.available_capacity.map(|c| c.0),
        Some(
            ledger
                .available_capacity(ReserveDirection::RobinhoodReserve)
                .unwrap()
        )
    );

    // The fee accrued on GOLDCOIN, where it was withheld.
    assert_eq!(
        stats.robinhood_reserve.accrued_fees_atomic.map(|v| v.0),
        Some(0)
    );
    assert!(stats.goldcoin_reserve.accrued_fees_atomic.0 > 0);

    // On the wire: a decimal string in canonical 8dp units, like the two
    // reserves beside it.
    let json = serde_json::to_value(&stats).unwrap();
    assert_eq!(
        json["robinhood_reserve"]["settled_volume_atomic"],
        serde_json::Value::String(net_settled.to_string())
    );
}

/// The local `RobinhoodReserve.paused` gate reaches `/stats`.
#[tokio::test]
async fn stats_reflects_the_local_robinhood_reserve_pause() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::RobinhoodReserve,
                10_000_000,
                0,
                8_000_000,
                5_000_000,
                3_000_000,
                0,
            )
            .unwrap();
        ledger
            .set_paused(ReserveDirection::RobinhoodReserve, true, Some("OPS-1400"))
            .unwrap();
    }
    let api = build(&db_path, 0);
    let stats = api.stats().await.unwrap();

    assert_eq!(stats.robinhood_reserve.paused, Some(true));
    assert!(
        !stats.goldcoin_paused && !stats.solana_paused,
        "pausing the Robinhood reserve must not move the other two"
    );
}

/// `/stats` and `/robinhood/reserve` must never disagree: both are
/// projections of `robinhood::admin::reserve_report`, the same one
/// `glc-admin robinhood-status` / `robinhood-reserve` print. Pinned so a
/// future change cannot give `/stats` its own second reading of
/// `reserve_ledger` that drifts from the operator-facing one.
#[tokio::test]
async fn stats_and_the_robinhood_reserve_endpoint_agree() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::RobinhoodReserve,
                7_777_777,
                1_111_111,
                6_000_000,
                4_000_000,
                2_000_000,
                0,
            )
            .unwrap();
    }
    let api = build(&db_path, 0);
    let stats = api.stats().await.unwrap();
    let view = api.robinhood_reserve().await.unwrap();

    assert_eq!(
        stats.robinhood_reserve.ledger_availability,
        view.ledger_availability
    );
    assert_eq!(stats.robinhood_reserve.paused, view.paused);
    assert_eq!(
        stats.robinhood_reserve.available_capacity.map(|c| c.0),
        view.available_capacity_atomic.map(|c| c.0)
    );
    assert_eq!(
        stats.robinhood_reserve.accrued_fees_atomic.map(|v| v.0),
        view.accrued_fees_atomic.map(|v| v.0)
    );
}

/// The two existing reserves are byte-for-byte unchanged by the addition.
/// The whole point of an additive field is that nothing else moves.
#[tokio::test]
async fn adding_the_robinhood_reserve_does_not_disturb_goldcoin_or_solana() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let stats = api.stats().await.unwrap();

    // `configure` funds both at 10_000_000 with a zero protected minimum.
    assert!(!stats.goldcoin_reserve.paused);
    assert_eq!(stats.goldcoin_reserve.available_capacity.0, 10_000_000);
    assert_eq!(stats.goldcoin_reserve.settled_volume_atomic.0, 0);
    assert_eq!(stats.goldcoin_reserve.accrued_fees_atomic.0, 0);
    assert!(!stats.solana_reserve.paused);
    assert_eq!(stats.solana_reserve.available_capacity.0, 10_000_000);
    assert_eq!(stats.solana_reserve.settled_volume_atomic.0, 0);
    assert_eq!(stats.solana_reserve.accrued_fees_atomic.0, 0);

    // And the two reserve objects still carry EXACTLY their four
    // historical keys — a client's existing parser must not meet a new
    // one where it did not expect it.
    let json = serde_json::to_value(&stats).unwrap();
    for reserve in ["goldcoin_reserve", "solana_reserve"] {
        let keys: std::collections::BTreeSet<&str> = json[reserve]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "accrued_fees_atomic",
                "available_capacity",
                "paused",
                "settled_volume_atomic",
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
            "{reserve} must keep exactly its historical key set"
        );
    }
}

/// The Robinhood entry's field NAMES match `ReserveStats`'s, so a client
/// can reuse the renderer it already has for the other two reserves. Only
/// the `ledger_availability` discriminator is additional.
#[tokio::test]
async fn the_robinhood_entry_mirrors_the_reserve_stats_field_names() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let json = serde_json::to_value(api.stats().await.unwrap()).unwrap();

    let robinhood: std::collections::BTreeSet<&str> = json["robinhood_reserve"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let goldcoin: std::collections::BTreeSet<&str> = json["goldcoin_reserve"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();

    assert!(
        goldcoin.is_subset(&robinhood),
        "every ReserveStats key must also appear on robinhood_reserve; \
         missing: {:?}",
        goldcoin.difference(&robinhood).collect::<Vec<_>>()
    );
    assert_eq!(
        robinhood.difference(&goldcoin).collect::<Vec<_>>(),
        vec![&"ledger_availability"],
        "the ONLY extra key is the availability discriminator"
    );
}

/// `POST` inputs stay backward compatible: a client sending the old JSON
/// number keeps working, and the new string form works too.
#[test]
fn transfer_and_quote_inputs_accept_both_a_number_and_a_string() {
    let from_number: CreateTransferInput =
        serde_json::from_str(r#"{"amount_atomic":500000,"recipient":"r"}"#).unwrap();
    let from_string: CreateTransferInput =
        serde_json::from_str(r#"{"amount_atomic":"500000","recipient":"r"}"#).unwrap();
    assert_eq!(from_number.amount_atomic.0, 500_000);
    assert_eq!(from_string.amount_atomic.0, 500_000);

    let q_number: QuoteInput =
        serde_json::from_str(r#"{"direction":"GlcToSol","gross_amount":500000}"#).unwrap();
    let q_string: QuoteInput =
        serde_json::from_str(r#"{"direction":"GlcToSol","gross_amount":"9408405829927559"}"#)
            .unwrap();
    assert_eq!(q_number.gross_amount.0, 500_000);
    assert_eq!(
        q_string.gross_amount.0, 9_408_405_829_927_559,
        "the string form carries amounts a JSON number could not"
    );
}

/// The refund-amount presentation contract, driven end to end through the
/// real ledger transitions on request #2477's exact shape.
///
/// #2477 was a `GlcToSol` request for 29 100 GLC whose deposit actually
/// arrived as 29 050 GLC. It parked on `deposit_amount_mismatch`, was
/// refunded in full, was charged no bridge fee, and released nothing on
/// Solana — yet `GET /transfers/:id` carried only the quote's
/// gross/fee/net trio, so the page read "you bridge 29 100 / fee 873 /
/// you receive 28 227". Every figure described a settlement that never
/// happened.
///
/// These assertions are about what the ENDPOINT exposes: that the
/// authoritative deposited and refunded amounts are present and come from
/// the refund row, and that the fee actually charged is stated as zero
/// rather than left for a client to infer.
mod refund_amount_presentation {
    use super::*;
    use crate::goldcoin::coin::VaultUtxo;
    use crate::ledger::{CreateRequestOutcome, RequestAmounts};

    /// #2477's figures, in canonical atomic units (8 decimals).
    const EXPECTED_GROSS: u64 = 2_910_000_000_000; // 29 100 GLC requested
    const OBSERVED: u64 = 2_905_000_000_000; // 29 050 GLC actually deposited
    const QUOTED_FEE: u64 = 87_300_000_000; // 873 GLC, never charged
    const QUOTED_NET: u64 = 2_822_700_000_000; // 28 227 GLC, never delivered

    const DEPOSIT_TXID: [u8; 32] = [0xAA; 32];
    const DEPOSIT_VOUT: u32 = 1;
    const REFUND_TXID: [u8; 32] = [0xF7; 32];
    const INPUT_TXID: [u8; 32] = [0xCC; 32];
    const PREV_TXID: [u8; 32] = [0xEA; 32];
    const SENDER_HASH: [u8; 20] = [0x5A; 20];
    const INPUT_AMOUNT: u64 = 3_000_000_000_000;
    const MINER_FEE: u64 = 50_000;

    /// Reserves big enough for a 29 100 GLC request — the shared
    /// [`configure`] helper's capacities are orders of magnitude too small
    /// for #2477's real size.
    fn configure_large(dir: &std::path::Path) -> std::path::PathBuf {
        let db_path = dir.join("ledger.sqlite3");
        let mut ledger = Ledger::open(&db_path).unwrap();
        for direction in [
            ReserveDirection::GoldcoinReserve,
            ReserveDirection::SolanaReserve,
        ] {
            ledger
                .configure_reserve(
                    direction,
                    100_000_000_000_000,
                    1_000,
                    50_000_000_000_000,
                    20_000_000_000_000,
                    10_000,
                    1_000,
                )
                .unwrap();
        }
        db_path
    }

    /// Walks #2477 through the real ledger transitions, stopping at
    /// `stop_at`, so each refund-lifecycle state is exercised by the same
    /// construction rather than by three hand-built rows.
    fn seed_2477(db_path: &std::path::Path, stop_at: RequestState) -> i64 {
        let mut ledger = Ledger::open(db_path).unwrap();
        ledger
            .conn_for_tests()
            .execute(
                "INSERT INTO vault_utxos (txid, vout, amount_atomic, script_pubkey_hex,
                                          confirmations, first_seen_at, state)
                 VALUES (?1, 0, ?2, 'a914deadbeef87', 50, 1000, 'Available')",
                rusqlite::params![INPUT_TXID.as_slice(), INPUT_AMOUNT as i64],
            )
            .unwrap();

        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(
                Direction::GlcToSol,
                RequestAmounts {
                    gross_atomic: EXPECTED_GROSS,
                    fee_bps: amount_conversion::BRIDGE_FEE_BPS,
                    fee_atomic: QUOTED_FEE,
                    net_atomic: QUOTED_NET,
                    net_destination_atomic: QUOTED_NET,
                },
                &[1u8; 32],
                None,
                3600,
                1_000,
            )
            .unwrap()
        else {
            panic!("expected a reservation")
        };

        // The indexer sees 29 050 where 29 100 was expected and parks the
        // request — the transition that made #2477 a ManualReview.
        ledger
            .record_glc_deposit_observed(
                request_id,
                DEPOSIT_TXID,
                DEPOSIT_VOUT,
                OBSERVED,
                10,
                [0xBB; 32],
                1_100,
            )
            .unwrap();
        if stop_at == RequestState::ManualReview {
            return request_id;
        }

        ledger
            .begin_goldcoin_refund(
                request_id,
                OBSERVED,
                PREV_TXID,
                0,
                SENDER_HASH,
                "mfTestSenderAddress1111111111111111",
                MINER_FEE,
                &[VaultUtxo {
                    txid: INPUT_TXID,
                    vout: 0,
                    amount_atomic: INPUT_AMOUNT,
                    script_pubkey_hex: "a914deadbeef87".to_string(),
                }],
                "00",
                "refunding the mismatched deposit",
                "operator",
                1_200,
            )
            .unwrap();
        if stop_at == RequestState::RefundPending {
            return request_id;
        }

        ledger
            .record_goldcoin_refund_signed(request_id, "00", 1_300)
            .unwrap();
        ledger
            .record_goldcoin_refund_broadcast(request_id, REFUND_TXID, 1_400)
            .unwrap();
        if stop_at == RequestState::RefundBroadcast {
            return request_id;
        }

        ledger
            .record_goldcoin_refund_confirmed(request_id, 6, 1_500)
            .unwrap();
        assert_eq!(stop_at, RequestState::Refunded);
        request_id
    }

    #[tokio::test]
    async fn refunded_2477_exposes_the_real_refund_principal_and_a_zero_fee() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = configure_large(dir.path());
        let id = seed_2477(&db_path, RequestState::Refunded);
        let api = build(&db_path, 0);

        let view = api.get_transfer(id).await.unwrap().unwrap();
        assert_eq!(view.state, "Refunded");

        let refund = view
            .refund
            .expect("a Refunded transfer must carry its refund facts");
        assert_eq!(refund.state, "Refunded");
        assert_eq!(
            refund.refund_amount_atomic.0, OBSERVED,
            "the refund principal is the 29 050 GLC actually deposited"
        );
        assert_eq!(
            refund.observed_amount_atomic.0, OBSERVED,
            "the deposited amount is reported independently of the expected gross"
        );
        assert_eq!(
            refund.fee_charged_atomic.0, 0,
            "a request that never settled was never charged a bridge fee"
        );
        assert_eq!(refund.refund_txid, Some(glc_hex::encode(&REFUND_TXID)));
        assert_eq!(refund.refunded_at, Some(1_500));

        // The quote trio is still carried — it is the honest record of what
        // was REQUESTED — but it is now distinguishable from the outcome,
        // which is the whole point.
        assert_eq!(view.gross_amount_atomic.0, EXPECTED_GROSS);
        assert_ne!(
            refund.refund_amount_atomic.0, view.gross_amount_atomic.0,
            "#2477's refund must not be derivable from the expected gross"
        );
        assert_ne!(refund.refund_amount_atomic.0, view.net_amount_atomic.0);
        assert_ne!(refund.refund_amount_atomic.0, view.fee_amount_atomic.0);
    }

    #[tokio::test]
    async fn every_refund_lifecycle_state_carries_the_authoritative_amounts() {
        for (state, refund_state, expect_txid) in [
            (RequestState::RefundPending, "Built", false),
            (RequestState::RefundBroadcast, "Broadcast", true),
            (RequestState::Refunded, "Refunded", true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db_path = configure_large(dir.path());
            let id = seed_2477(&db_path, state);
            let api = build(&db_path, 0);

            let view = api.get_transfer(id).await.unwrap().unwrap();
            assert_eq!(view.state, state.as_str());
            let refund = view
                .refund
                .unwrap_or_else(|| panic!("{} must carry its refund facts", state.as_str()));
            assert_eq!(
                refund.state, refund_state,
                "the refund row's own state is finer-grained than the request's"
            );
            assert_eq!(refund.refund_amount_atomic.0, OBSERVED);
            assert_eq!(refund.observed_amount_atomic.0, OBSERVED);
            assert_eq!(refund.fee_charged_atomic.0, 0);
            assert_eq!(
                refund.refund_txid.is_some(),
                expect_txid,
                "a refund transaction is only named once it exists"
            );
        }
    }

    #[tokio::test]
    async fn a_request_outside_the_refund_lifecycle_carries_no_refund_object() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = configure_large(dir.path());
        let id = seed_2477(&db_path, RequestState::ManualReview);
        let api = build(&db_path, 0);

        let view = api.get_transfer(id).await.unwrap().unwrap();
        assert_eq!(view.state, "ManualReview");
        assert!(
            view.refund.is_none(),
            "no refund has been started, so there is no refund to describe"
        );
    }

    /// `GET /transfers` shares the same projection, so a refunded row in a
    /// listing must not fall back to the misleading trio either.
    #[tokio::test]
    async fn the_listing_projection_carries_the_refund_too() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = configure_large(dir.path());
        let id = seed_2477(&db_path, RequestState::Refunded);
        let api = build(&db_path, 0);

        let page = api.list_transfers(None, None, None, 10).await.unwrap();
        let item = page
            .items
            .iter()
            .find(|t| t.id == id)
            .expect("the refunded transfer must appear in the listing");
        assert_eq!(
            item.refund.as_ref().map(|r| r.refund_amount_atomic.0),
            Some(OBSERVED)
        );
    }
}

// ===================================================================== //
// Robinhood route gating (Phase 1)                                      //
// ===================================================================== //
//
// These exercise the REAL `BridgeApi` — not `StubSource` — because the gate
// lives in `BridgeApi::resolve_route` and a stub would prove nothing about
// it. The recurring assertion is not just "the call failed" but "the call
// failed AND the ledger is untouched": a route that is refused must leave
// no request row, no reserved liquidity and no derived deposit address
// behind, or a rejected transfer would still consume real capacity.

/// Total reserved liquidity across both reserves, plus the request count —
/// the three numbers a leaked write would move.
fn ledger_footprint(db_path: &std::path::Path) -> (i64, i64, i64) {
    let ledger = Ledger::open(db_path).unwrap();
    let goldcoin = ledger
        .available_capacity(ReserveDirection::GoldcoinReserve)
        .unwrap();
    let solana = ledger
        .available_capacity(ReserveDirection::SolanaReserve)
        .unwrap();
    let requests: i64 = Direction::ALL
        .iter()
        .map(|d| {
            ledger
                .request_state_counts(*d)
                .unwrap()
                .iter()
                .map(|(_, n)| *n)
                .sum::<i64>()
        })
        .sum();
    (goldcoin, solana, requests)
}

#[tokio::test]
async fn post_transfers_refuses_both_robinhood_routes_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let before = ledger_footprint(&db_path);

    for route in ["GlcToRhn", "RhnToGlc"] {
        let err = api
            .create_goldcoin_deposit_transfer(CreateTransferInput {
                amount_atomic: AtomicU64(500_000),
                recipient: Keypair::new().pubkey().to_string(),
                route: Some(route.to_string()),
                source_address: None,
            })
            .await
            .expect_err("a disabled route must never create a transfer");
        assert!(
            matches!(err, ApiError::RouteDisabled),
            "{route} must be refused as RouteDisabled, got {err:?}"
        );
        assert_eq!(err.status(), StatusCode::CONFLICT);
    }

    assert_eq!(
        ledger_footprint(&db_path),
        before,
        "a refused route must leave no request row and no reserved liquidity"
    );
}

#[tokio::test]
async fn quote_refuses_both_robinhood_routes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    for route in ["GlcToRhn", "RhnToGlc"] {
        let err = api
            .quote(QuoteInput {
                direction: route.to_string(),
                gross_amount: AtomicU64(500_000),
            })
            .await
            .expect_err("a disabled route must never be quoted");
        assert!(
            matches!(err, ApiError::RouteDisabled),
            "{route} must not receive a quote, got {err:?}"
        );
    }
}

#[tokio::test]
async fn a_robinhood_route_is_a_recognised_name_refused_with_409_not_400() {
    // The distinction matters to the UI: 400 means "you sent nonsense",
    // 409 means "this route exists but is not open". Conflating them would
    // make a disabled route indistinguishable from a client bug.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let known = api
        .quote(QuoteInput {
            direction: "GlcToRhn".to_string(),
            gross_amount: AtomicU64(500_000),
        })
        .await
        .unwrap_err();
    assert_eq!(known.status(), StatusCode::CONFLICT);

    let nonsense = api
        .quote(QuoteInput {
            direction: "NotARoute".to_string(),
            gross_amount: AtomicU64(500_000),
        })
        .await
        .unwrap_err();
    assert_eq!(nonsense.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_rejected_direction_spellings_do_not_parse() {
    // Guards the naming decision: `L1ToRobinhood`/`RobinhoodToL1` were
    // considered and rejected in favour of `GlcToRhn`/`RhnToGlc`. If either
    // ever starts parsing, two spellings for one route exist and one of
    // them will eventually skip a gate.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    for spelling in ["L1ToRobinhood", "RobinhoodToL1"] {
        let err = api
            .quote(QuoteInput {
                direction: spelling.to_string(),
                gross_amount: AtomicU64(500_000),
            })
            .await
            .unwrap_err();
        assert_eq!(
            err.status(),
            StatusCode::BAD_REQUEST,
            "{spelling} must not be a recognised route name"
        );
    }
}

#[tokio::test]
async fn legacy_routes_are_unaffected_by_the_gate() {
    // The Solana regression guard at the API layer: naming `GlcToSol`
    // explicitly must behave exactly like omitting `route` entirely, which
    // is what every existing client does.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let implicit = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .expect("omitting route must keep working");
    let explicit = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: Some("GlcToSol".to_string()),
            source_address: None,
        })
        .await
        .expect("naming the legacy route explicitly must also work");
    assert_ne!(implicit.request_id, explicit.request_id);

    // And quoting the legacy directions is unchanged.
    for direction in ["GlcToSol", "SolToGlc"] {
        api.quote(QuoteInput {
            direction: direction.to_string(),
            gross_amount: AtomicU64(500_000),
        })
        .await
        .unwrap_or_else(|e| panic!("{direction} must still quote, got {e:?}"));
    }
}

#[tokio::test]
async fn sol_to_glc_is_rejected_by_this_endpoint_as_a_client_error_not_a_disabled_route() {
    // `SolToGlc` passes the gate (it is a live production route) but is
    // created by the depositor's own on-chain transaction, never here — so
    // it must read as a 400, distinct from Robinhood's 409.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let err = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: Some("SolToGlc".to_string()),
            source_address: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn chains_endpoint_reports_robinhood_visible_but_closed() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let view = api.chains().await.unwrap();
    assert_eq!(view.chains.len(), 3, "all three chains must be listed");
    assert!(view.chains.iter().any(|c| c.id == "robinhood"));
    assert_eq!(view.routes.len(), 6, "all six routes must be listed");

    for route in view.routes {
        match route.id.as_str() {
            "GlcToSol" | "SolToGlc" => {
                assert!(route.enabled, "{} must stay enabled", route.id);
                assert!(route.implemented);
                assert!(route.disabled_reason.is_none());
            }
            // The two Goldcoin<->Robinhood routes: settlement machinery
            // EXISTS (Phase F), so they report as implemented — and they
            // are still closed, because this fixture has no verified
            // Robinhood deployment. "Implemented" and "enabled" are
            // different facts and the listing must not conflate them.
            "GlcToRhn" | "RhnToGlc" => {
                assert!(!route.enabled, "{} must be disabled", route.id);
                assert!(
                    route.implemented,
                    "{} has settlement machinery as of Phase F",
                    route.id
                );
                assert_eq!(
                    route.disabled_reason.as_deref(),
                    Some(crate::routes::RouteGateError::UNAVAILABLE_MESSAGE)
                );
            }
            // The two Solana<->Robinhood routes: implemented as of
            // Phase H, visible in the listing so they can be audited,
            // and closed on every gate by default.
            "SolToRhn" | "RhnToSol" => {
                assert!(!route.enabled, "{} must be disabled", route.id);
                assert!(
                    route.implemented,
                    "{} has settlement machinery as of Phase H",
                    route.id
                );
                assert_eq!(
                    route.disabled_reason.as_deref(),
                    Some(crate::routes::RouteGateError::UNAVAILABLE_MESSAGE)
                );
            }
            other => panic!("unexpected route {other}"),
        }
    }
}

#[tokio::test]
async fn a_direct_http_request_cannot_bypass_the_disabled_route() {
    // The explicit "UI disabling is not sufficient" test: a caller who
    // never loads the UI at all, posting straight at the API with a
    // hand-written body, must still be refused — and must still leave the
    // ledger untouched.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let before = ledger_footprint(&db_path);
    let (base, _tx) = spawn_real_server(&db_path, 0).await;
    let client = reqwest::Client::new();

    for route in ["GlcToRhn", "RhnToGlc"] {
        // Raw JSON, not the typed struct — exactly what curl would send.
        let resp = client
            .post(format!("{base}/transfers"))
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"amount_atomic":500000,"recipient":"{}","route":"{route}"}}"#,
                Keypair::new().pubkey()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::CONFLICT,
            "{route} must be refused over raw HTTP"
        );

        let resp = client
            .post(format!("{base}/quote"))
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"direction":"{route}","gross_amount":500000}}"#
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
    }

    assert_eq!(
        ledger_footprint(&db_path),
        before,
        "raw HTTP attempts must not have moved any reserve accounting"
    );
}

#[tokio::test]
async fn get_chains_is_served_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let (base, _tx) = spawn_real_server(&db_path, 0).await;
    let resp = reqwest::get(format!("{base}/chains")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: ChainsView = resp.json().await.unwrap();
    // Implemented as of Phase F, and still closed — the two facts the
    // listing must keep apart.
    assert!(body
        .routes
        .iter()
        .any(|r| r.id == "GlcToRhn" && !r.enabled && r.implemented));
    // The Solana<->Robinhood routes: implemented as of Phase H, and
    // closed by default exactly like the Goldcoin pair.
    assert!(body
        .routes
        .iter()
        .any(|r| r.id == "RhnToSol" && !r.enabled && r.implemented));
    assert!(body
        .routes
        .iter()
        .any(|r| r.id == "SolToRhn" && !r.enabled && r.implemented));
}

// ------------------------------------ blocker I: the route-aware deposit --
//
// `POST /transfers` now creates either Goldcoin-sourced route. These
// tests pin both halves: `GlcToSol` is byte-for-byte what it was, and
// `GlcToRhn` is created AS `GlcToRhn` from its first and only INSERT.

/// The Robinhood reserve, alongside the two [`configure`] seeds — a
/// `GlcToRhn` request reserves capacity there, in canonical units.
fn configure_with_robinhood_reserve(dir: &std::path::Path) -> std::path::PathBuf {
    let db_path = configure(dir);
    let mut ledger = Ledger::open(&db_path).unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::RobinhoodReserve,
            10_000_000,
            0,
            5_000_000,
            2_000_000,
            1_000_000,
            0,
        )
        .unwrap();
    // The LEDGER gate only, through the supported operator path
    // (`glc-admin robinhood-route-enable` calls the same function) rather
    // than by hand-writing rows. Config and adapter are separate gates,
    // supplied by [`build_with_open_glc_to_rhn`], and production has all
    // three shut.
    for route in [
        crate::routes::Route::GlcToRhn,
        crate::routes::Route::RhnToGlc,
    ] {
        ledger.set_route_enabled(route, true, None).unwrap();
    }
    db_path
}

/// A verified deployment fixture, so the Robinhood ADAPTER leg is
/// operational. Mirrors `chains::tests::verified_deployment`.
fn test_verified_deployment() -> crate::robinhood::preflight::VerifiedDeployment {
    use crate::evm::{EvmAddress, EvmChainId, TxEnvelope};
    use crate::robinhood::auth::ProtocolChainPair;
    crate::robinhood::preflight::VerifiedDeployment {
        chain_id: EvmChainId::new(4663).unwrap(),
        bridge_contract: EvmAddress::from_bytes([0xb1; 20]),
        token: EvmAddress::from_bytes([0x70; 20]),
        token_decimals: 18,
        signers: [
            EvmAddress::from_bytes([0xa1; 20]),
            EvmAddress::from_bytes([0xa2; 20]),
            EvmAddress::from_bytes([0xa3; 20]),
        ],
        domain_separator: [0x5a; 32],
        glc_to_rhn_chains: ProtocolChainPair {
            source: 1001,
            dest: 2001,
        },
        rhn_to_glc_chains: ProtocolChainPair {
            source: 2001,
            dest: 1001,
        },
        sol_to_rhn_chains: ProtocolChainPair {
            source: 3001,
            dest: 2001,
        },
        rhn_to_sol_chains: ProtocolChainPair {
            source: 2001,
            dest: 3001,
        },
        tx_envelope: TxEnvelope::Eip1559,
        chain_has_base_fee: true,
    }
}

/// An API whose every gate admits `GlcToRhn`. TEST-ONLY: the shipping
/// configuration leaves all three shut, which
/// `post_transfers_refuses_both_robinhood_routes_and_writes_nothing`
/// above pins against the production fixture.
fn build_with_open_glc_to_rhn(db_path: &std::path::Path) -> BridgeApi<FakeSolanaRpc> {
    opt_down(BridgeApi::new(
        db_path.to_path_buf(),
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes(0, 100, TEST_PER_TRANSFER_LIMIT),
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        test_root_vault(),
        crate::goldcoin::address::Network::Testnet,
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::routes::RouteGate::new(
            crate::routes::RoutesConfig::default().with_robinhood(true, true, false, false),
            crate::chains::ChainRegistry::with_verified_robinhood(test_verified_deployment()),
        )),
        test_route_fees(),
    ))
}

/// A `0x`-prefixed 20-byte EVM address, all-lowercase so it claims no
/// EIP-55 checksum.
const TEST_EVM_RECIPIENT: &str = "0xe1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1";

/// `GlcToSol` is unchanged: the same request, the same amounts, the same
/// derived deposit address, whether the route is named explicitly or left
/// to the default. This is the compatibility assertion the whole widening
/// is measured against.
#[tokio::test]
async fn glc_to_sol_creation_is_identical_with_and_without_an_explicit_route() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    // One recipient per request: a second request to one pubkey inside
    // 24h is refused by the destination window, which is not what this
    // test is measuring.
    let recipient = Keypair::new().pubkey();
    let other_recipient = Keypair::new().pubkey();

    let implicit = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: recipient.to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();
    let explicit = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: other_recipient.to_string(),
            route: Some("GlcToSol".to_string()),
            source_address: None,
        })
        .await
        .unwrap();

    let ledger = Ledger::open(&db_path).unwrap();
    let a = ledger.get_request(implicit.request_id).unwrap().unwrap();
    let b = ledger.get_request(explicit.request_id).unwrap().unwrap();
    for (request, expected_recipient) in [(&a, recipient), (&b, other_recipient)] {
        assert_eq!(request.direction, Direction::GlcToSol);
        assert_eq!(request.recipient, expected_recipient.to_bytes());
        assert_eq!(request.gross_amount_atomic, 500_000);
    }
    assert_eq!(a.fee_bps, b.fee_bps);
    assert_eq!(a.fee_amount_atomic, b.fee_amount_atomic);
    assert_eq!(a.net_amount_atomic, b.net_amount_atomic);
    // Different requests get different derived addresses; that they are
    // both derived at all is the invariant.
    assert_ne!(implicit.deposit_address, explicit.deposit_address);
    assert!(!implicit.deposit_address.is_empty());
}

/// The core of blocker I: a `GlcToRhn` transfer is created, and it is
/// `GlcToRhn` in the row from the beginning. Nothing creates a `GlcToSol`
/// request and adjusts it afterwards.
#[tokio::test]
async fn a_glc_to_rhn_transfer_is_created_as_glc_to_rhn_from_the_first_insert() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);

    let created = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: TEST_EVM_RECIPIENT.to_string(),
            route: Some("GlcToRhn".to_string()),
            source_address: None,
        })
        .await
        .unwrap();

    let ledger = Ledger::open(&db_path).unwrap();
    let request = ledger.get_request(created.request_id).unwrap().unwrap();
    assert_eq!(request.direction, Direction::GlcToRhn);
    assert_eq!(request.state, RequestState::AwaitingDeposit);
    assert_eq!(
        request.recipient, [0xE1u8; 20],
        "the intended Robinhood recipient is stored as its 20 address bytes"
    );

    // The route is bound to the deposit script too, so the address alone
    // resolves back to this request AND this route.
    assert!(!created.deposit_address.is_empty());
    let derived = crate::goldcoin::derivation::derive_request_vault(
        &test_root_vault(),
        created.request_id,
        crate::goldcoin::address::Network::Testnet,
    )
    .unwrap();
    assert_eq!(created.deposit_address, derived.address());
    assert_eq!(
        ledger
            .find_goldcoin_deposit_request_by_script(&derived.script_pubkey_hex())
            .unwrap(),
        Some((created.request_id, Direction::GlcToRhn))
    );

    // The transition log records only the creation transitions — there is
    // no route change to find, because a route is never changed.
    let states: Vec<&str> = ledger
        .state_log(created.request_id)
        .unwrap()
        .into_iter()
        .map(|(_from, to, _at, _reason)| to.as_str())
        .collect();
    assert_eq!(states, vec!["LiquidityReserved", "AwaitingDeposit"]);
}

/// A `GlcToRhn` request reserves capacity on the ROBINHOOD reserve, in
/// canonical units, and leaves the Solana one untouched.
#[tokio::test]
async fn a_glc_to_rhn_transfer_reserves_the_robinhood_reserve_in_canonical_units() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);

    let before_solana = Ledger::open(&db_path)
        .unwrap()
        .available_capacity(ReserveDirection::SolanaReserve)
        .unwrap();
    let before_robinhood = Ledger::open(&db_path)
        .unwrap()
        .available_capacity(ReserveDirection::RobinhoodReserve)
        .unwrap();

    let created = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: TEST_EVM_RECIPIENT.to_string(),
            route: Some("GlcToRhn".to_string()),
            source_address: None,
        })
        .await
        .unwrap();

    let ledger = Ledger::open(&db_path).unwrap();
    let request = ledger.get_request(created.request_id).unwrap().unwrap();
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::RobinhoodReserve)
            .unwrap(),
        before_robinhood - request.net_amount_atomic as i64,
        "the reservation is the canonical NET, held against the Robinhood reserve"
    );
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        before_solana,
        "the Solana reserve is not a party to this route"
    );
}

/// Route selection fails closed. An unknown name is a client error; a
/// route created on its own source chain is a client error; and neither
/// ever falls back to `GlcToSol`.
#[tokio::test]
async fn an_unusable_route_is_refused_rather_than_defaulted_to_glc_to_sol() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);
    let before = ledger_footprint(&db_path);

    // Unknown name: 400, never a default.
    let err = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: Some("GlcToDoge".to_string()),
            source_address: None,
        })
        .await
        .expect_err("an unknown route must be refused");
    assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");
    assert_eq!(err.status(), StatusCode::BAD_REQUEST);

    // Contract-sourced routes are created by the depositor's own on-chain
    // transaction, not here.
    for route in ["SolToGlc", "RhnToGlc"] {
        let err = api
            .create_goldcoin_deposit_transfer(CreateTransferInput {
                amount_atomic: AtomicU64(500_000),
                recipient: Keypair::new().pubkey().to_string(),
                route: Some(route.to_string()),
                source_address: None,
            })
            .await
            .expect_err("{route} must not be creatable here");
        match err {
            ApiError::BadRequest(detail) => {
                assert!(
                    detail.contains("not created through this endpoint"),
                    "{detail}"
                )
            }
            other => panic!("{route}: {other:?}"),
        }
    }

    assert_eq!(
        ledger_footprint(&db_path),
        before,
        "no refused route may leave a row or hold liquidity"
    );
}

/// The two Solana<->Robinhood routes cannot enter this pipeline at all:
/// neither has a Goldcoin source, so there is no deposit address to hand
/// out. Refused by direction, before anything is written, whether or not
/// the route is switched on.
#[tokio::test]
async fn sol_to_rhn_and_rhn_to_sol_cannot_enter_the_goldcoin_deposit_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);
    let before = ledger_footprint(&db_path);

    for route in [
        crate::routes::Route::SolToRhn,
        crate::routes::Route::RhnToSol,
    ] {
        // The structural fact, independent of any gate or config.
        assert!(
            route
                .as_direction()
                .is_some_and(|d| !d.source_is_goldcoin()),
            "{route:?} is not Goldcoin-sourced"
        );
        assert_ne!(
            route.source_chain(),
            crate::routes::Chain::Goldcoin,
            "{route:?}'s source is not Goldcoin, so it has no deposit to intake"
        );

        let err = api
            .create_goldcoin_deposit_transfer(CreateTransferInput {
                amount_atomic: AtomicU64(500_000),
                recipient: Keypair::new().pubkey().to_string(),
                route: Some(route.as_str().to_string()),
                source_address: None,
            })
            .await
            .expect_err("a non-Goldcoin-sourced route can never be created here");
        assert!(
            matches!(err, ApiError::RouteDisabled | ApiError::BadRequest(_)),
            "{route:?}: {err:?}"
        );
    }

    assert_eq!(ledger_footprint(&db_path), before);
}

/// The recipient is parsed as the DESTINATION chain's address type, and
/// the two are not interchangeable in either direction.
#[tokio::test]
async fn a_recipient_of_the_wrong_chains_address_type_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);
    let before = ledger_footprint(&db_path);

    // A Solana pubkey offered to GlcToRhn.
    let err = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: Some("GlcToRhn".to_string()),
            source_address: None,
        })
        .await
        .expect_err("a Solana pubkey is not an EVM address");
    assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");

    // An EVM address offered to GlcToSol.
    let err = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: TEST_EVM_RECIPIENT.to_string(),
            route: Some("GlcToSol".to_string()),
            source_address: None,
        })
        .await
        .expect_err("an EVM address is not a Solana pubkey");
    assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");

    assert_eq!(ledger_footprint(&db_path), before);
}

/// The EVM zero address is a valid address and the burn sink. Accepting
/// it would reserve real capacity against a payout that destroys the
/// value, so it is refused at intake.
#[tokio::test]
async fn the_evm_zero_address_is_refused_as_a_glc_to_rhn_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);
    let before = ledger_footprint(&db_path);

    let err = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: format!("0x{}", "0".repeat(40)),
            route: Some("GlcToRhn".to_string()),
            source_address: None,
        })
        .await
        .expect_err("the zero address must be refused");
    match err {
        ApiError::BadRequest(detail) => assert!(detail.contains("burn sink"), "{detail}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(ledger_footprint(&db_path), before);
}

/// With the route SHUT — the shipping configuration — a `GlcToRhn`
/// transfer cannot be created at all, so no request exists to be paid
/// out. The route gate refuses before anything is written.
#[tokio::test]
async fn a_shut_glc_to_rhn_route_creates_nothing_to_pay_out() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    // The production API: config and adapter gates shut, even though the
    // ledger gate above was seeded open.
    let api = build(&db_path, 0);
    let before = ledger_footprint(&db_path);

    let err = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: TEST_EVM_RECIPIENT.to_string(),
            route: Some("GlcToRhn".to_string()),
            source_address: None,
        })
        .await
        .expect_err("the shipping configuration must refuse GlcToRhn");
    assert!(matches!(err, ApiError::RouteDisabled), "{err:?}");
    assert_eq!(err.status(), StatusCode::CONFLICT);
    assert_eq!(ledger_footprint(&db_path), before);
    assert!(
        !crate::routes::Route::GlcToRhn.default_enabled(),
        "GlcToRhn must still be disabled by default"
    );
    assert!(
        !crate::routes::Route::RhnToGlc.default_enabled(),
        "RhnToGlc must still be disabled by default"
    );
}

// =====================================================================
// Phase H: the address filter across two chains, and the two public
// Robinhood read endpoints.
// =====================================================================

/// A `BridgeApi` with `GlcToRhn`/`RhnToGlc` open on every local gate AND
/// the Robinhood read sources attached, so the public Robinhood endpoints
/// have something authoritative to report. TEST-ONLY: the shipping
/// configuration leaves all three route gates shut, and nothing here
/// changes that — `with_robinhood` attaches READERS, not permission.
fn build_with_robinhood_reads(
    db_path: &std::path::Path,
    contract: Option<Arc<dyn crate::robinhood::public::RobinhoodContractSource>>,
) -> BridgeApi<FakeSolanaRpc> {
    build_with_open_glc_to_rhn(db_path)
        .with_robinhood(crate::robinhood::RobinhoodHealth::unconfigured(), contract)
}

/// A live reader pointed at the in-process mock contract — the real
/// `eth_call` path, decoders included, with no node.
fn mock_contract_source() -> Arc<dyn crate::robinhood::public::RobinhoodContractSource> {
    Arc::new(crate::robinhood::public::LiveRobinhoodContractSource::new(
        crate::robinhood::testkit::MockNode::new(crate::robinhood::testkit::BRIDGE),
        crate::robinhood::testkit::BRIDGE,
    ))
}

/// Folds one FINAL Robinhood deposit observation into an `RhnToGlc`
/// request, returning its id. `depositor` is the EVM wallet the custody
/// contract recorded — the value `?address=0x...` must find.
fn fold_rhn_deposit(
    db_path: &std::path::Path,
    obligation_index: u64,
    depositor: [u8; 20],
    canonical: u64,
    route_open: bool,
) -> i64 {
    use crate::ledger::{RobinhoodDepositObservation, RobinhoodFinality, RobinhoodObservationRow};

    let destination = crate::goldcoin::address::encode_p2pkh(
        &[0x42; 20],
        crate::goldcoin::address::Network::Testnet,
    );
    let robinhood_atomic = u128::from(canonical) * 10_000_000_000;
    let row = RobinhoodObservationRow {
        id: obligation_index as i64 + 1,
        observation: RobinhoodDepositObservation {
            source_contract: crate::robinhood::testkit::BRIDGE.to_bytes(),
            obligation_index,
            route: crate::routes::Route::RhnToGlc,
            depositor,
            destination: destination.as_bytes().to_vec(),
            amount_robinhood_atomic: crate::evm::EvmU256::from_u128(robinhood_atomic).to_be_bytes(),
            amount_canonical_atomic: canonical,
            tx_hash: {
                let mut h = [0xaa; 32];
                h[0] = obligation_index as u8;
                h
            },
            log_index: 0,
            block_number: 500,
            block_hash: [0xbb; 32],
        },
        finality: RobinhoodFinality::Final,
        observed_at: 100,
        finalized_at: Some(200),
        reorged_at: None,
    };
    let mut ledger = Ledger::open(db_path).unwrap();
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
    let outcome = crate::robinhood::fold::fold_observation(
        &mut ledger,
        &row,
        crate::goldcoin::address::Network::Testnet,
        crate::amount_conversion::BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        route_open,
        1_000,
    )
    .expect("the observation folds");
    let request_id = outcome.request_id();
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE robinhood_deposit_observations SET folded_request_id = ?1 WHERE id = ?2",
            rusqlite::params![request_id, row.id],
        )
        .unwrap();
    request_id
}

// --------------------------------------------- the `?address=` filter --

/// The defect this closes: a 20-byte EVM address used to fail
/// `Pubkey::from_str` and return 400, so a Robinhood user could not see
/// their own activity at all.
#[tokio::test]
async fn the_activity_filter_accepts_an_evm_address_on_both_robinhood_routes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);

    // Outbound: the caller's own EVM address is the request's `recipient`.
    api.create_goldcoin_deposit_transfer(CreateTransferInput {
        amount_atomic: AtomicU64(500_000),
        recipient: TEST_EVM_RECIPIENT.to_string(),
        route: Some("GlcToRhn".to_string()),
        source_address: None,
    })
    .await
    .unwrap();
    // Inbound: the caller's own EVM address is the observation's
    // `depositor`, which is not a `bridge_requests` column at all.
    let depositor = TEST_EVM_RECIPIENT
        .parse::<crate::evm::address::EvmAddress>()
        .unwrap()
        .to_bytes();
    let inbound_id = fold_rhn_deposit(&db_path, 0, depositor, 400_000, true);

    let page = api
        .list_transfers(Some(TransferAddressFilter::Evm(depositor)), None, None, 50)
        .await
        .unwrap();

    let mut directions: Vec<&str> = page.items.iter().map(|t| t.direction.as_str()).collect();
    directions.sort_unstable();
    assert_eq!(directions, vec!["GlcToRhn", "RhnToGlc"]);
    assert!(page.items.iter().any(|t| t.id == inbound_id));
}

/// The existing Solana behaviour, restated against the widened filter: a
/// pubkey still matches `GlcToSol.recipient` and `SolToGlc.requester`,
/// and still matches nothing else.
#[tokio::test]
async fn the_activity_filter_is_unchanged_for_a_solana_address() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let mine = Keypair::new().pubkey();
    let theirs = Keypair::new().pubkey();

    for recipient in [mine, theirs] {
        api.create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: recipient.to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();
    }

    let page = api
        .list_transfers(
            Some(TransferAddressFilter::Solana(mine.to_bytes())),
            None,
            None,
            50,
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].direction, "GlcToSol");
}

/// Every way a `0x` string can be wrong is a 400 — never a silent
/// fallthrough to the base58 parser, and never a zero-padded blob.
#[test]
fn a_malformed_evm_address_is_refused_rather_than_coerced() {
    let short = "address=0xe1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1";
    let long = "address=0xe1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1";
    let non_hex = "address=0xzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
    let no_prefix_but_hexish = "address=e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1";
    // Mixed case claims an EIP-55 checksum; this one does not verify.
    let bad_checksum = "address=0xE1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1E1";
    for query in [short, long, non_hex, no_prefix_but_hexish, bad_checksum] {
        let err =
            parse_list_transfers_query(Some(query)).expect_err(&format!("{query} must be refused"));
        assert!(
            matches!(err, ApiError::BadRequest(_)),
            "{query} produced {err:?}"
        );
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    // The valid forms still parse, and to the right chain.
    assert!(matches!(
        parse_list_transfers_query(Some(&format!("address={TEST_EVM_RECIPIENT}")))
            .unwrap()
            .0,
        Some(TransferAddressFilter::Evm(_))
    ));
    assert!(matches!(
        parse_list_transfers_query(Some(&format!("address={}", Keypair::new().pubkey())))
            .unwrap()
            .0,
        Some(TransferAddressFilter::Solana(_))
    ));
}

/// The cross-chain assertion: neither filter can ever reach the other
/// chain's rows, whatever the byte values happen to be.
#[tokio::test]
async fn evm_and_solana_activity_filters_never_cross_match() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);
    let solana_recipient = Keypair::new().pubkey();

    api.create_goldcoin_deposit_transfer(CreateTransferInput {
        amount_atomic: AtomicU64(500_000),
        recipient: solana_recipient.to_string(),
        route: None,
        source_address: None,
    })
    .await
    .unwrap();
    api.create_goldcoin_deposit_transfer(CreateTransferInput {
        amount_atomic: AtomicU64(500_000),
        recipient: TEST_EVM_RECIPIENT.to_string(),
        route: Some("GlcToRhn".to_string()),
        source_address: None,
    })
    .await
    .unwrap();
    let evm = TEST_EVM_RECIPIENT
        .parse::<crate::evm::address::EvmAddress>()
        .unwrap()
        .to_bytes();
    fold_rhn_deposit(&db_path, 0, evm, 400_000, true);

    // An EVM filter sees only the two Robinhood-addressed directions.
    let evm_page = api
        .list_transfers(Some(TransferAddressFilter::Evm(evm)), None, None, 50)
        .await
        .unwrap();
    assert!(
        evm_page
            .items
            .iter()
            .all(|t| t.direction == "GlcToRhn" || t.direction == "RhnToGlc"),
        "{:?}",
        evm_page.items
    );

    // A Solana filter sees only the two Solana-addressed ones.
    let solana_page = api
        .list_transfers(
            Some(TransferAddressFilter::Solana(solana_recipient.to_bytes())),
            None,
            None,
            50,
        )
        .await
        .unwrap();
    assert_eq!(solana_page.items.len(), 1);
    assert_eq!(solana_page.items[0].direction, "GlcToSol");

    // The sharpest form: a Solana pubkey whose FIRST 20 BYTES are exactly
    // the EVM address. If the filter compared a prefix, or compared
    // untagged bytes, this would match the Robinhood rows.
    let mut spoof = [0u8; 32];
    spoof[..20].copy_from_slice(&evm);
    let spoof_page = api
        .list_transfers(Some(TransferAddressFilter::Solana(spoof)), None, None, 50)
        .await
        .unwrap();
    assert!(spoof_page.items.is_empty(), "{:?}", spoof_page.items);

    // And the reverse: no EVM filter can reach a Solana-addressed row.
    let mut evm_from_solana = [0u8; 20];
    evm_from_solana.copy_from_slice(&solana_recipient.to_bytes()[..20]);
    let reverse = api
        .list_transfers(
            Some(TransferAddressFilter::Evm(evm_from_solana)),
            None,
            None,
            50,
        )
        .await
        .unwrap();
    assert!(
        reverse
            .items
            .iter()
            .all(|t| t.direction != "GlcToSol" && t.direction != "SolToGlc"),
        "{:?}",
        reverse.items
    );
}

/// A reorged sighting is not evidence that this depositor funded this
/// request, so it must not put the request in their activity list.
#[tokio::test]
async fn a_reorged_observation_does_not_attribute_a_request_to_its_depositor() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);
    let depositor = [0x33; 20];
    let request_id = fold_rhn_deposit(&db_path, 0, depositor, 400_000, true);

    assert_eq!(
        api.list_transfers(Some(TransferAddressFilter::Evm(depositor)), None, None, 50)
            .await
            .unwrap()
            .items
            .len(),
        1
    );

    Ledger::open(&db_path)
        .unwrap()
        .conn_for_tests()
        .execute(
            "UPDATE robinhood_deposit_observations
                SET finality = 'Reorged', reorged_at = 900, finalized_at = NULL
              WHERE folded_request_id = ?1",
            [request_id],
        )
        .unwrap();

    assert!(api
        .list_transfers(Some(TransferAddressFilter::Evm(depositor)), None, None, 50)
        .await
        .unwrap()
        .items
        .is_empty());
}

// ------------------------------------ the public Robinhood endpoints --

/// Configured: the ledger figures are the real `reserve_ledger` row's,
/// and the contract figures are the real `eth_call` results.
#[tokio::test]
async fn a_configured_robinhood_reserve_reports_its_real_ledger_and_contract_figures() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_robinhood_reads(&db_path, Some(mock_contract_source()));

    let view = api.robinhood_reserve().await.unwrap();

    assert_eq!(
        view.ledger_availability,
        crate::robinhood::public::AVAILABILITY_AVAILABLE
    );
    // Exactly what `configure_with_robinhood_reserve` seeded: balance
    // 10_000_000, protected minimum 0, nothing reserved yet.
    assert_eq!(view.balance_atomic.unwrap(), AtomicU64(10_000_000));
    assert_eq!(view.protected_minimum_atomic.unwrap(), AtomicU64(0));
    assert_eq!(view.reserved_liquidity_atomic.unwrap(), AtomicU64(0));
    assert_eq!(view.pending_obligations_atomic.unwrap(), AtomicU64(0));
    assert_eq!(view.accrued_fees_atomic.unwrap(), AtomicU64(0));
    assert_eq!(view.paused, Some(false));
    // available = balance - protected_minimum - reserved
    assert_eq!(
        view.available_capacity_atomic.unwrap(),
        AtomicI64(10_000_000)
    );

    // NEVER netted against, or substituted from, the other two reserves:
    // three separate figures, each read from its own `reserve_ledger`
    // row, and `GET /reserve` still reports exactly the two it always
    // did.
    let legacy = api.reserve().await.unwrap();
    assert_eq!(legacy.goldcoin_available_capacity, AtomicI64(10_000_000));
    assert_eq!(legacy.solana_available_capacity, AtomicI64(10_000_000));
    let legacy_json = serde_json::to_value(&legacy).unwrap();
    assert_eq!(
        legacy_json.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["goldcoin_available_capacity", "solana_available_capacity"],
        "GET /reserve must not grow a Robinhood field"
    );

    // The contract half: the mock's own figures, in 18-decimal units.
    assert_eq!(
        view.onchain.availability,
        crate::robinhood::public::AVAILABILITY_AVAILABLE
    );
    assert_eq!(
        view.onchain.protected_min_reserve_atomic.as_deref(),
        Some("1000000000000000000000")
    );
    assert_eq!(view.onchain.deposits_paused, Some(false));
    assert_eq!(view.onchain.payouts_paused, Some(false));
    assert_eq!(view.onchain.window_seconds, Some(86_400));
    let inbound = view.onchain.inbound_window.as_ref().expect("a window");
    assert_eq!(inbound.limit_atomic, "100000000000000000000000");
    // The mock's bucket opened at 1_700_000_000 and is long expired
    // against wall-clock now, so the contract would reset it on its next
    // write — the honest reading is a full limit remaining, not the
    // stale 250 GLC total.
    assert!(!inbound.is_current);
    assert_eq!(inbound.used_atomic, "0");
    assert_eq!(inbound.remaining_atomic, inbound.limit_atomic);

    // Every route with a Robinhood leg is listed with the gate's own
    // verdict — the four the custody contract models.
    let ids: Vec<&str> = view.routes.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["GlcToRhn", "RhnToGlc", "SolToRhn", "RhnToSol"]);
}

/// Unconfigured — which is every production deployment today. The answer
/// is "not configured", and every figure is absent. A zero here would
/// claim an empty reserve exists.
#[tokio::test]
async fn an_absent_robinhood_reserve_reports_not_configured_never_zero() {
    let dir = tempfile::tempdir().unwrap();
    // `configure` seeds ONLY Goldcoin and Solana — no `[reserve.robinhood]`.
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let view = api.robinhood_reserve().await.unwrap();

    assert_eq!(
        view.ledger_availability,
        crate::robinhood::public::AVAILABILITY_NOT_CONFIGURED
    );
    assert!(view.balance_atomic.is_none());
    assert!(view.protected_minimum_atomic.is_none());
    assert!(view.reserved_liquidity_atomic.is_none());
    assert!(view.pending_obligations_atomic.is_none());
    assert!(view.available_capacity_atomic.is_none());
    assert!(view.accrued_fees_atomic.is_none());
    assert!(view.paused.is_none());
    assert_eq!(
        view.onchain.availability,
        crate::robinhood::public::AVAILABILITY_NOT_CONFIGURED
    );
    assert!(view.onchain.inbound_window.is_none());
    assert!(!view.indexer.configured);

    // The serialized form: JSON `null`, never `0` and never `"0"`.
    let json: serde_json::Value = serde_json::to_value(&view).unwrap();
    for field in [
        "balance_atomic",
        "protected_minimum_atomic",
        "available_capacity_atomic",
        "pending_obligations_atomic",
        "paused",
    ] {
        assert!(json[field].is_null(), "{field} is {}", json[field]);
    }

    // The legacy reserve endpoint is untouched by any of this.
    let legacy = api.reserve().await.unwrap();
    assert_eq!(legacy.goldcoin_available_capacity, AtomicI64(10_000_000));
    assert_eq!(legacy.solana_available_capacity, AtomicI64(10_000_000));
}

/// Limits with a reachable contract: the contract's own values, and
/// nothing borrowed from the Solana `BridgeConfig`.
#[tokio::test]
async fn robinhood_limits_come_from_the_contract_not_from_the_solana_config() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_robinhood_reads(&db_path, Some(mock_contract_source()));

    let view = api.robinhood_limits().await.unwrap();
    assert_eq!(
        view.availability,
        crate::robinhood::public::AVAILABILITY_AVAILABLE
    );
    assert_eq!(
        view.inbound_min_atomic.as_deref(),
        Some("1000000000000000000")
    );
    assert_eq!(
        view.inbound_max_atomic.as_deref(),
        Some("10000000000000000000000")
    );
    assert_eq!(
        view.inbound_rolling_limit_atomic.as_deref(),
        Some("100000000000000000000000")
    );
    assert_eq!(view.rolling_window_seconds, Some(86_400));
    // The ROBINHOOD routes' own rates — not the Solana one, which is what
    // this used to report. The contract stores no fee at all, so these
    // come entirely from this service's `[fees]` table, per route.
    assert_eq!(view.bridge_fee_bps, 600);
    assert_eq!(view.glc_to_rhn_fee_bps, 600);
    assert_eq!(view.rhn_to_glc_fee_bps, 600);
    assert_ne!(
        view.bridge_fee_bps,
        amount_conversion::BRIDGE_FEE_BPS,
        "the Solana rate must not be what a Robinhood surface reports"
    );

    // The Solana limits are a different program's, in a different unit,
    // and none of them appears here. `fake_bridge_config_bytes` sets
    // min 100 / per-transfer `TEST_PER_TRANSFER_LIMIT`.
    let solana = api.limits().await.unwrap();
    assert_eq!(solana.min_transfer_amount, AtomicU64(100));
    assert_eq!(
        solana.per_transfer_limit,
        AtomicU64(TEST_PER_TRANSFER_LIMIT)
    );
    assert_ne!(view.inbound_min_atomic.as_deref(), Some("100"));
    assert_ne!(
        view.inbound_max_atomic.as_deref(),
        Some(TEST_PER_TRANSFER_LIMIT.to_string().as_str())
    );
}

/// Each Robinhood route's rolling window is published on `GET
/// /robinhood/limits`, and each is the accumulator the CONTRACT charges
/// for that route — not the other direction's, and not a figure this
/// service reconstructed from its own ledger.
///
/// The mapping under test is the one `GlcRobinhoodBridge._routeLegs`
/// fixes: `ROUTE_RHN_TO_GLC = 0x02` is inbound, so `deposit()` charges
/// `_inboundWindow` against `inboundRollingLimit`; `ROUTE_GLC_TO_RHN =
/// 0x01` is outbound, so `executePayout` charges `_outboundWindow`
/// against `outboundRollingLimit`. The two directions are given
/// DIFFERENT limits and DIFFERENT consumption here precisely so a
/// crossed wiring cannot pass.
#[tokio::test]
async fn robinhood_limits_publish_each_routes_own_rolling_window() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());

    // A bucket that is CURRENT: opened one minute ago, so the contract
    // would NOT reset it on its next write and the recorded totals are
    // really still charged. `MockContract::healthy`'s default bucket
    // opened in 2023 and is long expired, which would make every
    // direction report a full limit and hide a crossed mapping.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let opened_at = now - 60;
    let glc = |whole: u64| crate::evm::EvmU256::from_u128(u128::from(whole) * 10u128.pow(18));

    let node = crate::robinhood::testkit::MockNode::new(crate::robinhood::testkit::BRIDGE);
    node.with(|s| {
        s.contract.limits.inbound_rolling_limit = glc(100_000);
        s.contract.limits.outbound_rolling_limit = glc(70_000);
        s.contract.inbound_window = crate::robinhood::calls::RollingWindow {
            window_start: opened_at,
            total: glc(250),
        };
        s.contract.outbound_window = crate::robinhood::calls::RollingWindow {
            window_start: opened_at,
            total: glc(400),
        };
    });
    let source: Arc<dyn crate::robinhood::public::RobinhoodContractSource> =
        Arc::new(crate::robinhood::public::LiveRobinhoodContractSource::new(
            node,
            crate::robinhood::testkit::BRIDGE,
        ));
    let api = build_with_robinhood_reads(&db_path, Some(source));

    let view = api.robinhood_limits().await.unwrap();
    assert_eq!(
        view.availability,
        crate::robinhood::public::AVAILABILITY_AVAILABLE
    );

    // RhnToGlc <- inbound: limit 100_000, used 250, remaining 99_750.
    let rhn_to_glc = view
        .rhn_to_glc_rolling_window
        .as_ref()
        .expect("RhnToGlc's window");
    assert_eq!(
        rhn_to_glc.limit_atomic,
        view.inbound_rolling_limit_atomic.clone().unwrap(),
        "RhnToGlc is the INBOUND route, so its window is charged against \
         inboundRollingLimit"
    );
    assert!(rhn_to_glc.is_current);
    assert_eq!(rhn_to_glc.used_atomic, "250000000000000000000");
    assert_eq!(rhn_to_glc.remaining_atomic, "99750000000000000000000");
    assert_eq!(rhn_to_glc.resets_at, opened_at + 86_400);

    // GlcToRhn <- outbound: limit 70_000, used 400, remaining 69_600.
    let glc_to_rhn = view
        .glc_to_rhn_rolling_window
        .as_ref()
        .expect("GlcToRhn's window");
    assert_eq!(
        glc_to_rhn.limit_atomic,
        view.outbound_rolling_limit_atomic.clone().unwrap(),
        "GlcToRhn is the OUTBOUND route, so its window is charged against \
         outboundRollingLimit"
    );
    assert!(glc_to_rhn.is_current);
    assert_eq!(glc_to_rhn.used_atomic, "400000000000000000000");
    assert_eq!(glc_to_rhn.remaining_atomic, "69600000000000000000000");

    // A crossed mapping would have passed every assertion above if the
    // two directions happened to agree, so state the disagreement.
    assert_ne!(rhn_to_glc.limit_atomic, glc_to_rhn.limit_atomic);
    assert_ne!(rhn_to_glc.remaining_atomic, glc_to_rhn.remaining_atomic);

    // The configured limit itself is untouched by any of this: what is
    // published is the ceiling the contract holds, and the consumption
    // against it.
    assert_eq!(
        view.inbound_rolling_limit_atomic.as_deref(),
        Some("100000000000000000000000")
    );
    assert_eq!(
        view.outbound_rolling_limit_atomic.as_deref(),
        Some("70000000000000000000000")
    );

    // One implementation of "what does `_consumeWindow` leave": the
    // reserve endpoint's direction-named pair and this endpoint's
    // route-named pair are the same projection, so they cannot drift.
    let reserve = api.robinhood_reserve().await.unwrap();
    let inbound = reserve.onchain.inbound_window.as_ref().expect("a window");
    let outbound = reserve.onchain.outbound_window.as_ref().expect("a window");
    assert_eq!(rhn_to_glc.remaining_atomic, inbound.remaining_atomic);
    assert_eq!(rhn_to_glc.used_atomic, inbound.used_atomic);
    assert_eq!(glc_to_rhn.remaining_atomic, outbound.remaining_atomic);
    assert_eq!(glc_to_rhn.used_atomic, outbound.used_atomic);
}

/// An EXPIRED bucket reports the full limit remaining, on both routes —
/// the contract zeroes `total` on its next write past the boundary, so
/// that is the real headroom and not an optimistic reading. Pinned
/// separately from the current-bucket case because the two arms of
/// `RollingWindow::remaining` are exactly where a display could start
/// reporting capacity nobody has.
#[tokio::test]
async fn an_expired_robinhood_bucket_reports_the_whole_limit_on_both_routes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    // `MockContract::healthy`'s buckets opened at 1_700_000_000 and both
    // carry a non-zero total, so this is a real rollover, not an empty
    // window that would pass either way.
    let api = build_with_robinhood_reads(&db_path, Some(mock_contract_source()));

    let view = api.robinhood_limits().await.unwrap();
    for (route, window) in [
        ("RhnToGlc", view.rhn_to_glc_rolling_window.as_ref()),
        ("GlcToRhn", view.glc_to_rhn_rolling_window.as_ref()),
    ] {
        let window = window.unwrap_or_else(|| panic!("{route} has a window"));
        assert!(!window.is_current, "{route}'s bucket has rolled over");
        assert_eq!(window.used_atomic, "0", "{route} charges a stale total");
        assert_eq!(
            window.remaining_atomic, window.limit_atomic,
            "{route} must report the whole limit once its bucket expires"
        );
    }
}

/// Unknown limits are reported as unknown. Not zero, not the Solana
/// figures, not a stale service-side copy — there is no such copy.
#[tokio::test]
async fn unknown_robinhood_limits_are_null_never_zero() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());

    // Case 1: no contract configured at all.
    let unconfigured = build(&db_path, 0).robinhood_limits().await.unwrap();
    assert_eq!(
        unconfigured.availability,
        crate::robinhood::public::AVAILABILITY_NOT_CONFIGURED
    );

    // Case 2: a contract IS configured but cannot be read. A DIFFERENT
    // answer from case 1, because only this one is worth retrying.
    let node = crate::robinhood::testkit::MockNode::new(crate::robinhood::testkit::BRIDGE);
    node.with(|s| s.contract.bridge_code.clear());
    let dead: Arc<dyn crate::robinhood::public::RobinhoodContractSource> =
        Arc::new(crate::robinhood::public::LiveRobinhoodContractSource::new(
            node,
            crate::robinhood::testkit::BRIDGE,
        ));
    let unavailable = build(&db_path, 0)
        .with_robinhood(
            crate::robinhood::RobinhoodHealth::unconfigured(),
            Some(dead),
        )
        .robinhood_limits()
        .await
        .unwrap();
    assert_eq!(
        unavailable.availability,
        crate::robinhood::public::AVAILABILITY_UNAVAILABLE
    );

    for view in [&unconfigured, &unavailable] {
        assert!(view.inbound_min_atomic.is_none());
        assert!(view.inbound_max_atomic.is_none());
        assert!(view.inbound_rolling_limit_atomic.is_none());
        assert!(view.outbound_min_atomic.is_none());
        assert!(view.outbound_max_atomic.is_none());
        assert!(view.outbound_rolling_limit_atomic.is_none());
        assert!(view.protected_min_reserve_atomic.is_none());
        assert!(view.rolling_window_seconds.is_none());
        // Unread consumption is unknown, never "nothing consumed" — a
        // zero-used window would publish a FULL remaining figure for a
        // contract nobody could reach.
        assert!(view.rhn_to_glc_rolling_window.is_none());
        assert!(view.glc_to_rhn_rolling_window.is_none());
        // The fees ARE known without a chain read — the contract holds
        // none, so they are purely this service's configured rates — and
        // they are the ROBINHOOD routes', not whatever `GET /limits`
        // reports for Solana.
        assert_eq!(view.bridge_fee_bps, 600);
        assert_eq!(view.glc_to_rhn_fee_bps, 600);
        assert_eq!(view.rhn_to_glc_fee_bps, 600);

        let json = serde_json::to_value(view).unwrap();
        for field in [
            "inbound_min_atomic",
            "inbound_max_atomic",
            "inbound_rolling_limit_atomic",
            "protected_min_reserve_atomic",
            "rhn_to_glc_rolling_window",
            "glc_to_rhn_rolling_window",
        ] {
            assert!(json[field].is_null(), "{field} is {}", json[field]);
        }
    }
}

// ------------------------- the RhnToGlc refund / manual-review shape --

/// A Robinhood deposit that cannot complete parks in `ManualReview` with
/// no refund block — the state a UI renders before a human has decided.
#[tokio::test]
async fn an_rhn_to_glc_manual_review_serializes_as_manual_review_with_no_refund() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);
    // Folded with the route SHUT: the deposit is real and irreversible,
    // so it is recorded and parked rather than dropped.
    let request_id = fold_rhn_deposit(&db_path, 0, [0x33; 20], 400_000, false);

    let view = api.get_transfer(request_id).await.unwrap().expect("a row");
    assert_eq!(view.direction, "RhnToGlc");
    assert_eq!(view.state, "ManualReview");
    assert!(view.refund.is_none());
    // Contract-sourced, so there is no confirmation count to progress
    // through — the field is absent rather than a misleading zero.
    assert!(view.required_source_confirmations.is_none());

    let json = serde_json::to_value(&view).unwrap();
    assert_eq!(json["state"], "ManualReview");
    assert!(json["refund"].is_null());
}

/// Once a refund is authorized, the refund block is present and every
/// figure in it comes from the refund OPERATION ROW — the obligation's
/// own on-chain principal — not from the request's expected gross.
#[tokio::test]
async fn an_rhn_to_glc_refund_serializes_its_authoritative_principal() {
    use crate::ledger::{NewRobinhoodTx, RobinhoodTxKind};

    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);
    let canonical = 400_000u64;
    let request_id = fold_rhn_deposit(&db_path, 0, [0x33; 20], canonical, false);

    let principal = crate::evm::EvmU256::from_u128(u128::from(canonical) * 10_000_000_000);
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .begin_robinhood_tx(
                &NewRobinhoodTx {
                    kind: RobinhoodTxKind::Refund,
                    request_id: Some(request_id),
                    rebalance_request_id: None,
                    route: Some(crate::routes::Route::RhnToGlc),
                    bridge_contract: crate::robinhood::testkit::BRIDGE.to_bytes(),
                    chain_id: 4663,
                    contract_request_id: [0x77; 32],
                    obligation_index: Some(0),
                    recipient: Some([0x33; 20]),
                    amount_robinhood: Some(principal.to_be_bytes()),
                    signer_epoch: 7,
                    expiry: 9_999_999_999,
                    auth_digest: [0x5a; 32],
                },
                1_500,
            )
            .unwrap();
        ledger
            .mark_robinhood_refund_pending(request_id, 1_600)
            .unwrap();
    }

    let view = api.get_transfer(request_id).await.unwrap().expect("a row");
    assert_eq!(view.state, "RefundPending");
    let refund = view.refund.as_ref().expect("a refund block");

    // The OPERATION's own state, finer-grained than `RefundPending`.
    assert_eq!(refund.state, "Authorizing");
    // The contract's principal, narrowed exactly — not re-labelled gross.
    assert_eq!(refund.observed_amount_atomic, AtomicU64(canonical));
    assert_eq!(refund.refund_amount_atomic, AtomicU64(canonical));
    // A refunded request never settles, so no fee was charged.
    assert_eq!(refund.fee_charged_atomic, AtomicU64(0));
    // Nothing has been broadcast yet.
    assert!(refund.refund_txid.is_none());
    assert!(refund.broadcast_at.is_none());
    assert!(refund.refunded_at.is_none());

    // The wire form a UI reads: amounts as decimal strings, matching
    // every other atomic amount on this API.
    let json = serde_json::to_value(&view).unwrap();
    assert_eq!(json["refund"]["state"], "Authorizing");
    assert_eq!(
        json["refund"]["refund_amount_atomic"],
        canonical.to_string()
    );
    assert_eq!(json["refund"]["fee_charged_atomic"], "0");

    // The same projection through the listing, not just the id lookup.
    let listed = api
        .list_transfers(Some(TransferAddressFilter::Evm([0x33; 20])), None, None, 50)
        .await
        .unwrap();
    assert_eq!(
        listed.items[0]
            .refund
            .as_ref()
            .map(|r| r.refund_amount_atomic),
        Some(AtomicU64(canonical))
    );
}

/// The compatibility assertion for the two legacy directions: adding a
/// third refund arm changed neither of the existing two.
#[tokio::test]
async fn the_legacy_refund_projection_is_unchanged_for_a_non_refund_request() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let created = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();

    let view = api
        .get_transfer(created.request_id)
        .await
        .unwrap()
        .expect("a row");
    assert_eq!(view.direction, "GlcToSol");
    assert!(view.refund.is_none());
    assert_eq!(view.required_source_confirmations, Some(6));
}

/// The two new paths are routed, GET-only, and shaped as documented.
/// Everything a Robinhood-unaware client asks for is untouched.
#[tokio::test]
async fn the_robinhood_read_endpoints_are_routed_and_get_only() {
    let (base, _tx) = spawn_stub_server().await;

    for path in ["/robinhood/reserve", "/robinhood/limits"] {
        let resp = reqwest::get(format!("{base}{path}")).await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "{path}");
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json",
            "{path}"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body["availability"]
                .as_str()
                .or_else(|| body["ledger_availability"].as_str()),
            Some(crate::robinhood::public::AVAILABILITY_NOT_CONFIGURED),
            "{path}"
        );

        // No write surface: a POST is a 404, the same as any unknown path.
        let resp = reqwest::Client::new()
            .post(format!("{base}{path}"))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND, "{path}");
    }
}

/// A malformed `0x` address on the wire is a 400 with a JSON error body,
/// not a 500 and not an empty page that would read as "you have no
/// transfers".
#[tokio::test]
async fn a_malformed_evm_address_on_the_wire_is_a_400() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/transfers?address=0xdeadbeef"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("invalid address"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// Quote route isolation
//
// A quote for a route with no Solana leg must not depend on Solana being
// reachable. `GlcToRhn`/`RhnToGlc` resolve both of their decimals from
// compile-time constants and settle nothing on Solana, so a Solana RPC
// outage answering them with a 5xx reports the wrong chain as down — it
// reads to a user as "the Robinhood route is broken" when it is healthy.
//
// The reads themselves are unchanged for the two Solana-legged routes:
// `GlcToSol` still resolves the reserve mint's live decimals and still
// refuses a net entitlement that mint cannot represent exactly, and
// `SolToGlc` still reports that mint's live precision as its source.
// ---------------------------------------------------------------------------

/// A Solana RPC that is down. Every read fails, which is the point: a
/// route with no Solana leg must not notice.
struct UnreachableSolanaRpc;

impl SolanaRpc for UnreachableSolanaRpc {
    async fn get_account(&self, _pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        Err(SolanaRpcError::Transport(
            "solana rpc is unreachable".into(),
        ))
    }
    async fn get_multiple_accounts(
        &self,
        _pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        unimplemented!()
    }
    async fn send_transaction(&self, _tx: &SolanaTx) -> Result<Signature, SolanaRpcError> {
        unimplemented!()
    }
    async fn simulate_transaction(
        &self,
        _tx: &SolanaTx,
    ) -> Result<crate::solana::rpc::SimulationOutcome, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_signature_status(
        &self,
        _signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        unimplemented!()
    }
    async fn is_blockhash_valid(&self, _blockhash: &Hash) -> Result<bool, SolanaRpcError> {
        unimplemented!()
    }
}

/// A working [`FakeSolanaRpc`] that records how many account reads it was
/// asked for. Answers everything the fake does — so a route that reaches
/// for Solana still SUCCEEDS here, and the count is what distinguishes
/// "did not need Solana" from "needed Solana and it happened to be up".
struct CountingSolanaRpc {
    inner: FakeSolanaRpc,
    get_account_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl CountingSolanaRpc {
    fn new() -> (Self, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rpc = CountingSolanaRpc {
            inner: FakeSolanaRpc {
                bridge_config: fake_bridge_config_bytes(0, 100, TEST_PER_TRANSFER_LIMIT),
                rolling_volume_windows: (
                    fake_rolling_volume_window_bytes(0, 0, 0),
                    fake_rolling_volume_window_bytes(1, 0, 0),
                ),
            },
            get_account_calls: std::sync::Arc::clone(&counter),
        };
        (rpc, counter)
    }
}

impl SolanaRpc for CountingSolanaRpc {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        self.get_account_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.get_account(pubkey).await
    }
    async fn get_multiple_accounts(
        &self,
        _pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        unimplemented!()
    }
    async fn send_transaction(&self, _tx: &SolanaTx) -> Result<Signature, SolanaRpcError> {
        unimplemented!()
    }
    async fn simulate_transaction(
        &self,
        _tx: &SolanaTx,
    ) -> Result<crate::solana::rpc::SimulationOutcome, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_signature_status(
        &self,
        _signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        unimplemented!()
    }
    async fn is_blockhash_valid(&self, _blockhash: &Hash) -> Result<bool, SolanaRpcError> {
        unimplemented!()
    }
}

/// [`build`], parameterised by RPC and gate. Everything else matches
/// `build` exactly so these tests differ from the rest of the module in
/// the two things they are about, and nothing else.
fn build_with<R: SolanaRpc>(
    db_path: &std::path::Path,
    rpc: R,
    gate: crate::routes::RouteGate,
) -> BridgeApi<R> {
    opt_down(BridgeApi::new(
        db_path.to_path_buf(),
        rpc,
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        test_root_vault(),
        crate::goldcoin::address::Network::Testnet,
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(gate),
        test_route_fees(),
    ))
}

/// A gate that admits BOTH Robinhood routes. TEST-ONLY, exactly as
/// [`build_with_open_glc_to_rhn`] is: production ships them shut, which
/// `robinhood_quotes_are_still_refused_when_the_route_is_closed` pins.
fn both_robinhood_routes_open() -> crate::routes::RouteGate {
    crate::routes::RouteGate::new(
        crate::routes::RoutesConfig::default().with_robinhood(true, true, false, false),
        crate::chains::ChainRegistry::with_verified_robinhood(test_verified_deployment()),
    )
}

/// 0.005 GLC in canonical units. At the Solana routes' 300 bps: fee
/// 15_000, net 485_000; at the Robinhood routes' 600 bps: fee 30_000, net
/// 470_000. Both nets are exact multiples of 100, so either survives the
/// test mint's 6 decimals (2 fewer than canonical) and every direction
/// below quotes it without a precision refusal.
const QUOTE_GROSS: u64 = 500_000;

/// One rate's pinned arithmetic on [`QUOTE_GROSS`], spelled out rather
/// than recomputed — the point of a pin is to state what the numbers must
/// BE, not to repeat the calculation under test.
struct PinnedFee {
    bps: u64,
    amount: u64,
    display: &'static str,
    net: u64,
    net_display: &'static str,
}

/// What the two Solana-legged routes are priced at in `test_route_fees()`
/// (`amount_conversion::BRIDGE_FEE_BPS`).
const SOLANA_PINNED_FEE: PinnedFee = PinnedFee {
    bps: 300,
    amount: 15_000,
    display: "0.00015000",
    net: 485_000,
    net_display: "0.00485000",
};

/// What the two Robinhood routes are priced at in `test_route_fees()` —
/// deliberately NOT the Solana rate. A Robinhood quote reading 300 here
/// would be the global-fee leak that `crate::fees` closed, showing up in
/// the isolation tests as well as in the per-route fee tests below.
const ROBINHOOD_PINNED_FEE: PinnedFee = PinnedFee {
    bps: 600,
    amount: 30_000,
    display: "0.00030000",
    net: 470_000,
    net_display: "0.00470000",
};

/// The full `QuoteOutput` for [`QUOTE_GROSS`], as it must be for every
/// direction. Recorded BEFORE the Solana reads were scoped to the routes
/// that need them, so it measures rather than asserts that scoping
/// changed no figure: same fee, same net, same display strings, same
/// decimals, same asset labels.
///
/// The `fee` a direction is measured against is its OWN configured rate
/// ([`SOLANA_PINNED_FEE`] / [`ROBINHOOD_PINNED_FEE`]), which is the one
/// thing per-route pricing was allowed to change here. Everything else —
/// gross, decimals, asset labels, and the fact that fee + net reconciles
/// to gross — is pinned identically for all four directions, so scoping
/// the Solana reads still cannot move a figure without this failing.
///
/// The display strings are all rendered at Goldcoin's 8 decimals because
/// `gross_amount`/`fee_amount`/`net_amount` are CANONICAL units for every
/// direction — `source_decimals`/`destination_decimals` describe the
/// chains' own token precision and are not the unit of those fields
/// (`QuoteOutput`'s field docs; docs/20-bridge-fee.md).
fn assert_pinned_quote(
    quote: &QuoteOutput,
    direction: &str,
    fee: &PinnedFee,
    source_decimals: u8,
    destination_decimals: u8,
    source_asset: &str,
    destination_asset: &str,
) {
    assert_eq!(quote.direction, direction);
    assert_eq!(quote.gross_amount.0, QUOTE_GROSS, "{direction} gross");
    assert_eq!(
        quote.gross_display_amount, "0.00500000",
        "{direction} gross"
    );
    assert_eq!(quote.fee_bps, fee.bps, "{direction} fee_bps");
    assert_eq!(quote.fee_amount.0, fee.amount, "{direction} fee");
    assert_eq!(quote.fee_display_amount, fee.display, "{direction} fee");
    assert_eq!(quote.net_amount.0, fee.net, "{direction} net");
    assert_eq!(quote.net_display_amount, fee.net_display, "{direction} net");
    assert_eq!(
        quote.fee_amount.0 + quote.net_amount.0,
        quote.gross_amount.0,
        "{direction} must reconcile exactly at its own rate"
    );
    assert_eq!(quote.source_decimals, source_decimals, "{direction} source");
    assert_eq!(
        quote.destination_decimals, destination_decimals,
        "{direction} destination"
    );
    assert_eq!(quote.source_asset, source_asset, "{direction} source asset");
    assert_eq!(
        quote.destination_asset, destination_asset,
        "{direction} destination asset"
    );
}

/// The regression baseline: every field of every direction's quote,
/// pinned. If scoping the Solana reads changed any figure anywhere, this
/// is what says so.
#[tokio::test]
async fn quote_amounts_and_display_strings_are_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with(
        &db_path,
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes(0, 100, TEST_PER_TRANSFER_LIMIT),
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        both_robinhood_routes_open(),
    );

    let quote = |direction: &'static str| {
        let api = &api;
        async move {
            api.quote(QuoteInput {
                direction: direction.to_string(),
                gross_amount: AtomicU64(QUOTE_GROSS),
            })
            .await
            .unwrap_or_else(|e| panic!("{direction} must quote, got {e:?}"))
        }
    };

    // `TEST_SOLANA_DECIMALS` is the reserve mint's live value, read from
    // chain for the two Solana-legged routes.
    assert_pinned_quote(
        &quote("GlcToSol").await,
        "GlcToSol",
        &SOLANA_PINNED_FEE,
        8,
        TEST_SOLANA_DECIMALS,
        "GLC (Goldcoin)",
        "GLC (Solana)",
    );
    assert_pinned_quote(
        &quote("SolToGlc").await,
        "SolToGlc",
        &SOLANA_PINNED_FEE,
        TEST_SOLANA_DECIMALS,
        8,
        "GLC (Solana)",
        "GLC (Goldcoin)",
    );
    // Robinhood's 18 is a compile-time constant, never a chain read.
    assert_pinned_quote(
        &quote("GlcToRhn").await,
        "GlcToRhn",
        &ROBINHOOD_PINNED_FEE,
        8,
        18,
        "GLC (Goldcoin)",
        "GLC (Robinhood)",
    );
    assert_pinned_quote(
        &quote("RhnToGlc").await,
        "RhnToGlc",
        &ROBINHOOD_PINNED_FEE,
        18,
        8,
        "GLC (Robinhood)",
        "GLC (Goldcoin)",
    );
}

/// The isolation itself: a `RhnToGlc` quote is answered in full with
/// Solana unreachable. Both of its decimals are compile-time constants and
/// it settles on Goldcoin, whose atomic unit IS the canonical one — there
/// is nothing on Solana for this route to be waiting on.
#[tokio::test]
async fn rhn_to_glc_quotes_while_solana_rpc_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with(&db_path, UnreachableSolanaRpc, both_robinhood_routes_open());

    let quote = api
        .quote(QuoteInput {
            direction: "RhnToGlc".to_string(),
            gross_amount: AtomicU64(QUOTE_GROSS),
        })
        .await
        .expect("a route with no Solana leg must not depend on Solana RPC");

    assert_pinned_quote(
        &quote,
        "RhnToGlc",
        &ROBINHOOD_PINNED_FEE,
        18,
        8,
        "GLC (Robinhood)",
        "GLC (Goldcoin)",
    );
}

/// The same for the other direction. `GlcToRhn` widens canonical -> 18
/// decimals, which is a pure conversion against a constant.
#[tokio::test]
async fn glc_to_rhn_quotes_while_solana_rpc_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with(&db_path, UnreachableSolanaRpc, both_robinhood_routes_open());

    let quote = api
        .quote(QuoteInput {
            direction: "GlcToRhn".to_string(),
            gross_amount: AtomicU64(QUOTE_GROSS),
        })
        .await
        .expect("a route with no Solana leg must not depend on Solana RPC");

    assert_pinned_quote(
        &quote,
        "GlcToRhn",
        &ROBINHOOD_PINNED_FEE,
        8,
        18,
        "GLC (Goldcoin)",
        "GLC (Robinhood)",
    );
}

/// The direct guard against re-hoisting the reads.
///
/// Counted against a WORKING Solana RPC, so the assertion cannot pass for
/// the wrong reason: if a Robinhood quote reached for Solana here it would
/// still succeed, and only the counter would notice.
#[tokio::test]
async fn robinhood_quotes_make_no_solana_calls() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let (rpc, calls) = CountingSolanaRpc::new();
    let api = build_with(&db_path, rpc, both_robinhood_routes_open());

    for direction in ["GlcToRhn", "RhnToGlc"] {
        api.quote(QuoteInput {
            direction: direction.to_string(),
            gross_amount: AtomicU64(QUOTE_GROSS),
        })
        .await
        .unwrap_or_else(|e| panic!("{direction} must quote, got {e:?}"));
    }
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a Robinhood-only quote must not read a single Solana account"
    );

    // And the counter is real: the Solana-legged routes still read.
    for direction in ["GlcToSol", "SolToGlc"] {
        api.quote(QuoteInput {
            direction: direction.to_string(),
            gross_amount: AtomicU64(QUOTE_GROSS),
        })
        .await
        .unwrap_or_else(|e| panic!("{direction} must quote, got {e:?}"));
    }
    assert!(
        calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the Solana-legged routes must still read the reserve mint"
    );
}

/// No capability is widened: the two Solana-legged routes keep their
/// dependency on Solana exactly as before. A quote that cannot read the
/// reserve mint's live precision must not be answered from a guess.
#[tokio::test]
async fn solana_legged_quotes_still_require_solana_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with(&db_path, UnreachableSolanaRpc, both_robinhood_routes_open());

    for direction in ["GlcToSol", "SolToGlc"] {
        let err = api
            .quote(QuoteInput {
                direction: direction.to_string(),
                gross_amount: AtomicU64(QUOTE_GROSS),
            })
            .await
            .expect_err("a Solana-legged quote must not be answered without Solana RPC");
        assert!(
            matches!(err, ApiError::Upstream(_)),
            "{direction} must fail as an upstream fault, got {err:?}"
        );
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE, "{direction}");
    }
}

/// `GlcToSol`'s deliverability check still fires — proof that the
/// `(GlcToSol, Some(..))` arm still runs the `to_solana` conversion rather
/// than being skipped by the new `Option`.
///
/// `net` here is 485_001 canonical, which is not a whole number of the
/// test mint's 6-decimal units (2 fewer decimals than canonical), so the
/// quote must refuse rather than promise an amount a real transfer would
/// reject.
#[tokio::test]
async fn glc_to_sol_still_refuses_an_amount_the_mint_cannot_represent() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let err = api
        .quote(QuoteInput {
            direction: "GlcToSol".to_string(),
            gross_amount: AtomicU64(500_001),
        })
        .await
        .expect_err("a net the reserve mint cannot represent exactly must be refused");
    assert_eq!(err.status(), StatusCode::BAD_REQUEST);

    // The same amount is fine on a route that settles at canonical
    // precision — the refusal is about the Solana mint, not the amount.
    let robinhood_dir = tempfile::tempdir().unwrap();
    let robinhood_db = configure_with_robinhood_reserve(robinhood_dir.path());
    let robinhood = build_with(
        &robinhood_db,
        UnreachableSolanaRpc,
        both_robinhood_routes_open(),
    );
    robinhood
        .quote(QuoteInput {
            direction: "RhnToGlc".to_string(),
            gross_amount: AtomicU64(500_001),
        })
        .await
        .expect("RhnToGlc settles on Goldcoin at canonical precision — always exact");
}

/// Route admission is untouched: with the production gate, both Robinhood
/// routes are still refused, and still with a 409 rather than a 400.
#[tokio::test]
async fn robinhood_quotes_are_still_refused_when_the_route_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    for direction in ["GlcToRhn", "RhnToGlc"] {
        let err = api
            .quote(QuoteInput {
                direction: direction.to_string(),
                gross_amount: AtomicU64(QUOTE_GROSS),
            })
            .await
            .expect_err("a closed route must not be quotable");
        assert!(
            matches!(err, ApiError::RouteDisabled),
            "{direction} must be refused as a disabled route, got {err:?}"
        );
        assert_eq!(err.status(), StatusCode::CONFLICT, "{direction}");
    }
}

/// And an unrecognised name is still a client error, not a disabled route
/// — the distinction the gate exists to preserve.
#[tokio::test]
async fn an_unrecognised_route_is_still_a_400() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with(&db_path, UnreachableSolanaRpc, both_robinhood_routes_open());

    let err = api
        .quote(QuoteInput {
            direction: "NotARoute".to_string(),
            gross_amount: AtomicU64(QUOTE_GROSS),
        })
        .await
        .expect_err("an unknown route name must be refused");
    assert_eq!(err.status(), StatusCode::BAD_REQUEST);
}

// ================================================== per-route fees ==
//
// The API is where a global fee used to leak into a route it did not
// belong to. These pin that it cannot any more, from both surfaces that
// price anything: `POST /transfers` and `GET /quote`.
//
// `test_route_fees()` deliberately gives Solana 300 and Robinhood 600, so
// a leak in either direction changes a number one of these reads.

#[tokio::test]
async fn a_quote_uses_the_selected_routes_own_fee() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);

    let gross = 1_000_000_000u64; // 10 GLC

    let solana = api
        .quote(QuoteInput {
            direction: "GlcToSol".to_string(),
            gross_amount: AtomicU64(gross),
        })
        .await
        .unwrap();
    assert_eq!(solana.fee_bps, 300);
    assert_eq!(solana.fee_amount, AtomicU64(30_000_000));
    assert_eq!(solana.net_amount, AtomicU64(970_000_000));

    let robinhood = api
        .quote(QuoteInput {
            direction: "GlcToRhn".to_string(),
            gross_amount: AtomicU64(gross),
        })
        .await
        .unwrap();
    assert_eq!(
        robinhood.fee_bps, 600,
        "a Robinhood quote must carry the Robinhood rate — it used to \
         carry the compiled-in global one"
    );
    assert_eq!(robinhood.fee_amount, AtomicU64(60_000_000));
    assert_eq!(robinhood.net_amount, AtomicU64(940_000_000));

    // Display strings and net amount are derived from the SAME rate, so a
    // UI reading them cannot show one route's fee beside another's total.
    assert_eq!(robinhood.fee_display_amount, "0.60000000");
    assert_eq!(robinhood.net_display_amount, "9.40000000");
    assert_eq!(solana.fee_display_amount, "0.30000000");
    assert_eq!(solana.net_display_amount, "9.70000000");
}

#[tokio::test]
async fn the_two_directions_of_a_pair_quote_from_their_own_entries() {
    // `GlcToRhn` and `RhnToGlc` are separate entries and could differ; the
    // fixture happens to price them the same, so this pins that BOTH are
    // read from the Robinhood entries and neither falls through to Solana.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);

    for route in ["GlcToRhn", "RhnToGlc"] {
        let quote = api
            .quote(QuoteInput {
                direction: route.to_string(),
                gross_amount: AtomicU64(1_000_000_000),
            })
            .await
            .unwrap();
        assert_eq!(quote.fee_bps, 600, "{route}");
        assert_ne!(
            quote.fee_bps,
            amount_conversion::BRIDGE_FEE_BPS,
            "{route} must not be priced at the Solana rate"
        );
    }
}

#[tokio::test]
async fn a_created_glc_to_rhn_transfer_is_charged_the_robinhood_rate() {
    // The pricing bug in its original form: this request used to be
    // written into the ledger with the SOLANA rate snapshotted onto it,
    // which then settled at that rate for the rest of its life.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);

    // Sized against the Robinhood reserve fixture's available capacity;
    // the rate, not the amount, is what this test is about.
    let created = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(1_000_000),
            recipient: TEST_EVM_RECIPIENT.to_string(),
            route: Some("GlcToRhn".to_string()),
            source_address: None,
        })
        .await
        .unwrap();

    let ledger = Ledger::open(&db_path).unwrap();
    let request = ledger.get_request(created.request_id).unwrap().unwrap();
    assert_eq!(request.direction, Direction::GlcToRhn);
    assert_eq!(request.fee_bps, 600);
    assert_eq!(request.fee_amount_atomic, 60_000);
    assert_eq!(request.net_amount_atomic, 940_000);
    assert_eq!(
        request.gross_amount_atomic,
        request.fee_amount_atomic + request.net_amount_atomic,
        "gross must still reconcile exactly"
    );
}

#[tokio::test]
async fn a_created_glc_to_sol_transfer_keeps_the_solana_rate() {
    // The other side of the same coin: nothing about Robinhood being
    // priced differently may move the Solana route's economics.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_glc_to_rhn(&db_path);

    let created = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(1_000_000_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: Some("GlcToSol".to_string()),
            source_address: None,
        })
        .await
        .unwrap();

    let ledger = Ledger::open(&db_path).unwrap();
    let request = ledger.get_request(created.request_id).unwrap().unwrap();
    assert_eq!(request.direction, Direction::GlcToSol);
    assert_eq!(request.fee_bps, amount_conversion::BRIDGE_FEE_BPS);
    assert_eq!(request.fee_amount_atomic, 30_000_000);
    assert_eq!(request.net_amount_atomic, 970_000_000);
}

#[tokio::test]
async fn stats_report_every_routes_fee_not_one_global_number() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let stats = api.stats().await.unwrap();
    let table: std::collections::BTreeMap<String, u64> = stats
        .route_fees
        .iter()
        .map(|entry| (entry.route.clone(), entry.fee_bps))
        .collect();

    assert_eq!(table.get("GlcToSol"), Some(&300));
    assert_eq!(table.get("SolToGlc"), Some(&300));
    assert_eq!(table.get("GlcToRhn"), Some(&600));
    assert_eq!(table.get("RhnToGlc"), Some(&600));
    assert_eq!(
        table.len(),
        4,
        "only the executable routes are priced: {table:?}"
    );
    // The legacy single field still parses, and is the Solana route's —
    // beside the Solana program's limits it sits next to.
    assert_eq!(stats.bridge_fee_bps, 300);

    let solana_limits = api.limits().await.unwrap();
    assert_eq!(solana_limits.bridge_fee_bps, 300);
}

#[tokio::test]
async fn a_route_with_no_configured_fee_is_refused_rather_than_priced() {
    // Fail closed. An API that can serve a quote without knowing what to
    // charge serves a wrong one, so the refusal is the correct answer.
    //
    // The amounts below are exactly the 100 GLC source minimum, so this
    // runs on the PRODUCTION floor with no opt-down: the refusal under
    // test is the missing fee, and an amount that tripped the minimum
    // first would prove nothing about pricing.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = BridgeApi::new(
        db_path.to_path_buf(),
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes(0, 100, TEST_PER_TRANSFER_LIMIT),
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        test_root_vault(),
        crate::goldcoin::address::Network::Testnet,
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::routes::RouteGate::legacy_only()),
        crate::fees::RouteFees::new(),
    );

    let err = api
        .quote(QuoteInput {
            direction: "GlcToSol".to_string(),
            gross_amount: AtomicU64(10_000_000_000),
        })
        .await
        .expect_err("an unpriced route must not be quoted");
    assert!(
        format!("{err:?}").contains("no fee is configured"),
        "got {err:?}"
    );

    let err = api
        .create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(10_000_000_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: Some("GlcToSol".to_string()),
            source_address: None,
        })
        .await
        .expect_err("an unpriced route must not be charged");
    assert!(
        format!("{err:?}").contains("no fee is configured"),
        "got {err:?}"
    );

    // And nothing was written.
    let ledger = Ledger::open(&db_path).unwrap();
    assert!(ledger.get_request(1).unwrap().is_none());
}

// -------------------------------------------- per-route `available` --
//
// The production launch-blocker: `GET /chains` reported `RhnToGlc` as
// `enabled: true` — correctly, the route gate was open — while
// `reserve_ledger.admission_closed` was set on `GoldcoinReserve`, so
// every newly observed Robinhood deposit folded straight into
// `ManualReview` with `admission_closed_at_fold`. `RhnToGlc` deposits go
// direct to the custody contract with no `POST /transfers` preflight in
// front of them, so the availability signal the UI reads is the only
// thing standing between a user and an irreversible deposit into a
// closed gate.
//
// `enabled` keeps its meaning (config + `bridge_routes` + adapter).
// `available` is the new, separate answer. These tests pin both.

/// A ledger with the Robinhood route's LEDGER gate open, so the only
/// remaining route-gate leg under test is config + adapter.
fn configure_with_open_rhn_route(dir: &std::path::Path) -> std::path::PathBuf {
    let db_path = configure(dir);
    let mut ledger = Ledger::open(&db_path).unwrap();
    ledger
        .set_route_enabled(crate::routes::Route::RhnToGlc, true, Some("test"))
        .unwrap();
    db_path
}

/// An API whose every route gate admits `RhnToGlc`. TEST-ONLY, exactly
/// like [`build_with_open_glc_to_rhn`]: production ships all three shut.
fn build_with_open_rhn_to_glc(db_path: &std::path::Path) -> BridgeApi<FakeSolanaRpc> {
    opt_down(BridgeApi::new(
        db_path.to_path_buf(),
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes(0, 100, TEST_PER_TRANSFER_LIMIT),
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        test_root_vault(),
        crate::goldcoin::address::Network::Testnet,
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::routes::RouteGate::new(
            crate::routes::RoutesConfig::default().with_robinhood(false, true, false, false),
            crate::chains::ChainRegistry::with_verified_robinhood(test_verified_deployment()),
        )),
        test_route_fees(),
    ))
}

fn route<'a>(view: &'a ChainsView, id: &str) -> &'a RouteView {
    view.routes
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("{id} must be listed"))
}

/// **The regression.** Route gate wide open, admission closed by an
/// operator: `enabled` must stay `true` (it is a statement about
/// configuration, and the configuration did not change) while
/// `available` goes `false`.
#[tokio::test]
async fn rhn_to_glc_is_enabled_but_unavailable_while_admission_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_open_rhn_route(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_admission(ReserveDirection::GoldcoinReserve, true, Some("incident"))
            .unwrap();
    }
    let api = build_with_open_rhn_to_glc(&db_path);
    let view = api.chains().await.unwrap();
    let rhn = route(&view, "RhnToGlc");

    assert!(
        rhn.enabled,
        "closing admission must NOT redefine `enabled` — that is the field's whole point"
    );
    assert!(rhn.implemented);
    assert!(rhn.disabled_reason.is_none(), "the route gate is open");
    assert!(
        !rhn.available,
        "a closed admission gate means a new deposit would park in ManualReview"
    );
    assert_eq!(
        rhn.unavailable_reason.as_deref(),
        Some(DIRECTION_UNAVAILABLE_MESSAGE),
        "a capacity/pause condition gets the capacity copy, never the route-gate copy"
    );

    // The SAME `GoldcoinReserve` admission gate `RhnToGlc` folds against
    // is what `/status` reports — under both its historical name and the
    // destination-neutral one.
    let status = api.status().await.unwrap();
    assert!(!status.goldcoin_destination_admission_open);
    assert_eq!(
        status.sol_to_glc_admission_open, status.goldcoin_destination_admission_open,
        "the two names must be one value"
    );
}

/// The other half: gate open AND reserve healthy => `available: true`.
#[tokio::test]
async fn rhn_to_glc_is_available_when_enabled_and_the_reserve_is_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_open_rhn_route(dir.path());
    let api = build_with_open_rhn_to_glc(&db_path);
    let view = api.chains().await.unwrap();
    let rhn = route(&view, "RhnToGlc");

    assert!(rhn.enabled);
    assert!(rhn.available);
    assert!(rhn.unavailable_reason.is_none());

    let status = api.status().await.unwrap();
    assert!(status.goldcoin_destination_admission_open);
}

/// Every runtime gate the fold depends on must move `available`, one at a
/// time — so no gate is silently missing from the API's answer.
#[tokio::test]
async fn every_runtime_gate_closes_rhn_to_glc_availability() {
    type Mutate = fn(&mut Ledger);
    let cases: [(&str, Mutate); 5] = [
        ("operator admission", |l| {
            l.set_admission(ReserveDirection::GoldcoinReserve, true, Some("t"))
                .unwrap()
        }),
        ("reserve pause", |l| {
            l.set_paused(ReserveDirection::GoldcoinReserve, true, Some("t"))
                .unwrap()
        }),
        ("capacity", |l| {
            // `configure_reserve` updates thresholds, not the cached
            // balance: raising `protected_minimum` to the whole balance
            // drives confirmed headroom to zero.
            l.configure_reserve(
                ReserveDirection::GoldcoinReserve,
                0,
                10_000_000,
                20_000_000,
                15_000_000,
                10_000_001,
                0,
            )
            .unwrap()
        }),
        ("confirmed-liquidity buffer", |l| {
            l.set_admission_liquidity_thresholds(
                ReserveDirection::GoldcoinReserve,
                9_000_000_000,
                9_000_000_000,
            )
            .unwrap();
            l.evaluate_liquidity_admission_gate(ReserveDirection::GoldcoinReserve, 1)
                .unwrap();
        }),
        ("mature UTXO pool floor", |l| {
            l.set_utxo_pool_thresholds(ReserveDirection::GoldcoinReserve, 3, 5)
                .unwrap()
        }),
    ];

    for (label, mutate) in cases {
        let dir = tempfile::tempdir().unwrap();
        let db_path = configure_with_open_rhn_route(dir.path());

        // Healthy first, so each row proves the mutation is what moved it.
        let api = build_with_open_rhn_to_glc(&db_path);
        assert!(
            route(&api.chains().await.unwrap(), "RhnToGlc").available,
            "[{label}] the fixture must start available"
        );

        {
            let mut ledger = Ledger::open(&db_path).unwrap();
            mutate(&mut ledger);
        }
        let view = api.chains().await.unwrap();
        let rhn = route(&view, "RhnToGlc");
        assert!(
            rhn.enabled,
            "[{label}] a runtime gate must never redefine `enabled`"
        );
        assert!(!rhn.available, "[{label}] must close availability");
        assert_eq!(
            rhn.unavailable_reason.as_deref(),
            Some(DIRECTION_UNAVAILABLE_MESSAGE)
        );
    }
}

/// A route the gate refuses is unavailable regardless of reserve health,
/// and reports the ROUTE-GATE copy rather than the capacity copy.
#[tokio::test]
async fn a_disabled_route_is_never_available() {
    let dir = tempfile::tempdir().unwrap();
    // The shipping fixture: both Robinhood routes closed at every gate,
    // with a perfectly healthy Goldcoin reserve behind them.
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let view = api.chains().await.unwrap();

    for id in ["GlcToRhn", "RhnToGlc"] {
        let r = route(&view, id);
        assert!(!r.enabled, "{id} must be disabled in this fixture");
        assert!(!r.available, "{id} must not be available while disabled");
        assert_eq!(
            r.unavailable_reason.as_deref(),
            Some(crate::routes::RouteGateError::UNAVAILABLE_MESSAGE),
            "{id}: a closed route gate must report the route-gate copy"
        );
    }

    // ...and the reserve really is healthy, so the assertions above are
    // about the route gate and not about capacity.
    assert!(route(&view, "SolToGlc").available);
}

/// The two Solana<->Robinhood routes are implemented but default closed
/// on every gate, so they are never available on an unmodified
/// deployment — and the copy is the route-gate copy, never a capacity
/// reason.
#[tokio::test]
async fn cross_routes_are_implemented_but_closed_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let view = api.chains().await.unwrap();

    for id in ["SolToRhn", "RhnToSol"] {
        let r = route(&view, id);
        assert!(r.implemented, "{id} has settlement machinery");
        assert!(!r.enabled);
        assert!(!r.available, "{id} must never report available");
        assert_eq!(
            r.unavailable_reason.as_deref(),
            Some(crate::routes::RouteGateError::UNAVAILABLE_MESSAGE)
        );
    }
}

/// `GET /robinhood/reserve`'s `routes` are built by the same
/// [`RouteView::build`], so the two endpoints can never disagree about
/// either field.
#[tokio::test]
async fn robinhood_reserve_routes_report_the_same_availability_as_chains() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_open_rhn_route(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_admission(ReserveDirection::GoldcoinReserve, true, Some("incident"))
            .unwrap();
    }
    let api = build_with_open_rhn_to_glc(&db_path);
    let chains = api.chains().await.unwrap();
    let reserve = api.robinhood_reserve().await.unwrap();

    for id in ["GlcToRhn", "RhnToGlc"] {
        let from_chains = route(&chains, id);
        let from_reserve = reserve
            .routes
            .iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("{id} must be listed on the reserve view"));
        assert_eq!(from_chains.enabled, from_reserve.enabled, "{id}: enabled");
        assert_eq!(
            from_chains.available, from_reserve.available,
            "{id}: available"
        );
        assert_eq!(
            from_chains.unavailable_reason, from_reserve.unavailable_reason,
            "{id}: unavailable_reason"
        );
    }
    assert!(!route(&chains, "RhnToGlc").available);
}

// ------------------------------- route-scoped admission (schema v25) --

/// **The property the route-scoped axis exists for, at the API
/// boundary.** Closing ONE inbound route's own admission gate must turn
/// that route's `available` false and leave the other route's alone —
/// even though both settle out of the same `GoldcoinReserve`.
///
/// A UI gates its transfer button on `available`, and an `RhnToGlc`
/// deposit is irreversible with no `POST /transfers` preflight in front
/// of it, so a `/chains` that ignored this gate would offer a transfer
/// the fold then parks. That is the same failure the `enabled`/
/// `available` split was introduced to close.
#[tokio::test]
async fn closing_one_inbound_route_only_affects_that_route_on_chains() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_open_rhn_route(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_route_admission(crate::routes::Route::RhnToGlc, true, Some("route incident"))
            .unwrap();
    }
    let api = build_with_open_rhn_to_glc(&db_path);
    let view = api.chains().await.unwrap();

    let rhn = route(&view, "RhnToGlc");
    assert!(
        rhn.enabled,
        "closing route ADMISSION must not redefine `enabled` — enablement is a separate axis"
    );
    assert!(rhn.implemented);
    assert!(
        !rhn.available,
        "a route whose own admission gate is closed must not report available"
    );
    assert_eq!(
        rhn.unavailable_reason.as_deref(),
        Some(DIRECTION_UNAVAILABLE_MESSAGE),
        "a closed gate is a capacity/pause condition, not a 'route does not exist' one"
    );

    let sol = route(&view, "SolToGlc");
    assert!(
        sol.available,
        "SolToGlc shares the reserve but not the gate — it must stay available"
    );
    assert!(sol.unavailable_reason.is_none());
}

/// The mirror: closing `SolToGlc` leaves `RhnToGlc` available.
#[tokio::test]
async fn closing_sol_to_glc_leaves_rhn_to_glc_available_on_chains() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_open_rhn_route(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_route_admission(crate::routes::Route::SolToGlc, true, Some("route incident"))
            .unwrap();
    }
    let api = build_with_open_rhn_to_glc(&db_path);
    let view = api.chains().await.unwrap();

    assert!(!route(&view, "SolToGlc").available);
    assert!(
        route(&view, "RhnToGlc").available,
        "closing SolToGlc must not close RhnToGlc"
    );
    // GlcToSol draws on the Solana reserve and has no route-level gate at
    // all; it is untouched by anything on the Goldcoin side.
    assert!(route(&view, "GlcToSol").available);
}

/// Reopening the reserve-wide pause does not override a route-specific
/// closed gate — asserted at the API boundary, where an operator would
/// go looking after an incident.
#[tokio::test]
async fn reopening_the_reserve_does_not_make_a_closed_route_available() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_open_rhn_route(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_route_admission(crate::routes::Route::RhnToGlc, true, Some("route incident"))
            .unwrap();
        ledger
            .set_paused(ReserveDirection::GoldcoinReserve, true, Some("maintenance"))
            .unwrap();
        // Maintenance over: the reserve-wide stop is lifted.
        ledger
            .set_paused(ReserveDirection::GoldcoinReserve, false, Some("done"))
            .unwrap();
    }
    let api = build_with_open_rhn_to_glc(&db_path);
    let view = api.chains().await.unwrap();

    assert!(
        !route(&view, "RhnToGlc").available,
        "unpausing the reserve must not open a route an operator closed"
    );
    assert!(
        route(&view, "SolToGlc").available,
        "the sibling route recovers with the reserve, as it always did"
    );
}

/// The reserve-wide pause remains the emergency stop: it closes both
/// inbound routes regardless of their own gates being open.
#[tokio::test]
async fn the_reserve_wide_pause_still_closes_both_inbound_routes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_open_rhn_route(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // Both route gates explicitly open.
        ledger
            .set_route_admission(crate::routes::Route::SolToGlc, false, None)
            .unwrap();
        ledger
            .set_route_admission(crate::routes::Route::RhnToGlc, false, None)
            .unwrap();
        ledger
            .set_paused(ReserveDirection::GoldcoinReserve, true, Some("emergency"))
            .unwrap();
    }
    let api = build_with_open_rhn_to_glc(&db_path);
    let view = api.chains().await.unwrap();

    for id in ["SolToGlc", "RhnToGlc"] {
        assert!(
            !route(&view, id).available,
            "{id} must be closed by the reserve-wide pause even with its own gate open"
        );
    }
}

/// `/status`'s `sol_to_glc_available` must agree with `/chains`'s
/// `SolToGlc.available`.
///
/// Two endpoints disagreeing about whether a route is open is the exact
/// failure class this area keeps producing; the route-scoped gate would
/// have reintroduced it by omission had `/status` not been narrowed too.
#[tokio::test]
async fn status_and_chains_agree_about_sol_to_glc_availability() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_open_rhn_route(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_route_admission(crate::routes::Route::SolToGlc, true, Some("route incident"))
            .unwrap();
    }
    let api = build_with_open_rhn_to_glc(&db_path);
    let status = api.status().await.unwrap();
    let chains = api.chains().await.unwrap();

    assert!(!status.sol_to_glc_available);
    assert!(!status.sol_to_glc_admission_open);
    assert_eq!(
        status.sol_to_glc_available,
        route(&chains, "SolToGlc").available,
        "/status and /chains must never disagree about SolToGlc"
    );
    // The RESERVE-wide field keeps its own, unchanged meaning: the
    // reserve would still admit, it is this route that will not.
    assert!(
        status.goldcoin_destination_admission_open,
        "a route-level closure must not be reported as a reserve-wide one"
    );
}

/// The non-executable Solana<->Robinhood routes are unchanged by the new
/// axis: still closed on enablement, still on the route-gate copy.
#[tokio::test]
async fn route_scoped_admission_does_not_change_the_closed_cross_routes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_open_rhn_route(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_route_admission(crate::routes::Route::RhnToGlc, true, Some("incident"))
            .unwrap();
    }
    let api = build_with_open_rhn_to_glc(&db_path);
    let view = api.chains().await.unwrap();

    for id in ["SolToRhn", "RhnToSol"] {
        let r = route(&view, id);
        assert!(r.implemented);
        assert!(!r.enabled);
        assert!(!r.available);
        assert_eq!(
            r.unavailable_reason.as_deref(),
            Some(crate::routes::RouteGateError::UNAVAILABLE_MESSAGE),
            "{id} keeps the route-gate copy, never the capacity copy"
        );
    }
}

// ------------------------------------------ Solana behaviour unchanged --

/// The legacy routes keep every field they had, and gain an `available`
/// that tracks their own destination reserve — never Goldcoin's for
/// `GlcToSol`, and never the Robinhood reserve's for either.
#[tokio::test]
async fn legacy_routes_are_available_on_a_healthy_default_deployment() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let view = api.chains().await.unwrap();

    for id in ["GlcToSol", "SolToGlc"] {
        let r = route(&view, id);
        assert!(r.enabled, "{id} must stay enabled by construction");
        assert!(r.implemented);
        assert!(r.disabled_reason.is_none());
        assert!(r.available, "{id} must be available on a healthy fixture");
        assert!(r.unavailable_reason.is_none());
    }
}

/// Each legacy route reads its OWN destination reserve. Pausing Goldcoin
/// must close `SolToGlc` and leave `GlcToSol` untouched, and vice versa —
/// the property that would break if `available` were derived from one
/// global reserve.
#[tokio::test]
async fn each_route_reads_only_its_own_destination_reserve() {
    for (paused_reserve, closed, open) in [
        (ReserveDirection::GoldcoinReserve, "SolToGlc", "GlcToSol"),
        (ReserveDirection::SolanaReserve, "GlcToSol", "SolToGlc"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = configure(dir.path());
        {
            let mut ledger = Ledger::open(&db_path).unwrap();
            ledger.set_paused(paused_reserve, true, Some("t")).unwrap();
        }
        let api = build(&db_path, 0);
        let view = api.chains().await.unwrap();
        assert!(
            !route(&view, closed).available,
            "pausing {paused_reserve:?} must close {closed}"
        );
        assert!(
            route(&view, open).available,
            "pausing {paused_reserve:?} must NOT touch {open}"
        );
        // `enabled` is untouched either way — a pause is not a
        // configuration change.
        assert!(route(&view, closed).enabled);
        assert!(route(&view, open).enabled);
    }
}

/// `GlcToRhn`'s availability comes from the ROBINHOOD reserve, and
/// `RhnToGlc`'s from the GOLDCOIN one. Confusing the two is what made the
/// production incident hard to read (`glc-admin robinhood-status` prints
/// the Robinhood reserve's `paused`, which governs `GlcToRhn` only).
#[tokio::test]
async fn the_two_robinhood_routes_read_different_reserves() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_route_enabled(crate::routes::Route::RhnToGlc, true, Some("test"))
            .unwrap();
        ledger
            .set_paused(ReserveDirection::RobinhoodReserve, true, Some("outbound"))
            .unwrap();
    }
    let api = build_with_open_rhn_to_glc(&db_path);
    let view = api.chains().await.unwrap();
    assert!(
        route(&view, "RhnToGlc").available,
        "RhnToGlc pays out of the GOLDCOIN reserve; the Robinhood reserve's pause is a \
         different route's gate"
    );
}

/// An unconfigured destination reserve fails CLOSED rather than erroring
/// or reporting available — the rule that matters most for a route with
/// no preflight between the answer and an irreversible deposit.
#[tokio::test]
async fn an_unconfigured_destination_reserve_is_unavailable_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    // `configure` seeds Goldcoin and Solana only — there is no
    // `RobinhoodReserve` row, which is every production deployment today.
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_route_enabled(crate::routes::Route::GlcToRhn, true, Some("test"))
            .unwrap();
    }
    let api = build_with_open_glc_to_rhn(&db_path);
    let view = api.chains().await.unwrap();
    let r = route(&view, "GlcToRhn");
    assert!(r.enabled, "every route gate is open in this fixture");
    assert!(
        !r.available,
        "a destination reserve with no ledger row can admit nothing"
    );
    assert_eq!(
        r.unavailable_reason.as_deref(),
        Some(DIRECTION_UNAVAILABLE_MESSAGE)
    );
}

/// Listing routes is READ-ONLY: it reports the confirmed-liquidity gate's
/// persisted state and must never evaluate the hysteresis, which would
/// let a public GET move an admission gate.
#[tokio::test]
async fn listing_routes_never_moves_the_liquidity_admission_gate() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_open_rhn_route(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // A buffer far above headroom: an evaluating reader would close
        // the gate as a side effect of being asked.
        ledger
            .set_admission_liquidity_thresholds(
                ReserveDirection::GoldcoinReserve,
                9_000_000_000,
                9_000_000_000,
            )
            .unwrap();
    }
    let api = build_with_open_rhn_to_glc(&db_path);
    for _ in 0..3 {
        let _ = api.chains().await.unwrap();
        let _ = api.status().await.unwrap();
    }
    let ledger = Ledger::open(&db_path).unwrap();
    assert!(
        !ledger
            .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        "a read-only listing must never close the automatic gate"
    );
}

// ---------------------------------------------- Solana<->Robinhood quotes --

/// An API with both cross routes open on every service gate and priced
/// (SolToRhn at 450, RhnToSol at 500 — deliberately different from every
/// other route), so a quote's rate can only have come from its own entry.
fn build_with_open_cross_routes(db_path: &std::path::Path) -> BridgeApi<FakeSolanaRpc> {
    {
        let mut ledger = Ledger::open(db_path).unwrap();
        for route in [
            crate::routes::Route::SolToRhn,
            crate::routes::Route::RhnToSol,
        ] {
            ledger.set_route_enabled(route, true, None).unwrap();
        }
    }
    let mut fees = test_route_fees();
    fees.insert(crate::routes::Route::SolToRhn, 450).unwrap();
    fees.insert(crate::routes::Route::RhnToSol, 500).unwrap();
    opt_down(BridgeApi::new(
        db_path.to_path_buf(),
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes(0, 100, TEST_PER_TRANSFER_LIMIT),
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        test_root_vault(),
        crate::goldcoin::address::Network::Testnet,
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::routes::RouteGate::new(
            crate::routes::RoutesConfig::default().with_robinhood(true, true, true, true),
            crate::chains::ChainRegistry::with_verified_robinhood(test_verified_deployment()),
        )),
        fees,
    ))
}

#[tokio::test]
async fn cross_route_quotes_price_at_their_own_rate_and_name_the_right_units() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_cross_routes(&db_path);

    let sol_to_rhn = api
        .quote(QuoteInput {
            direction: "SolToRhn".to_string(),
            gross_amount: AtomicU64(1_000_000_000),
        })
        .await
        .unwrap();
    assert_eq!(sol_to_rhn.fee_bps, 450);
    assert_eq!(sol_to_rhn.fee_amount, AtomicU64(45_000_000));
    assert_eq!(sol_to_rhn.net_amount, AtomicU64(955_000_000));
    assert_eq!(sol_to_rhn.source_decimals, TEST_SOLANA_DECIMALS);
    assert_eq!(sol_to_rhn.destination_decimals, 18);
    assert_eq!(sol_to_rhn.source_asset, "GLC (Solana)");
    assert_eq!(sol_to_rhn.destination_asset, "GLC (Robinhood)");

    let rhn_to_sol = api
        .quote(QuoteInput {
            direction: "RhnToSol".to_string(),
            gross_amount: AtomicU64(1_000_000_000),
        })
        .await
        .unwrap();
    assert_eq!(rhn_to_sol.fee_bps, 500);
    assert_eq!(rhn_to_sol.fee_amount, AtomicU64(50_000_000));
    assert_eq!(rhn_to_sol.net_amount, AtomicU64(950_000_000));
    assert_eq!(rhn_to_sol.source_decimals, 18);
    assert_eq!(rhn_to_sol.destination_decimals, TEST_SOLANA_DECIMALS);
    assert_eq!(rhn_to_sol.source_asset, "GLC (Robinhood)");
    assert_eq!(rhn_to_sol.destination_asset, "GLC (Solana)");
}

#[tokio::test]
async fn an_rhn_to_sol_quote_refuses_a_net_the_mint_cannot_spell() {
    // 1.00000010 GLC: canonical-exact, but net at 500 bps ends in ...10,
    // which a 6-decimal mint cannot represent. Refused here, before the
    // deposit, exactly as GlcToSol refuses the same shape.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_cross_routes(&db_path);
    let err = api
        .quote(QuoteInput {
            direction: "RhnToSol".to_string(),
            gross_amount: AtomicU64(100_000_010),
        })
        .await
        .expect_err("an undeliverable net must not be quoted");
    assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");
    assert!(
        err.to_string().contains("cannot be represented exactly"),
        "{err}"
    );
    // The same amount towards Robinhood is always deliverable.
    api.quote(QuoteInput {
        direction: "SolToRhn".to_string(),
        gross_amount: AtomicU64(100_000_010),
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cross_route_quotes_are_refused_while_the_routes_are_closed() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build(&db_path, 0);
    for route in ["SolToRhn", "RhnToSol"] {
        let err = api
            .quote(QuoteInput {
                direction: route.to_string(),
                gross_amount: AtomicU64(1_000_000_000),
            })
            .await
            .expect_err("a closed route is not quoted");
        assert!(matches!(err, ApiError::RouteDisabled), "{route}: {err:?}");
    }
}

#[tokio::test]
async fn open_cross_routes_are_listed_available_and_gated_by_their_own_admission() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_with_open_cross_routes(&db_path);
    let view = api.chains().await.unwrap();
    for id in ["SolToRhn", "RhnToSol"] {
        let r = route(&view, id);
        assert!(r.implemented, "{id}");
        assert!(r.enabled, "{id}");
        assert!(r.available, "{id}: {:?}", r.unavailable_reason);
    }
    // Close SolToRhn's own admission: it alone goes unavailable, on the
    // capacity copy; RhnToSol and every other route are untouched.
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_route_admission(crate::routes::Route::SolToRhn, true, Some("incident"))
            .unwrap();
    }
    let view = api.chains().await.unwrap();
    let closed = route(&view, "SolToRhn");
    assert!(closed.enabled);
    assert!(!closed.available);
    assert_eq!(
        closed.unavailable_reason.as_deref(),
        Some(DIRECTION_UNAVAILABLE_MESSAGE)
    );
    assert!(route(&view, "RhnToSol").available);
    assert!(route(&view, "GlcToRhn").available);
    assert!(route(&view, "GlcToSol").available);
    assert!(route(&view, "SolToGlc").available);
}

// ------------------------------- the source-side minimum, on the DEFAULT path --

/// A `BridgeApi` built exactly as production builds one: every route
/// open and priced, and **no opt-down**.
///
/// The `opt_down` wrapper every other constructor in this file ends in is
/// deliberately absent here. That is the whole point of these tests — they
/// are the ones that would still fail if `BridgeApi::new` stopped applying
/// `min_transfer::SOURCE_MINIMUM_CANONICAL`, or if a config key were ever
/// wired to that field.
fn build_at_production_minimum(db_path: &std::path::Path) -> BridgeApi<FakeSolanaRpc> {
    {
        let mut ledger = Ledger::open(db_path).unwrap();
        for route in [
            crate::routes::Route::SolToRhn,
            crate::routes::Route::RhnToSol,
        ] {
            ledger.set_route_enabled(route, true, None).unwrap();
        }
    }
    let mut fees = test_route_fees();
    fees.insert(crate::routes::Route::SolToRhn, 450).unwrap();
    fees.insert(crate::routes::Route::RhnToSol, 500).unwrap();
    BridgeApi::new(
        db_path.to_path_buf(),
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes(0, 100, TEST_PER_TRANSFER_LIMIT),
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        test_root_vault(),
        crate::goldcoin::address::Network::Testnet,
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::routes::RouteGate::new(
            crate::routes::RoutesConfig::default().with_robinhood(true, true, true, true),
            crate::chains::ChainRegistry::with_verified_robinhood(test_verified_deployment()),
        )),
        fees,
    )
}

/// Exactly 100.00000000 GLC, canonical 8dp — the policy boundary, written
/// out so the assertion below reads as the rule it is testing.
const EXACTLY_ONE_HUNDRED_GLC: u64 = 10_000_000_000;
/// One atomic unit below it.
const ONE_UNIT_BELOW_ONE_HUNDRED_GLC: u64 = 9_999_999_999;

/// Every route this bridge implements, so a route added later is caught
/// here rather than shipping without a floor.
fn every_implemented_route() -> Vec<crate::routes::Route> {
    let routes: Vec<_> = crate::routes::Route::ALL
        .into_iter()
        .filter(|r| r.as_direction().is_some())
        .collect();
    assert_eq!(
        routes.len(),
        6,
        "all six routes are implemented on this build"
    );
    routes
}

/// The default a production `BridgeApi` admits against is the policy
/// constant, with nothing configured and no builder called.
#[tokio::test]
async fn the_production_default_source_minimum_is_one_hundred_glc() {
    assert_eq!(
        crate::min_transfer::SOURCE_MINIMUM_CANONICAL.0,
        EXACTLY_ONE_HUNDRED_GLC
    );
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_at_production_minimum(&db_path);

    // Proved through behaviour rather than by reading the field back: a
    // getter could agree with the constant while the admission path used
    // something else.
    for route in every_implemented_route() {
        let refused = api
            .quote(QuoteInput {
                direction: route.as_str().to_string(),
                gross_amount: AtomicU64(ONE_UNIT_BELOW_ONE_HUNDRED_GLC),
            })
            .await
            .expect_err("99.99999999 GLC is below the policy floor");
        let message = refused.to_string();
        assert!(
            message.contains("10000000000"),
            "{}: the refusal must name the 100 GLC floor, got: {message}",
            route.as_str()
        );
    }
}

/// The boundary, on every implemented route, through the real admission
/// path: exactly 100 passes, one atomic unit less does not.
#[tokio::test]
async fn exactly_one_hundred_glc_quotes_on_every_route_and_below_it_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_at_production_minimum(&db_path);

    for route in every_implemented_route() {
        let accepted = api
            .quote(QuoteInput {
                direction: route.as_str().to_string(),
                gross_amount: AtomicU64(EXACTLY_ONE_HUNDRED_GLC),
            })
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "{}: exactly 100.00000000 GLC must quote, got {e}",
                    route.as_str()
                )
            });
        assert_eq!(accepted.gross_amount, AtomicU64(EXACTLY_ONE_HUNDRED_GLC));

        assert!(
            api.quote(QuoteInput {
                direction: route.as_str().to_string(),
                gross_amount: AtomicU64(ONE_UNIT_BELOW_ONE_HUNDRED_GLC),
            })
            .await
            .is_err(),
            "{}: 99.99999999 GLC must be refused",
            route.as_str()
        );
    }
}

/// The fee still comes off AFTER the check, so a minimum transfer delivers
/// less than the minimum — on every route, at that route's own rate.
///
/// This is the assertion most likely to be "corrected" by someone reading
/// a 97 GLC payout as a violation of a 100 GLC minimum. It is not: the
/// policy is a statement about what was SENT.
#[tokio::test]
async fn a_minimum_transfer_is_priced_normally_and_nets_below_the_minimum() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_at_production_minimum(&db_path);

    for route in every_implemented_route() {
        let quote = api
            .quote(QuoteInput {
                direction: route.as_str().to_string(),
                gross_amount: AtomicU64(EXACTLY_ONE_HUNDRED_GLC),
            })
            .await
            .unwrap();
        // The fee is this route's own configured rate, applied to the
        // gross that was checked — not reduced, waived or clamped because
        // the amount sits on the floor.
        let expected_fee = EXACTLY_ONE_HUNDRED_GLC * quote.fee_bps / 10_000;
        assert_eq!(
            quote.fee_amount,
            AtomicU64(expected_fee),
            "{}: a minimum transfer is charged the ordinary fee",
            route.as_str()
        );
        assert_eq!(
            quote.net_amount,
            AtomicU64(EXACTLY_ONE_HUNDRED_GLC - expected_fee)
        );
        assert!(
            quote.fee_bps > 0 && quote.net_amount.0 < EXACTLY_ONE_HUNDRED_GLC,
            "{}: the destination figure is BELOW the source minimum, by design",
            route.as_str()
        );
    }
}

/// The same boundary on `POST /transfers`, for the two routes this service
/// originates — the path where a refusal means nothing moves at all.
#[tokio::test]
async fn creating_a_transfer_applies_the_same_floor_as_quoting_one() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_at_production_minimum(&db_path);

    for (route, recipient) in [
        (
            crate::routes::Route::GlcToSol,
            Keypair::new().pubkey().to_string(),
        ),
        (
            crate::routes::Route::GlcToRhn,
            "0xAbAbabababababababababababababababababAb".to_string(),
        ),
    ] {
        let refused = api
            .create_goldcoin_deposit_transfer(CreateTransferInput {
                amount_atomic: AtomicU64(ONE_UNIT_BELOW_ONE_HUNDRED_GLC),
                recipient,
                route: Some(route.as_str().to_string()),
                source_address: None,
            })
            .await
            .expect_err("99.99999999 GLC must be refused at creation");
        assert!(
            refused.to_string().contains("10000000000"),
            "{}: {refused}",
            route.as_str()
        );
        // And nothing was written: a refused amount leaves no row behind.
        let listed = api.list_transfers(None, None, None, 50).await.unwrap();
        assert!(
            listed.items.is_empty(),
            "{}: a refused creation must not persist a request",
            route.as_str()
        );
    }
}

// =====================================================================
// The route-generic wallet pre-check (`GET /routes/{route}/eligibility`)
// and the `POST /transfers` wallet windows — the API face of
// `ledger::wallet_window`.
// =====================================================================

/// Folds one attempt on `direction` straight into the ledger at
/// `created_at`, so the endpoint answers from authoritative state.
fn seed_attempt(
    db_path: &std::path::Path,
    direction: Direction,
    seq: u64,
    source: &[u8],
    destination: &[u8],
    created_at: i64,
) {
    let mut ledger = Ledger::open(db_path).unwrap();
    let amounts = crate::ledger::RequestAmounts {
        gross_atomic: 50_000,
        fee_bps: 0,
        fee_atomic: 0,
        net_atomic: 50_000,
        net_destination_atomic: 50_000,
    };
    match direction {
        Direction::SolToGlc => {
            let outcome = ledger
                .fold_sol_deposit(
                    seq,
                    amounts,
                    source.try_into().unwrap(),
                    destination,
                    None,
                    created_at,
                )
                .unwrap();
            assert!(
                matches!(
                    outcome,
                    crate::ledger::SolFoldOutcome::FoldedFinalized { .. }
                ),
                "{outcome:?}"
            );
        }
        Direction::SolToRhn => {
            let outcome = ledger
                .fold_sol_deposit_to_robinhood(
                    seq,
                    amounts,
                    source.try_into().unwrap(),
                    Some(destination.try_into().unwrap()),
                    destination,
                    true,
                    None,
                    created_at,
                )
                .unwrap();
            assert!(
                matches!(
                    outcome,
                    crate::ledger::SolFoldOutcome::FoldedFinalized { .. }
                ),
                "{outcome:?}"
            );
        }
        Direction::RhnToGlc | Direction::RhnToSol => {
            let obs = crate::ledger::RobinhoodDepositObservation {
                source_contract: [0xC0; 20],
                obligation_index: seq,
                route: crate::routes::Route::from(direction),
                depositor: source.try_into().unwrap(),
                destination: destination.to_vec(),
                amount_robinhood_atomic: crate::evm::EvmU256::from_u128(
                    50_000u128 * 10_000_000_000,
                )
                .to_be_bytes(),
                amount_canonical_atomic: 50_000,
                tx_hash: {
                    let mut h = [0xAA; 32];
                    h[0] = seq as u8;
                    h
                },
                log_index: 0,
                block_number: 500 + seq,
                block_hash: [0xBB; 32],
            };
            ledger
                .robinhood_record_final_observation(&obs, created_at)
                .unwrap();
            let row = ledger
                .robinhood_observation_by_source([0xC0; 20], seq)
                .unwrap()
                .unwrap();
            let outcome = ledger
                .fold_robinhood_deposit(&row, amounts, Some(destination), true, None, created_at)
                .unwrap();
            assert!(
                matches!(
                    outcome,
                    crate::robinhood::fold::FoldOutcome::FoldedFinalized { .. }
                ),
                "{outcome:?}"
            );
        }
        Direction::GlcToSol | Direction::GlcToRhn => {
            let outcome = ledger
                .create_request_from(
                    direction,
                    amounts,
                    destination,
                    None,
                    Some(source),
                    3_600,
                    created_at,
                )
                .unwrap();
            assert!(
                matches!(outcome, CreateRequestOutcome::Reserved { .. }),
                "{outcome:?}"
            );
        }
    }
}

/// `(source, destination)` spellings and bytes for one route, per `tag`.
fn route_wallets(route: crate::routes::Route, tag: u8) -> ((String, Vec<u8>), (String, Vec<u8>)) {
    fn one(chain: crate::routes::Chain, tag: u8) -> (String, Vec<u8>) {
        match chain {
            crate::routes::Chain::Goldcoin => {
                let address = test_glc_address(tag);
                (address.clone(), address.into_bytes())
            }
            crate::routes::Chain::Solana => {
                let key = Pubkey::new_from_array([tag; 32]);
                (key.to_string(), key.to_bytes().to_vec())
            }
            crate::routes::Chain::Robinhood => {
                let mut bytes = [tag; 20];
                bytes[0] = 0xEE;
                (
                    crate::evm::address::EvmAddress::from_bytes(bytes).to_string(),
                    bytes.to_vec(),
                )
            }
        }
    }
    (
        one(route.source_chain(), tag),
        one(route.destination_chain(), tag),
    )
}

fn configure_every_reserve(dir: &std::path::Path) -> std::path::PathBuf {
    configure_with_robinhood_reserve(dir)
}

#[tokio::test]
async fn route_eligibility_reports_fresh_wallets_as_eligible_on_every_route() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_every_reserve(dir.path());
    let api = build(&db_path, 0);
    for route in crate::routes::Route::ALL {
        let ((src, _), (dst, _)) = route_wallets(route, 7);
        let out = api
            .route_wallet_eligibility(route, Some(src.clone()), Some(dst.clone()))
            .await
            .unwrap();
        assert_eq!(out.route, route.as_str());
        assert!(out.eligible, "{route:?}: {out:?}");
        assert_eq!(out.blocked_reason, None);
        assert_eq!(out.blocked_reasons, Vec::<String>::new());
        assert_eq!(out.retry_after, None);
        assert_eq!(out.window_seconds, 86_400);
        let source = out.source.unwrap();
        assert_eq!(source.address, src);
        assert!(source.eligible);
        assert_eq!(source.reason, None);
        let destination = out.destination.unwrap();
        assert_eq!(destination.address, dst);
        assert!(destination.eligible);
    }
}

#[tokio::test]
async fn route_eligibility_reports_each_blocked_leg_with_its_reason_and_reopen_time_on_every_route()
{
    for route in crate::routes::Route::ALL {
        let direction = route.as_direction().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db_path = configure_every_reserve(dir.path());
        let api = build(&db_path, 0);
        let ((src, src_bytes), (dst, dst_bytes)) = route_wallets(route, 7);
        let ((other_src, _), (other_dst, _)) = route_wallets(route, 8);
        let created_at = now_unix() - 100;
        seed_attempt(&db_path, direction, 1, &src_bytes, &dst_bytes, created_at);

        // Both legs busy: source reported first, both listed.
        let both = api
            .route_wallet_eligibility(route, Some(src.clone()), Some(dst.clone()))
            .await
            .unwrap();
        assert!(!both.eligible, "{route:?}");
        assert_eq!(
            both.blocked_reason.as_deref(),
            Some(BLOCKED_REASON_WALLET_SOURCE_24H_LIMIT),
            "{route:?}"
        );
        assert_eq!(
            both.blocked_reasons,
            vec![
                BLOCKED_REASON_WALLET_SOURCE_24H_LIMIT.to_string(),
                BLOCKED_REASON_WALLET_DESTINATION_24H_LIMIT.to_string()
            ],
            "{route:?}"
        );
        assert_eq!(both.retry_after, Some(created_at + 86_400), "{route:?}");
        let remaining = both.retry_after_seconds.unwrap();
        assert!(
            (86_000..=86_300).contains(&remaining),
            "{route:?}: {remaining}"
        );
        let source = both.source.as_ref().unwrap();
        assert!(!source.eligible);
        assert_eq!(
            source.reason.as_deref(),
            Some(BLOCKED_REASON_WALLET_SOURCE_24H_LIMIT)
        );
        assert_eq!(source.retry_after, Some(created_at + 86_400));
        let destination = both.destination.as_ref().unwrap();
        assert!(!destination.eligible);
        assert_eq!(
            destination.reason.as_deref(),
            Some(BLOCKED_REASON_WALLET_DESTINATION_24H_LIMIT)
        );

        // Only the destination busy.
        let dst_only = api
            .route_wallet_eligibility(route, Some(other_src.clone()), Some(dst.clone()))
            .await
            .unwrap();
        assert_eq!(
            dst_only.blocked_reason.as_deref(),
            Some(BLOCKED_REASON_WALLET_DESTINATION_24H_LIMIT),
            "{route:?}"
        );
        assert!(dst_only.source.as_ref().unwrap().eligible);
        assert!(!dst_only.destination.as_ref().unwrap().eligible);

        // Only the source busy.
        let src_only = api
            .route_wallet_eligibility(route, Some(src.clone()), Some(other_dst.clone()))
            .await
            .unwrap();
        assert_eq!(
            src_only.blocked_reason.as_deref(),
            Some(BLOCKED_REASON_WALLET_SOURCE_24H_LIMIT),
            "{route:?}"
        );
        assert_eq!(src_only.blocked_reasons.len(), 1);

        // A leg not asked about is `null`, not "eligible".
        let one_leg = api
            .route_wallet_eligibility(route, None, Some(dst.clone()))
            .await
            .unwrap();
        assert!(one_leg.source.is_none());
        assert!(!one_leg.eligible);

        // Fresh wallets on the same route are unaffected.
        let fresh = api
            .route_wallet_eligibility(route, Some(other_src), Some(other_dst))
            .await
            .unwrap();
        assert!(fresh.eligible, "{route:?}");
    }
}

#[tokio::test]
async fn route_eligibility_validates_each_address_as_its_own_chains_type() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_every_reserve(dir.path());
    let api = build(&db_path, 0);
    let sol = Pubkey::new_from_array([7; 32]).to_string();
    let evm = crate::evm::address::EvmAddress::from_bytes([0xE7; 20]).to_string();
    let glc = test_glc_address(7);
    use crate::routes::Route;
    // The wrong chain's spelling on either leg is a 400.
    for (route, source, destination) in [
        (Route::GlcToSol, evm.clone(), sol.clone()),
        (Route::GlcToSol, glc.clone(), glc.clone()),
        (Route::SolToGlc, glc.clone(), glc.clone()),
        (Route::SolToGlc, sol.clone(), sol.clone()),
        (Route::RhnToSol, sol.clone(), sol.clone()),
        (Route::RhnToGlc, evm.clone(), evm.clone()),
        (Route::SolToRhn, sol.clone(), glc.clone()),
        (
            Route::GlcToRhn,
            glc.clone(),
            "0x0000000000000000000000000000000000000000".to_string(),
        ),
    ] {
        let err = api
            .route_wallet_eligibility(route, Some(source), Some(destination))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "{route:?}: {err}");
    }
    // The right spellings, with whitespace, are accepted and echoed
    // canonically.
    let out = api
        .route_wallet_eligibility(
            Route::SolToRhn,
            Some(format!(" {sol} ")),
            Some(format!("{evm}\n")),
        )
        .await
        .unwrap();
    assert_eq!(out.source.unwrap().address, sol);
    assert_eq!(out.destination.unwrap().address, evm);
    // A P2SH Goldcoin SOURCE is a wallet a deposit can be funded from;
    // as a DESTINATION only P2PKH is payable.
    let p2sh = crate::goldcoin::address::encode_p2sh(
        &[0x33; 20],
        crate::goldcoin::address::Network::Testnet,
    );
    let out = api
        .route_wallet_eligibility(Route::GlcToSol, Some(p2sh.clone()), None)
        .await
        .unwrap();
    assert_eq!(out.source.unwrap().address, p2sh);
    let err = api
        .route_wallet_eligibility(Route::SolToGlc, None, Some(p2sh))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::BadRequest(_)));
}

#[tokio::test]
async fn route_eligibility_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_every_reserve(dir.path());
    let api = build(&db_path, 0);
    let ((src, _), (dst, _)) = route_wallets(crate::routes::Route::RhnToSol, 7);
    for _ in 0..3 {
        let out = api
            .route_wallet_eligibility(
                crate::routes::Route::RhnToSol,
                Some(src.clone()),
                Some(dst.clone()),
            )
            .await
            .unwrap();
        assert!(out.eligible, "reading consumes nothing");
    }
    let rows: i64 = Ledger::open(&db_path)
        .unwrap()
        .conn_for_tests()
        .query_row("SELECT COUNT(*) FROM bridge_requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn get_route_eligibility_routes_over_http_and_requires_a_wallet() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!(
        "{base}/routes/RhnToSol/eligibility?source=0xe1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1&destination=11111111111111111111111111111111"
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: RouteWalletEligibilityView = resp.json().await.unwrap();
    assert_eq!(body.route, "RhnToSol");
    assert!(body.eligible);
    assert!(body.source.is_some() && body.destination.is_some());

    // One leg is enough.
    let resp = reqwest::get(format!(
        "{base}/routes/GlcToSol/eligibility?destination=11111111111111111111111111111111"
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: RouteWalletEligibilityView = resp.json().await.unwrap();
    assert!(body.source.is_none());

    // No leg at all, or an unknown route, is a 400; a wrong path shape
    // is not this endpoint.
    let resp = reqwest::get(format!("{base}/routes/GlcToSol/eligibility"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let resp = reqwest::get(format!("{base}/routes/glc-to-sol/eligibility?source=x"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let resp = reqwest::get(format!("{base}/routes/GlcToSol/other"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn post_transfers_refuses_a_busy_destination_or_declared_source_with_429_and_reserves_nothing(
) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let (base, _tx) = spawn_real_server(&db_path, 0).await;
    let client = reqwest::Client::new();
    let recipient = Keypair::new().pubkey().to_string();
    let source = test_glc_address(0x41);

    let first = client
        .post(format!("{base}/transfers"))
        .json(&CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: recipient.clone(),
            route: None,
            source_address: Some(format!(" {source} ")),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), reqwest::StatusCode::CREATED);
    let first: CreateTransferOutput = first.json().await.unwrap();
    {
        let ledger = Ledger::open(&db_path).unwrap();
        let row = ledger.get_request(first.request_id).unwrap().unwrap();
        assert_eq!(
            row.source_wallet.as_deref(),
            Some(source.as_bytes()),
            "the declared source is recorded, trimmed"
        );
    }
    let capacity_after_first = client
        .get(format!("{base}/reserve"))
        .send()
        .await
        .unwrap()
        .json::<ReserveAvailability>()
        .await
        .unwrap()
        .solana_available_capacity
        .0;

    // Same destination, no declared source: refused for the destination.
    let dup_destination = client
        .post(format!("{base}/transfers"))
        .json(&CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: recipient.clone(),
            route: None,
            source_address: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(
        dup_destination.status(),
        reqwest::StatusCode::TOO_MANY_REQUESTS
    );
    let body: serde_json::Value = dup_destination.json().await.unwrap();
    assert_eq!(
        body["blocked_reasons"],
        serde_json::json!([BLOCKED_REASON_WALLET_DESTINATION_24H_LIMIT])
    );
    assert!(
        body["retry_after"].as_i64().unwrap() > now_unix() + 86_000,
        "{body}"
    );
    assert!(
        body["error"].as_str().unwrap().contains("24 hours"),
        "{body}"
    );

    // Fresh destination, same declared source: refused for the source.
    let dup_source = client
        .post(format!("{base}/transfers"))
        .json(&CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: Some(source.clone()),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(dup_source.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    let body: serde_json::Value = dup_source.json().await.unwrap();
    assert_eq!(
        body["blocked_reasons"],
        serde_json::json!([BLOCKED_REASON_WALLET_SOURCE_24H_LIMIT])
    );

    // Neither refusal reserved anything or left a row.
    let capacity_now = client
        .get(format!("{base}/reserve"))
        .send()
        .await
        .unwrap()
        .json::<ReserveAvailability>()
        .await
        .unwrap()
        .solana_available_capacity
        .0;
    assert_eq!(capacity_now, capacity_after_first);
    let rows: i64 = Ledger::open(&db_path)
        .unwrap()
        .conn_for_tests()
        .query_row("SELECT COUNT(*) FROM bridge_requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);

    // The pre-check agrees with the refusal, before anything is signed.
    let pre = client
        .get(format!(
            "{base}/routes/GlcToSol/eligibility?source={source}&destination={recipient}"
        ))
        .send()
        .await
        .unwrap()
        .json::<RouteWalletEligibilityView>()
        .await
        .unwrap();
    assert!(!pre.eligible);
    assert_eq!(pre.blocked_reasons.len(), 2);

    // A malformed declared source is a 400, never silently ignored.
    let bad = client
        .post(format!("{base}/transfers"))
        .json(&CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: Some("not-an-address".to_string()),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_transfers_without_a_declared_source_is_source_checked_only_when_the_deposit_lands() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    // Two requests, no declared source: both created. Whichever the busy
    // wallet then funds second is parked by the indexer's observation —
    // covered end to end by `goldcoin::indexer::tests`.
    for _ in 0..2 {
        api.create_goldcoin_deposit_transfer(CreateTransferInput {
            amount_atomic: AtomicU64(500_000),
            recipient: Keypair::new().pubkey().to_string(),
            route: None,
            source_address: None,
        })
        .await
        .unwrap();
    }
    let ledger = Ledger::open(&db_path).unwrap();
    for id in [1, 2] {
        assert_eq!(ledger.get_request(id).unwrap().unwrap().source_wallet, None);
    }
}

// ------------------------------- Solana program pause flags (2026-09-12) --
//
// The Solana custody program's own circuit breakers (`BridgeConfig.paused`
// / `release_paused` / `deposit_paused`) must close the routes whose
// Solana leg the program refuses — on `GET /status`, `GET /chains` and
// `GET /robinhood/reserve` alike. Before 2026-09-12 all three consulted
// only the LOCAL reserve gates, so an on-chain `deposit_paused` (set
// 2026-09-09, never cleared by launch.sh, which only unpauses the local
// Solana row) left `SolToGlc` and `SolToRhn` advertised as available while
// every `deposit_to_reserve` reverted with `DepositDirectionPaused`.

/// `fake_bridge_config_bytes` with the three pause flags set explicitly.
/// Layout: 8 discriminator + 1 protocol_version + 32 admin + 1 pending
/// tag => `paused` at 42, `release_paused` at 43, `deposit_paused` at 44.
fn fake_bridge_config_bytes_with_pause(paused: bool, release: bool, deposit: bool) -> Vec<u8> {
    let mut v = fake_bridge_config_bytes(0, 100, TEST_PER_TRANSFER_LIMIT);
    v[42] = paused as u8;
    v[43] = release as u8;
    v[44] = deposit as u8;
    v
}

/// All six routes open on every SERVICE gate (reserves configured and
/// running, ledger route gates enabled, config flags on, adapter
/// verified), with the program's `bridge_config` bytes supplied by the
/// caller — so the only thing that can close a route is the program.
fn build_all_routes_with_bridge_config(
    db_path: &std::path::Path,
    bridge_config: Vec<u8>,
) -> BridgeApi<FakeSolanaRpc> {
    {
        let mut ledger = Ledger::open(db_path).unwrap();
        for route in [
            crate::routes::Route::SolToRhn,
            crate::routes::Route::RhnToSol,
        ] {
            ledger.set_route_enabled(route, true, None).unwrap();
        }
    }
    let mut fees = test_route_fees();
    fees.insert(crate::routes::Route::SolToRhn, 450).unwrap();
    fees.insert(crate::routes::Route::RhnToSol, 500).unwrap();
    opt_down(BridgeApi::new(
        db_path.to_path_buf(),
        FakeSolanaRpc {
            bridge_config,
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        test_root_vault(),
        crate::goldcoin::address::Network::Testnet,
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::routes::RouteGate::new(
            crate::routes::RoutesConfig::default().with_robinhood(true, true, true, true),
            crate::chains::ChainRegistry::with_verified_robinhood(test_verified_deployment()),
        )),
        fees,
    ))
}

const ALL_ROUTES: [&str; 6] = [
    "GlcToSol", "SolToGlc", "GlcToRhn", "RhnToGlc", "SolToRhn", "RhnToSol",
];

/// Asserts exactly `closed` are unavailable on `/chains` (with the
/// capacity copy, never the route-gate copy) and every other route is
/// available; then that `/status` agrees for the two legacy routes and
/// `/robinhood/reserve`'s `routes` agrees for the four it lists.
async fn assert_program_pause_closes_exactly(api: &BridgeApi<FakeSolanaRpc>, closed: &[&str]) {
    let chains = api.chains().await.unwrap();
    for id in ALL_ROUTES {
        let r = route(&chains, id);
        assert!(
            r.enabled,
            "{id}: the program's pause is not an enablement matter"
        );
        if closed.contains(&id) {
            assert!(
                !r.available,
                "{id} must be unavailable while the program refuses its leg"
            );
            assert_eq!(
                r.unavailable_reason.as_deref(),
                Some(DIRECTION_UNAVAILABLE_MESSAGE),
                "{id}: a program pause is a pause condition and gets the capacity copy"
            );
        } else {
            assert!(
                r.available,
                "{id} must stay available; the program does not gate it"
            );
            assert_eq!(r.unavailable_reason, None);
        }
    }
    let status = api.status().await.unwrap();
    assert_eq!(
        status.glc_to_sol_available,
        route(&chains, "GlcToSol").available,
        "/status and /chains must never disagree about GlcToSol"
    );
    assert_eq!(
        status.sol_to_glc_available,
        route(&chains, "SolToGlc").available,
        "/status and /chains must never disagree about SolToGlc"
    );
    let stats = api.stats().await.unwrap();
    assert_eq!(stats.glc_to_sol_available, status.glc_to_sol_available);
    assert_eq!(stats.sol_to_glc_available, status.sol_to_glc_available);
    let reserve = api.robinhood_reserve().await.unwrap();
    for r in &reserve.routes {
        assert_eq!(
            r.available,
            route(&chains, &r.id).available,
            "/robinhood/reserve and /chains must never disagree about {}",
            r.id
        );
    }
}

#[tokio::test]
async fn all_six_routes_are_available_when_the_program_is_not_paused() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_all_routes_with_bridge_config(
        &db_path,
        fake_bridge_config_bytes_with_pause(false, false, false),
    );
    assert_program_pause_closes_exactly(&api, &[]).await;
}

/// `deposit_paused` guards `deposit_to_reserve`, the SOURCE leg of both
/// Solana-originating routes — not just `SolToGlc`.
#[tokio::test]
async fn onchain_deposit_pause_closes_sol_to_glc_and_sol_to_rhn_only() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_all_routes_with_bridge_config(
        &db_path,
        fake_bridge_config_bytes_with_pause(false, false, true),
    );
    assert_program_pause_closes_exactly(&api, &["SolToGlc", "SolToRhn"]).await;
    // The local layer is untouched and still says "running": the
    // disagreement is exactly what the availability booleans must absorb.
    let status = api.status().await.unwrap();
    assert!(!status.solana_paused);
    assert!(!status.goldcoin_paused);
    assert!(status.sol_to_glc_admission_open);
}

/// `release_paused` guards `release_from_reserve`, the DESTINATION leg of
/// both Solana-bound routes.
#[tokio::test]
async fn onchain_release_pause_closes_glc_to_sol_and_rhn_to_sol_only() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_all_routes_with_bridge_config(
        &db_path,
        fake_bridge_config_bytes_with_pause(false, true, false),
    );
    assert_program_pause_closes_exactly(&api, &["GlcToSol", "RhnToSol"]).await;
}

/// The global flag closes every route with a Solana leg and nothing else.
#[tokio::test]
async fn onchain_global_pause_closes_every_solana_leg_route_only() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_all_routes_with_bridge_config(
        &db_path,
        fake_bridge_config_bytes_with_pause(true, false, false),
    );
    assert_program_pause_closes_exactly(&api, &["GlcToSol", "SolToGlc", "SolToRhn", "RhnToSol"])
        .await;
}

/// An unreadable `bridge_config` fails CLOSED on `/chains` for the four
/// Solana-leg routes — "unknown" never renders as available — while the
/// listing itself still serves and the two Goldcoin<->Robinhood routes,
/// which the program cannot affect, stay open.
#[tokio::test]
async fn unreadable_bridge_config_fails_closed_for_solana_routes_on_chains() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_all_routes_with_bridge_config(&db_path, Vec::new());
    let chains = api.chains().await.unwrap();
    for id in ["GlcToSol", "SolToGlc", "SolToRhn", "RhnToSol"] {
        let r = route(&chains, id);
        assert!(
            !r.available,
            "{id} must fail closed when bridge_config is unreadable"
        );
        assert_eq!(
            r.unavailable_reason.as_deref(),
            Some(DIRECTION_UNAVAILABLE_MESSAGE)
        );
    }
    for id in ["GlcToRhn", "RhnToGlc"] {
        assert!(
            route(&chains, id).available,
            "{id} does not depend on the Solana program"
        );
    }
    let reserve = api.robinhood_reserve().await.unwrap();
    for r in &reserve.routes {
        assert_eq!(r.available, route(&chains, &r.id).available);
    }
}

#[test]
fn solana_program_pause_maps_flags_to_directions_by_program_semantics() {
    use crate::api::SolanaProgramPause;
    let deposit = SolanaProgramPause {
        deposit_paused: true,
        ..Default::default()
    };
    let release = SolanaProgramPause {
        release_paused: true,
        ..Default::default()
    };
    let global = SolanaProgramPause {
        paused: true,
        ..Default::default()
    };
    let none = SolanaProgramPause::default();
    for d in Direction::ALL.iter().copied() {
        let src = d.source_is_solana();
        let dst = d.destination_is_solana();
        assert_eq!(
            deposit.blocks(d),
            src,
            "{d:?}: deposit_paused gates the Solana SOURCE leg"
        );
        assert_eq!(
            release.blocks(d),
            dst,
            "{d:?}: release_paused gates the Solana DESTINATION leg"
        );
        assert_eq!(
            global.blocks(d),
            src || dst,
            "{d:?}: global pause gates any Solana leg"
        );
        assert!(!none.blocks(d));
    }
    assert!(SolanaProgramPause::UNKNOWN.blocks(Direction::GlcToSol));
    assert!(SolanaProgramPause::UNKNOWN.blocks(Direction::SolToRhn));
    assert!(!SolanaProgramPause::UNKNOWN.blocks(Direction::GlcToRhn));
}

// ------------------------------------ availability probed at a real size --
//
// The 2026-09-12 incident: `SolToGlc` reported `available: true` on
// `/status` and `/chains` while every normal 50,000 GLC deposit folded
// straight into `ManualReview` with `liquidity_buffer_low_at_fold`. The
// public verdict was computed at ONE atomic unit (`headroom > buffer`),
// which held, while a real deposit failed `headroom - net >= buffer`.
// These tests pin the fix: `SolToGlc` is evaluated at the program's
// `per_transfer_limit`, names the gate that refused, and publishes the
// capacity figures the rule is stated in — without moving any gate.
//
// Fixture arithmetic, in canonical units: the reserve has 10_000_000 of
// headroom (`configure`), the probe is `TEST_PER_TRANSFER_LIMIT` =
// 50_000 mint units = 5_000_000 canonical, netting to 4_850_000 at the
// 300 bps test rate.

/// The probe every test below evaluates against, derived the same way
/// the API derives it, so a change to either constant is caught here.
const PROBE_GROSS: u64 = TEST_PER_TRANSFER_LIMIT * 100; // 6 -> 8 decimals
const PROBE_NET: i64 = 4_850_000; // 5_000_000 less 3%

#[test]
fn the_probe_fixture_is_what_the_api_will_derive() {
    let gross = amount_conversion::SolanaAtomic(TEST_PER_TRANSFER_LIMIT)
        .to_canonical(TEST_SOLANA_DECIMALS)
        .unwrap();
    assert_eq!(gross.0, PROBE_GROSS);
    let fb =
        amount_conversion::compute_fee_at_bps(gross, amount_conversion::BRIDGE_FEE_BPS).unwrap();
    assert_eq!(fb.net.0 as i64, PROBE_NET);
}

/// Sets the Goldcoin reserve's admission buffer so that a ONE-UNIT
/// probe passes (`headroom > buffer`) while the normal-size probe fails
/// (`headroom - PROBE_NET < buffer`) — the incident's exact shape.
fn make_buffer_bind_on_a_normal_transfer(db_path: &std::path::Path) {
    let mut ledger = Ledger::open(db_path).unwrap();
    // headroom 10_000_000; 10_000_000 - 4_850_000 = 5_150_000 < 6_000_000
    ledger
        .set_admission_liquidity_thresholds(ReserveDirection::GoldcoinReserve, 6_000_000, 7_000_000)
        .unwrap();
}

/// Headroom cannot admit the max transfer under the buffer rule =>
/// `available: false`, on `/chains`, `/status` and `/stats` alike, with
/// the exact reason and the figures that explain it — while the
/// automatic gate itself stays OPEN (nothing here moves a gate).
#[tokio::test]
async fn sol_to_glc_is_unavailable_when_headroom_cannot_admit_the_max_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    make_buffer_bind_on_a_normal_transfer(&db_path);
    let api = build(&db_path, 0);

    let chains = api.chains().await.unwrap();
    let r = route(&chains, "SolToGlc");
    assert!(
        r.enabled,
        "the route is switched on; it is closed for capacity"
    );
    assert!(!r.available);
    assert_eq!(
        r.availability_reason.as_deref(),
        Some("liquidity_buffer_low")
    );
    assert_eq!(
        r.unavailable_reason.as_deref(),
        Some(DIRECTION_UNAVAILABLE_MESSAGE),
        "end-user copy is unchanged and still cause-agnostic"
    );
    // Bound = 10_000_000 - 6_000_000 = 4_000_000 net => 4_123_711 gross
    // at 3% (4_123_711 * 0.97 = 4_000_000, floor-exact), which is below
    // the 5_000_000 probe — the sentence "the reserve admits up to
    // 4.12 GLC, a normal transfer is 5 GLC" stated as two numbers.
    assert_eq!(
        r.capacity,
        Some(RouteCapacityView {
            confirmed_headroom_atomic: AtomicI64(10_000_000),
            liquidity_buffer_atomic: AtomicI64(6_000_000),
            max_admissible_gross_atomic: AtomicU64(4_123_711),
            liquidity_admission_closed: false,
            probe_gross_atomic: AtomicU64(PROBE_GROSS),
        })
    );
    assert!(
        r.capacity.as_ref().unwrap().max_admissible_gross_atomic.0 < PROBE_GROSS,
        "closed exactly because the reserve's bound is below the probe"
    );

    let status = api.status().await.unwrap();
    assert!(!status.sol_to_glc_available);
    assert_eq!(
        status.sol_to_glc_availability_reason.as_deref(),
        Some("liquidity_buffer_low")
    );
    assert_eq!(status.sol_to_glc_capacity, r.capacity);
    // The AUTOMATIC gate is untouched: the hysteresis has not closed
    // (headroom is above the buffer), the operator switch is open, and
    // `sol_to_glc_admission_open` — which reports those two — still
    // says so. Unavailability here is a per-amount verdict, not a gate.
    assert!(status.sol_to_glc_admission_open);
    assert!(status.goldcoin_destination_admission_open);
    assert!(!status.goldcoin_paused);
    {
        let ledger = Ledger::open(&db_path).unwrap();
        assert!(!ledger
            .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap());
    }

    let stats = api.stats().await.unwrap();
    assert!(!stats.sol_to_glc_available);
    assert_eq!(
        stats.sol_to_glc_availability_reason.as_deref(),
        Some("liquidity_buffer_low")
    );
}

/// The regression itself, stated as the contradiction it was: the
/// one-unit evaluator still says "open" for exactly this ledger, and the
/// public verdict must no longer be derived from it.
#[tokio::test]
async fn a_tiny_transfer_probe_does_not_make_sol_to_glc_available() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    make_buffer_bind_on_a_normal_transfer(&db_path);
    {
        let ledger = Ledger::open(&db_path).unwrap();
        assert_eq!(
            ledger.route_admission_blocker(Direction::SolToGlc).unwrap(),
            None,
            "the one-atomic-unit form is satisfied — this is the shape that used to be advertised"
        );
        assert_eq!(
            ledger
                .inbound_admission_gates(Direction::SolToGlc)
                .unwrap()
                .route_blocker_at(PROBE_NET),
            Some(crate::ledger::InboundAdmissionBlocker::LiquidityBufferLow)
        );
    }
    let api = build(&db_path, 0);
    assert!(!route(&api.chains().await.unwrap(), "SolToGlc").available);
    assert!(!api.status().await.unwrap().sol_to_glc_available);
}

/// Sufficient headroom => `available: true`, no reason, and the capacity
/// view says how much: the buffer bound grossed up through the fee and
/// capped at the probe (the program refuses anything larger regardless).
#[tokio::test]
async fn sol_to_glc_is_available_with_sufficient_headroom_and_reports_its_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // 10_000_000 - 4_850_000 = 5_150_000 >= 5_000_000: the max
        // transfer fits, with 150_000 to spare.
        ledger
            .set_admission_liquidity_thresholds(
                ReserveDirection::GoldcoinReserve,
                5_000_000,
                6_000_000,
            )
            .unwrap();
    }
    let api = build(&db_path, 0);

    let chains = api.chains().await.unwrap();
    let r = route(&chains, "SolToGlc");
    assert!(r.available);
    assert_eq!(r.availability_reason, None);
    assert_eq!(r.unavailable_reason, None);
    let cap = r
        .capacity
        .clone()
        .expect("SolToGlc always carries its capacity view");
    assert_eq!(cap.confirmed_headroom_atomic, AtomicI64(10_000_000));
    assert_eq!(cap.liquidity_buffer_atomic, AtomicI64(5_000_000));
    assert!(!cap.liquidity_admission_closed);
    assert_eq!(cap.probe_gross_atomic, AtomicU64(PROBE_GROSS));
    // max net under the buffer = 5_000_000; grossed up at 3% that is
    // 5_154_639 — at or above the probe, which is exactly what `available`
    // says. Not capped at the program's limit: the two are comparable.
    assert_eq!(cap.max_admissible_gross_atomic, AtomicU64(5_154_639));
    assert!(cap.max_admissible_gross_atomic.0 >= PROBE_GROSS);

    let status = api.status().await.unwrap();
    assert!(status.sol_to_glc_available);
    assert_eq!(status.sol_to_glc_availability_reason, None);
    assert_eq!(status.sol_to_glc_capacity, Some(cap));
    assert!(api.stats().await.unwrap().sol_to_glc_available);
}

/// `available` and `max_admissible_gross_atomic` are one statement made
/// twice: the route is open exactly when the reserve's grossed-up bound
/// reaches the probe. Swept across buffers on both sides of the boundary
/// so the identity is proved against the real endpoint, not assumed.
#[tokio::test]
async fn available_is_exactly_max_admissible_gross_reaching_the_probe() {
    // headroom 10_000_000; probe net 4_850_000 fits iff buffer <= 5_150_000.
    for buffer in [
        0u64, 1_000_000, 5_149_999, 5_150_000, 5_150_001, 6_000_000, 9_999_999,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = configure(dir.path());
        {
            let mut ledger = Ledger::open(&db_path).unwrap();
            ledger
                .set_admission_liquidity_thresholds(
                    ReserveDirection::GoldcoinReserve,
                    buffer,
                    buffer,
                )
                .unwrap();
        }
        let api = build(&db_path, 0);
        let r = route(&api.chains().await.unwrap(), "SolToGlc").clone();
        let cap = r.capacity.expect("always present for SolToGlc");
        assert_eq!(
            r.available,
            buffer <= 5_150_000,
            "buffer {buffer}: the buffer rule decides availability"
        );
        assert_eq!(
            r.available,
            cap.max_admissible_gross_atomic.0 >= PROBE_GROSS,
            "buffer {buffer}: available iff the reserve's bound reaches the probe \
             (bound {}, probe {PROBE_GROSS})",
            cap.max_admissible_gross_atomic.0
        );
        assert_eq!(cap.liquidity_buffer_atomic, AtomicI64(buffer as i64));
        assert_eq!(cap.confirmed_headroom_atomic, AtomicI64(10_000_000));
        // The bound nets to no more than the rule allows, and one more
        // gross unit would exceed it — the gross-up is exact.
        let bound_net = 10_000_000u64.saturating_sub(buffer);
        let net = |g: u64| {
            amount_conversion::compute_fee_at_bps(
                amount_conversion::CanonicalAtomic(g),
                amount_conversion::BRIDGE_FEE_BPS,
            )
            .unwrap()
            .net
            .0
        };
        assert!(net(cap.max_admissible_gross_atomic.0) <= bound_net);
        assert!(net(cap.max_admissible_gross_atomic.0 + 1) > bound_net);
    }
}

/// Closing `SolToGlc` for capacity at its normal size touches NOTHING
/// else: the other five routes keep the verdict they had, including
/// `RhnToGlc`, which draws on the same Goldcoin reserve but carries no
/// probe and so is still evaluated at the weakest form.
#[tokio::test]
async fn the_probe_closes_sol_to_glc_alone_and_leaves_the_other_five_routes_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure_with_robinhood_reserve(dir.path());
    let api = build_all_routes_with_bridge_config(
        &db_path,
        fake_bridge_config_bytes_with_pause(false, false, false),
    );
    let before = api.chains().await.unwrap();
    for id in ALL_ROUTES {
        assert!(
            route(&before, id).available,
            "{id} is open on a healthy deployment"
        );
    }

    make_buffer_bind_on_a_normal_transfer(&db_path);
    let after = api.chains().await.unwrap();
    for id in ALL_ROUTES {
        let r = route(&after, id);
        if id == "SolToGlc" {
            assert!(!r.available);
            assert_eq!(
                r.availability_reason.as_deref(),
                Some("liquidity_buffer_low")
            );
            assert!(r.capacity.is_some());
        } else {
            assert!(r.available, "{id} must be unaffected by SolToGlc's probe");
            assert_eq!(r.availability_reason, None, "{id}");
            assert_eq!(r.unavailable_reason, None, "{id}");
            assert_eq!(
                r.capacity, None,
                "{id} carries no probe and so no capacity view"
            );
        }
    }
    // `/status`'s other legacy direction and `/robinhood/reserve`'s four
    // routes agree with `/chains`.
    let status = api.status().await.unwrap();
    assert!(status.glc_to_sol_available);
    assert!(!status.sol_to_glc_available);
    let rh = api.robinhood_reserve().await.unwrap();
    for r in &rh.routes {
        assert!(r.available, "{} on /robinhood/reserve", r.id);
        assert_eq!(r.capacity, None, "{}", r.id);
    }
}

/// Every gate names itself: the exact `availability_reason` for each
/// way `SolToGlc` can be closed, on `/chains` and `/status` alike.
#[tokio::test]
async fn sol_to_glc_availability_reason_names_the_exact_gate() {
    async fn reason_for(
        setup: impl FnOnce(&mut Ledger),
    ) -> (Option<String>, Option<String>, bool, bool) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = configure(dir.path());
        {
            let mut ledger = Ledger::open(&db_path).unwrap();
            setup(&mut ledger);
        }
        let api = build(&db_path, 0);
        let chains = api.chains().await.unwrap();
        let status = api.status().await.unwrap();
        let r = route(&chains, "SolToGlc");
        (
            r.availability_reason.clone(),
            status.sol_to_glc_availability_reason.clone(),
            r.available,
            status.sol_to_glc_available,
        )
    }

    // Open: no reason anywhere.
    assert_eq!(reason_for(|_| {}).await, (None, None, true, true));

    type Setup = Box<dyn FnOnce(&mut Ledger)>;
    let cases: [(&str, Setup); 5] = [
        (
            "liquidity_buffer_low",
            Box::new(|l| {
                l.set_admission_liquidity_thresholds(
                    ReserveDirection::GoldcoinReserve,
                    6_000_000,
                    7_000_000,
                )
                .unwrap()
            }),
        ),
        (
            "route_admission_closed",
            Box::new(|l| {
                l.set_route_admission(crate::routes::Route::SolToGlc, true, Some("t"))
                    .unwrap()
            }),
        ),
        (
            "reserve_admission_closed",
            Box::new(|l| {
                l.set_admission(ReserveDirection::GoldcoinReserve, true, Some("t"))
                    .unwrap()
            }),
        ),
        (
            "reserve_paused",
            Box::new(|l| {
                l.set_paused(ReserveDirection::GoldcoinReserve, true, Some("t"))
                    .unwrap()
            }),
        ),
        (
            "insufficient_capacity",
            Box::new(|l| {
                // No buffer, and headroom below the probe's net: raise
                // the protected minimum (`configure_reserve` upserts the
                // thresholds) so 10_000_000 - 6_000_000 < 4_850_000.
                l.set_admission_liquidity_thresholds(ReserveDirection::GoldcoinReserve, 0, 0)
                    .unwrap();
                l.configure_reserve(
                    ReserveDirection::GoldcoinReserve,
                    10_000_000,
                    6_000_000,
                    8_000_000,
                    7_000_000,
                    6_000_001,
                    0,
                )
                .unwrap();
            }),
        ),
    ];
    for (expected, setup) in cases {
        let (chains_reason, status_reason, chains_open, status_open) = reason_for(setup).await;
        assert_eq!(
            chains_reason.as_deref(),
            Some(expected),
            "/chains for {expected}"
        );
        assert_eq!(
            status_reason.as_deref(),
            Some(expected),
            "/status for {expected}"
        );
        assert!(!chains_open && !status_open, "{expected} closes the route");
    }
}

/// `/status` applies one gate `/chains` does not — the program's
/// rolling-24h-volume window — and names it when it is the only one.
#[tokio::test]
async fn status_names_quota_exhaustion_when_it_is_the_only_blocker() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    // deposit/SolToGlc window fully used against a 2_000_000 limit.
    let api = build_with_rolling_volume(&db_path, 2_000_000, 0, 2_000_000);
    let status = api.status().await.unwrap();
    assert!(status.sol_to_glc_quota_exhausted);
    assert!(!status.sol_to_glc_available);
    assert_eq!(
        status.sol_to_glc_availability_reason.as_deref(),
        Some(AVAILABILITY_REASON_QUOTA_EXHAUSTED)
    );
    // Not a reserve condition: `/chains` (which has no quota gate) is open.
    assert!(route(&api.chains().await.unwrap(), "SolToGlc").available);
}

/// A probe that cannot be built fails CLOSED with its own reason — never
/// a silent fall-back to the one-unit form that caused the incident —
/// and closes only `SolToGlc`.
#[tokio::test]
async fn an_unbuildable_probe_fails_sol_to_glc_closed_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    // Point `reserve_token_mint` at an account the fake RPC does not
    // serve, so the mint's decimals — and therefore the probe — are
    // unreadable. Offset: 8 (discriminator) + 1 + 32 + 1 + 1 + 1 + 1 + 1.
    let mut bridge_config = fake_bridge_config_bytes(0, 100, TEST_PER_TRANSFER_LIMIT);
    bridge_config[47..79].copy_from_slice(&[8u8; 32]);
    let api = opt_down(BridgeApi::new(
        db_path.to_path_buf(),
        FakeSolanaRpc {
            bridge_config,
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        test_root_vault(),
        crate::goldcoin::address::Network::Testnet,
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::routes::RouteGate::legacy_only()),
        test_route_fees(),
    ));
    let chains = api.chains().await.unwrap();
    let r = route(&chains, "SolToGlc");
    assert!(!r.available);
    assert_eq!(
        r.availability_reason.as_deref(),
        Some(AVAILABILITY_REASON_PROBE_UNAVAILABLE)
    );
    assert_eq!(r.capacity, None);
    assert!(
        route(&chains, "GlcToSol").available,
        "GlcToSol needs no probe"
    );
    let status = api.status().await.unwrap();
    assert!(!status.sol_to_glc_available);
    assert_eq!(
        status.sol_to_glc_availability_reason.as_deref(),
        Some(AVAILABILITY_REASON_PROBE_UNAVAILABLE)
    );
    assert_eq!(status.sol_to_glc_capacity, None);
}

/// Wire compatibility: the new fields are additive. A payload from a
/// daemon without them still deserializes, and the new fields serialize
/// under the exact names the fix names.
#[test]
fn capacity_and_reason_fields_are_additive_on_the_wire() {
    let old = serde_json::json!({
        "id": "SolToGlc", "source_chain": "solana", "destination_chain": "goldcoin",
        "enabled": true, "disabled_reason": null, "implemented": true,
        "available": true, "unavailable_reason": null, "min_transfer_atomic": "10000000000"
    });
    let v: RouteView = serde_json::from_value(old).unwrap();
    assert_eq!(v.availability_reason, None);
    assert_eq!(v.capacity, None);

    let cap = RouteCapacityView {
        confirmed_headroom_atomic: AtomicI64(28_025_210_893_041),
        liquidity_buffer_atomic: AtomicI64(25_000_000_000_000),
        max_admissible_gross_atomic: AtomicU64(0),
        liquidity_admission_closed: false,
        probe_gross_atomic: AtomicU64(5_000_000_000_000),
    };
    let json = serde_json::to_value(&cap).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "confirmed_headroom_atomic": "28025210893041",
            "liquidity_buffer_atomic": "25000000000000",
            "max_admissible_gross_atomic": "0",
            "liquidity_admission_closed": false,
            "probe_gross_atomic": "5000000000000"
        })
    );
}
