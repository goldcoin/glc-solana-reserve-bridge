//! `glc-bridge-daemon` — the long-running reserve bridge process
//! (docs/15-post-phase6-audit.md P0 item 1). Loads production config
//! (`glc_reserve_bridge_service::config`), wires the real Goldcoin/Solana
//! RPC clients and the configured signers into an [`Orchestrator`], and
//! drives it forever via [`glc_reserve_bridge_service::daemon::run`] —
//! reconciling both reserve directions every tick — while serving
//! `/health` and `/metrics` alongside it until `SIGINT`/`SIGTERM`.
//!
//! # Signer loading is mode-gated — see `config::Config::load_signers`
//!
//! `operators.mode` in the config file selects the entire signer-loading
//! path: `"dev"` loads local plaintext key files
//! (`config::Config::load_attestation_signers`/`load_vault_signers`) —
//! DEV/TEST POSTURE ONLY, never point this at production keys. `
//! "production"` connects to the configured remote signer endpoints
//! instead (`signing::remote`, docs/26-production-signer-deployment.md)
//! — no private key material is ever loaded into this process in that
//! mode. `Config::load` itself already refuses to resolve a config where
//! `mode` and the populated signer fields disagree (see `config.rs`
//! module docs) — this binary never has to re-check that itself, only
//! call the one mode-aware entry point (`load_signers`).
//! `load_submitter` (the Solana fee-payer key) is unaffected by `mode` —
//! see that method's own docs for why it is not a custody authority.
//!
//! # Startup fails closed
//!
//! Every step below — config parsing, key loading and cross-validation,
//! vault construction, opening the ledger — exits non-zero with a clear
//! message rather than starting in a partially-configured state. Nothing
//! here guesses a default for a value the operator didn't provide.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use glc_reserve_bridge_service::admin_api::{
    self, auth::OperatorRegistry, glc_refund_exec, AdminApi,
};
use glc_reserve_bridge_service::api::{self, BridgeApi};
use glc_reserve_bridge_service::config::Config;
use glc_reserve_bridge_service::daemon::{self, DaemonLoopConfig};
use glc_reserve_bridge_service::goldcoin::indexer::{Indexer, IndexerConfig};
use glc_reserve_bridge_service::goldcoin::rpc::{
    RpcClient as GoldcoinRpcClient, RpcConfig as GoldcoinRpcConfig,
};
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{Ledger, ReserveDirection};
use glc_reserve_bridge_service::ops::{self, collector::OpsCollector, health};
use glc_reserve_bridge_service::orchestrator::{Orchestrator, OrchestratorConfig};
use glc_reserve_bridge_service::robinhood;
use glc_reserve_bridge_service::solana::accounts;
use glc_reserve_bridge_service::solana::indexer::SolanaIndexer;
use glc_reserve_bridge_service::solana::rpc::RealSolanaRpc;

const USAGE: &str = "glc-bridge-daemon — long-running reserve bridge process

  glc-bridge-daemon --config PATH

Drives Orchestrator::tick() on an interval, reconciling both the Goldcoin
and Solana reserve directions every tick, and serves /health and /metrics
until SIGINT/SIGTERM (graceful — a tick in progress always finishes).

DEV/TEST KEY POSTURE ONLY (docs/12-management-decisions.md item 2) — do
not point this at production keys or endpoints.";

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Every startup step below either succeeds or exits(2) with a logged
/// reason — this is the one place that pattern lives, so `main` reads as
/// a plain sequence of steps rather than a wall of repeated match arms.
fn or_exit<T, E: std::fmt::Display>(result: Result<T, E>, doing: &str) -> T {
    match result {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "could not {doing} — refusing to start");
            std::process::exit(2);
        }
    }
}

fn open_ledger(path: &Path) -> Ledger {
    or_exit(Ledger::open(path), "open the ledger database")
}

fn goldcoin_rpc_config(config: &Config) -> GoldcoinRpcConfig {
    GoldcoinRpcConfig {
        url: config.goldcoin.rpc_url.clone(),
        user: config.goldcoin.rpc_user.clone(),
        password: config.goldcoin.rpc_password.clone(),
        connect_timeout_ms: config.goldcoin.rpc_connect_timeout_ms,
        read_timeout_ms: config.goldcoin.rpc_read_timeout_ms,
    }
}

/// Log targets that are NOT this crate's module path: the fail-safe and
/// incident lines the daemon emits under its own names (`tracing::error!
/// (target: "robinhood_reserve", ...)` when reconciliation auto-pauses the
/// Robinhood reserve; the auto-resume decisions). A `RUST_LOG` that names
/// only crate paths — `glc_reserve_bridge_service=debug,glc_bridge_daemon=
/// debug`, the production override on 2026-09-12 — matches none of these,
/// and `EnvFilter` disables every event no directive matches, so the
/// reserve auto-paused with NOTHING in the journal and the only record was
/// the `reconciliation_findings` row. [`log_filter`] floors each of these
/// at `info` unless the operator addressed it explicitly.
const OWN_LOG_TARGETS: &[&str] = &["robinhood_reserve", "auto_resume"];

/// The daemon's log filter: `RUST_LOG` as given (or `info` when unset or
/// unparsable), plus an `info` floor for every target in
/// [`OWN_LOG_TARGETS`] that `RUST_LOG` neither names nor covers with a
/// bare level. An explicit `robinhood_reserve=warn` or a bare `debug` is
/// honoured as written — the floor only fills a target the operator did
/// not mention, never overrides one they did.
fn log_filter(rust_log: Option<&str>) -> tracing_subscriber::EnvFilter {
    use tracing_subscriber::EnvFilter;
    let mut filter = rust_log
        .and_then(|s| EnvFilter::try_new(s).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    let directives: Vec<&str> = rust_log
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // A bare level (`info`, `debug`) already applies to every target.
    if directives.is_empty()
        || directives
            .iter()
            .any(|d| !d.contains('=') && !d.contains('['))
    {
        return filter;
    }
    for target in OWN_LOG_TARGETS {
        let named = directives
            .iter()
            .any(|d| d.split(['=', '[']).next().map(str::trim) == Some(*target));
        if !named {
            filter = filter.add_directive(
                format!("{target}=info")
                    .parse()
                    .expect("a static `target=info` directive parses"),
            );
        }
    }
    filter
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(log_filter(std::env::var("RUST_LOG").ok().as_deref()))
        // Daemon convention: logs to stderr, stdout kept free for any
        // future structured output an operator might pipe elsewhere.
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
    let Some(config_path) = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
    else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };

    let config = or_exit(Config::load(Path::new(config_path)), "load config");
    tracing::info!(mode = ?config.operators.mode, "signer mode");
    // Mode-gated: `"dev"` loads local plaintext key files, `"production"`
    // connects to the configured remote signer endpoints — never both,
    // `Config::load` already refused to resolve a config where `mode`
    // and the populated signer fields disagree (config.rs module docs,
    // signing::remote module docs). Either way this returns the trait
    // objects `Orchestrator` actually depends on — this binary never
    // names a concrete signer type.
    let (attestation_signers, vault_signers): glc_reserve_bridge_service::config::LoadedSigners =
        or_exit(config.load_signers().await, "load signers");
    // A second set of signer CLIENTS for the admin API's refund executor,
    // built from the same `operators.vault_remote_signers` configuration
    // and the same env-resolved tokens — same endpoints, same identities,
    // same 2-of-3 threshold. Separate client objects only, exactly as
    // `goldcoin_rpc_for_indexer`/`_for_orchestrator` are, so an
    // operator-initiated refund never contends with the tick loop. There
    // is no second signer set to configure and no way to point this at
    // different signers: it is the same config, loaded twice.
    let (_attestation_signers_unused, vault_signers_for_refunds_vec): glc_reserve_bridge_service::config::LoadedSigners =
        or_exit(
            config.load_signers().await,
            "load vault signer clients for the refund executor",
        );
    let vault_signers_for_refunds = Arc::new(vault_signers_for_refunds_vec);
    let submitter = or_exit(config.load_submitter(), "load the submitter key");
    let vault = or_exit(
        MultisigVault::new(
            config.operators.vault_pubkeys.clone(),
            config.operators.vault_threshold,
            config.goldcoin.network,
        ),
        "construct the vault from configured signer pubkeys",
    );
    tracing::info!(vault_address = %vault.address(), "vault constructed from configured signer set");
    let vault_address = vault.address().to_string();
    let root_vault_for_api = vault.clone();

    let goldcoin_cfg = goldcoin_rpc_config(&config);
    let goldcoin_rpc_for_indexer = or_exit(
        GoldcoinRpcClient::new(&goldcoin_cfg),
        "construct the Goldcoin RPC client",
    );
    let goldcoin_rpc_for_orchestrator = or_exit(
        GoldcoinRpcClient::new(&goldcoin_cfg),
        "construct the Goldcoin RPC client",
    );
    // A separate client for the admin API's refund executor, so an
    // operator-initiated refund never contends with the tick loop's own
    // client.
    let goldcoin_rpc_for_refunds = Arc::new(or_exit(
        GoldcoinRpcClient::new(&goldcoin_cfg),
        "construct the Goldcoin RPC client",
    ));
    let vault_for_refunds = vault.clone();

    // Fail closed before anything else touches the chain: the configured
    // reserve mint must be owned by a supported SPL token program (legacy
    // SPL Token or Token-2022 — docs/18-token-2022-support.md), and, if
    // it's Token-2022, every extension it carries must be on the
    // explicitly reviewed allowlist (accounts::verify_reserve_mint_token_
    // program's docs explain why an unreviewed extension isn't just "not
    // tested" but actually unsafe to assume — extensions like transfer
    // fees/hooks would silently break the 1:1 reserve invariant). The
    // on-chain program's own constraints and `crate::token_extensions`
    // would already reject a bad mint/program/extension at the first
    // instruction that touched it, but failing here is clearer and
    // earlier — before any indexer/orchestrator wiring, let alone a real
    // transfer, is ever attempted.
    let mint_basics = or_exit(
        accounts::verify_reserve_mint_token_program(
            &RealSolanaRpc::new(config.solana.rpc_url.clone()),
            &config.solana.reserve_token_mint,
        )
        .await,
        "verify the configured reserve_token_mint's token program",
    );
    tracing::info!(
        decimals = mint_basics.decimals,
        supply = mint_basics.supply,
        mint_authority = ?mint_basics.mint_authority,
        freeze_authority = ?mint_basics.freeze_authority,
        token_program = %mint_basics.token_program,
        "reserve_token_mint verified: supported token program, extensions reviewed, decimals read live on every transfer"
    );

    // Idempotent every startup: only the bounds (protected_minimum/
    // target/warning/critical) are (re)applied from config on every run —
    // `Ledger::configure_reserve`'s `ON CONFLICT` never touches an
    // existing total_reserve_balance. `initial_balance: 0` here is
    // deliberate, not a placeholder to fix later: it is only ever
    // consulted the very first time a direction is configured, and
    // reconciliation's very next tick overwrites it with a real observed
    // balance regardless. Seeding anything other than 0 would risk baking
    // in a stale guess; 0 can only ever look like a balance *increase*
    // (never flagged as a breach — see `reconciliation::reconcile`),
    // never a false "unexplained drop" — the same cold-start reasoning
    // docs/15-post-phase6-audit.md documents for the equivalent race the
    // Phase 6 real-node tests hit.
    // The Robinhood reserve is configured only when `[reserve.robinhood]`
    // is present — which no existing production config file has. An
    // unconfigured reserve has no `reserve_ledger` row at all, so nothing
    // can be reserved against it and no Robinhood settlement can pass its
    // admission check: the fail-closed default is "this reserve does not
    // exist", not "this reserve is empty".
    let mut reserve_bounds = vec![
        (ReserveDirection::SolanaReserve, config.reserve.solana),
        (ReserveDirection::GoldcoinReserve, config.reserve.goldcoin),
    ];
    if let Some(robinhood) = config.reserve.robinhood {
        reserve_bounds.push((ReserveDirection::RobinhoodReserve, robinhood));
    }
    for (direction, bounds) in reserve_bounds {
        let mut ledger = open_ledger(&config.service.db_path);
        or_exit(
            ledger.configure_reserve(
                direction,
                0,
                bounds.protected_minimum,
                bounds.target_reserve,
                bounds.warning_reserve,
                bounds.critical_reserve,
                now_unix(),
            ),
            "configure reserve bounds",
        );
        // UTXO-liquidity admission backpressure is Goldcoin-specific
        // (docs/09-runbook.md's "UTXO liquidity" section) — SolanaReserve
        // has no vault_utxos concept, so it's left at its default (0, 0),
        // i.e. no backpressure, same as never calling this at all.
        if direction == ReserveDirection::GoldcoinReserve {
            or_exit(
                ledger.set_utxo_pool_thresholds(
                    direction,
                    config.goldcoin.utxo_pool_min_available_count,
                    config.goldcoin.utxo_pool_warning_count,
                ),
                "configure UTXO pool thresholds",
            );
            // Confirmed-liquidity admission safety buffer, same posture
            // and same reason as the UTXO-pool thresholds above: SolToGlc
            // is the only direction whose admission this governs, so
            // SolanaReserve is left at its default (0, 0) — identical to
            // never calling this at all.
            or_exit(
                ledger.set_admission_liquidity_thresholds(
                    direction,
                    config.goldcoin.admission_safety_buffer_atomic,
                    config.goldcoin.admission_reopen_headroom_atomic,
                ),
                "configure admission liquidity thresholds",
            );
        }
    }
    // The rapid-burst anti-abuse policy (`[rapid_burst]`,
    // `ledger::rapid_burst`): seeded once here so every fold — which runs
    // inside the ledger's own transaction — and `glc-admin
    // rapid-burst-policy-show` read the same numbers. Disabled unless the
    // config enables it, in which case the effective values are logged
    // for cross-signer drift diagnosis like the shaping knobs below.
    or_exit(
        open_ledger(&config.service.db_path)
            .set_rapid_burst_policy(&config.rapid_burst, now_unix()),
        "configure rapid-burst policy",
    );
    tracing::info!(
        enabled = config.rapid_burst.enabled,
        window_secs = config.rapid_burst.window_secs,
        max_per_source_wallet = config.rapid_burst.max_per_source_wallet,
        max_per_destination_wallet = config.rapid_burst.max_per_destination_wallet,
        max_per_pair = config.rapid_burst.max_per_pair,
        minimum_review_hold_secs = config.rapid_burst.minimum_review_hold_secs,
        "rapid-burst hold policy (effective)"
    );

    // Operator/signer-mismatch diagnostic (PR #35 maintainer-review
    // finding 4): every independent signer's own instance of this
    // codebase must agree on `change_fanout_target_atomic`/
    // `change_fanout_max_outputs` (both fed into `PayoutPolicy`, which
    // must independently re-derive byte-identical transactions across the
    // 2-of-3 signer set — docs/09-runbook.md "UTXO liquidity") and on
    // `vault_min_confirmations` (governs when this vault's own change
    // becomes spendable again). A silent drift here — e.g. a rolling
    // deploy where one signer picked up a new config field before another
    // — doesn't corrupt anything (a divergent signer's signature simply
    // fails `multisig::assemble`'s cryptographic verification, never
    // broadcasting anything malformed), but surfaces only as an opaque
    // stuck-payout/signing failure unless an operator can directly diff
    // what each signer instance actually loaded. Logged at startup, once,
    // at INFO — cheap, always visible, never gated behind a flag.
    tracing::info!(
        utxo_pool_min_available_count = config.goldcoin.utxo_pool_min_available_count,
        utxo_pool_warning_count = config.goldcoin.utxo_pool_warning_count,
        change_fanout_target_atomic = config.goldcoin.change_fanout_target_atomic,
        change_fanout_max_outputs = config.goldcoin.change_fanout_max_outputs,
        zero_conf_change_max_depth = config.goldcoin.zero_conf_change_max_depth,
        vault_min_confirmations = config.goldcoin.vault_min_confirmations,
        max_inputs = config.goldcoin.max_inputs,
        utxo_shaping_enabled = config.goldcoin.utxo_shaping_enabled,
        utxo_shaping_target_available_count = config.goldcoin.utxo_shaping_target_available_count,
        utxo_shaping_min_source_atomic = config.goldcoin.utxo_shaping_min_source_atomic,
        utxo_shaping_max_outputs_per_split = config.goldcoin.utxo_shaping_max_outputs_per_split,
        "effective UTXO liquidity settings for this instance — compare across every independent \
         signer to catch config drift before it surfaces as a stuck payout"
    );

    let goldcoin_indexer = Indexer::new(
        goldcoin_rpc_for_indexer,
        open_ledger(&config.service.db_path),
        IndexerConfig {
            vault_script_hex: vault.script_pubkey_hex(),
            confirmation_depth: config.goldcoin.confirmation_depth,
            max_reorg_depth: config.goldcoin.max_reorg_depth,
            initial_checkpoint: config.goldcoin.initial_checkpoint.clone(),
            network: config.goldcoin.network,
        },
    );
    // `SolToGlc`'s own configured rate. Resolved once, here, and handed
    // to the indexer that folds that route's deposits — never looked up
    // globally at fold time.
    let sol_to_glc_fee_bps = or_exit(
        config
            .route_fees
            .fee_bps(glc_reserve_bridge_service::routes::Route::SolToGlc),
        "resolving the SolToGlc fee",
    );
    let solana_indexer = SolanaIndexer::new(
        RealSolanaRpc::new(config.solana.rpc_url.clone()),
        open_ledger(&config.service.db_path),
        sol_to_glc_fee_bps,
    );

    let orchestrator_config = OrchestratorConfig {
        attestation_threshold: config.operators.attestation_threshold,
        vault_threshold: config.operators.vault_threshold as usize,
        required_goldcoin_confirmations: config.goldcoin.required_payout_confirmations,
        fee_rate_per_kb: config.goldcoin.fee_rate_per_kb,
        dust_threshold: config.goldcoin.dust_threshold,
        max_inputs: config.goldcoin.max_inputs,
        change_fanout_target_atomic: config.goldcoin.change_fanout_target_atomic,
        change_fanout_max_outputs: config.goldcoin.change_fanout_max_outputs,
        zero_conf_change_max_depth: config.goldcoin.zero_conf_change_max_depth,
        zero_conf_change_mode: config.goldcoin.zero_conf_change_mode,
        zero_conf_change_recursive_chain_limit: config
            .goldcoin
            .zero_conf_change_recursive_chain_limit,
        reconciliation_tolerance: config.reserve.reconciliation_tolerance,
        vault_min_confirmations: config.goldcoin.vault_min_confirmations,
        goldcoin_network: config.goldcoin.network,
        signer_timeout: Duration::from_millis(config.service.signer_timeout_ms),
        max_auto_resumes_per_tick: config.goldcoin.max_auto_resumes_per_tick,
        utxo_shaping_enabled: config.goldcoin.utxo_shaping_enabled,
        utxo_shaping_target_available_count: config.goldcoin.utxo_shaping_target_available_count,
        utxo_shaping_min_source_atomic: config.goldcoin.utxo_shaping_min_source_atomic,
        utxo_shaping_max_outputs_per_split: config.goldcoin.utxo_shaping_max_outputs_per_split,
    };

    // Captured before `orchestrator_config` moves into the orchestrator:
    // the refund executor must use the SAME payout policy the tick loop
    // does, not a second copy that could drift.
    let refund_payout_policy = orchestrator_config.payout_policy();

    let mut orchestrator = Orchestrator::new(
        goldcoin_indexer,
        solana_indexer,
        open_ledger(&config.service.db_path),
        goldcoin_rpc_for_orchestrator,
        RealSolanaRpc::new(config.solana.rpc_url.clone()),
        vault,
        vault_signers,
        attestation_signers,
        submitter,
        orchestrator_config,
        now_unix(),
    );

    // The route admission gate (crate::routes). Built once from the
    // resolved config plus the Phase-1 chain registry, then shared by every
    // route-bearing entry point. Logged at startup so an operator can see
    // exactly which routes this process will admit without inspecting the
    // config file, the database and the build separately.
    // The Robinhood settlement preflight, and the adapter it produces.
    //
    // Everything downstream hangs off this ONE value: a
    // `VerifiedDeployment` can only be produced by
    // `robinhood::preflight::verify`, which reads the deployed contracts
    // and refuses on any disagreement. No verified deployment means the
    // Robinhood adapter refuses every route, which means all three
    // `RouteGate` gates cannot all open, which means nothing settles —
    // whatever the config file or the `bridge_routes` table say.
    //
    // A FAILED preflight is fatal for the same reason a bad reserve mint
    // is: an operator who configured `[robinhood.settlement]` asked for
    // this deployment to be able to settle, and starting anyway with it
    // silently disabled would hide the misconfiguration behind a route
    // that "just never opens".
    let robinhood_deployment = match (&config.robinhood_indexer, &config.robinhood_settlement) {
        (Some(indexer_cfg), Some(settlement_cfg)) => {
            let rpc = or_exit(
                robinhood::rpc::EvmRpcClient::new(&robinhood::rpc::EvmRpcConfig {
                    url: indexer_cfg.rpc_url.clone(),
                    connect_timeout_ms: indexer_cfg.request_timeout_ms,
                    read_timeout_ms: indexer_cfg.request_timeout_ms,
                }),
                "construct the Robinhood EVM RPC client for the settlement preflight",
            );
            let verified = or_exit(
                robinhood::preflight::verify(&rpc, indexer_cfg, settlement_cfg).await,
                "verify the deployed Robinhood contracts (preflight)",
            );
            tracing::info!(
                chain_id = verified.chain_id.get(),
                bridge_contract = %verified.bridge_contract,
                token = %verified.token,
                token_decimals = verified.token_decimals,
                tx_envelope = verified.tx_envelope.as_str(),
                chain_has_base_fee = verified.chain_has_base_fee,
                submitter = %settlement_cfg.submitter_address,
                "Robinhood settlement preflight PASSED — this verifies contract identity, token \
                 identity and decimals, the signer set, the EIP-712 domain and the fee-market \
                 envelope. It does NOT establish anything about the token's mint authority, \
                 blocklist, transfer hooks or upgradeability; those need a separate mainnet \
                 token review."
            );

            // ---- the approved launch policy against the contract ----
            //
            // The contract is the enforcement layer for both the
            // per-transfer maximum and the rolling window. A configured
            // backend limit is therefore only ever a STATEMENT about
            // what the contract holds, and a statement nobody checks is
            // a lie waiting to be told to a user at quote time.
            //
            // Fatal only when the BACKEND is the more permissive side:
            // that is the case where this process would admit, price and
            // promise a transfer the contract reverts. The reverse — a
            // contract more permissive than the approved policy — is
            // loud but not fatal here, because nothing this process does
            // can exceed the policy it is itself configured with; it is
            // still a FAIL at the operator preflight gate
            // (`glc-admin robinhood-preflight`), which is what launch
            // approval runs.
            match config
                .chain_policies
                .get(glc_reserve_bridge_service::routes::Chain::Robinhood)
            {
                None => tracing::info!(
                    "no [robinhood.policy] section — Robinhood requests price at the compiled-in \
                     BRIDGE_FEE_BPS and this process states no backend transfer limits, so the \
                     contract's own limits() is the only policy in force and nothing has \
                     confirmed it is the one an operator approved"
                ),
                Some(policy) => {
                    let binding = or_exit(
                        robinhood::RobinhoodPolicyBinding::new(*policy),
                        "express the configured [robinhood.policy] against the Robinhood contract",
                    );
                    let limits = or_exit(
                        robinhood::calls::BridgeReader::new(settlement_cfg.bridge_contract)
                            .limits(&rpc, robinhood::rpc::EvmBlockTag::Latest)
                            .await,
                        "read the Robinhood contract's limits() for the policy preflight",
                    );
                    let mismatches = binding.compare(&limits);
                    tracing::info!(
                        fee_bps = policy.fee_bps(),
                        per_transfer_limit_canonical = policy.per_transfer_limit().0,
                        rolling_daily_limit_canonical = policy.rolling_daily_limit().0,
                        expected_onchain_rolling_limit_canonical =
                            binding.expected_onchain_rolling_limit_canonical().0,
                        chain_inbound_max = %limits.inbound_max,
                        chain_outbound_max = %limits.outbound_max,
                        chain_inbound_rolling_limit = %limits.inbound_rolling_limit,
                        chain_outbound_rolling_limit = %limits.outbound_rolling_limit,
                        mismatches = mismatches.len(),
                        "Robinhood launch policy (backend, canonical 8dp) against the deployed \
                         contract's limits() (18dp). The on-chain rolling limit must be exactly \
                         HALF the strict 24h policy: the contract's window is a fixed bucket, so \
                         2x the configured limit can move in one 86,400s span"
                    );
                    let mut fatal = false;
                    for mismatch in &mismatches {
                        if robinhood::policy::is_backend_over_claim(mismatch) {
                            fatal = true;
                            tracing::error!(mismatch = %mismatch, "Robinhood policy mismatch");
                        } else {
                            tracing::warn!(mismatch = %mismatch, "Robinhood policy mismatch");
                        }
                    }
                    if fatal {
                        tracing::error!(
                            "the configured [robinhood.policy] claims a larger usable limit than \
                             the deployed contract allows — refusing to start rather than quote \
                             a limit the chain will revert"
                        );
                        std::process::exit(2);
                    }
                }
            }

            Some(verified)
        }
        (None, Some(_)) => {
            // Structurally unreachable — `Config::load` refuses this —
            // but stated rather than assumed.
            tracing::error!(
                "[robinhood.settlement] without [robinhood.indexer] — refusing to start"
            );
            std::process::exit(2);
        }
        _ => {
            tracing::info!(
                "no [robinhood.settlement] section — this process holds no Robinhood submitter \
                 key, constructs no Robinhood authorization signer, and cannot broadcast a \
                 Robinhood transaction"
            );
            None
        }
    };

    let route_gate = Arc::new(glc_reserve_bridge_service::routes::RouteGate::new(
        config.routes,
        match robinhood_deployment.clone() {
            Some(verified) => {
                glc_reserve_bridge_service::chains::ChainRegistry::with_verified_robinhood(verified)
            }
            None => glc_reserve_bridge_service::chains::ChainRegistry::legacy_only(),
        },
    ));
    {
        let gate_ledger = open_ledger(&config.service.db_path);
        for route in glc_reserve_bridge_service::routes::Route::ALL {
            tracing::info!(
                route = route.as_str(),
                source_chain = route.source_chain().as_str(),
                destination_chain = route.destination_chain().as_str(),
                implemented = route.as_direction().is_some(),
                enabled = route_gate.is_enabled(&gate_ledger, route),
                "bridge route admission state"
            );
        }
    }

    // The two Solana<->Robinhood folds, wired ONLY where the route is
    // priced. An unpriced cross route folds nothing and pays nothing: a
    // Robinhood-bound Solana deposit then folds as `SolToGlc` exactly as
    // before the route existed, and a finalized `RhnToSol` observation
    // stays recorded and unfolded. Both folds park rather than pay while
    // the route gate is closed, so wiring them opens nothing.
    for route in [
        glc_reserve_bridge_service::routes::Route::SolToRhn,
        glc_reserve_bridge_service::routes::Route::RhnToSol,
    ] {
        match config.route_fees.get(route) {
            Some(fee_bps) => {
                let fold = glc_reserve_bridge_service::orchestrator::CrossRouteFold {
                    fee_bps,
                    route_gate: Arc::clone(&route_gate),
                };
                orchestrator = match route {
                    glc_reserve_bridge_service::routes::Route::SolToRhn => {
                        orchestrator.with_sol_to_rhn(fold)
                    }
                    _ => orchestrator.with_rhn_to_sol(fold),
                };
                tracing::info!(
                    route = route.as_str(),
                    fee_bps,
                    "cross-route fold wired — deposits on this route are folded (parked while \
                     the route gate is closed)"
                );
            }
            None => tracing::info!(
                route = route.as_str(),
                "cross-route fold NOT wired: no [fees] entry for this route, so its deposits \
                 are not classified or folded"
            ),
        }
    }

    // The Robinhood health state is created HERE, before the health
    // endpoint is served, rather than inside the indexer task below.
    //
    // It has to be: `/health` must be able to report "configured but not
    // ticking yet" and "not configured at all" from the first scrape, and
    // a state object created inside the task would not exist until the
    // task did. `unconfigured()` is a real, permanent value — not a
    // placeholder — for a deployment with no `[robinhood.indexer]`.
    let robinhood_health = match &config.robinhood_indexer {
        // The RPC URL is handed over ONLY so the health state can strip
        // it, and any credential embedded in it, out of every error it
        // publishes (`robinhood::redact`). It is never stored as a
        // readable field and never leaves this call.
        Some(rhn_config) => {
            robinhood::RobinhoodHealth::new(rhn_config.chain_id, &rhn_config.rpc_url, now_unix())
        }
        None => robinhood::RobinhoodHealth::unconfigured(),
    };

    // Authorization signers are loaded ONCE, here, and used twice: their
    // count feeds the health surface, and the signers themselves move
    // into the settlement engine below. Loading them twice would mean
    // connecting to every custody domain twice and, worse, could report a
    // quorum on the health surface that the engine does not actually
    // hold.
    let robinhood_auth_signers = match &config.robinhood_settlement {
        None => Vec::new(),
        Some(_) => or_exit(
            config.load_robinhood_auth_signers().await,
            "load the Robinhood authorization signers",
        ),
    };
    let robinhood_signers_available = robinhood_auth_signers.len();
    if config.robinhood_settlement.is_some() {
        if robinhood_signers_available < robinhood::SIGNER_THRESHOLD {
            // Fail-closed and LOUD rather than silently never settling: an
            // operator who configured settlement asked for this deployment
            // to be able to act.
            //
            // No longer expected in production: the v2 EIP-712 signer
            // protocol exists (`signing::remote`), so reaching here in
            // production mode means
            // `[[robinhood.settlement.auth_remote_signers]]` names fewer
            // than a quorum's worth of custody domains.
            tracing::warn!(
                available = robinhood_signers_available,
                required = robinhood::SIGNER_THRESHOLD,
                mode = ?config.operators.mode,
                "fewer Robinhood authorization signers than a quorum requires — no Robinhood \
                 operation can be authorized, and nothing will be broadcast"
            );
        } else {
            tracing::info!(
                available = robinhood_signers_available,
                required = robinhood::SIGNER_THRESHOLD,
                mode = ?config.operators.mode,
                "Robinhood authorization signers loaded — each is a separate custody domain \
                 that receives the STRUCTURED authorization and derives the EIP-712 digest \
                 itself; this process never asks any of them to sign bare bytes"
            );
        }
    }

    let robinhood_deployment_verified = robinhood_deployment.is_some();

    let collector = {
        let base = OpsCollector::new(
            config.service.db_path.clone(),
            orchestrator.goldcoin_indexer_status(),
            orchestrator.solana_indexer_status(),
        );
        // Attached only when Robinhood is configured at all. A deployment
        // that never was produces exactly the report it always did — no
        // Robinhood invariant, no Robinhood gauge, no reserve row.
        match &config.robinhood_indexer {
            None => Arc::new(base),
            Some(_) => Arc::new(
                base.with_robinhood(glc_reserve_bridge_service::ops::collector::RobinhoodOps {
                    health: Arc::clone(&robinhood_health),
                    route_gate: Arc::clone(&route_gate),
                    settlement_configured: config.robinhood_settlement.is_some(),
                    deployment_verified: robinhood_deployment_verified,
                    signers_available: robinhood_signers_available,
                    signers_required: robinhood::SIGNER_THRESHOLD,
                    submitter: config
                        .robinhood_settlement
                        .as_ref()
                        .map(|s| (s.submitter_address, s.chain_id.get())),
                }),
            ),
        }
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let health_addr: SocketAddr = config.service.health_bind_addr;
    let health_shutdown_rx = shutdown_rx.clone();
    let health_task = tokio::spawn(async move {
        if let Err(e) = health::serve(health_addr, collector, health_shutdown_rx).await {
            tracing::error!(error = %e, "health/metrics endpoint exited with an error");
        }
    });

    // Read-only Robinhood contract state for the two public Robinhood
    // endpoints, built only when a `[robinhood.settlement]` section names
    // a contract to read AND a `[robinhood.indexer]` section names the
    // endpoint to read it over. Without both, the public endpoints report
    // `"not_configured"` — never zeroes, and never a figure borrowed from
    // the Solana `BridgeConfig`.
    //
    // Its own RPC client, not one shared with the indexer or the
    // submitter: a public read must never be able to consume a connection
    // or a timeout budget that a settlement path is depending on. It
    // holds no key and calls only `eth_call`, so it cannot write to the
    // chain, and it does not touch the route gate, so it cannot open a
    // route.
    let robinhood_public_contract: Option<Arc<dyn robinhood::public::RobinhoodContractSource>> =
        match (&config.robinhood_indexer, &config.robinhood_settlement) {
            (Some(indexer_cfg), Some(settlement_cfg)) => {
                let rpc = or_exit(
                robinhood::rpc::EvmRpcClient::new(&robinhood::rpc::EvmRpcConfig {
                    url: indexer_cfg.rpc_url.clone(),
                    connect_timeout_ms: indexer_cfg.request_timeout_ms,
                    read_timeout_ms: indexer_cfg.request_timeout_ms,
                }),
                "construct the Robinhood EVM RPC client for the public reserve/limits endpoints",
            );
                Some(Arc::new(
                    robinhood::public::LiveRobinhoodContractSource::new(
                        rpc,
                        settlement_cfg.bridge_contract,
                    ),
                ))
            }
            _ => None,
        };

    let api_task = config.service.api_bind_addr.map(|api_addr| {
        let api_source = Arc::new(
            BridgeApi::new(
                config.service.db_path.clone(),
                RealSolanaRpc::new(config.solana.rpc_url.clone()),
                vault_address,
                root_vault_for_api,
                config.goldcoin.network,
                config.service.reservation_ttl_secs,
                i64::from(config.goldcoin.confirmation_depth),
                orchestrator.goldcoin_indexer_status(),
                orchestrator.solana_indexer_status(),
                Arc::clone(&route_gate),
                config.route_fees.clone(),
            )
            .with_robinhood(Arc::clone(&robinhood_health), robinhood_public_contract),
        );
        let api_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = api::serve(api_addr, api_source, api_shutdown_rx).await {
                tracing::error!(error = %e, "bridge API exited with an error");
            }
        })
    });

    // The authenticated admin control plane (admin_api module docs):
    // read-only reserve/on-chain views plus the local Ledger mutations
    // glc-admin already supports. Holds no keys — the on-chain admin
    // keypair stays CLI-only on the operator's machine. Operator tokens
    // are resolved from their env vars HERE, not in Config::load, so
    // glc-admin's --config recovery commands never need them; the daemon
    // still fails closed before serving a single request if any is
    // missing, empty, or duplicated.
    let admin_task = config.service.admin_bind_addr.map(|admin_addr| {
        // The ONE fund-moving capability on this API, wired here and
        // nowhere else: only the daemon owns the vault signer clients, so
        // only the daemon can grant it. `glc-admin` calls the endpoint; it
        // never holds a signer or a key.
        //
        // Refund execution is additionally gated per-operator by
        // `may_execute_glc_refunds`, and refuses outright if no operator
        // has it — see admin_api's route.
        let refund_executor = Arc::new(glc_refund_exec::DaemonGlcRefundExecutor::new(
            config.service.db_path.clone(),
            glc_refund_exec::RealGoldcoinRefundRpc(Arc::clone(&goldcoin_rpc_for_refunds)),
            RealSolanaRpc::new(config.solana.rpc_url.clone()),
            vault_for_refunds.clone(),
            refund_payout_policy,
            config.goldcoin.network,
            config.goldcoin.vault_min_confirmations,
            Arc::clone(&vault_signers_for_refunds),
            config.operators.vault_threshold as usize,
            Duration::from_millis(config.service.signer_timeout_ms),
        ));
        let admin_api_base = AdminApi::new(
            config.service.db_path.clone(),
            RealSolanaRpc::new(config.solana.rpc_url.clone()),
        )
        .with_refund_executor(refund_executor)
        .with_route_fees(config.route_fees.clone());
        // Attached only when Robinhood is configured. It grants no
        // capability — the admin API remains structurally incapable of
        // broadcasting a Robinhood transaction, and `glc-admin
        // robinhood-refund` stays the only place one can be executed.
        let admin_api_base = match &config.robinhood_indexer {
            None => admin_api_base,
            Some(_) => {
                let snapshot = robinhood_health.snapshot();
                admin_api_base.with_robinhood(admin_api::RobinhoodAdminContext {
                    route_gate: Arc::clone(&route_gate),
                    readiness: robinhood::admin::RobinhoodReadiness {
                        deployment_verified: robinhood_deployment_verified,
                        signers_available: robinhood_signers_available,
                        signers_required: robinhood::SIGNER_THRESHOLD,
                        halted: snapshot.halt.as_ref().map(|h| h.reason),
                        chain_id_disagrees: match (
                            snapshot.expected_chain_id,
                            snapshot.observed_chain_id,
                        ) {
                            (Some(expected), Some(observed)) => expected != observed,
                            _ => false,
                        },
                        never_connected: !snapshot.connected,
                        // Read per-request by the admin API rather than
                        // frozen at startup would be better, but the
                        // reserve's configured-ness cannot change without
                        // a restart and its pause is reported separately
                        // by `/reserve-health`; this is the startup fact.
                        reserve_paused: false,
                        reserve_unconfigured: config.reserve.robinhood.is_none(),
                    },
                    // Copied from the already-validated config rather
                    // than re-read or re-parsed: possessing a
                    // `ChainPolicy` is the evidence its checks passed,
                    // and a second parse here could disagree with the one
                    // the rest of the process prices and preflights
                    // against. `None` when no `[robinhood.policy]`
                    // section was configured — the same deployment state
                    // the startup log above already names.
                    policy: config
                        .chain_policies
                        .get(glc_reserve_bridge_service::routes::Chain::Robinhood)
                        .copied(),
                })
            }
        };
        let admin_source = Arc::new(admin_api_base);
        let resolved = or_exit(
            admin_api::auth::resolve_operator_tokens(&config.service.admin_operators),
            "resolve admin operator tokens",
        );
        let registry = Arc::new(or_exit(
            OperatorRegistry::new(resolved),
            "build the admin operator registry",
        ));
        let admin_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) =
                admin_api::serve(admin_addr, admin_source, registry, admin_shutdown_rx).await
            {
                tracing::error!(error = %e, "admin API exited with an error");
            }
        })
    });

    // The Robinhood deposit indexer — spawned ONLY when
    // `[robinhood.indexer]` is configured, exactly like the optional
    // admin and alert listeners above. With no section there is no task,
    // no client, and no Robinhood RPC call: this whole block is skipped
    // and the daemon's behaviour is byte-for-byte what it was.
    //
    // It observes. It settles nothing, opens no route, holds no key, and
    // cannot broadcast a transaction (glc_reserve_bridge_service::
    // robinhood's module docs). A fault in it halts only itself and
    // pauses no reserve — the Solana<->Goldcoin loop below is unaffected.
    let robinhood_task = match config.robinhood_indexer.clone() {
        None => {
            tracing::info!(
                "no [robinhood.indexer] section — the Robinhood deposit indexer is not running \
                 in this process and no Robinhood RPC endpoint will be contacted"
            );
            None
        }
        Some(rhn_config) => {
            let rpc = or_exit(
                robinhood::rpc::EvmRpcClient::new(&robinhood::rpc::EvmRpcConfig {
                    url: rhn_config.rpc_url.clone(),
                    connect_timeout_ms: rhn_config.request_timeout_ms,
                    read_timeout_ms: rhn_config.request_timeout_ms,
                }),
                "construct the Robinhood EVM RPC client",
            );
            let health = Arc::clone(&robinhood_health);
            tracing::info!(
                chain_id = rhn_config.chain_id.get(),
                bridge_contract = %rhn_config.bridge_contract,
                // Carried and reported, NOT verified against the chain in
                // this phase: proving the deployed contract's TOKEN
                // equals this needs `eth_call`, which the read-only RPC
                // client deliberately does not implement.
                expected_token = %rhn_config.expected_token,
                start_block = rhn_config.start_block,
                confirmation_depth = rhn_config.confirmation_depth,
                max_log_block_range = rhn_config.max_log_block_range,
                "Robinhood deposit indexer configured — OBSERVATION ONLY: no route is enabled, \
                 nothing is settled, and this process cannot sign or broadcast a Robinhood \
                 transaction"
            );
            let loop_config = robinhood::daemon::RobinhoodLoopConfig {
                tick_interval: Duration::from_millis(rhn_config.poll_interval_ms),
                max_backoff: Duration::from_secs(60),
            };
            let mut indexer = robinhood::RobinhoodIndexer::new(
                rpc,
                open_ledger(&config.service.db_path),
                rhn_config,
                health,
            );
            let rhn_shutdown_rx = shutdown_rx.clone();
            let task = tokio::spawn(async move {
                let ticks =
                    robinhood::daemon::run(&mut indexer, loop_config, rhn_shutdown_rx, now_unix)
                        .await;
                tracing::info!(ticks, "Robinhood indexer loop stopped");
            });
            Some(task)
        }
    };

    // The Robinhood RESERVE RECONCILIATION loop — spawned when
    // `[robinhood.indexer]` names an endpoint AND `[reserve.robinhood]`
    // created a reserve row. Deliberately NOT conditional on
    // `[robinhood.settlement]`: reading `balanceOf` needs a token, a
    // holder and an endpoint, all three of which come from the indexer
    // section, and requiring settlement config would have meant the only
    // way to make the reserve report its true balance was to also hand
    // this process a submitter key and an authorization quorum. See
    // `robinhood::reserve`'s module docs.
    //
    // Without it the reserve row keeps the `initial_balance: 0` seeded
    // above forever — the reason `glc-admin robinhood-status` reported a
    // zero balance, a negative available capacity and a false invariant
    // against a bridge contract that was in fact funded.
    //
    // Read-only by type: the reconciler's RPC bound is `EvmRpc +
    // EvmCallRpc`, with `EvmSubmitRpc` absent, so nothing reachable from
    // this task can broadcast. It opens no route and settles nothing;
    // its single write is the shared `reconciliation::reconcile` every
    // reserve direction already goes through.
    let robinhood_reserve_task = match (&config.robinhood_indexer, &config.reserve.robinhood) {
        (Some(rhn_config), Some(_)) => {
            let rpc = or_exit(
                robinhood::rpc::EvmRpcClient::new(&robinhood::rpc::EvmRpcConfig {
                    url: rhn_config.rpc_url.clone(),
                    connect_timeout_ms: rhn_config.request_timeout_ms,
                    read_timeout_ms: rhn_config.request_timeout_ms,
                }),
                "construct the Robinhood EVM RPC client for reserve reconciliation",
            );
            tracing::info!(
                bridge_contract = %rhn_config.bridge_contract,
                expected_token = %rhn_config.expected_token,
                confirmation_depth = rhn_config.confirmation_depth,
                settlement_configured = config.robinhood_settlement.is_some(),
                "Robinhood reserve reconciliation configured — READ-ONLY: it calls balanceOf on \
                 the configured token for the configured bridge contract and feeds the result \
                 through the same reconciliation path the Goldcoin and Solana reserves use. It \
                 enables no route, requires no [robinhood.settlement] section, and cannot \
                 broadcast a transaction."
            );
            let reconciler = robinhood::ReserveReconciler::new(
                rpc,
                rhn_config.clone(),
                config.reserve.reconciliation_tolerance,
            );
            let mut reserve_ledger = open_ledger(&config.service.db_path);
            let loop_config = robinhood::daemon::RobinhoodLoopConfig {
                tick_interval: Duration::from_millis(rhn_config.poll_interval_ms),
                max_backoff: Duration::from_secs(60),
            };
            let rhn_reserve_shutdown_rx = shutdown_rx.clone();
            Some(tokio::spawn(async move {
                let ticks = robinhood::daemon::run_reserve_reconciliation(
                    &reconciler,
                    &mut reserve_ledger,
                    loop_config,
                    rhn_reserve_shutdown_rx,
                    now_unix,
                )
                .await;
                tracing::info!(ticks, "Robinhood reserve reconciliation loop stopped");
            }))
        }
        (Some(_), None) => {
            tracing::info!(
                "no [reserve.robinhood] section — there is no Robinhood reserve row, so nothing \
                 is reconciled and no balanceOf read is performed"
            );
            None
        }
        (None, _) => None,
    };

    // The Robinhood SETTLEMENT loop — spawned ONLY when preflight
    // produced a verified deployment, which requires
    // `[robinhood.settlement]` to be present and to agree with the
    // deployed contracts.
    //
    // Its own task, its own ledger handle, its own backoff. A Robinhood
    // incident cannot stall the Solana<->Goldcoin loop, and the two
    // interleave only through committed ledger transactions.
    let robinhood_settlement_task = match (robinhood_deployment, &config.robinhood_settlement) {
        (Some(verified), Some(settlement_cfg)) => {
            let submitter = or_exit(
                robinhood::Submitter::load(settlement_cfg),
                "load the Robinhood submitter key from the environment variable named in \
                 robinhood.settlement.submitter_key_env",
            );
            tracing::info!(
                submitter = %submitter.address(),
                "Robinhood submitter loaded — this account PAYS GAS and BROADCASTS. It holds no \
                 bridge authority: every value-moving call carries a 2-of-3 EIP-712 quorum in \
                 its calldata and the contract never reads msg.sender on an authorized path."
            );
            // Loaded and reported once, above.
            let auth_signers = robinhood_auth_signers;
            let rpc = or_exit(
                robinhood::rpc::EvmRpcClient::new(&robinhood::rpc::EvmRpcConfig {
                    url: config
                        .robinhood_indexer
                        .as_ref()
                        .expect("a verified deployment implies a configured indexer")
                        .rpc_url
                        .clone(),
                    connect_timeout_ms: config
                        .robinhood_indexer
                        .as_ref()
                        .expect("a verified deployment implies a configured indexer")
                        .request_timeout_ms,
                    read_timeout_ms: config
                        .robinhood_indexer
                        .as_ref()
                        .expect("a verified deployment implies a configured indexer")
                        .request_timeout_ms,
                }),
                "construct the Robinhood EVM RPC client for the settlement loop",
            );
            let settler = robinhood::Settler::new(
                rpc,
                submitter,
                auth_signers,
                verified,
                settlement_cfg.clone(),
                Duration::from_millis(config.service.signer_timeout_ms),
                config.goldcoin.network,
                config.goldcoin.required_payout_confirmations,
                // `RhnToGlc` specifically — the only route `tick_fold`
                // folds. `GlcToRhn` is priced where its requests are
                // created, from its own entry in the same table.
                or_exit(
                    config
                        .route_fees
                        .fee_bps(glc_reserve_bridge_service::routes::Route::RhnToGlc),
                    "resolving the RhnToGlc fee",
                ),
            );
            let mut settlement_ledger = open_ledger(&config.service.db_path);
            let loop_config = robinhood::daemon::RobinhoodLoopConfig {
                tick_interval: Duration::from_millis(config.service.tick_interval_ms),
                max_backoff: Duration::from_secs(60),
            };
            let gate = Arc::clone(&route_gate);
            let rhn_settle_shutdown_rx = shutdown_rx.clone();
            Some(tokio::spawn(async move {
                let ticks = robinhood::daemon::run_settlement(
                    &settler,
                    &mut settlement_ledger,
                    move |ledger, route| {
                        use glc_reserve_bridge_service::routes::Route;
                        match route {
                            // BOTH Goldcoin<->Robinhood routes must be
                            // open for the engine to act on either —
                            // unchanged: the fold phase serves RhnToGlc
                            // and the authorize phase serves both, and a
                            // per-phase gate would let one half of a
                            // deployment run while the other did not.
                            Route::GlcToRhn | Route::RhnToGlc => {
                                gate.is_enabled(ledger, Route::RhnToGlc)
                                    && gate.is_enabled(ledger, Route::GlcToRhn)
                            }
                            // Each Solana<->Robinhood route is gated on
                            // its own: closing one must stop exactly
                            // that route and nothing else.
                            Route::SolToRhn | Route::RhnToSol => gate.is_enabled(ledger, route),
                            Route::GlcToSol | Route::SolToGlc => false,
                        }
                    },
                    loop_config,
                    rhn_settle_shutdown_rx,
                    now_unix,
                )
                .await;
                tracing::info!(ticks, "Robinhood settlement loop stopped");
            }))
        }
        _ => None,
    };

    let alert_task = config.service.alert_webhook_url.clone().map(|webhook_url| {
        let alert_config = ops::alerting::AlertConfig {
            webhook_url,
            poll_interval: Duration::from_secs(config.service.alert_poll_interval_secs),
        };
        let alert_shutdown_rx = shutdown_rx.clone();
        let db_path = config.service.db_path.clone();
        tokio::spawn(ops::alerting::run(db_path, alert_config, alert_shutdown_rx))
    });

    let signal_shutdown_tx = shutdown_tx.clone();
    let signal_task = tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("shutdown signal received");
        let _ = signal_shutdown_tx.send(true);
    });

    let loop_config = DaemonLoopConfig {
        tick_interval: Duration::from_millis(config.service.tick_interval_ms),
        max_backoff: Duration::from_secs(60),
    };
    tracing::info!(
        tick_interval_ms = config.service.tick_interval_ms,
        health_addr = %health_addr,
        "glc-bridge-daemon starting"
    );
    let ticks_run = daemon::run(&mut orchestrator, loop_config, shutdown_rx, now_unix).await;
    tracing::info!(ticks_run, "tick loop stopped; shutting down");

    // The tick loop only stops once `shutdown_tx` is set — by the signal
    // task above in the normal case — so the health server is already
    // unwinding; make sure it's set regardless (e.g. if `run` somehow
    // returned on its own) and wait for the health task to actually exit
    // before this process does.
    let _ = shutdown_tx.send(true);
    let _ = health_task.await;
    if let Some(api_task) = api_task {
        let _ = api_task.await;
    }
    if let Some(admin_task) = admin_task {
        let _ = admin_task.await;
    }
    if let Some(alert_task) = alert_task {
        let _ = alert_task.await;
    }
    if let Some(robinhood_task) = robinhood_task {
        let _ = robinhood_task.await;
    }
    if let Some(task) = robinhood_settlement_task {
        let _ = task.await;
    }
    if let Some(task) = robinhood_reserve_task {
        let _ = task.await;
    }
    signal_task.abort();
}

/// Waits for either `SIGTERM` or `Ctrl+C` (`SIGINT`), whichever comes
/// first — both are graceful-shutdown requests to this process.
async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        let Ok(mut sigterm) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            std::future::pending::<()>().await;
            return;
        };
        sigterm.recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod log_filter_tests {
    use super::{log_filter, OWN_LOG_TARGETS};
    use tracing::{Level, Subscriber};
    use tracing_subscriber::layer::SubscriberExt;

    struct Probe;
    impl tracing::Callsite for Probe {
        fn set_interest(&self, _: tracing::subscriber::Interest) {}
        fn metadata(&self) -> &tracing::Metadata<'_> {
            unreachable!("the probe callsite is only used as an identifier")
        }
    }
    static CALLSITE: Probe = Probe;

    /// Whether a subscriber filtered by `log_filter(rust_log)` lets an
    /// event at `level` on `target` through — asked of the subscriber
    /// itself, exactly as the daemon's `tracing::error!` would be.
    fn enabled(rust_log: Option<&str>, target: &'static str, level: Level) -> bool {
        let fields = tracing::field::FieldSet::new(&[], tracing::callsite::Identifier(&CALLSITE));
        let meta = tracing::Metadata::new(
            "probe",
            target,
            level,
            None,
            None,
            None,
            fields,
            tracing::metadata::Kind::EVENT,
        );
        tracing_subscriber::registry()
            .with(log_filter(rust_log))
            .enabled(&meta)
    }

    /// The production override that hid the 2026-09-12 auto-pause.
    #[test]
    fn a_crate_scoped_rust_log_can_no_longer_silence_the_fail_safe() {
        let rust_log = Some("glc_reserve_bridge_service=debug,glc_bridge_daemon=debug");
        for target in OWN_LOG_TARGETS {
            assert!(enabled(rust_log, target, Level::ERROR), "{target} ERROR");
            assert!(enabled(rust_log, target, Level::INFO), "{target} INFO");
            assert!(
                !enabled(rust_log, target, Level::DEBUG),
                "{target} stays at info"
            );
        }
        assert!(enabled(
            rust_log,
            "glc_reserve_bridge_service::daemon",
            Level::DEBUG
        ));
        assert!(
            !enabled(rust_log, "hyper", Level::INFO),
            "unrelated targets stay off"
        );
    }

    #[test]
    fn an_explicit_directive_for_an_own_target_is_honoured_as_written() {
        let rust_log = Some("glc_bridge_daemon=info,robinhood_reserve=warn");
        assert!(enabled(rust_log, "robinhood_reserve", Level::ERROR));
        assert!(!enabled(rust_log, "robinhood_reserve", Level::INFO));
        assert!(
            enabled(rust_log, "auto_resume", Level::INFO),
            "the unnamed one is floored"
        );
        let rust_log = Some("glc_bridge_daemon=info,robinhood_reserve=trace");
        assert!(enabled(rust_log, "robinhood_reserve", Level::TRACE));
    }

    #[test]
    fn a_bare_level_already_covers_every_target_and_is_left_alone() {
        assert!(enabled(Some("debug"), "robinhood_reserve", Level::DEBUG));
        let rust_log = Some("warn,glc_bridge_daemon=info");
        assert!(enabled(rust_log, "robinhood_reserve", Level::ERROR));
        assert!(!enabled(rust_log, "robinhood_reserve", Level::INFO));
    }

    #[test]
    fn unset_or_unparsable_rust_log_falls_back_to_info_everywhere() {
        for value in [None, Some("this is not a directive ==")] {
            assert!(
                enabled(value, "robinhood_reserve", Level::INFO),
                "{value:?}"
            );
            assert!(
                enabled(value, "glc_bridge_daemon", Level::INFO),
                "{value:?}"
            );
            assert!(
                !enabled(value, "glc_bridge_daemon", Level::DEBUG),
                "{value:?}"
            );
        }
    }
}
