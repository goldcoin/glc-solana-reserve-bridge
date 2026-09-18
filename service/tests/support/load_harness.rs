//! Load/soak test harness for sustained bidirectional bridge traffic
//! (docs/22-production-readiness-review.md P1-3: "a load-generation
//! harness (can reuse the existing real-node test infrastructure, driven
//! for hours instead of seconds) plus explicit accounting-drift
//! assertions across the run").
//!
//! This is deliberately built on the exact same real-node infrastructure
//! and real code paths as `tests/regtest_acceptance.rs`
//! ([`super::RegtestNode`], [`super::LocalValidator`],
//! [`crate::ledger::Ledger`], [`crate::orchestrator::Orchestrator`]) — no
//! new bridge states, no new APIs, no new settlement semantics. What this
//! module adds on top of that file is purely test-harness plumbing:
//! configurable, seeded workload generation and end-of-run invariant
//! checks, driven for a configurable duration/volume instead of one or
//! two requests.
//!
//! # What "concurrency" means here
//!
//! Many requests can be simultaneously in flight through the bridge's own
//! state machine (reserved-but-not-yet-settled) — that is what
//! `LoadProfile::concurrency` bounds, and it is exactly the kind of
//! concurrent capacity/reservation pressure P1-3 calls out. Raw
//! *submission* to the two chains is issued from a single sequential
//! driver loop, not fanned out across OS threads: `deposit_to_reserve`
//! requires reading the live `BridgeConfig.obligation_count` and using it
//! in the same transaction (a stale read fails closed by construction —
//! see `solana::instructions::deposit_to_reserve`'s own docs), and the
//! regtest node's wallet is a single shared resource. Serializing
//! submission is both simpler and realistic: a real relayer's own request
//! intake is a single pipeline too, even though many settlements progress
//! concurrently behind it.
//!
//! # Determinism
//!
//! Which direction and what amount each issued request uses is
//! deterministic given [`LoadProfile::seed`] (a small seeded splitmix64
//! generator, not `rand`, so the exact sequence is reviewable and stable
//! across Rust/crate versions). Real wall-clock timing — exact
//! confirmation latency, exact tick interleaving with real `goldcoind`/
//! `solana-test-validator` subprocess scheduling — is NOT bit-reproducible
//! between runs, because it depends on real subprocess and OS scheduling.
//! "Deterministic" here means the intended workload is reproducible, not
//! that two runs produce byte-identical timelines.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use solana_client::rpc_client::RpcClient as BlockingRpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;

use glc_reserve_bridge_service::amount_conversion::{self, CanonicalAtomic, SolanaAtomic};
use glc_reserve_bridge_service::goldcoin::address::Network;
use glc_reserve_bridge_service::goldcoin::indexer::{Indexer, IndexerConfig};
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{
    CreateRequestOutcome, Direction, Ledger, RequestAmounts, RequestState, ReserveDirection,
};
use glc_reserve_bridge_service::orchestrator::{Orchestrator, OrchestratorConfig};
use glc_reserve_bridge_service::signing::attestation::DevAttestationSigner;
use glc_reserve_bridge_service::signing::goldcoin_vault::DevVaultSigner;
use glc_reserve_bridge_service::signing::signers::{AttestationSigner, VaultSigner};
use glc_reserve_bridge_service::solana::accounts;
use glc_reserve_bridge_service::solana::instructions;

use super::{LocalValidator, RegtestNode};

/// Matches `tests/regtest_acceptance.rs`'s own constant — the canonical
/// Solana GLC mint's real, verified decimals (docs/18-token-2022-
/// support.md), deliberately different from Goldcoin's own 8 so this
/// harness exercises the real cross-chain conversion, not a degenerate
/// same-decimals case.
const SOLANA_GLC_DECIMALS: u8 = 6;

/// Fixed test-only safety margin, in GLC, added on top of a profile's
/// exact `initial_goldcoin_reserve` requirement when deciding how much
/// matured regtest balance to provision before the vault-funding
/// `sendtoaddress` call — covers that transaction's own network fee, not
/// bridge/reserve economics.
const FUNDING_FEE_HEADROOM_GLC: f64 = 100.0;

/// How many blocks to mine per maturation-check iteration while waiting
/// for enough regtest coinbase outputs to mature. Purely a batching
/// choice for the bootstrap loop below (fewer RPC round-trips); does not
/// affect how much is ultimately mined.
const FUNDING_MATURATION_BATCH_BLOCKS: u32 = 5;

/// Safety cap on how many additional blocks the funding-maturation loop
/// will mine before giving up with a clear panic, rather than looping
/// forever if a profile's reserve requirement is unreachable for some
/// other reason (e.g. a misconfigured regtest node).
const MAX_FUNDING_MATURATION_BLOCKS: u32 = 2_000;

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// `atomic` is Goldcoin-native/canonical (8 decimals) — the same unit
/// `RegtestNode::send_deposit_with_binding`'s `amount_glc` parameter
/// expects as a decimal GLC amount.
fn atomic_to_glc(atomic: u64) -> f64 {
    atomic as f64 / 100_000_000.0
}

/// The regtest wallet's own reported spendable (matured) balance, in
/// GLC — used only to decide how many additional blocks the funding
/// bootstrap needs to mine, never fed into any bridge/reserve accounting
/// path.
fn regtest_spendable_balance_glc(goldcoin: &RegtestNode) -> f64 {
    goldcoin
        .cli(&["getbalance"])
        .parse()
        .expect("goldcoin-cli getbalance returned a non-numeric value")
}

/// The exact same read reconciliation itself performs
/// (`Orchestrator::tick_goldcoin_reconciliation`): `listunspent
/// min_confirmations 9999999 [vault_address]`, filtered to `solvable`
/// entries, summed to atomic units. Used only to decide when it is safe
/// to start the orchestrator's tick loop — never fed into any bridge
/// accounting path itself.
fn goldcoin_vault_solvable_balance_atomic(
    goldcoin: &RegtestNode,
    vault_address: &str,
    min_confirmations: i64,
) -> u64 {
    let raw = goldcoin.cli(&[
        "listunspent",
        &min_confirmations.to_string(),
        "9999999",
        &format!("[\"{vault_address}\"]"),
    ]);
    let entries: Vec<serde_json::Value> =
        serde_json::from_str(&raw).expect("listunspent returned non-JSON output");
    entries
        .iter()
        .filter(|e| e["solvable"].as_bool().unwrap_or(false))
        .map(|e| {
            let glc = e["amount"]
                .as_f64()
                .expect("listunspent entry missing amount");
            (glc * 100_000_000.0).round() as u64
        })
        .sum()
}

/// Waits until the vault's own `listunspent`-solvable balance — read the
/// exact same way `Orchestrator::tick_goldcoin_reconciliation` reads it —
/// actually reaches `expected_atomic`, up to a bounded number of attempts.
/// See the call site's doc comment for why this matters: without it, a
/// real, once-observed cold-start race is possible between the vault
/// funding transaction being mined and the node's own `importaddress`-
/// triggered wallet rescan actually catching up to make the funded UTXO
/// visible/solvable, during which reconciliation's live read can
/// legitimately under-report the true vault balance and misclassify the
/// gap as an unexplained breach.
fn wait_for_goldcoin_vault_funded(
    goldcoin: &RegtestNode,
    vault_address: &str,
    expected_atomic: u64,
    min_confirmations: i64,
) {
    for _ in 0..200 {
        if goldcoin_vault_solvable_balance_atomic(goldcoin, vault_address, min_confirmations)
            >= expected_atomic
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!(
        "goldcoin vault {vault_address} never reached a solvable, {min_confirmations}-confirmation \
         balance of {expected_atomic} atomic units — reconciliation would misclassify this as a \
         breach if the orchestrator started now"
    );
}

/// Mirrors `regtest_acceptance.rs`'s `glc_to_sol_amounts` exactly — the
/// real gross/fee/net breakdown `api::create_goldcoin_deposit_transfer` itself
/// computes (docs/20-bridge-fee.md), not a harness-invented shortcut.
///
/// Returns `None` when the fee-adjusted net amount is not exactly
/// representable at the destination mint's (coarser) decimals — a real,
/// legitimate input-validation rejection (`ConversionError::
/// NotExactlyRepresentable`), not a harness bug: a randomly chosen gross
/// amount can genuinely land on a value whose post-fee net has leftover
/// precision the 6-decimal mint cannot express. A real API layer would
/// reject this before ever creating a request; the harness does the same
/// — see the call site's `requests_rejected_at_creation` handling.
fn glc_to_sol_amounts(gross: u64, solana_decimals: u8) -> Option<RequestAmounts> {
    let fb = amount_conversion::compute_fee(CanonicalAtomic(gross)).unwrap();
    let net_destination = fb.net.to_solana(solana_decimals).ok()?;
    Some(RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: net_destination.0,
        quote: None,
    })
}

fn box_vault_signers(signers: Vec<DevVaultSigner>) -> Vec<Box<dyn VaultSigner>> {
    signers
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn VaultSigner>)
        .collect()
}

fn box_attestation_signers(signers: Vec<DevAttestationSigner>) -> Vec<Box<dyn AttestationSigner>> {
    signers
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn AttestationSigner>)
        .collect()
}

fn three_vault_signers() -> (MultisigVault, Vec<DevVaultSigner>) {
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
    (vault, signers)
}

fn three_attestation_signers() -> (Vec<Pubkey>, Vec<DevAttestationSigner>) {
    let signers = vec![
        DevAttestationSigner::generate(),
        DevAttestationSigner::generate(),
        DevAttestationSigner::generate(),
    ];
    let pubkeys = signers.iter().map(|s| s.pubkey()).collect();
    (pubkeys, signers)
}

fn base_orchestrator_config() -> OrchestratorConfig {
    OrchestratorConfig {
        attestation_threshold: 2,
        vault_threshold: 2,
        required_goldcoin_confirmations: 3,
        // Matches regtest_acceptance.rs's own comment: this real regtest
        // node's relay policy rejects a too-low fee rate as insufficient
        // priority for freshly generated (low-priority) inputs.
        fee_rate_per_kb: 100_000,
        dust_threshold: 1_000,
        max_inputs: 10,
        change_fanout_target_atomic: 2_500 * 100_000_000,
        change_fanout_max_outputs: 10,
        zero_conf_change_max_depth: 0,
        zero_conf_change_mode:
            glc_reserve_bridge_service::goldcoin::payout::ZeroConfChangeMode::DepthLimited,
        zero_conf_change_recursive_chain_limit: 20,
        reconciliation_tolerance: 0,
        vault_min_confirmations: 1,
        goldcoin_network: Network::Testnet,
        signer_timeout: Duration::from_secs(5),
        max_auto_resumes_per_tick: 20,
        // Shaping disabled here: this harness pins pre-existing flows;
        // goldcoin::liquidity has its own dedicated coverage
        // (service/tests/utxo_liquidity_autoshaping.rs).
        utxo_shaping_enabled: false,
        utxo_shaping_target_available_count: 15,
        utxo_shaping_min_source_atomic: 4 * 2_500 * 100_000_000,
        utxo_shaping_max_outputs_per_split: 25,
    }
}

/// A minimal seeded PRNG (splitmix64) — deliberately not the `rand` crate,
/// so the exact workload sequence for a given seed is reviewable in this
/// file and stable regardless of crate/version drift. Only ever used for
/// harness-level workload decisions (which direction, what amount to
/// generate next), never anything that touches bridge/settlement logic.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn gen_range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u64() % (hi - lo + 1)
    }

    /// Like [`Rng::gen_range`], but only ever returns multiples of `step`
    /// (rounding the bounds inward first) — see
    /// [`glc_to_sol_representable_step`] for why GlcToSol amounts need
    /// this.
    fn gen_range_step(&mut self, lo: u64, hi: u64, step: u64) -> u64 {
        let lo_units = lo.div_ceil(step);
        let hi_units = hi / step;
        self.gen_range(lo_units, hi_units.max(lo_units)) * step
    }

    fn choose_direction(&mut self, glc_to_sol_weight: u32, sol_to_glc_weight: u32) -> Direction {
        let total = u64::from(glc_to_sol_weight.max(1).saturating_add(sol_to_glc_weight));
        let r = self.next_u64() % total.max(1);
        if r < u64::from(glc_to_sol_weight.max(1)) {
            Direction::GlcToSol
        } else {
            Direction::SolToGlc
        }
    }
}

/// Configuration for one load/soak run. All fields are `pub` and every
/// constructor below is a starting point, not a fixed choice — override
/// whatever the caller needs (e.g. `LoadProfile { concurrency: 16,
/// ..LoadProfile::soak(Duration::from_secs(4 * 3600)) }`).
#[derive(Debug, Clone)]
pub struct LoadProfile {
    pub name: &'static str,
    /// Hard wall-clock cap on the workload-generation phase. The run may
    /// end earlier if `target_requests` is reached first.
    pub duration: Duration,
    /// Stop generating new requests once this many have been issued
    /// (summed across both directions), even if `duration` has not
    /// elapsed. `None` means duration-bound only — the natural choice for
    /// a genuine soak run.
    pub target_requests: Option<u32>,
    /// Maximum number of requests simultaneously in a non-terminal
    /// (reservation-active) state before the generator pauses issuing new
    /// ones — see the module docs on what "concurrency" means here.
    pub concurrency: usize,
    /// Minimum spacing between successive new-request submissions (a
    /// simple token-bucket-free rate cap, not average-rate pacing).
    pub pacing_interval: Duration,
    /// How often `Orchestrator::tick` runs.
    pub tick_interval: Duration,
    /// Mine one Goldcoin regtest block every N ticks — the only way
    /// confirmations and payout-broadcast finality actually advance on a
    /// regtest node (nothing mines on its own without `-regtest`
    /// autogeneration, which this harness deliberately does not enable,
    /// to keep block timing under the harness's own control).
    pub mine_every_n_ticks: u32,
    /// After generation stops (duration/volume reached), keep ticking and
    /// mining for up to this long, waiting for every issued request to
    /// reach a terminal state, before giving up and reporting whatever is
    /// still active as stuck.
    pub drain_timeout: Duration,
    pub seed: u64,
    /// Relative weight of GlcToSol vs SolToGlc requests (need not sum to
    /// any particular total — only the ratio matters).
    pub glc_to_sol_weight: u32,
    pub sol_to_glc_weight: u32,
    /// Goldcoin-native/canonical (8-decimal) atomic gross amount range for
    /// GlcToSol requests.
    pub glc_to_sol_amount_range: (u64, u64),
    /// Solana-native (mint decimals, 6 here) atomic amount range for
    /// SolToGlc requests.
    pub sol_to_glc_amount_range: (u64, u64),
    /// Starting `SolanaReserve.total_reserve_balance`, in the reserve
    /// mint's own atomic units. Must comfortably exceed the total possible
    /// settled GlcToSol volume for the run (see the two `sized_for`
    /// constructors) or requests will legitimately start being rejected
    /// with `InsufficientLiquidity` partway through — which is realistic
    /// behavior, not a harness bug, but defeats the point of a soak run
    /// meant to exercise sustained *successful* traffic.
    pub initial_solana_reserve: u64,
    /// Starting `GoldcoinReserve.total_reserve_balance`, in Goldcoin-native
    /// atomic units (8 decimals). Same sizing caveat as
    /// `initial_solana_reserve`.
    pub initial_goldcoin_reserve: u64,
}

const RESERVATION_TTL_SECS: i64 = 3_600;

/// The GlcToSol gross-amount granularity that guarantees the fee-adjusted
/// net amount is exactly representable at the destination mint's coarser
/// decimals (`amount_conversion::ConversionError::NotExactlyRepresentable`
/// otherwise — a real, correct fail-closed rejection, not a bug). With the
/// fixed 3% bridge fee (`BRIDGE_FEE_BPS = 300`) and an 8-decimal ->
/// 6-decimal narrowing (scale 100), `gross` a multiple of 10,000 is
/// necessary and sufficient for `net` to itself be a multiple of 100
/// (net = 97 * gross / 100 with 97 coprime to 100 — worked out directly,
/// not a magic number; see docs/24-load-soak-harness.md's design-notes
/// section for the derivation if this ever needs re-deriving for a
/// different fee rate or decimals pair). Real callers hit the
/// exact same constraint; a real UI/API layer would need to round a
/// user's requested amount the same way before ever creating a request.
const GLC_TO_SOL_REPRESENTABLE_STEP: u64 = 10_000;

impl LoadProfile {
    /// A short, deterministic profile sized for CI/local verification:
    /// bounded by both a short duration AND a small request count,
    /// whichever comes first, so it finishes in well under a minute of
    /// real regtest/validator time.
    pub fn smoke() -> Self {
        LoadProfile {
            name: "smoke",
            duration: Duration::from_secs(45),
            target_requests: Some(10),
            concurrency: 4,
            pacing_interval: Duration::from_millis(400),
            tick_interval: Duration::from_millis(250),
            mine_every_n_ticks: 2,
            drain_timeout: Duration::from_secs(60),
            seed: 20260815,
            glc_to_sol_weight: 1,
            sol_to_glc_weight: 1,
            glc_to_sol_amount_range: (50_000_000, 300_000_000), // 0.5 - 3.0 GLC
            sol_to_glc_amount_range: (500_000, 2_000_000),      // 0.5 - 2.0 tokens
            initial_solana_reserve: 2_000_000_000,              // 2000 tokens
            initial_goldcoin_reserve: 200_000_000_000,          // 2000 GLC
        }
    }

    /// A configurable sustained profile intended for a later, real
    /// multi-hour execution — duration-bound only (no `target_requests`
    /// cap), wider amount ranges, and reserve sizing generous relative to
    /// a multi-hour run at the default pacing. Override `duration`,
    /// `concurrency`, `pacing_interval`, and the reserve sizes for the
    /// actual rehearsal being run; the defaults here are a reasonable
    /// starting point, not a prescription.
    pub fn soak(duration: Duration) -> Self {
        LoadProfile {
            name: "soak",
            duration,
            target_requests: None,
            concurrency: 12,
            pacing_interval: Duration::from_secs(3),
            tick_interval: Duration::from_millis(500),
            mine_every_n_ticks: 4,
            drain_timeout: Duration::from_secs(180),
            seed: 20260815,
            glc_to_sol_weight: 1,
            sol_to_glc_weight: 1,
            glc_to_sol_amount_range: (10_000_000, 500_000_000), // 0.1 - 5.0 GLC
            sol_to_glc_amount_range: (100_000, 5_000_000),      // 0.1 - 5.0 tokens
            // Sized for roughly duration / pacing_interval requests at the
            // top of each amount range, times a safety factor — see the
            // module-level doc comment on reserve sizing. Override for a
            // workload shaped differently from these defaults.
            initial_solana_reserve: {
                let max_requests = (duration.as_secs() / 3).max(1);
                max_requests.saturating_mul(5_000_000).saturating_mul(3)
            },
            initial_goldcoin_reserve: {
                let max_requests = (duration.as_secs() / 3).max(1);
                max_requests.saturating_mul(500_000_000).saturating_mul(3)
            },
        }
    }
}

/// A request this harness issued, tracked for end-of-run reporting.
#[derive(Debug, Clone)]
struct IssuedRequest {
    direction: Direction,
    issued_at: Instant,
    /// Canonical (8-decimal) gross amount — for GlcToSol this is exactly
    /// what was submitted; for SolToGlc it is the Solana-native amount
    /// converted to canonical, matching how `solana::indexer::fold_sol_deposit`
    /// itself computes it.
    gross_canonical: u64,
}

#[derive(Debug, Clone)]
pub struct StuckRequest {
    pub id: i64,
    pub direction: Direction,
    pub state: RequestState,
    pub age_seconds: u64,
}

/// End-of-run results. See the module docs and `docs/24-load-soak-
/// harness.md` for how to interpret these.
#[derive(Debug)]
pub struct LoadRunReport {
    pub profile_name: &'static str,
    pub wall_clock: Duration,
    pub requests_issued: HashMap<Direction, u32>,
    pub requests_rejected_at_creation: u32,
    pub sol_to_glc_submission_failures: u32,
    pub sol_to_glc_submission_failure_samples: Vec<String>,
    /// SolToGlc deposits that submitted successfully on-chain but never
    /// got folded into a `bridge_requests` row within the run's drain
    /// window — distinct from `stuck_requests` (which requires a request
    /// id to exist at all).
    pub unresolved_sol_to_glc_submissions: usize,
    pub final_state_counts: HashMap<Direction, Vec<(RequestState, i64)>>,
    pub stuck_requests: Vec<StuckRequest>,
    pub expected_fee_atomic: HashMap<ReserveDirection, u64>,
    pub observed_accrued_fee_atomic: HashMap<ReserveDirection, u64>,
    pub settled_liquidity: HashMap<ReserveDirection, u64>,
    pub settled_sum_from_rows: HashMap<ReserveDirection, u64>,
    pub invariant_ok: HashMap<ReserveDirection, bool>,
    pub duplicate_binding_check_ok: bool,
    pub replay_idempotency_ok: bool,
    pub replay_idempotency_detail: String,
    pub tick_errors: Vec<String>,
    pub reconciliation_breaches: u32,
    pub reconciliation_ticks: u32,
}

impl LoadRunReport {
    /// True when every invariant this harness checks held for the whole
    /// run. Does NOT require zero stuck requests by itself — a handful of
    /// requests still in flight at the drain timeout on a short smoke
    /// profile is expected (see `docs/24-load-soak-harness.md`'s
    /// success-criteria section); callers that care about zero-stuck
    /// should check `self.stuck_requests.is_empty()` explicitly in
    /// addition to this.
    pub fn accounting_healthy(&self) -> bool {
        self.invariant_ok.values().all(|ok| *ok)
            && self.duplicate_binding_check_ok
            && self.replay_idempotency_ok
            && self.unresolved_sol_to_glc_submissions == 0
            && self
                .expected_fee_atomic
                .iter()
                .all(|(d, expected)| self.observed_accrued_fee_atomic.get(d) == Some(expected))
    }

    pub fn summary(&self) -> String {
        format!(
            "profile={} wall_clock={:.1}s issued={:?} rejected_at_creation={} \
             sol_to_glc_submission_failures={} sol_to_glc_submission_failure_samples={:?} \
             unresolved_sol_to_glc_submissions={} stuck={} \
             accounting_healthy={} invariant_ok={:?} duplicate_binding_check_ok={} \
             expected_fee_atomic={:?} observed_accrued_fee_atomic={:?} \
             replay_idempotency_ok={} ({}) reconciliation_breaches={}/{} \
             tick_errors={}",
            self.profile_name,
            self.wall_clock.as_secs_f64(),
            self.requests_issued,
            self.requests_rejected_at_creation,
            self.sol_to_glc_submission_failures,
            self.sol_to_glc_submission_failure_samples,
            self.unresolved_sol_to_glc_submissions,
            self.stuck_requests.len(),
            self.accounting_healthy(),
            self.invariant_ok,
            self.duplicate_binding_check_ok,
            self.expected_fee_atomic,
            self.observed_accrued_fee_atomic,
            self.replay_idempotency_ok,
            self.replay_idempotency_detail,
            self.reconciliation_breaches,
            self.reconciliation_ticks,
            self.tick_errors.len(),
        )
    }
}

/// Runs one full load/soak profile against fresh, throwaway regtest
/// Goldcoin + local Solana validator infrastructure (same shape as
/// `tests/regtest_acceptance.rs`'s own bootstrap), then returns a full
/// report. Everything is torn down (nodes killed, temp dirs removed) when
/// this function returns, via `RegtestNode`/`LocalValidator`'s `Drop`.
pub async fn run_load_profile(
    goldcoind: &Path,
    cli: &Path,
    so: &Path,
    profile: &LoadProfile,
) -> LoadRunReport {
    let run_start = Instant::now();

    // ---- Goldcoin regtest: vault + spendable coin + reserve funding ----
    let goldcoin = RegtestNode::start(goldcoind, cli);
    let (vault, vault_signers) = three_vault_signers();
    goldcoin.import_vault(vault.address(), &vault.redeem_script_hex());
    let miner_address = goldcoin.new_address();
    goldcoin.generate(101, &miner_address);

    // The regtest wallet's spendable balance only grows as coinbase
    // outputs mature (standard 100-confirmation maturity); the initial
    // 101 blocks above mature exactly one coinbase. A profile whose
    // `initial_goldcoin_reserve` exceeds that one matured coinbase's
    // value fails the `sendtoaddress` funding call below before any
    // workload runs — this was never exercised until `LoadProfile::soak`
    // was actually driven at a real multi-hour duration, whose
    // duration-scaled reserve sizing is much larger than every
    // previously-run profile's. Mine additional blocks — cheap and fast
    // on a local regtest node — until the wallet's own reported spendable
    // balance provably covers the requested reserve plus a fixed
    // test-only fee headroom, deterministically and independent of the
    // profile's size, rather than assuming a fixed block count happens to
    // mature enough.
    let required_glc = atomic_to_glc(profile.initial_goldcoin_reserve) + FUNDING_FEE_HEADROOM_GLC;
    let mut additional_blocks_mined = 0u32;
    while regtest_spendable_balance_glc(&goldcoin) < required_glc {
        assert!(
            additional_blocks_mined < MAX_FUNDING_MATURATION_BLOCKS,
            "regtest wallet failed to mature {required_glc} GLC of spendable balance after \
             mining {additional_blocks_mined} additional blocks (spendable={}) — funding \
             bootstrap cannot proceed for this profile's reserve sizing",
            regtest_spendable_balance_glc(&goldcoin)
        );
        goldcoin.generate(FUNDING_MATURATION_BATCH_BLOCKS, &miner_address);
        additional_blocks_mined += FUNDING_MATURATION_BATCH_BLOCKS;
    }

    goldcoin.cli(&[
        "sendtoaddress",
        vault.address(),
        &format!("{:.8}", atomic_to_glc(profile.initial_goldcoin_reserve)),
    ]);
    goldcoin.generate(3, &miner_address);

    // Wait for the vault's own confirmed-received balance — read exactly
    // the way `Orchestrator::tick_goldcoin_reconciliation` reads it
    // (`vault_min_confirmations`, not `required_goldcoin_confirmations`,
    // which gates deposit source-finality, a different, unrelated config
    // value) — to actually reach the funded amount before starting the
    // orchestrator's tick loop below. Without this, a real cold-start
    // race is possible: the ledger's cached reserve baseline is set to
    // the funded target immediately, but the very first reconciliation
    // tick can run before the vault's watched balance has caught up,
    // producing a spurious near-total "unexplained drop" breach and
    // auto-pausing the reserve for the rest of the run — a real,
    // once-observed failure mode (docs/22-production-readiness-review.md
    // item 7's documented "zero cold-start grace period" caveat), not a
    // hypothetical one. Mirrors `support::wait_for_finalized_balance`'s
    // already-correct pattern for the Solana side.
    wait_for_goldcoin_vault_funded(
        &goldcoin,
        vault.address(),
        profile.initial_goldcoin_reserve,
        base_orchestrator_config().vault_min_confirmations,
    );

    // ---- Solana localnet: program, throwaway mint, reserve funding ----
    let upgrade_authority = Keypair::new();
    let validator = LocalValidator::start(so, &accounts::PROGRAM_ID, &upgrade_authority.pubkey());
    let blocking = validator.blocking_client();
    super::airdrop(&blocking, &upgrade_authority.pubkey(), 50_000_000_000);

    let (attestation_pubkeys, attestation_signers) = three_attestation_signers();
    let token_program = spl_token_2022::ID;
    let mint =
        super::create_throwaway_token2022_mint(&blocking, &upgrade_authority, SOLANA_GLC_DECIMALS);
    super::bootstrap_program(
        &blocking,
        &upgrade_authority,
        &attestation_pubkeys,
        2,
        &mint.pubkey(),
        &token_program,
    );

    let reserve_authority = accounts::reserve_authority_pda();
    let reserve_ata =
        accounts::associated_token_address(&reserve_authority, &mint.pubkey(), &token_program);
    super::mint_to(
        &blocking,
        &upgrade_authority,
        &mint.pubkey(),
        &token_program,
        &reserve_ata,
        &upgrade_authority,
        profile.initial_solana_reserve,
    );
    super::wait_for_finalized_balance(
        &validator.real_rpc(),
        &reserve_ata,
        profile.initial_solana_reserve,
    )
    .await;

    // A small fixed pool of GlcToSol recipients, each with a pre-created
    // ATA (release_from_reserve has no init_if_needed — see
    // `programs/glc-reserve-bridge/src/instructions/release_from_reserve.rs`).
    // Chosen round-robin by the seeded RNG below, not one recipient per
    // request — realistic enough for a load-shape test without per-request
    // ATA-creation overhead dominating the run. `release_from_reserve` is
    // submitted by the orchestrator via `RealSolanaRpc` (always
    // `finalized`, per its own doc comment) — waiting here for each ATA to
    // actually be visible at `finalized` (not just `confirmed`, which
    // `create_ata`'s `blocking` client already waited for) avoids the
    // orchestrator's first release attempt spuriously failing with
    // "no record of a prior credit" against an ATA that exists at
    // `confirmed` but hasn't finalized yet — the same `confirmed`/
    // `finalized` lag `support::wait_for_finalized_balance` exists to
    // work around for reserve funding. Finalization is a property of the
    // slot, not the individual transaction — once the LAST of these four
    // back-to-back creations finalizes, the earlier three (already
    // finalized at an earlier slot) are guaranteed finalized too, so one
    // wait at the end suffices instead of four sequential ones.
    let mut recipient_pool: Vec<(Keypair, Pubkey)> = Vec::new();
    for _ in 0..4 {
        let kp = Keypair::new();
        let ata = super::create_ata(
            &blocking,
            &upgrade_authority,
            &kp.pubkey(),
            &mint.pubkey(),
            &token_program,
        );
        recipient_pool.push((kp, ata));
    }
    super::wait_for_finalized_balance(&validator.real_rpc(), &recipient_pool.last().unwrap().1, 0)
        .await;

    let submitter = Keypair::new();
    super::airdrop(&blocking, &submitter.pubkey(), 50_000_000_000);

    // ---- Ledger + orchestrator ----
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                profile.initial_solana_reserve,
                0,
                profile.initial_solana_reserve / 2,
                profile.initial_solana_reserve / 5,
                profile.initial_solana_reserve / 10,
                now_unix(),
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                profile.initial_goldcoin_reserve,
                0,
                profile.initial_goldcoin_reserve / 2,
                profile.initial_goldcoin_reserve / 5,
                profile.initial_goldcoin_reserve / 10,
                now_unix(),
            )
            .unwrap();
    }

    let goldcoin_indexer = Indexer::new(
        goldcoin.rpc_client(),
        Ledger::open(&db_path).unwrap(),
        IndexerConfig {
            vault_script_hex: vault.script_pubkey_hex(),
            confirmation_depth: 3,
            max_reorg_depth: 50,
            initial_checkpoint: None,
            network: glc_reserve_bridge_service::goldcoin::address::Network::Testnet,
        },
    );
    let solana_indexer = glc_reserve_bridge_service::solana::indexer::SolanaIndexer::new(
        validator.real_rpc(),
        Ledger::open(&db_path).unwrap(),
        glc_reserve_bridge_service::amount_conversion::BRIDGE_FEE_BPS,
    );
    // A separate connection from the orchestrator's own — mirrors the real
    // deployed shape (an API layer creates requests with its own `Ledger`
    // handle; `daemon::run` ticks a wholly separate one against the same
    // on-disk file; SQLite WAL mode serializes writers, per
    // `ledger/mod.rs`'s own concurrency docs).
    let mut request_ledger = Ledger::open(&db_path).unwrap();

    let mut orchestrator = Orchestrator::new(
        goldcoin_indexer,
        solana_indexer,
        Ledger::open(&db_path).unwrap(),
        goldcoin.rpc_client(),
        validator.real_rpc(),
        vault.clone(),
        box_vault_signers(vault_signers),
        box_attestation_signers(attestation_signers),
        Keypair::try_from(submitter.to_bytes().as_slice()).unwrap(),
        base_orchestrator_config(),
        now_unix(),
    );

    // ---- Workload generation + settlement drive loop ----
    let mut rng = Rng::new(profile.seed);
    let mut issued: HashMap<i64, IssuedRequest> = HashMap::new();
    let mut sol_to_glc_by_obligation: HashMap<u64, (Instant, u64)> = HashMap::new();
    let mut requests_rejected_at_creation = 0u32;
    let mut sol_to_glc_submission_failures = 0u32;
    let mut sol_to_glc_submission_failure_samples: Vec<String> = Vec::new();
    let mut tick_errors: Vec<String> = Vec::new();
    let mut reconciliation_breaches = 0u32;
    let mut reconciliation_ticks = 0u32;
    let mut tick_count: u32 = 0;
    let mut last_issue = Instant::now() - profile.pacing_interval;

    let issued_total = |issued: &HashMap<i64, IssuedRequest>| issued.len() as u32;
    // Measured from HERE, not `run_start` (captured before bootstrap):
    // bootstrapping fresh regtest/validator infrastructure plus the
    // finalization waits above can itself take a real double-digit number
    // of seconds, and a deadline computed from `run_start` could already
    // be in the past by the time this loop's first iteration runs,
    // silently issuing zero traffic on a short profile.
    let generation_start = Instant::now();
    let generation_deadline = generation_start + profile.duration;

    loop {
        let now_instant = Instant::now();
        let generation_done = now_instant >= generation_deadline
            || profile
                .target_requests
                .is_some_and(|t| issued_total(&issued) >= t);

        if !generation_done
            && now_instant.duration_since(last_issue) >= profile.pacing_interval
            && active_in_flight(&request_ledger) < profile.concurrency
        {
            last_issue = now_instant;
            let direction =
                rng.choose_direction(profile.glc_to_sol_weight, profile.sol_to_glc_weight);
            match direction {
                Direction::GlcToSol => {
                    let gross = rng.gen_range_step(
                        profile.glc_to_sol_amount_range.0,
                        profile.glc_to_sol_amount_range.1,
                        GLC_TO_SOL_REPRESENTABLE_STEP,
                    );
                    let recipient_idx = rng.gen_range(0, recipient_pool.len() as u64 - 1) as usize;
                    let (recipient_keypair, _recipient_ata) = &recipient_pool[recipient_idx];
                    match glc_to_sol_amounts(gross, SOLANA_GLC_DECIMALS) {
                        None => {
                            // Post-fee net not exactly representable at
                            // the destination mint's decimals — a
                            // legitimate input-validation rejection a real
                            // API layer would also refuse before creating
                            // a request.
                            requests_rejected_at_creation += 1;
                        }
                        Some(amounts) => match request_ledger
                            .create_request(
                                Direction::GlcToSol,
                                amounts,
                                &recipient_keypair.pubkey().to_bytes(),
                                None,
                                RESERVATION_TTL_SECS,
                                now_unix(),
                            )
                            .unwrap()
                        {
                            CreateRequestOutcome::Reserved { request_id } => {
                                goldcoin.send_deposit_with_binding(
                                    vault.address(),
                                    atomic_to_glc(gross),
                                    request_id,
                                );
                                issued.insert(
                                    request_id,
                                    IssuedRequest {
                                        direction: Direction::GlcToSol,
                                        issued_at: now_instant,
                                        gross_canonical: gross,
                                    },
                                );
                            }
                            CreateRequestOutcome::InsufficientLiquidity { .. }
                            | CreateRequestOutcome::Paused
                            | CreateRequestOutcome::RouteAdmissionClosed
                            | CreateRequestOutcome::WalletLimited { .. } => {
                                requests_rejected_at_creation += 1;
                            }
                        },
                    }
                }
                Direction::SolToGlc => {
                    let amount_atomic = rng.gen_range(
                        profile.sol_to_glc_amount_range.0,
                        profile.sol_to_glc_amount_range.1,
                    );
                    let destination = goldcoin.new_address();
                    match issue_sol_to_glc(
                        &blocking,
                        &mint.pubkey(),
                        &token_program,
                        &upgrade_authority,
                        &destination,
                        amount_atomic,
                    )
                    .await
                    {
                        Ok(obligation_index) => {
                            let gross_canonical = SolanaAtomic(amount_atomic)
                                .to_canonical(SOLANA_GLC_DECIMALS)
                                .unwrap()
                                .0;
                            sol_to_glc_by_obligation
                                .insert(obligation_index, (now_instant, gross_canonical));
                        }
                        Err(e) => {
                            sol_to_glc_submission_failures += 1;
                            if sol_to_glc_submission_failure_samples.len() < 5 {
                                sol_to_glc_submission_failure_samples.push(e);
                            }
                        }
                    }
                }
                // `rng.choose_direction` only ever produces the two
                // Solana<->Goldcoin directions, so these are unreachable
                // here. Named rather than wildcarded so that a harness
                // extended to drive a Robinhood route has to write the
                // generator for it instead of inheriting one silently.
                Direction::GlcToRhn
                | Direction::RhnToGlc
                | Direction::SolToRhn
                | Direction::RhnToSol => {
                    unreachable!("this harness's generator produces no Robinhood direction")
                }
            }
        }

        let report = orchestrator.tick(now_unix()).await;
        tick_count += 1;
        tick_errors.extend(report.errors);
        if let Some(Ok(r)) = &report.solana_reconciliation {
            reconciliation_ticks += 1;
            if matches!(
                r.classification,
                glc_reserve_bridge_service::reconciliation::Classification::Breach
            ) {
                reconciliation_breaches += 1;
            }
        }
        if let Some(Ok(r)) = &report.goldcoin_reconciliation {
            reconciliation_ticks += 1;
            if matches!(
                r.classification,
                glc_reserve_bridge_service::reconciliation::Classification::Breach
            ) {
                reconciliation_breaches += 1;
            }
        }

        if tick_count.is_multiple_of(profile.mine_every_n_ticks) {
            goldcoin.generate(1, &miner_address);
        }

        // Resolve any SolToGlc obligations the indexer has, by now, folded
        // into a real `bridge_requests` row, so `issued`/`all_terminal`
        // stay accurate every tick rather than only after the loop ends.
        if !sol_to_glc_by_obligation.is_empty() {
            for row in all_sol_to_glc_rows(&request_ledger) {
                if let Some(idx) = row.source_obligation_index {
                    if let Some((issued_at, gross_canonical)) =
                        sol_to_glc_by_obligation.remove(&idx)
                    {
                        issued.insert(
                            row.id,
                            IssuedRequest {
                                direction: Direction::SolToGlc,
                                issued_at,
                                gross_canonical,
                            },
                        );
                    }
                }
            }
        }

        let drain_deadline_reached =
            generation_done && now_instant >= generation_deadline + profile.drain_timeout;
        if generation_done
            && sol_to_glc_by_obligation.is_empty()
            && all_terminal(&request_ledger, &issued)
        {
            break;
        }
        if drain_deadline_reached {
            break;
        }

        tokio::time::sleep(profile.tick_interval).await;
    }

    // Final resolution attempt for any SolToGlc obligations the loop's own
    // per-tick resolution above didn't catch (e.g. the drain timeout was
    // hit before the indexer folded a very recently submitted deposit).
    // Anything still unresolved after this never became a bridge_requests
    // row at all within the run's drain window — reported directly as a
    // stuck submission below, since `issued` cannot carry a request id
    // for it.
    for row in all_sol_to_glc_rows(&request_ledger) {
        if let Some(idx) = row.source_obligation_index {
            if let Some((issued_at, gross_canonical)) = sol_to_glc_by_obligation.remove(&idx) {
                issued.insert(
                    row.id,
                    IssuedRequest {
                        direction: Direction::SolToGlc,
                        issued_at,
                        gross_canonical,
                    },
                );
            }
        }
    }
    let unresolved_sol_to_glc_submissions = sol_to_glc_by_obligation.len();

    // ---- Stats collection ----
    let mut requests_issued: HashMap<Direction, u32> = HashMap::new();
    let mut stuck_requests = Vec::new();
    for (id, issued_req) in &issued {
        *requests_issued.entry(issued_req.direction).or_insert(0) += 1;
        if let Ok(Some(row)) = request_ledger.get_request(*id) {
            if row.state.is_active() {
                stuck_requests.push(StuckRequest {
                    id: *id,
                    direction: issued_req.direction,
                    state: row.state,
                    age_seconds: issued_req.issued_at.elapsed().as_secs(),
                });
            }
        }
    }

    // Expected fee is scoped to requests that actually reached `Settled` —
    // a request still in flight at the drain deadline has not yet accrued
    // any fee and must not be counted as "expected" (that would make
    // `accounting_healthy` spuriously fail on a run that simply hasn't
    // finished draining, which is a distinct, separately-reported
    // condition — see `stuck_requests`). Each settled row's own recorded
    // `fee_amount_atomic` is used directly rather than recomputed, so this
    // checks "the reserve's accrued-fee counter matches the sum of what
    // every settled request's own row says it paid," not a
    // harness-recomputed shadow value.
    //
    // Accrues to the SOURCE reserve, not the destination — deliberate,
    // documented bridge behavior (docs/20-bridge-fee.md: "the fee remains
    // on the source side where it was collected"; see the matching
    // comments in `Ledger::mark_release_confirmed`/
    // `Ledger::mark_goldcoin_completion_confirmed`), the opposite of
    // `Direction::destination_reserve()`.
    let mut expected_fee_atomic: HashMap<ReserveDirection, u64> = HashMap::new();
    for direction in [Direction::GlcToSol, Direction::SolToGlc] {
        let rows = request_ledger
            .requests_by_state(direction, RequestState::Settled)
            .unwrap();
        let sum: u64 = rows.iter().map(|r| r.fee_amount_atomic).sum();
        // This harness drives the two Solana<->Goldcoin directions only,
        // so the Robinhood reserve is unreachable here — stated as an
        // explicit refusal rather than a wildcard, so a future harness
        // that DOES drive a Robinhood route has to say where its fee
        // accrues instead of silently inheriting an answer.
        let source_reserve = match direction.destination_reserve() {
            ReserveDirection::SolanaReserve => ReserveDirection::GoldcoinReserve,
            ReserveDirection::GoldcoinReserve => ReserveDirection::SolanaReserve,
            ReserveDirection::RobinhoodReserve => {
                unreachable!("this harness drives no Robinhood route")
            }
        };
        *expected_fee_atomic.entry(source_reserve).or_insert(0) += sum;
    }

    let mut final_state_counts = HashMap::new();
    final_state_counts.insert(
        Direction::GlcToSol,
        request_ledger
            .request_state_counts(Direction::GlcToSol)
            .unwrap(),
    );
    final_state_counts.insert(
        Direction::SolToGlc,
        request_ledger
            .request_state_counts(Direction::SolToGlc)
            .unwrap(),
    );

    let mut observed_accrued_fee_atomic = HashMap::new();
    let mut settled_liquidity = HashMap::new();
    let mut invariant_ok = HashMap::new();
    for dir in [
        ReserveDirection::SolanaReserve,
        ReserveDirection::GoldcoinReserve,
    ] {
        observed_accrued_fee_atomic.insert(dir, request_ledger.accrued_fees(dir).unwrap());
        settled_liquidity.insert(dir, request_ledger.settled_liquidity(dir).unwrap());
        invariant_ok.insert(dir, request_ledger.check_invariant(dir).is_ok());
    }

    let mut settled_sum_from_rows: HashMap<ReserveDirection, u64> = HashMap::new();
    for (direction, state) in [
        (Direction::GlcToSol, RequestState::Settled),
        (Direction::SolToGlc, RequestState::Settled),
    ] {
        let rows = request_ledger.requests_by_state(direction, state).unwrap();
        let sum: u64 = rows.iter().map(|r| r.net_destination_atomic).sum();
        *settled_sum_from_rows
            .entry(direction.destination_reserve())
            .or_insert(0) += sum;
    }

    // Duplicate-settlement / duplicate-entitlement check: no two Settled
    // GlcToSol requests share a (source_txid, source_vout) pair, and no
    // two Settled SolToGlc requests share a source_obligation_index. The
    // DB schema already enforces this structurally (UNIQUE constraints);
    // this is a harness-level regression check on top, not the primary
    // defense.
    let duplicate_binding_check_ok = {
        let glc_rows = request_ledger
            .requests_by_state(Direction::GlcToSol, RequestState::Settled)
            .unwrap();
        let mut seen = std::collections::HashSet::new();
        let glc_ok = glc_rows
            .iter()
            .all(|r| seen.insert((r.source_txid, r.source_vout)));
        let sol_rows = request_ledger
            .requests_by_state(Direction::SolToGlc, RequestState::Settled)
            .unwrap();
        let mut seen_ob = std::collections::HashSet::new();
        let sol_ok = sol_rows
            .iter()
            .all(|r| seen_ob.insert(r.source_obligation_index));
        glc_ok && sol_ok
    };

    // Replay/idempotency check: take one settled GlcToSol request and
    // re-deliver its exact recorded deposit observation directly against
    // the ledger. `record_glc_deposit_observed`'s `AlreadyRecorded` branch
    // only matches `DepositObserved`/`Confirming` (see its own doc
    // comment); a request picked from `Settled` has already moved well
    // past that, so the correct, safe outcome here is `NoMatchingRequest`
    // — the same "never resurrect/reopen a request past its live window"
    // property `ledger::tests::glc_deposit_with_no_matching_request_is_never_silently_dropped`
    // pins at the unit level, exercised here against a ledger that has
    // been through a real sustained run, not a fresh in-memory one. What
    // actually matters, and what this checks, is that replaying an
    // already-settled deposit's observation never has any additional
    // economic effect, regardless of which safe outcome variant it hits.
    let (replay_idempotency_ok, replay_idempotency_detail) = {
        let settled_glc = request_ledger
            .requests_by_state(Direction::GlcToSol, RequestState::Settled)
            .unwrap();
        match settled_glc.into_iter().find(|r| r.source_txid.is_some()) {
            None => (
                true,
                "no settled GlcToSol request available to replay (not a failure)".to_string(),
            ),
            Some(row) => {
                let before_settled = request_ledger
                    .settled_liquidity(ReserveDirection::SolanaReserve)
                    .unwrap();
                let before_reserved = request_ledger
                    .reserve_snapshot(ReserveDirection::SolanaReserve)
                    .unwrap()
                    .2;
                let outcome = request_ledger
                    .record_glc_deposit_observed(
                        row.id,
                        row.source_txid.unwrap(),
                        row.source_vout.unwrap(),
                        row.gross_amount_atomic,
                        row.source_block_height.unwrap_or(0),
                        row.source_block_hash.unwrap_or([0u8; 32]),
                        now_unix(),
                    )
                    .unwrap();
                let after_settled = request_ledger
                    .settled_liquidity(ReserveDirection::SolanaReserve)
                    .unwrap();
                let after_reserved = request_ledger
                    .reserve_snapshot(ReserveDirection::SolanaReserve)
                    .unwrap()
                    .2;
                let ok = matches!(
                    outcome,
                    glc_reserve_bridge_service::ledger::GlcObservationOutcome::AlreadyRecorded
                        | glc_reserve_bridge_service::ledger::GlcObservationOutcome::NoMatchingRequest
                ) && before_settled == after_settled
                    && before_reserved == after_reserved;
                (
                    ok,
                    format!(
                        "request {}: outcome={outcome:?} settled_before={before_settled} settled_after={after_settled} \
                         reserved_before={before_reserved} reserved_after={after_reserved}",
                        row.id
                    ),
                )
            }
        }
    };

    LoadRunReport {
        profile_name: profile.name,
        wall_clock: run_start.elapsed(),
        requests_issued,
        requests_rejected_at_creation,
        sol_to_glc_submission_failures,
        sol_to_glc_submission_failure_samples,
        unresolved_sol_to_glc_submissions,
        final_state_counts,
        stuck_requests,
        expected_fee_atomic,
        observed_accrued_fee_atomic,
        settled_liquidity,
        settled_sum_from_rows,
        invariant_ok,
        duplicate_binding_check_ok,
        replay_idempotency_ok,
        replay_idempotency_detail,
        tick_errors,
        reconciliation_breaches,
        reconciliation_ticks,
    }
}

fn active_in_flight(ledger: &Ledger) -> usize {
    let mut n = 0i64;
    for direction in [Direction::GlcToSol, Direction::SolToGlc] {
        if let Ok(counts) = ledger.request_state_counts(direction) {
            for (state, count) in counts {
                if state.is_active() {
                    n += count;
                }
            }
        }
    }
    n.max(0) as usize
}

/// True once every request id in `issued` has left its active
/// (reservation-holding) states. Callers must separately confirm
/// `sol_to_glc_by_obligation` (submitted-but-not-yet-folded obligations)
/// is empty — a submission with no request id yet is not represented in
/// `issued` at all, so this function alone cannot see it.
fn all_terminal(ledger: &Ledger, issued: &HashMap<i64, IssuedRequest>) -> bool {
    issued.keys().all(|id| {
        ledger
            .get_request(*id)
            .ok()
            .flatten()
            .map(|r| !r.state.is_active())
            .unwrap_or(false)
    })
}

/// Every `bridge_requests` row for `SolToGlc`, across all states —
/// used to resolve a submitted obligation index to the request id the
/// indexer folded it into (there is no direct lookup-by-obligation-index
/// API on `Ledger`, so this scans each state bucket, same as any other
/// full-table read in this codebase's test/ops tooling).
fn all_sol_to_glc_rows(ledger: &Ledger) -> Vec<glc_reserve_bridge_service::ledger::BridgeRequest> {
    let mut rows = Vec::new();
    for state in [
        RequestState::LiquidityReserved,
        RequestState::AwaitingDeposit,
        RequestState::DepositObserved,
        RequestState::Confirming,
        RequestState::SourceFinalized,
        RequestState::SettlementAuthorized,
        RequestState::DestinationSubmitted,
        RequestState::DestinationConfirmed,
        RequestState::Settled,
        RequestState::Expired,
        RequestState::Cancelled,
        RequestState::Reorged,
        RequestState::InsufficientReserveAtSettlement,
        RequestState::DestinationSubmissionFailed,
        RequestState::ManualReview,
        RequestState::Failed,
    ] {
        rows.extend(
            ledger
                .requests_by_state(Direction::SolToGlc, state)
                .unwrap_or_default(),
        );
    }
    rows
}

#[allow(clippy::too_many_arguments)]
async fn issue_sol_to_glc(
    blocking: &BlockingRpcClient,
    mint: &Pubkey,
    token_program: &Pubkey,
    upgrade_authority: &Keypair,
    destination_glc_address: &str,
    amount_atomic: u64,
) -> Result<u64, String> {
    // Read the live obligation_count via the SAME (`confirmed`) client
    // every other bootstrap step here already uses, not `RealSolanaRpc`
    // (always `finalized` — see its own doc comment). `finalized` lags
    // `confirmed` by ~32 slots on a fresh single-node validator (the same
    // lag `support::wait_for_finalized_balance` exists to work around for
    // reserve funding); reading it here would see a stale obligation_count
    // whenever a prior `deposit_to_reserve` in this same run had only just
    // confirmed, deriving the wrong `withdrawal_obligation` PDA and
    // failing the on-chain seeds constraint.
    let account = blocking
        .get_account(&accounts::bridge_config_pda())
        .map_err(|e| e.to_string())?;
    let snapshot = accounts::decode_bridge_config(&account.data).map_err(|e| e.to_string())?;
    let obligation_index = snapshot.obligation_count;

    let user = Keypair::new();
    super::airdrop(blocking, &user.pubkey(), 2_000_000_000);
    let user_ata = super::create_ata(
        blocking,
        upgrade_authority,
        &user.pubkey(),
        mint,
        token_program,
    );
    super::mint_to(
        blocking,
        upgrade_authority,
        mint,
        token_program,
        &user_ata,
        upgrade_authority,
        amount_atomic,
    );

    let deposit_ix = instructions::deposit_to_reserve(
        &user.pubkey(),
        mint,
        token_program,
        obligation_index,
        amount_atomic,
        destination_glc_address.as_bytes(),
    );
    let blockhash = blocking.get_latest_blockhash().map_err(|e| e.to_string())?;
    let tx = Transaction::new_signed_with_payer(
        &[deposit_ix],
        Some(&user.pubkey()),
        &[&user],
        blockhash,
    );
    blocking
        .send_and_confirm_transaction(&tx)
        .map_err(|e| e.to_string())?;
    Ok(obligation_index)
}
