//! Production configuration loading (docs/15-post-phase6-audit.md, P0 item
//! 3). A single TOML file, plus a handful of environment-variable
//! overrides for values that legitimately vary per deployment (RPC
//! endpoints/credentials) without editing a checked-in file.
//!
//! # What this does NOT do
//!
//! It does not embed or generate production secrets. Signing-key material
//! is loaded from local files whose *paths* are named here — DEV/TEST
//! POSTURE ONLY (see `signing::attestation`/`signing::goldcoin_vault`
//! module docs and docs/12-management-decisions.md item 2). A real
//! HSM/KMS-backed signer is later, explicitly-approved work
//! (docs/15-post-phase6-audit.md P2) and will replace this loading path
//! entirely, not extend it — there is deliberately no "production mode"
//! flag here that changes what this loader does; it always loads
//! plaintext key files, and that is exactly why it must never be pointed
//! at real custody keys.
//!
//! # Fail closed on anything malformed
//!
//! [`Config::load`] validates every cross-reference it can before
//! returning: threshold/pubkey-count consistency, `critical_reserve >
//! protected_minimum` (the same invariant `Ledger::configure_reserve`
//! itself enforces), and commitment/network settings restricted to what
//! this service actually implements. `goldcoin.network` accepts
//! `"regtest"`/`"testnet"`/`"mainnet"` — all three have real, verified
//! `goldcoin::address` version bytes (docs/16-p0-checkpoint.md) — and
//! nothing else; an unrecognized value fails closed rather than silently
//! defaulting. A malformed config is refused at startup, never
//! silently defaulted or partially applied. Key-file loading
//! ([`Config::load_attestation_signers`]/[`Config::load_vault_signers`]/
//! [`Config::load_submitter`]) cross-checks every loaded key's public
//! half against the pubkey this same config file declares at that
//! position, and refuses to proceed on any mismatch — the file naming a
//! set of attestation/vault pubkeys is itself a safety property (an
//! operator reading the file can see which keys are supposed to be in
//! play), and silently substituting whatever a key file actually
//! contains would quietly defeat that.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use crate::signing::attestation::DevAttestationSigner;
use crate::signing::goldcoin_vault::DevVaultSigner;
use crate::signing::remote::{
    RemoteAttestationSigner, RemoteEvmAuthSigner, RemoteSignerConfig, RemoteVaultSigner,
};
use crate::signing::signers::{AttestationSigner, VaultSigner};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid config field `{field}`: {detail}")]
    Invalid { field: &'static str, detail: String },
    #[error("could not read key file {path}: {source}")]
    KeyFileRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("key file {path} is malformed: {detail}")]
    KeyFileMalformed { path: PathBuf, detail: String },
    #[error(
        "key file {path} holds pubkey {actual}, but the config declares {expected} at the same \
         position — refusing to start with mismatched key material"
    )]
    KeyMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    /// `operators.mode = "production"` but `attestation_key_paths`/
    /// `vault_key_paths` (local plaintext dev/test signer files) are
    /// still populated — the exact refuse-to-start guard docs/22-
    /// production-readiness-review.md's P0-1 item requires: production
    /// mode must never even be capable of loading a local plaintext
    /// signer file, not just prefer not to.
    #[error(
        "operators.mode = \"production\" but {field} is non-empty — production mode refuses to \
         start with any local plaintext dev/test signer file configured"
    )]
    ProductionModeForbidsLocalSigners { field: &'static str },
    /// `operators.mode = "dev"` but remote-signer endpoints are
    /// configured — the same fail-closed discipline in the other
    /// direction: never silently ignore a configured remote signer
    /// because dev mode happened to be selected.
    #[error(
        "operators.mode = \"dev\" but {field} is non-empty — dev mode never uses remote signer \
         endpoints; remove them or set operators.mode = \"production\""
    )]
    DevModeForbidsRemoteSigners { field: &'static str },
    #[error(
        "{field} has {actual} entr{ies}, but {pubkeys_field} declares {expected} pubkey(s) — \
         these must be 1:1, same as the dev-mode key-path lists"
    )]
    RemoteSignerCountMismatch {
        field: &'static str,
        pubkeys_field: &'static str,
        expected: usize,
        actual: usize,
        ies: &'static str,
    },
    #[error(
        "{field}[{index}].expected_public_key ({declared}) does not match {pubkeys_field}[{index}] \
         ({from_pubkeys}) — these must agree; the pubkeys list and the remote-signer list are \
         cross-checked against each other by design, never silently trusted from just one"
    )]
    RemoteSignerExpectedKeyMismatch {
        field: &'static str,
        pubkeys_field: &'static str,
        index: usize,
        declared: String,
        from_pubkeys: String,
    },
    /// Two remote-signer entries in the same group claim the same
    /// `expected_public_key`. Production's 2-of-3 threshold assumes
    /// three genuinely separate custody domains (docs/02-trust-model.md,
    /// docs/12-management-decisions.md item 2) — a duplicated pubkey
    /// means at most two (or one) domains actually exist, silently
    /// weakening the threshold below what the config file claims.
    #[error(
        "{field} has duplicate expected_public_key {pubkey} at indices {first_index} and \
         {dup_index} — production custody domains must be genuinely independent; two remote \
         signers claiming the same public key are not separate custody domains, and this \
         silently weakens the configured threshold"
    )]
    DuplicateRemoteSignerPubkey {
        field: &'static str,
        pubkey: String,
        first_index: usize,
        dup_index: usize,
    },
    /// Two remote-signer entries in the same group share an
    /// `endpoint_url`. Even with distinct keys, a shared endpoint is a
    /// shared network/operational compromise blast radius, which
    /// defeats the "genuinely separate custody domain" requirement just
    /// as surely as a shared key would.
    #[error(
        "{field} has duplicate endpoint_url {endpoint_url:?} at indices {first_index} and \
         {dup_index} — production custody domains must be reachable via independent endpoints; \
         two remote-signer entries pointing at the same URL share a compromise blast radius \
         regardless of which public key each one claims"
    )]
    DuplicateRemoteSignerEndpoint {
        field: &'static str,
        endpoint_url: String,
        first_index: usize,
        dup_index: usize,
    },
    #[error("could not connect to remote signer {endpoint_url} ({field}[{index}]): {source}")]
    RemoteSignerConnect {
        field: &'static str,
        index: usize,
        endpoint_url: String,
        #[source]
        source: crate::signing::remote::RemoteSignerConfigError,
    },
}

// ------------------------------------------------------------- raw/TOML --

#[derive(Debug, Deserialize)]
struct RawConfig {
    solana: RawSolana,
    goldcoin: RawGoldcoin,
    reserve: RawReserve,
    operators: RawOperators,
    service: RawService,
    /// OPTIONAL, and optional is the point: every production config file
    /// in existence has no `[robinhood]` section, and must keep loading
    /// byte-for-byte unchanged. `None` resolves to both Robinhood routes
    /// disabled (`crate::routes::Route::default_enabled`), which is the
    /// same answer an explicitly-present-but-all-false section gives.
    ///
    /// There is deliberately no chain-parameter field here — no RPC URL,
    /// no chain id, no token contract, no decimals, no reserve address.
    /// Those are unresolved pending verified network information
    /// (`crate::chains::robinhood::UNRESOLVED_CHAIN_PARAMETERS`), and a
    /// config field for a value nobody has verified is an invitation to
    /// fill it in with a guess.
    #[serde(default)]
    robinhood: Option<RawRobinhood>,
    /// OPTIONAL `[fees]` — one basis-point rate per EXECUTABLE route,
    /// keyed by the route's wire spelling:
    ///
    /// ```toml
    /// [fees]
    /// GlcToSol = 300
    /// SolToGlc = 300
    /// GlcToRhn = 600
    /// RhnToGlc = 600
    /// ```
    ///
    /// Absent — which is every production config file that exists today —
    /// resolves through the documented migration fallback in
    /// [`resolve_route_fees`], which reproduces today's economics exactly.
    /// PRESENT means it is authoritative and must name every executable
    /// route: a partial table is refused rather than half-filled from the
    /// fallback, because a table that names three of four routes is far
    /// more likely to be an unfinished edit than a deliberate one. The
    /// one exception is the pair of Solana<->Robinhood routes, which may
    /// be omitted while they are disabled in `[robinhood]` (they became
    /// executable after existing tables were written) and are mandatory
    /// the moment either is enabled — see `crate::fees`.
    ///
    /// A `BTreeMap<String, u64>` rather than four named fields, so a
    /// future executable route is priced by adding a line here and
    /// nothing else. The keys are parsed as `crate::routes::Route`, so an
    /// unknown or non-executable name is a config error naming itself.
    #[serde(default)]
    fees: Option<std::collections::BTreeMap<String, u64>>,
    /// OPTIONAL `[rapid_burst]` — the deterministic anti-abuse rule
    /// (`ledger::rapid_burst`, schema v30). Absent, or present with
    /// `enabled = false` (the default), means NO rule: every fold
    /// behaves exactly as before v30. See [`RawRapidBurst`].
    #[serde(default)]
    rapid_burst: Option<RawRapidBurst>,
}

/// The `[rapid_burst]` section — every threshold of the rapid-burst hold
/// (`docs/09-runbook.md`, "Rapid-burst hold"). Each rule is scoped to an
/// identity the bridge itself observed and counts requests created
/// inside a rolling `window_secs` window; a deposit that would take an
/// identity past its maximum is held for an explicit operator decision,
/// normally not before `minimum_review_hold_secs` have elapsed.
///
/// ```toml
/// [rapid_burst]
/// enabled = true
/// window_secs = 900
/// max_per_source_wallet = 3
/// max_per_destination_wallet = 3
/// max_per_pair = 2
/// minimum_review_hold_secs = 259200   # 72 h — a MINIMUM review hold, not a refund timer
/// ```
///
/// Defaults are chosen so that `[rapid_burst]` with only `enabled = true`
/// is a sane policy; `enabled` itself defaults to `false` so a config
/// file that never mentions the section is unaffected.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRapidBurst {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_rapid_burst_window_secs")]
    window_secs: i64,
    #[serde(default = "default_rapid_burst_max_per_wallet")]
    max_per_source_wallet: u32,
    #[serde(default = "default_rapid_burst_max_per_wallet")]
    max_per_destination_wallet: u32,
    #[serde(default = "default_rapid_burst_max_per_pair")]
    max_per_pair: u32,
    #[serde(default = "default_rapid_burst_minimum_review_hold_secs")]
    minimum_review_hold_secs: i64,
}

fn default_rapid_burst_window_secs() -> i64 {
    900
}
fn default_rapid_burst_max_per_wallet() -> u32 {
    3
}
fn default_rapid_burst_max_per_pair() -> u32 {
    2
}
/// 72 hours.
fn default_rapid_burst_minimum_review_hold_secs() -> i64 {
    72 * 3600
}

/// The `[robinhood]` section: four enable flags and nothing else.
///
/// Both default to `false`, so `[robinhood]` present but empty is
/// identical to the section being absent. Setting either to `true` is
/// necessary but NOT sufficient to open a route — the ledger and adapter
/// gates still apply, and the Phase-1
/// `crate::chains::robinhood::RobinhoodAdapter` refuses unconditionally.
#[derive(Debug, Deserialize)]
struct RawRobinhood {
    #[serde(default)]
    glc_to_rhn_enabled: bool,
    #[serde(default)]
    rhn_to_glc_enabled: bool,
    /// Solana → Robinhood. Present so the route is nameable and its
    /// disabled state is explicit; the Solana and Goldcoin adapters both
    /// refuse it regardless of this flag.
    #[serde(default)]
    sol_to_rhn_enabled: bool,
    /// Robinhood → Solana. Same as above.
    #[serde(default)]
    rhn_to_sol_enabled: bool,
    /// OPTIONAL `[robinhood.indexer]`. Absent means no Robinhood indexer
    /// exists in this process at all: no RPC client is constructed, no
    /// task is spawned, and no Robinhood endpoint is ever contacted.
    ///
    /// Entirely independent of the four flags above. Enabling the
    /// indexer does NOT enable a route — it only starts WATCHING the
    /// chain, and the three gates in `crate::routes::RouteGate` are
    /// untouched by it. That separation is deliberate: observing a chain
    /// and transacting on it are different privileges and must be
    /// configured separately.
    #[serde(default)]
    indexer: Option<RawRobinhoodIndexer>,
    /// OPTIONAL `[robinhood.settlement]`. Absent means this process
    /// cannot settle on Robinhood at all: no submitter key is read from
    /// the environment, no authorization signer is constructed, and no
    /// code path can reach `eth_sendRawTransaction`.
    ///
    /// Independent of the four flags above and STRICTLY STRONGER than
    /// them: a flag says an operator is willing to open a route, this
    /// section says the process is equipped to. Present without
    /// `[robinhood.indexer]` is refused — a deployment that cannot
    /// observe Robinhood deposits must not be able to settle them.
    #[serde(default)]
    settlement: Option<RawRobinhoodSettlement>,
    /// OPTIONAL `[robinhood.policy]` — the approved launch policy: the
    /// fee rate and the transfer ceilings for this chain.
    ///
    /// Absent means exactly what it meant before the section existed:
    /// Robinhood requests price at the compiled-in
    /// `amount_conversion::BRIDGE_FEE_BPS`, and no backend limit is
    /// stated, so the preflight limit checks report UNVERIFIED rather
    /// than passing. Every production config file in existence has no
    /// `[robinhood]` section at all and keeps loading unchanged.
    ///
    /// Independent of the four route flags, of `[robinhood.indexer]` and
    /// of `[robinhood.settlement]`: stating the commercial terms a chain
    /// launches under is not the same act as observing that chain,
    /// settling on it, or opening a route to it.
    #[serde(default)]
    policy: Option<RawChainPolicy>,
}

/// A `[<chain>.policy]` section. Every field is REQUIRED, same discipline
/// as every other Robinhood section: a defaulted fee rate would price
/// real money at a number nobody chose, and a defaulted ceiling would
/// state a limit nobody approved.
///
/// Amounts are canonical 8-decimal atomic units — the unit every ledger
/// figure in this service already uses (`amount_conversion`'s module
/// docs). `2000000000000` is 20,000 GLC. They are NOT the destination
/// chain's native units, and the conversion to those is made once, in
/// that chain's binding module, where it is checked for exactness and
/// overflow.
#[derive(Debug, Deserialize)]
struct RawChainPolicy {
    /// Basis points, e.g. `600` for 6%. Any rate in
    /// `fees::MIN_FEE_BPS..=fees::MAX_FEE_BPS` (0..=9999) is accepted —
    /// there is no list of previously-charged rates and no rebuild
    /// involved in moving between them.
    ///
    /// This is the LEGACY per-chain fee. It prices the Robinhood routes
    /// only for a config with no `[fees]` section (the documented
    /// migration fallback); once `[fees]` exists, that table is what
    /// prices every route.
    fee_bps: u64,
    /// The largest single transfer, canonical 8dp. Checked against the
    /// deployed contract's own `inboundMax`/`outboundMax` at preflight —
    /// the contract is the enforcement layer, this is the statement of
    /// what it is believed to hold.
    per_transfer_limit: u64,
    /// The STRICT 24-hour ceiling, canonical 8dp: the most that may move
    /// in any 86,400-second span. This is NOT the number that goes on
    /// chain — `GlcRobinhoodBridge`'s rolling window is a fixed bucket,
    /// so the contract must be configured with exactly HALF of this. See
    /// `robinhood::policy` for the derivation and the check.
    rolling_daily_limit: u64,
}

/// The `[robinhood.settlement]` section. Every field is REQUIRED — same
/// discipline as `[robinhood.indexer]`, and for the same reason: a
/// guessed settlement parameter is worse than an absent one.
///
/// See `crate::robinhood::settlement_config` for what each field means
/// and why none has a default — in particular `tx_envelope`, the one
/// chain property this repository had no evidence for, which is
/// configured AND verified against the chain at startup.
#[derive(Debug, Deserialize)]
struct RawRobinhoodSettlement {
    /// Must equal `[robinhood.indexer].chain_id`.
    chain_id: u64,
    /// Must equal `[robinhood.indexer].bridge_contract`.
    bridge_contract: String,
    /// `"legacy"` or `"eip1559"`. NO DEFAULT, and cross-checked against
    /// the chain's own `baseFeePerGas` at preflight.
    tx_envelope: String,
    /// The NAME of the environment variable holding the submitter EOA's
    /// hex private key — never the key itself. Same "config names the
    /// env var" discipline as `RawRemoteSigner.auth_token_env`.
    submitter_key_env: String,
    /// The address that key is expected to control, stated independently
    /// and cross-checked against the loaded key at startup.
    submitter_address: String,
    /// The three addresses the deployed contract holds as its signer set.
    /// Cross-checked against the contract's own `signers()` at preflight.
    authorized_signers: Vec<String>,
    authorization_ttl_secs: u64,
    required_confirmations: u64,
    gas_limit_margin_percent: u64,
    max_gas_limit: u64,
    /// Wei. TOML has no unsigned 128-bit integer, so fee ceilings are
    /// decimal STRINGS: a wei value can exceed `i64::MAX` on a chain with
    /// an expensive gas token, and silently truncating one would sign a
    /// transaction at a price the operator did not choose.
    max_fee_per_gas_wei: String,
    priority_fee_wei: String,
    rebroadcast_after_secs: u64,
    max_replacements: u32,
    min_submitter_balance_wei: String,
    /// The dev-mode authorization signer key files, one per signer. Read
    /// only when `operators.mode = "dev"`; a production deployment leaves
    /// this empty and its authorization signers live in genuinely
    /// separate custody domains.
    #[serde(default)]
    dev_signer_key_paths: Vec<PathBuf>,
    /// `[[robinhood.settlement.auth_remote_signers]]` — the PRODUCTION
    /// authorization signers, one genuinely separate custody domain per
    /// entry, speaking the v2 EIP-712 protocol
    /// (`crate::signing::remote`'s module docs).
    ///
    /// Deliberately its own list rather than a reuse of
    /// `operators.attestation_remote_signers`/`vault_remote_signers`: an
    /// EVM authorization signer answers a different protocol on different
    /// paths, is identified by a 20-byte address rather than a public
    /// key, and — most importantly — must NOT be the same credential. A
    /// pool that could serve both would mean one compromised bearer token
    /// authorizing both a Goldcoin payout and a Robinhood one.
    ///
    /// Each entry's `expected_public_key` holds the signer's EVM ADDRESS
    /// (the field name is shared with the v1 signer table so operators
    /// have one shape to learn; the value is checked as an address here).
    /// Read only when `operators.mode = "production"`.
    #[serde(default)]
    auth_remote_signers: Vec<RawRemoteSigner>,
}

/// The `[robinhood.indexer]` section. Every field is REQUIRED — see
/// `crate::robinhood::config`'s module docs for why none of them has a
/// default.
#[derive(Debug, Deserialize)]
struct RawRobinhoodIndexer {
    rpc_url: String,
    /// The EIP-155 chain id, as a plain integer (`4663` for Robinhood
    /// mainnet, `46630` for its testnet — `crate::evm::networks`).
    /// Deliberately a number, not a network NAME: the name is a label
    /// this service would have to map, while the id is the value
    /// `eth_chainId` actually returns and the one that gets compared.
    chain_id: u64,
    /// `0x`-prefixed 20-byte address of the deployed `GlcRobinhoodBridge`.
    bridge_contract: String,
    /// `0x`-prefixed 20-byte address of the ERC-20 GLC it custodies.
    /// Recorded and reported; verifying it against the contract needs
    /// `eth_call`, which this phase's RPC client does not have.
    expected_token: String,
    /// The block to start from when the ledger holds no cursor.
    start_block: u64,
    confirmation_depth: u64,
    poll_interval_ms: u64,
    request_timeout_ms: u64,
    max_log_block_range: u64,
}

#[derive(Debug, Deserialize)]
struct RawSolana {
    rpc_url: String,
    commitment: String,
    reserve_token_mint: String,
}

#[derive(Debug, Deserialize)]
struct RawGoldcoin {
    network: String,
    rpc_url: String,
    #[serde(default)]
    rpc_user: String,
    #[serde(default)]
    rpc_password: String,
    #[serde(default = "default_connect_timeout_ms")]
    rpc_connect_timeout_ms: u64,
    #[serde(default = "default_read_timeout_ms")]
    rpc_read_timeout_ms: u64,
    confirmation_depth: u32,
    max_reorg_depth: u32,
    required_payout_confirmations: i64,
    vault_min_confirmations: i64,
    fee_rate_per_kb: u64,
    dust_threshold: u64,
    max_inputs: usize,
    /// Target size (canonical atomic units) for each deterministic change
    /// FAN-OUT output a Goldcoin payout produces (docs/09-runbook.md's
    /// "UTXO liquidity" section) — production-aware: sized relative to the
    /// current maximum net payout, not a stale historical limit. Defaults
    /// so an existing config file with none of these four new fields keeps
    /// loading and behaving sensibly unchanged.
    #[serde(default = "default_change_fanout_target_atomic")]
    change_fanout_target_atomic: u64,
    /// Hard cap on how many change outputs one payout may ever produce.
    #[serde(default = "default_change_fanout_max_outputs")]
    change_fanout_max_outputs: usize,
    /// Maximum unconfirmed OWN-payout ancestor depth for spending this
    /// service's own payout change at 0 confirmations (docs/09-runbook.md
    /// "Zero-conf payout change"): 0 disables the policy entirely; 1 (the
    /// default) allows exactly one unconfirmed hop (change whose only
    /// unconfirmed ancestor is its own parent payout). Applies ONLY to
    /// authoritative payout change; external deposits and vault-split
    /// outputs always wait `vault_min_confirmations` regardless.
    #[serde(default = "default_zero_conf_change_max_depth")]
    zero_conf_change_max_depth: u32,
    /// "depth_limited" (default — the exact pre-existing behavior and the
    /// rollback target) or "bridge_owned_recursive" (recursive reuse of
    /// verified bridge-created payout change, bounded by the chain limit
    /// below). In BOTH modes `zero_conf_change_max_depth = 0` stays the
    /// kill switch.
    #[serde(default = "default_zero_conf_change_mode")]
    zero_conf_change_mode: String,
    /// Recursive mode's chain bound (per-input depth cap AND
    /// per-transaction sum-of-depths budget). Validated 1..=24: must stay
    /// below the Goldcoin node's `-limitancestorcount` mempool policy
    /// (25 in-mempool ancestors including the transaction itself,
    /// Goldcoin Core v0.17 — verified against the production binary;
    /// exceeding it is rejected as `too-long-mempool-chain`). The default
    /// of 20 leaves a >= 4-transaction margin. Ignored in
    /// "depth_limited" mode.
    #[serde(default = "default_zero_conf_change_recursive_chain_limit")]
    zero_conf_change_recursive_chain_limit: u32,
    /// Mature, unreserved vault UTXOs that must remain after admitting one
    /// more SolToGlc obligation, or `Ledger::fold_sol_deposit` parks it
    /// (`utxo_liquidity_low_at_fold`) instead — see
    /// `Ledger::set_utxo_pool_thresholds`.
    #[serde(default = "default_utxo_pool_min_available_count")]
    utxo_pool_min_available_count: u32,
    /// Purely observational early-warning threshold (>=
    /// `utxo_pool_min_available_count`) — never itself gates admission.
    #[serde(default = "default_utxo_pool_warning_count")]
    utxo_pool_warning_count: u32,
    /// Confirmed-liquidity admission safety buffer, in Goldcoin atomic
    /// units (8 decimals) — the headroom BELOW which SolToGlc admission
    /// closes, sitting on top of (never replacing) the hard
    /// `reserve.goldcoin.protected_minimum`. Fed to
    /// `Ledger::set_admission_liquidity_thresholds` at startup, exactly
    /// like `utxo_pool_min_available_count`. `0` disables the whole
    /// mechanism. See docs/09-runbook.md's "Confirmed-liquidity admission
    /// safety buffer".
    #[serde(default = "default_admission_safety_buffer_atomic")]
    admission_safety_buffer_atomic: u64,
    /// The (higher) headroom at which SolToGlc admission automatically
    /// REOPENS. The gap between this and
    /// `admission_safety_buffer_atomic` is the hysteresis band inside
    /// which the gate holds its state — what makes flapping structurally
    /// impossible rather than merely unlikely. Validated `>=
    /// admission_safety_buffer_atomic` at load time.
    #[serde(default = "default_admission_reopen_headroom_atomic")]
    admission_reopen_headroom_atomic: u64,
    /// Upper bound on how many `ManualReview` requests
    /// `Orchestrator::tick_auto_resume_utxo_liquidity_backlog` resumes in
    /// a single tick — bounds worst-case tick duration on a large backlog
    /// and rate-limits how much demand re-enters the payout pipeline at
    /// once after a recovery.
    #[serde(default = "default_max_auto_resumes_per_tick")]
    max_auto_resumes_per_tick: usize,
    /// Whether the daemon automatically creates NEW splits of oversized
    /// mature root-vault UTXOs
    /// (`goldcoin::liquidity::run_shaping_tick`,
    /// docs/09-runbook.md's "Automatic UTXO liquidity shaping" section).
    /// Default `false` — autonomous vault self-spends are explicit
    /// opt-in, never switched on by a binary upgrade; production sets
    /// `true` via the REQUIRED pilot-template key. Lifecycle maintenance
    /// of splits that already exist runs regardless of this flag.
    #[serde(default = "default_utxo_shaping_enabled")]
    utxo_shaping_enabled: bool,
    /// Shaping trigger floor: an automatic split is only ever considered
    /// while the count of payout-ready mature UTXOs (amount >=
    /// `change_fanout_target_atomic / 2`) is BELOW this. A healthy pool
    /// (count at/above it) never produces a self-transaction.
    #[serde(default = "default_utxo_shaping_target_available_count")]
    utxo_shaping_target_available_count: u32,
    /// Minimum size for a mature root-vault UTXO to be an automatic-split
    /// candidate. Omitted (the default) it resolves to `4 *
    /// change_fanout_target_atomic` — at the shipped defaults, 20,000 GLC,
    /// i.e. roughly the per-transfer maximum: anything at or below one
    /// maximum payout is left for payouts to consume naturally. Must
    /// resolve to at least `2 * change_fanout_target_atomic` (a split
    /// needs at least two whole chunks).
    #[serde(default)]
    utxo_shaping_min_source_atomic: Option<u64>,
    /// Hard cap on chunk outputs per automatic split — bounds transaction
    /// size and shapes a very large UTXO gradually (each pass leaves
    /// chunks that are themselves re-split later, only if the pool thins
    /// again) instead of in one enormous transaction.
    #[serde(default = "default_utxo_shaping_max_outputs_per_split")]
    utxo_shaping_max_outputs_per_split: usize,
    /// Production initial-checkpoint bootstrap (docs/09-runbook.md
    /// "Goldcoin indexer initial checkpoint") — used ONLY when the
    /// ledger has no indexed Goldcoin blocks yet; ignored forever after
    /// that (`goldcoin::indexer::InitialCheckpoint`'s own docs). All
    /// three fields default to "absent"/`false` so every existing
    /// dev/test/regtest config keeps working unchanged.
    #[serde(default)]
    initial_checkpoint_height: Option<i64>,
    #[serde(default)]
    initial_checkpoint_hash: Option<String>,
    #[serde(default)]
    initial_checkpoint_operator_acknowledged_no_prior_deposits: bool,
}

/// 2,500 GLC — the UPGRADE-SAFE default, deliberately NOT the current
/// production value (2026-08-31 production-readiness review, M2): an
/// existing deployment whose config omits this key must keep behaving
/// exactly as it did before this binary, honoring the 2026-08-29 note
/// that the fan-out re-tune would never be changed silently. The
/// PRODUCTION value is 5,000 GLC — the explicit 2026-08-30 re-tune for
/// the 20,000 GLC per-transfer maximum (a 19,400 GLC net payout needs
/// ~4 chunks, comfortable margin below `max_inputs = 25`; 2,500 GLC
/// chunks rode the old `max_inputs = 10` edge into the incident's
/// `TooManyInputs` failures) — and is a REQUIRED explicit key in
/// `service/config.pilot-template.toml`. This is also the chunk target
/// `goldcoin::liquidity`'s automatic shaping splits to: one canonical
/// payout-chunk size for the whole crate, whatever the operator sets.
fn default_change_fanout_target_atomic() -> u64 {
    2_500 * 100_000_000
}
/// `false` — automatic vault self-spend transactions are EXPLICIT
/// OPT-IN, never something a binary upgrade switches on for a config
/// that predates the feature (2026-08-31 production-readiness review,
/// M2). Production enables it via the REQUIRED
/// `utxo_shaping_enabled = true` key in the pilot template. In-flight
/// split lifecycle maintenance (resume/confirm/re-broadcast of splits
/// that already exist) runs regardless of this flag — only the creation
/// of NEW automatic splits is gated.
fn default_utxo_shaping_enabled() -> bool {
    false
}
/// 15 — matches `default_utxo_pool_warning_count`: shaping starts
/// rebuilding the pool at the same point an operator would start seeing
/// the warning gauge, comfortably above the admission-backpressure floor
/// (10), so automatic restructuring normally restores liquidity before
/// backpressure ever engages.
fn default_utxo_shaping_target_available_count() -> u32 {
    15
}
/// 25 chunk outputs per split: one input + 25 P2SH outputs is well under
/// 2 KB of transaction, and a 1,000,000 GLC deposit shapes as 25 x
/// 40,000 GLC first (each itself a later split candidate, consumed only
/// as the pool actually thins) rather than as one 200-output
/// transaction.
fn default_utxo_shaping_max_outputs_per_split() -> usize {
    25
}
fn default_zero_conf_change_max_depth() -> u32 {
    1
}
fn default_zero_conf_change_mode() -> String {
    "depth_limited".to_string()
}
fn default_zero_conf_change_recursive_chain_limit() -> u32 {
    20
}
fn default_change_fanout_max_outputs() -> usize {
    10
}
/// 10 mature UTXOs — the verified-safe floor for the incident's own vault
/// shape (4,770 GLC chunks, 1,880 GLC maximum net payout, 20,000 GLC
/// protected minimum): empirically confirmed
/// (`service/tests/utxo_liquidity_production_tuning.rs::
/// test_prod_recommended_floor_10_survives_the_25_burst_with_margin`) to
/// engage backpressure with a full payout of margin before the hard
/// invariant's own 11-payout survival limit for that shape. `8` (this
/// default's previous value) was shown insufficient — it lets the hard
/// invariant breach one payout before count-based backpressure would ever
/// engage on its own
/// (`test_prod_defaults_floor_8_breaches_before_backpressure_engages`).
/// Recompute for a vault with a materially different chunk size or total
/// balance — see docs/09-runbook.md's "UTXO liquidity" tuning section.
fn default_utxo_pool_min_available_count() -> u32 {
    10
}
fn default_utxo_pool_warning_count() -> u32 {
    15
}
/// 250 000 GLC (8 decimals) — the production admission safety buffer.
/// A real default rather than `0`, matching how
/// `default_utxo_pool_min_available_count` ships a real floor: a
/// deployment that says nothing gets the protective posture, and
/// disabling the buffer has to be a deliberate, written-down `0`.
fn default_admission_safety_buffer_atomic() -> u64 {
    250_000 * 100_000_000
}
/// 350 000 GLC (8 decimals) — the production reopen threshold, 100 000
/// GLC above the close threshold. Sized so a reserve that has just closed
/// admission has to genuinely recover (roughly five maximum-size
/// transfers' worth of headroom) before taking on new demand, not merely
/// tick back over the line it fell under.
fn default_admission_reopen_headroom_atomic() -> u64 {
    350_000 * 100_000_000
}
/// 20 — comfortably above a single tick's realistic backlog for the
/// incident's own vault shape, while still bounding worst-case tick
/// duration if a much larger backlog ever accumulates. Revisit if real
/// operational experience shows a materially different backlog size.
fn default_max_auto_resumes_per_tick() -> usize {
    20
}
fn default_connect_timeout_ms() -> u64 {
    5_000
}
fn default_read_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Deserialize)]
struct RawReserveBounds {
    protected_minimum: u64,
    target_reserve: u64,
    warning_reserve: u64,
    critical_reserve: u64,
}

#[derive(Debug, Deserialize)]
struct RawReserve {
    reconciliation_tolerance: u64,
    solana: RawReserveBounds,
    goldcoin: RawReserveBounds,
    /// OPTIONAL `[reserve.robinhood]`. Absent means the Robinhood reserve
    /// is never configured, which is what every existing production
    /// config file resolves to — and an unconfigured reserve cannot be
    /// drawn against, because `Ledger::configure_reserve` was never
    /// called for it.
    ///
    /// Bounds are in CANONICAL 8-decimal units, like every other reserve
    /// row, NOT Robinhood's native 18 decimals — see
    /// `ReserveDirection::RobinhoodReserve`'s docs for why.
    #[serde(default)]
    robinhood: Option<RawReserveBounds>,
}

#[derive(Debug, Deserialize)]
struct RawOperators {
    /// `"dev"` or `"production"` — see [`SignerMode`]. Defaults to
    /// `"dev"` when omitted so every existing config (this project's own
    /// tests and any operator's config predating this field) keeps
    /// working unchanged; a real production deployment must set this
    /// explicitly, never rely on the default.
    #[serde(default = "default_signer_mode")]
    mode: String,
    admin_pubkey: String,
    attestation_threshold: usize,
    attestation_pubkeys: Vec<String>,
    #[serde(default)]
    attestation_key_paths: Vec<PathBuf>,
    #[serde(default)]
    attestation_remote_signers: Vec<RawRemoteSigner>,
    vault_threshold: u8,
    vault_pubkeys: Vec<String>,
    #[serde(default)]
    vault_key_paths: Vec<PathBuf>,
    #[serde(default)]
    vault_remote_signers: Vec<RawRemoteSigner>,
    submitter_key_path: PathBuf,
}

fn default_signer_mode() -> String {
    "dev".to_string()
}

/// One `[[operators.attestation_remote_signers]]`/
/// `[[operators.vault_remote_signers]]` TOML table — see
/// `signing::remote` module docs for the wire protocol this connects to.
/// `auth_token_env` is a NAME, never the secret itself — see that
/// module's `AuthToken` (constraint 9: no authentication secrets
/// committed to git).
#[derive(Debug, Clone, Deserialize)]
struct RawRemoteSigner {
    endpoint_url: String,
    /// Cross-checked against the positionally-matching entry in
    /// `attestation_pubkeys`/`vault_pubkeys` at `resolve()` time — see
    /// [`ConfigError::RemoteSignerExpectedKeyMismatch`]. Redundant by
    /// design: an operator can read either list and see the same
    /// identity, and a mismatch between them (a copy/paste error, a
    /// reordered list) is caught at config-load time rather than only
    /// discovered against the live endpoint.
    expected_public_key: String,
    auth_token_env: String,
    #[serde(default = "default_remote_signer_timeout_ms")]
    timeout_ms: u64,
}

fn default_remote_signer_timeout_ms() -> u64 {
    5_000
}

/// One authorized admin-API operator: an identity name (recorded as the
/// `actor` on every `admin_audit_log` row) and the NAME of the
/// environment variable holding that operator's bearer token — the same
/// "config names the env var, never the secret" discipline
/// `RawRemoteSigner.auth_token_env` uses.
#[derive(Debug, Deserialize)]
struct RawAdminOperator {
    name: String,
    token_env: String,
    /// Grants this operator the ONE fund-moving admin capability:
    /// `POST /refunds/glc/{id}/execute`. Defaults to `false`, so an
    /// existing deployment's operators gain nothing on upgrade and the
    /// capability must be granted deliberately, per person.
    #[serde(default)]
    may_execute_glc_refunds: bool,
}

#[derive(Debug, Deserialize)]
struct RawService {
    db_path: PathBuf,
    #[serde(default = "default_tick_interval_ms")]
    tick_interval_ms: u64,
    health_bind_addr: String,
    api_bind_addr: Option<String>,
    /// Where the authenticated admin API (`admin_api::serve`) listens —
    /// omit to not start it at all. Bind privately (localhost or an
    /// internal interface); never a public address.
    admin_bind_addr: Option<String>,
    /// Required (non-empty) whenever `admin_bind_addr` is set.
    #[serde(default)]
    admin_operators: Vec<RawAdminOperator>,
    /// How long a reservation made by `POST /transfers`
    /// (`api::BridgeApi::create_goldcoin_deposit_transfer`) holds capacity
    /// before it expires if no matching deposit ever arrives
    /// (docs/12-management-decisions.md item 7 — no default asserted;
    /// operators must decide this for their own deployment).
    reservation_ttl_secs: i64,
    /// Where to POST a JSON notification when a reserve direction
    /// transitions into a pause (ops::alerting) — omit to run without
    /// outbound alerting (matches `ops::health`'s own stance: no alerting
    /// integration is mandatory, an operator's own monitoring can poll
    /// `/health` instead).
    alert_webhook_url: Option<String>,
    #[serde(default = "default_alert_poll_interval_secs")]
    alert_poll_interval_secs: u64,
    /// Bounds each individual signer call (`signing::signers` module
    /// docs) — an operational tuning knob, not a safety-critical value,
    /// so (unlike reserve/rate-limit fields) a sensible default applies
    /// when omitted rather than requiring every deployment to set it.
    #[serde(default = "default_signer_timeout_ms")]
    signer_timeout_ms: u64,
}

fn default_alert_poll_interval_secs() -> u64 {
    30
}

fn default_signer_timeout_ms() -> u64 {
    10_000
}

fn default_tick_interval_ms() -> u64 {
    5_000
}

// -------------------------------------------------------------- resolved --

#[derive(Debug, Clone)]
pub struct SolanaConfig {
    pub rpc_url: String,
    pub reserve_token_mint: Pubkey,
}

#[derive(Debug, Clone)]
pub struct GoldcoinConfig {
    /// `"regtest"`/`"testnet"` both resolve to
    /// `goldcoin::address::Network::Testnet` (verified to share identical
    /// version bytes — docs/16-p0-checkpoint.md); the config file keeps
    /// the operator-facing distinction for clarity even though the
    /// address math doesn't need it.
    pub network: crate::goldcoin::address::Network,
    pub rpc_url: String,
    pub rpc_user: String,
    pub rpc_password: String,
    pub rpc_connect_timeout_ms: u64,
    pub rpc_read_timeout_ms: u64,
    pub confirmation_depth: u32,
    pub max_reorg_depth: u32,
    pub required_payout_confirmations: i64,
    pub vault_min_confirmations: i64,
    pub fee_rate_per_kb: u64,
    pub dust_threshold: u64,
    pub max_inputs: usize,
    pub change_fanout_target_atomic: u64,
    pub change_fanout_max_outputs: usize,
    pub zero_conf_change_max_depth: u32,
    pub zero_conf_change_mode: crate::goldcoin::payout::ZeroConfChangeMode,
    pub zero_conf_change_recursive_chain_limit: u32,
    pub utxo_pool_min_available_count: u32,
    pub utxo_pool_warning_count: u32,
    /// See `RawGoldcoin`'s matching fields and docs/09-runbook.md's
    /// "Confirmed-liquidity admission safety buffer". Validated
    /// `admission_reopen_headroom_atomic >= admission_safety_buffer_atomic`
    /// at load time; `admission_safety_buffer_atomic == 0` disables the
    /// mechanism entirely.
    pub admission_safety_buffer_atomic: u64,
    pub admission_reopen_headroom_atomic: u64,
    pub max_auto_resumes_per_tick: usize,
    pub utxo_shaping_enabled: bool,
    pub utxo_shaping_target_available_count: u32,
    /// Always resolved to a concrete value (`4 *
    /// change_fanout_target_atomic` when the raw field is omitted) and
    /// validated `>= 2 * change_fanout_target_atomic` at load time.
    pub utxo_shaping_min_source_atomic: u64,
    pub utxo_shaping_max_outputs_per_split: usize,
    /// See `RawGoldcoin`'s matching fields and
    /// `goldcoin::indexer::InitialCheckpoint`'s own docs. Structurally
    /// validated here (hex format, non-negative height); the live
    /// getblockhash/above-tip checks happen at indexer bootstrap time,
    /// since only that has a chain connection.
    pub initial_checkpoint: Option<crate::goldcoin::indexer::InitialCheckpoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveBounds {
    pub protected_minimum: u64,
    pub target_reserve: u64,
    pub warning_reserve: u64,
    pub critical_reserve: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ReserveConfig {
    pub reconciliation_tolerance: u64,
    pub solana: ReserveBounds,
    pub goldcoin: ReserveBounds,
    /// `None` when no `[reserve.robinhood]` section is present — the
    /// resolved state of every existing production config file.
    pub robinhood: Option<ReserveBounds>,
}

#[derive(Debug, Clone)]
pub struct OperatorsConfig {
    pub mode: SignerMode,
    pub admin_pubkey: Pubkey,
    pub attestation_threshold: usize,
    pub attestation_pubkeys: Vec<Pubkey>,
    pub attestation_key_paths: Vec<PathBuf>,
    pub attestation_remote_signers: Vec<RemoteSignerConfig>,
    pub vault_threshold: u8,
    pub vault_pubkeys: Vec<[u8; 33]>,
    pub vault_key_paths: Vec<PathBuf>,
    pub vault_remote_signers: Vec<RemoteSignerConfig>,
    pub submitter_key_path: PathBuf,
}

/// Which signer-loading path a deployment uses — see `Config::load_signers`.
/// Deliberately just these two: there is no "mixed" mode (e.g. some
/// attestation signers local, some remote) — a deployment is either
/// entirely dev/test-posture or entirely production-posture, so an
/// operator (or reviewer) can answer "is this a real deployment" by
/// reading one field, not by auditing every signer entry individually.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerMode {
    Dev,
    Production,
}

/// Return type of [`Config::load_signers`] — named so call sites (and
/// clippy) don't have to spell out the full nested `Vec<Box<dyn _>>`
/// tuple.
pub type LoadedSigners = (Vec<Box<dyn AttestationSigner>>, Vec<Box<dyn VaultSigner>>);

/// One declared admin-API operator: an identity name and the NAME of the
/// environment variable holding that operator's bearer token.
///
/// Deliberately NOT resolved at [`Config::load`] time: the token env vars
/// exist only in the daemon's environment, while the same config file is
/// also loaded by `glc-admin`'s `--config` recovery commands
/// (`retry-goldcoin-payout`, `split-vault-utxo`) from operator shells
/// that legitimately lack them — resolving eagerly would make the
/// stuck-payout tooling refuse to start during exactly the incidents it
/// exists for. The daemon resolves tokens via
/// [`crate::admin_api::auth::resolve_operator_tokens`] right before
/// starting the admin listener, failing closed there instead.
#[derive(Debug, Clone)]
pub struct AdminOperatorConfig {
    pub name: String,
    pub token_env: String,
    /// See [`RawAdminOperator::may_execute_glc_refunds`]. Ordinary admin
    /// access is deliberately NOT sufficient to move funds: every other
    /// admin operation is a read or a local ledger mutation, so a leaked
    /// read-only token cannot spend from the vault.
    pub may_execute_glc_refunds: bool,
}

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub db_path: PathBuf,
    pub tick_interval_ms: u64,
    /// Serves both `/health` and `/metrics` — see `ops::health::serve`,
    /// which already exposes them on one listener.
    pub health_bind_addr: SocketAddr,
    pub api_bind_addr: Option<SocketAddr>,
    pub admin_bind_addr: Option<SocketAddr>,
    /// Empty exactly when `admin_bind_addr` is unset — `resolve()`
    /// enforces that a configured admin listener has at least one
    /// operator.
    pub admin_operators: Vec<AdminOperatorConfig>,
    pub reservation_ttl_secs: i64,
    pub alert_webhook_url: Option<String>,
    pub alert_poll_interval_secs: u64,
    pub signer_timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub solana: SolanaConfig,
    pub goldcoin: GoldcoinConfig,
    pub reserve: ReserveConfig,
    pub operators: OperatorsConfig,
    pub service: ServiceConfig,
    /// Resolved per-route enable flags — the CONFIG leg of
    /// [`crate::routes::RouteGate`]'s three-place AND. Absent
    /// `[robinhood]` section resolves to
    /// [`crate::routes::RoutesConfig::default`], i.e. legacy routes
    /// enabled and Robinhood routes disabled.
    pub routes: crate::routes::RoutesConfig,
    /// One fee rate per EXECUTABLE route, fully materialised at load.
    ///
    /// After this point there is no fallback anywhere: every pricing call
    /// site asks [`crate::fees::RouteFees::fee_bps`] for the route it is
    /// actually pricing, and a route with no entry is an error rather
    /// than a number borrowed from another route or from the compiled-in
    /// constant. The fallback that fills this in for a config with no
    /// `[fees]` section happens exactly once, in [`resolve_route_fees`],
    /// where it is visible and documented.
    pub route_fees: crate::fees::RouteFees,
    /// The Robinhood deposit indexer, if `[robinhood.indexer]` is
    /// present. `None` — which is every config file that exists today —
    /// means the daemon starts exactly as it did before this phase and
    /// makes no Robinhood RPC call of any kind.
    ///
    /// Says nothing about route admission: see `RawRobinhood::indexer`.
    pub robinhood_indexer: Option<crate::robinhood::RobinhoodIndexerConfig>,
    /// The Robinhood SETTLEMENT configuration, if
    /// `[robinhood.settlement]` is present. `None` — which is every
    /// config file that exists today — means this process holds no
    /// submitter key, constructs no authorization signer, and has no
    /// code path that can broadcast a Robinhood transaction.
    ///
    /// Present is still not sufficient to open a route: the startup
    /// preflight must also pass against the deployed contracts, and all
    /// three `RouteGate` gates plus the contract's own `routeEnabled`
    /// must independently agree.
    pub robinhood_settlement: Option<crate::robinhood::RobinhoodSettlementConfig>,
    /// The dev-mode authorization signer key files. Empty in production,
    /// where the signers are genuinely separate custody domains.
    pub robinhood_dev_signer_key_paths: Vec<PathBuf>,
    /// The PRODUCTION Robinhood authorization signer endpoints, paired
    /// with the address each is expected to control. Empty in dev mode,
    /// and empty in production only when the operator has not provisioned
    /// them — in which case no quorum can form and nothing broadcasts,
    /// which is the correct fail-closed outcome rather than a fallback.
    pub robinhood_auth_remote_signers: Vec<(RemoteSignerConfig, crate::evm::EvmAddress)>,
    /// Every configured per-chain launch policy — the fee rate and the
    /// transfer ceilings an operator approved for one chain.
    ///
    /// Empty for every config file that predates `[robinhood.policy]`,
    /// which is all of them, and empty is a complete answer: a chain with
    /// no policy prices at the compiled-in `BRIDGE_FEE_BPS` and states no
    /// backend limit, exactly as before.
    ///
    /// Goldcoin<->Solana can never appear here — `chain_policy::
    /// POLICY_GOVERNED_CHAINS` lists which chains a policy may name and
    /// `ChainPolicies::insert` refuses the rest — so no edit to a config
    /// file can change the Solana fee or the Solana limits.
    pub chain_policies: crate::chain_policy::ChainPolicies,
    /// The effective rapid-burst policy (`[rapid_burst]`), seeded into
    /// the ledger at startup so folds and `glc-admin` read one source.
    /// Disabled unless the section enables it.
    pub rapid_burst: crate::ledger::RapidBurstPolicy,
}

impl Config {
    /// Reads `path`, applies environment-variable overrides for RPC
    /// endpoints/credentials, then validates and resolves every field.
    /// Never partially succeeds: any read/parse/validation failure is
    /// returned before any field is usable.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mut raw: RawConfig = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        apply_env_overrides(&mut raw);
        resolve(raw)
    }

    /// Loads and validates every attestation signer named in
    /// `operators.attestation_key_paths`, in order, cross-checking each
    /// one's public half against `operators.attestation_pubkeys` at the
    /// same position. DEV/TEST POSTURE ONLY — see module docs.
    pub fn load_attestation_signers(&self) -> Result<Vec<DevAttestationSigner>, ConfigError> {
        if self.operators.attestation_key_paths.len() != self.operators.attestation_pubkeys.len() {
            return Err(ConfigError::Invalid {
                field: "operators.attestation_key_paths",
                detail: format!(
                    "{} path(s) but {} declared attestation_pubkeys — these must be 1:1",
                    self.operators.attestation_key_paths.len(),
                    self.operators.attestation_pubkeys.len()
                ),
            });
        }
        self.operators
            .attestation_key_paths
            .iter()
            .zip(&self.operators.attestation_pubkeys)
            .map(|(path, expected)| {
                let keypair = read_solana_keypair_file(path)?;
                if keypair.pubkey() != *expected {
                    return Err(ConfigError::KeyMismatch {
                        path: path.clone(),
                        expected: expected.to_string(),
                        actual: keypair.pubkey().to_string(),
                    });
                }
                Ok(DevAttestationSigner { keypair })
            })
            .collect()
    }

    /// Loads and validates every vault signer named in
    /// `operators.vault_key_paths`, cross-checked against
    /// `operators.vault_pubkeys` at the same position. DEV/TEST POSTURE
    /// ONLY — see module docs.
    pub fn load_vault_signers(&self) -> Result<Vec<DevVaultSigner>, ConfigError> {
        if self.operators.vault_key_paths.len() != self.operators.vault_pubkeys.len() {
            return Err(ConfigError::Invalid {
                field: "operators.vault_key_paths",
                detail: format!(
                    "{} path(s) but {} declared vault_pubkeys — these must be 1:1",
                    self.operators.vault_key_paths.len(),
                    self.operators.vault_pubkeys.len()
                ),
            });
        }
        self.operators
            .vault_key_paths
            .iter()
            .zip(&self.operators.vault_pubkeys)
            .map(|(path, expected)| {
                let text =
                    std::fs::read_to_string(path).map_err(|source| ConfigError::KeyFileRead {
                        path: path.clone(),
                        source,
                    })?;
                let bytes: [u8; 32] =
                    crate::goldcoin::hex::decode_exact(text.trim()).map_err(|e| {
                        ConfigError::KeyFileMalformed {
                            path: path.clone(),
                            detail: format!("expected 64 hex chars (32 bytes): {e}"),
                        }
                    })?;
                let secret_key = libsecp256k1::SecretKey::parse(&bytes).map_err(|e| {
                    ConfigError::KeyFileMalformed {
                        path: path.clone(),
                        detail: format!("not a valid secp256k1 secret key: {e:?}"),
                    }
                })?;
                let pubkey =
                    libsecp256k1::PublicKey::from_secret_key(&secret_key).serialize_compressed();
                if pubkey != *expected {
                    return Err(ConfigError::KeyMismatch {
                        path: path.clone(),
                        expected: crate::goldcoin::hex::encode(expected),
                        actual: crate::goldcoin::hex::encode(&pubkey),
                    });
                }
                Ok(DevVaultSigner { secret_key, pubkey })
            })
            .collect()
    }

    /// Loads the transaction fee-payer/submitter keypair. Not a custody
    /// authority (see `orchestrator::Orchestrator` docs) — no pubkey
    /// cross-check is declared for it in config, since nothing else
    /// derives trust from which key this is. DEV/TEST POSTURE ONLY.
    pub fn load_submitter(&self) -> Result<Keypair, ConfigError> {
        read_solana_keypair_file(&self.operators.submitter_key_path)
    }

    /// Loads the Robinhood AUTHORIZATION signers — the 2-of-3 quorum
    /// whose EIP-712 signatures the custody contract verifies.
    ///
    /// # Mode-gated, exactly like every other signer in this service
    ///
    /// `"dev"` reads local plaintext key files, which is a DEV/TEST
    /// POSTURE ONLY and never points at production custody keys.
    ///
    /// `"production"` returns an empty pool, and that is deliberate
    /// rather than unfinished: a production Robinhood authorization
    /// signer is a genuinely separate custody domain reached over the
    /// network, and the existing `signing::remote` protocol signs
    /// Goldcoin sighashes and Solana messages — neither of which is an
    /// EIP-712 digest returned as a 65-byte compact secp256k1 signature
    /// with EIP-2's low-`s` rule applied.
    ///
    /// Extending that protocol is a change to the SIGNER-SIDE deployment
    /// as much as to this client, so it belongs in the same reviewable
    /// change as the signer service that answers it. Until then, a
    /// production deployment has no Robinhood authorization signers, and
    /// therefore cannot assemble a quorum, and therefore cannot broadcast
    /// — which is exactly the fail-closed outcome, and is named as a
    /// launch blocker rather than papered over with a dev signer.
    ///
    /// Every loaded signer's derived address is cross-checked against
    /// `robinhood.settlement.authorized_signers`, so a key file from a
    /// different deployment is a refusal to start.
    pub fn load_robinhood_dev_auth_signers(
        &self,
    ) -> Result<Vec<Box<dyn crate::robinhood::EvmAuthSigner>>, ConfigError> {
        let Some(settlement) = &self.robinhood_settlement else {
            return Ok(Vec::new());
        };
        // Belt and braces: `load_robinhood_auth_signers` already routes
        // by mode, and `resolve()` already refuses a production config
        // that names key files at all. A local key file must nevertheless
        // never be readable through a production-mode process, so the
        // refusal is also stated here rather than only upstream.
        if self.operators.mode != SignerMode::Dev {
            return Ok(Vec::new());
        }
        let mut signers: Vec<Box<dyn crate::robinhood::EvmAuthSigner>> = Vec::new();
        for (index, path) in self.robinhood_dev_signer_key_paths.iter().enumerate() {
            let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::KeyFileRead {
                path: path.clone(),
                source,
            })?;
            let text = raw.trim();
            let hex = text.strip_prefix("0x").unwrap_or(text);
            if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                // Deliberately does NOT echo the file's contents: this is
                // key material, and an error message is a log line.
                return Err(ConfigError::Invalid {
                    field: "robinhood.settlement.dev_signer_key_paths",
                    detail: format!(
                        "entry {index} does not contain 32 hex-encoded bytes (with or without a \
                         0x prefix)"
                    ),
                });
            }
            let mut bytes = [0u8; 32];
            for (i, byte) in bytes.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| {
                    ConfigError::Invalid {
                        field: "robinhood.settlement.dev_signer_key_paths",
                        detail: format!("entry {index} is not valid hex"),
                    }
                })?;
            }
            let signer = crate::robinhood::DevEvmAuthSigner::from_bytes(&bytes).map_err(|e| {
                ConfigError::Invalid {
                    field: "robinhood.settlement.dev_signer_key_paths",
                    detail: format!("entry {index}: {e}"),
                }
            })?;
            let address = crate::robinhood::EvmAuthSigner::address(&signer);
            if !settlement.authorized_signers.contains(&address) {
                return Err(ConfigError::Invalid {
                    field: "robinhood.settlement.dev_signer_key_paths",
                    detail: format!(
                        "entry {index} controls {}, which is not one of the configured \
                         authorized_signers",
                        address.to_checksum_string()
                    ),
                });
            }
            signers.push(Box::new(signer));
        }
        Ok(signers)
    }

    /// Connects every `[[robinhood.settlement.auth_remote_signers]]`
    /// endpoint, cross-checking each one's self-reported EVM identity
    /// against the address the operator declared for it (which `resolve()`
    /// has already checked is one of the contract's `authorized_signers`).
    ///
    /// PRODUCTION-CAPABLE, and the thing Phase F's launch blocker A named
    /// as missing. Only ever called when
    /// `operators.mode == SignerMode::Production` — see
    /// [`Config::load_robinhood_auth_signers`].
    async fn load_robinhood_auth_signers_remote(
        &self,
    ) -> Result<Vec<Box<dyn crate::robinhood::EvmAuthSigner>>, ConfigError> {
        let mut signers: Vec<Box<dyn crate::robinhood::EvmAuthSigner>> =
            Vec::with_capacity(self.robinhood_auth_remote_signers.len());
        for (index, (endpoint, expected_address)) in
            self.robinhood_auth_remote_signers.iter().enumerate()
        {
            let signer = RemoteEvmAuthSigner::connect(endpoint, *expected_address)
                .await
                .map_err(|source| ConfigError::RemoteSignerConnect {
                    field: "robinhood.settlement.auth_remote_signers",
                    index,
                    endpoint_url: endpoint.endpoint_url.clone(),
                    source,
                })?;
            signers.push(Box::new(signer));
        }
        Ok(signers)
    }

    /// The one entry point `glc-bridge-daemon` and the operator tooling
    /// call: loads the Robinhood authorization signers appropriate to
    /// `operators.mode`, already boxed into the trait object the
    /// settlement engine depends on.
    ///
    /// Mirrors [`Config::load_signers`] exactly, including the property
    /// that matters most: `resolve()` has already fail-closed on any
    /// mismatch between `mode` and which of
    /// {`dev_signer_key_paths`, `auth_remote_signers`} is populated, so
    /// this only has to pick the matching loader — it can never silently
    /// fall back from one posture to the other.
    ///
    /// An empty result is a legitimate, fail-closed outcome: no quorum
    /// forms, nothing is authorized, and nothing is broadcast.
    pub async fn load_robinhood_auth_signers(
        &self,
    ) -> Result<Vec<Box<dyn crate::robinhood::EvmAuthSigner>>, ConfigError> {
        match self.operators.mode {
            SignerMode::Dev => self.load_robinhood_dev_auth_signers(),
            SignerMode::Production => self.load_robinhood_auth_signers_remote().await,
        }
    }

    /// Connects every Robinhood authorization signer as a GOVERNANCE
    /// signer.
    ///
    /// PRODUCTION MODE ONLY, deliberately. A dev signer set is local
    /// keys held by this process; letting those authorize a `setLimits`
    /// against a real deployment would make the 2-of-3 custody split
    /// decorative for exactly the actions it matters most for. A dev
    /// deployment therefore has no governance path at all rather than a
    /// weaker one.
    pub async fn load_robinhood_governance_signers(
        &self,
    ) -> Result<Vec<RemoteEvmAuthSigner>, ConfigError> {
        if self.operators.mode != SignerMode::Production {
            return Err(ConfigError::Invalid {
                field: "operators.mode",
                detail: "governance signing requires operators.mode = \"production\" and \
                         [[robinhood.settlement.auth_remote_signers]]. A dev signer set is local \
                         keys held by this process, and a governance action authorized by them \
                         would not be a 2-of-3 custody decision at all"
                    .to_string(),
            });
        }
        let mut signers = Vec::with_capacity(self.robinhood_auth_remote_signers.len());
        for (index, (endpoint, expected_address)) in
            self.robinhood_auth_remote_signers.iter().enumerate()
        {
            let signer = RemoteEvmAuthSigner::connect(endpoint, *expected_address)
                .await
                .map_err(|source| ConfigError::RemoteSignerConnect {
                    field: "robinhood.settlement.auth_remote_signers",
                    index,
                    endpoint_url: endpoint.endpoint_url.clone(),
                    source,
                })?;
            signers.push(signer);
        }
        Ok(signers)
    }

    /// Connects every `operators.attestation_remote_signers` endpoint,
    /// cross-checking each one's self-reported identity against the
    /// positionally-matching `operators.attestation_pubkeys` entry
    /// (`resolve()` already checked the two config lists agree with each
    /// other; this is the live check that the actual endpoint agrees
    /// with both). PRODUCTION-CAPABLE — see `signing::remote` module
    /// docs. Only ever called when `operators.mode ==
    /// SignerMode::Production` (see [`Config::load_signers`]).
    async fn load_attestation_signers_remote(
        &self,
    ) -> Result<Vec<RemoteAttestationSigner>, ConfigError> {
        let mut signers = Vec::with_capacity(self.operators.attestation_remote_signers.len());
        for (i, (raw, expected)) in self
            .operators
            .attestation_remote_signers
            .iter()
            .zip(&self.operators.attestation_pubkeys)
            .enumerate()
        {
            let signer = RemoteAttestationSigner::connect(raw, *expected)
                .await
                .map_err(|source| ConfigError::RemoteSignerConnect {
                    field: "operators.attestation_remote_signers",
                    index: i,
                    endpoint_url: raw.endpoint_url.clone(),
                    source,
                })?;
            signers.push(signer);
        }
        Ok(signers)
    }

    /// Connects every `operators.vault_remote_signers` endpoint. See
    /// [`Config::load_attestation_signers_remote`] — identical shape,
    /// secp256k1 rather than ed25519.
    async fn load_vault_signers_remote(&self) -> Result<Vec<RemoteVaultSigner>, ConfigError> {
        let mut signers = Vec::with_capacity(self.operators.vault_remote_signers.len());
        for (i, (raw, expected)) in self
            .operators
            .vault_remote_signers
            .iter()
            .zip(&self.operators.vault_pubkeys)
            .enumerate()
        {
            let signer = RemoteVaultSigner::connect(raw, *expected)
                .await
                .map_err(|source| ConfigError::RemoteSignerConnect {
                    field: "operators.vault_remote_signers",
                    index: i,
                    endpoint_url: raw.endpoint_url.clone(),
                    source,
                })?;
            signers.push(signer);
        }
        Ok(signers)
    }

    /// The one entry point `glc-bridge-daemon` actually calls: loads
    /// attestation and vault signers appropriate to
    /// `operators.mode`, already boxed into the trait objects
    /// `Orchestrator` depends on. `resolve()` has already fail-closed on
    /// any mismatch between `mode` and which of
    /// {`*_key_paths`, `*_remote_signers`} is populated — this function
    /// only has to pick the matching loader.
    pub async fn load_signers(&self) -> Result<LoadedSigners, ConfigError> {
        match self.operators.mode {
            SignerMode::Dev => {
                let attestation = self
                    .load_attestation_signers()?
                    .into_iter()
                    .map(|s| Box::new(s) as Box<dyn AttestationSigner>)
                    .collect();
                let vault = self
                    .load_vault_signers()?
                    .into_iter()
                    .map(|s| Box::new(s) as Box<dyn VaultSigner>)
                    .collect();
                Ok((attestation, vault))
            }
            SignerMode::Production => {
                let attestation = self
                    .load_attestation_signers_remote()
                    .await?
                    .into_iter()
                    .map(|s| Box::new(s) as Box<dyn AttestationSigner>)
                    .collect();
                let vault = self
                    .load_vault_signers_remote()
                    .await?
                    .into_iter()
                    .map(|s| Box::new(s) as Box<dyn VaultSigner>)
                    .collect();
                Ok((attestation, vault))
            }
        }
    }
}

/// Reads a `solana-keygen`-format key file: a JSON array of the 64
/// secret+public bytes.
fn read_solana_keypair_file(path: &Path) -> Result<Keypair, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::KeyFileRead {
        path: path.to_path_buf(),
        source,
    })?;
    let bytes: Vec<u8> =
        serde_json::from_str(&text).map_err(|e| ConfigError::KeyFileMalformed {
            path: path.to_path_buf(),
            detail: format!("expected a JSON array of 64 bytes (solana-keygen format): {e}"),
        })?;
    Keypair::try_from(bytes.as_slice()).map_err(|e| ConfigError::KeyFileMalformed {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })
}

fn apply_env_overrides(raw: &mut RawConfig) {
    if let Ok(v) = std::env::var("GLC_BRIDGE_SOLANA_RPC_URL") {
        raw.solana.rpc_url = v;
    }
    if let Ok(v) = std::env::var("GLC_BRIDGE_GOLDCOIN_RPC_URL") {
        raw.goldcoin.rpc_url = v;
    }
    if let Ok(v) = std::env::var("GLC_BRIDGE_GOLDCOIN_RPC_USER") {
        raw.goldcoin.rpc_user = v;
    }
    if let Ok(v) = std::env::var("GLC_BRIDGE_GOLDCOIN_RPC_PASSWORD") {
        raw.goldcoin.rpc_password = v;
    }
    // Only overrides an endpoint that the config file already declares.
    // Setting this variable can never CREATE a Robinhood indexer that the
    // file did not ask for — an environment variable must not be able to
    // start this service watching a chain.
    if let Ok(v) = std::env::var("GLC_BRIDGE_ROBINHOOD_RPC_URL") {
        if let Some(indexer) = raw.robinhood.as_mut().and_then(|r| r.indexer.as_mut()) {
            indexer.rpc_url = v;
        }
    }
}

fn resolve(raw: RawConfig) -> Result<Config, ConfigError> {
    if raw.solana.commitment != "finalized" {
        return Err(ConfigError::Invalid {
            field: "solana.commitment",
            detail: format!(
                "only \"finalized\" is supported — this service's RPC layer never reads at a \
                 looser commitment (solana::rpc::RealSolanaRpc), got {:?}",
                raw.solana.commitment
            ),
        });
    }
    let reserve_token_mint =
        Pubkey::from_str(&raw.solana.reserve_token_mint).map_err(|e| ConfigError::Invalid {
            field: "solana.reserve_token_mint",
            detail: e.to_string(),
        })?;

    let network = match raw.goldcoin.network.as_str() {
        "regtest" | "testnet" => crate::goldcoin::address::Network::Testnet,
        "mainnet" => crate::goldcoin::address::Network::Mainnet,
        other => {
            return Err(ConfigError::Invalid {
                field: "goldcoin.network",
                detail: format!("expected \"regtest\", \"testnet\", or \"mainnet\", got {other:?}"),
            });
        }
    };

    let solana_bounds = resolve_bounds(&raw.reserve.solana, "reserve.solana")?;
    let goldcoin_bounds = resolve_bounds(&raw.reserve.goldcoin, "reserve.goldcoin")?;

    let mode = match raw.operators.mode.as_str() {
        "dev" => SignerMode::Dev,
        "production" => SignerMode::Production,
        other => {
            return Err(ConfigError::Invalid {
                field: "operators.mode",
                detail: format!("expected \"dev\" or \"production\", got {other:?}"),
            });
        }
    };

    let admin_pubkey =
        Pubkey::from_str(&raw.operators.admin_pubkey).map_err(|e| ConfigError::Invalid {
            field: "operators.admin_pubkey",
            detail: e.to_string(),
        })?;
    let attestation_pubkeys = raw
        .operators
        .attestation_pubkeys
        .iter()
        .map(|s| {
            Pubkey::from_str(s).map_err(|e| ConfigError::Invalid {
                field: "operators.attestation_pubkeys",
                detail: format!("{s:?}: {e}"),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if raw.operators.attestation_threshold == 0
        || raw.operators.attestation_threshold > attestation_pubkeys.len()
    {
        return Err(ConfigError::Invalid {
            field: "operators.attestation_threshold",
            detail: format!(
                "must be between 1 and the number of attestation_pubkeys ({}), got {}",
                attestation_pubkeys.len(),
                raw.operators.attestation_threshold
            ),
        });
    }
    match mode {
        SignerMode::Production => {
            if !raw.operators.attestation_key_paths.is_empty() {
                return Err(ConfigError::ProductionModeForbidsLocalSigners {
                    field: "operators.attestation_key_paths",
                });
            }
            if raw.operators.attestation_remote_signers.len() != attestation_pubkeys.len() {
                return Err(ConfigError::RemoteSignerCountMismatch {
                    field: "operators.attestation_remote_signers",
                    pubkeys_field: "operators.attestation_pubkeys",
                    expected: attestation_pubkeys.len(),
                    actual: raw.operators.attestation_remote_signers.len(),
                    ies: plural_ies(raw.operators.attestation_remote_signers.len()),
                });
            }
            for (i, (raw_signer, expected)) in raw
                .operators
                .attestation_remote_signers
                .iter()
                .zip(&attestation_pubkeys)
                .enumerate()
            {
                let declared = Pubkey::from_str(&raw_signer.expected_public_key).map_err(|e| {
                    ConfigError::Invalid {
                        field: "operators.attestation_remote_signers.expected_public_key",
                        detail: format!("{:?}: {e}", raw_signer.expected_public_key),
                    }
                })?;
                if declared != *expected {
                    return Err(ConfigError::RemoteSignerExpectedKeyMismatch {
                        field: "operators.attestation_remote_signers",
                        pubkeys_field: "operators.attestation_pubkeys",
                        index: i,
                        declared: declared.to_string(),
                        from_pubkeys: expected.to_string(),
                    });
                }
            }
            if let Some((first_index, dup_index)) = find_duplicate(&attestation_pubkeys) {
                return Err(ConfigError::DuplicateRemoteSignerPubkey {
                    field: "operators.attestation_remote_signers",
                    pubkey: attestation_pubkeys[dup_index].to_string(),
                    first_index,
                    dup_index,
                });
            }
            let attestation_endpoint_urls: Vec<&str> = raw
                .operators
                .attestation_remote_signers
                .iter()
                .map(|s| s.endpoint_url.as_str())
                .collect();
            if let Some((first_index, dup_index)) = find_duplicate(&attestation_endpoint_urls) {
                return Err(ConfigError::DuplicateRemoteSignerEndpoint {
                    field: "operators.attestation_remote_signers",
                    endpoint_url: attestation_endpoint_urls[dup_index].to_string(),
                    first_index,
                    dup_index,
                });
            }
        }
        SignerMode::Dev => {
            if !raw.operators.attestation_remote_signers.is_empty() {
                return Err(ConfigError::DevModeForbidsRemoteSigners {
                    field: "operators.attestation_remote_signers",
                });
            }
        }
    }
    let attestation_remote_signers = raw
        .operators
        .attestation_remote_signers
        .iter()
        .map(raw_remote_signer_to_config)
        .collect();

    let vault_pubkeys = raw
        .operators
        .vault_pubkeys
        .iter()
        .map(|s| {
            crate::goldcoin::hex::decode_exact::<33>(s).map_err(|e| ConfigError::Invalid {
                field: "operators.vault_pubkeys",
                detail: format!("{s:?}: {e}"),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if raw.operators.vault_threshold == 0
        || raw.operators.vault_threshold as usize > vault_pubkeys.len()
    {
        return Err(ConfigError::Invalid {
            field: "operators.vault_threshold",
            detail: format!(
                "must be between 1 and the number of vault_pubkeys ({}), got {}",
                vault_pubkeys.len(),
                raw.operators.vault_threshold
            ),
        });
    }
    match mode {
        SignerMode::Production => {
            if !raw.operators.vault_key_paths.is_empty() {
                return Err(ConfigError::ProductionModeForbidsLocalSigners {
                    field: "operators.vault_key_paths",
                });
            }
            if raw.operators.vault_remote_signers.len() != vault_pubkeys.len() {
                return Err(ConfigError::RemoteSignerCountMismatch {
                    field: "operators.vault_remote_signers",
                    pubkeys_field: "operators.vault_pubkeys",
                    expected: vault_pubkeys.len(),
                    actual: raw.operators.vault_remote_signers.len(),
                    ies: plural_ies(raw.operators.vault_remote_signers.len()),
                });
            }
            for (i, (raw_signer, expected)) in raw
                .operators
                .vault_remote_signers
                .iter()
                .zip(&vault_pubkeys)
                .enumerate()
            {
                let declared =
                    crate::goldcoin::hex::decode_exact::<33>(&raw_signer.expected_public_key)
                        .map_err(|e| ConfigError::Invalid {
                            field: "operators.vault_remote_signers.expected_public_key",
                            detail: format!("{:?}: {e}", raw_signer.expected_public_key),
                        })?;
                if declared != *expected {
                    return Err(ConfigError::RemoteSignerExpectedKeyMismatch {
                        field: "operators.vault_remote_signers",
                        pubkeys_field: "operators.vault_pubkeys",
                        index: i,
                        declared: crate::goldcoin::hex::encode(&declared),
                        from_pubkeys: crate::goldcoin::hex::encode(expected),
                    });
                }
            }
            if let Some((first_index, dup_index)) = find_duplicate(&vault_pubkeys) {
                return Err(ConfigError::DuplicateRemoteSignerPubkey {
                    field: "operators.vault_remote_signers",
                    pubkey: crate::goldcoin::hex::encode(&vault_pubkeys[dup_index]),
                    first_index,
                    dup_index,
                });
            }
            let vault_endpoint_urls: Vec<&str> = raw
                .operators
                .vault_remote_signers
                .iter()
                .map(|s| s.endpoint_url.as_str())
                .collect();
            if let Some((first_index, dup_index)) = find_duplicate(&vault_endpoint_urls) {
                return Err(ConfigError::DuplicateRemoteSignerEndpoint {
                    field: "operators.vault_remote_signers",
                    endpoint_url: vault_endpoint_urls[dup_index].to_string(),
                    first_index,
                    dup_index,
                });
            }
        }
        SignerMode::Dev => {
            if !raw.operators.vault_remote_signers.is_empty() {
                return Err(ConfigError::DevModeForbidsRemoteSigners {
                    field: "operators.vault_remote_signers",
                });
            }
        }
    }
    let vault_remote_signers = raw
        .operators
        .vault_remote_signers
        .iter()
        .map(raw_remote_signer_to_config)
        .collect();

    let health_bind_addr =
        SocketAddr::from_str(&raw.service.health_bind_addr).map_err(|e| ConfigError::Invalid {
            field: "service.health_bind_addr",
            detail: e.to_string(),
        })?;
    let api_bind_addr = raw
        .service
        .api_bind_addr
        .as_deref()
        .map(SocketAddr::from_str)
        .transpose()
        .map_err(|e: std::net::AddrParseError| ConfigError::Invalid {
            field: "service.api_bind_addr",
            detail: e.to_string(),
        })?;
    let admin_bind_addr = raw
        .service
        .admin_bind_addr
        .as_deref()
        .map(SocketAddr::from_str)
        .transpose()
        .map_err(|e: std::net::AddrParseError| ConfigError::Invalid {
            field: "service.admin_bind_addr",
            detail: e.to_string(),
        })?;
    let admin_operators = if admin_bind_addr.is_some() {
        if raw.service.admin_operators.is_empty() {
            return Err(ConfigError::Invalid {
                field: "service.admin_operators",
                detail: "admin_bind_addr is set but no admin_operators are configured".to_string(),
            });
        }
        let mut seen_names = std::collections::HashSet::new();
        let mut seen_token_envs = std::collections::HashSet::new();
        let mut operators = Vec::with_capacity(raw.service.admin_operators.len());
        for op in &raw.service.admin_operators {
            if op.name.trim().is_empty() {
                return Err(ConfigError::Invalid {
                    field: "service.admin_operators",
                    detail: "operator name must not be empty".to_string(),
                });
            }
            if !seen_names.insert(op.name.as_str()) {
                return Err(ConfigError::Invalid {
                    field: "service.admin_operators",
                    detail: format!("duplicate operator name {:?}", op.name),
                });
            }
            if op.token_env.trim().is_empty() {
                return Err(ConfigError::Invalid {
                    field: "service.admin_operators",
                    detail: format!("operator {:?} has an empty token_env", op.name),
                });
            }
            // Two operators sharing one env var would share one token,
            // and the audit log would silently attribute both people's
            // actions to whichever operator the registry resolves the
            // token to — reject the misconfiguration structurally here
            // (the duplicate token VALUE case is caught at resolution,
            // see `admin_api::auth::OperatorRegistry::new`).
            if !seen_token_envs.insert(op.token_env.as_str()) {
                return Err(ConfigError::Invalid {
                    field: "service.admin_operators",
                    detail: format!(
                        "duplicate token_env {:?} — every operator needs their own token",
                        op.token_env
                    ),
                });
            }
            // Deliberately no env-var read here — see AdminOperatorConfig.
            operators.push(AdminOperatorConfig {
                name: op.name.clone(),
                token_env: op.token_env.clone(),
                may_execute_glc_refunds: op.may_execute_glc_refunds,
            });
        }
        operators
    } else {
        // A configured operator list without a listener is inert — treat
        // it as a mistake worth failing on, matching this module's
        // fail-closed stance, rather than silently ignoring it.
        if !raw.service.admin_operators.is_empty() {
            return Err(ConfigError::Invalid {
                field: "service.admin_operators",
                detail: "admin_operators are configured but admin_bind_addr is not set".to_string(),
            });
        }
        Vec::new()
    };
    let alert_webhook_url = raw
        .service
        .alert_webhook_url
        .map(|url| {
            reqwest::Url::parse(&url)
                .map(|_| url)
                .map_err(|e| ConfigError::Invalid {
                    field: "service.alert_webhook_url",
                    detail: e.to_string(),
                })
        })
        .transpose()?;

    if raw.goldcoin.change_fanout_max_outputs == 0 {
        return Err(ConfigError::Invalid {
            field: "goldcoin.change_fanout_max_outputs",
            detail: "must be at least 1".to_string(),
        });
    }
    let zero_conf_change_mode = match raw.goldcoin.zero_conf_change_mode.as_str() {
        "depth_limited" => crate::goldcoin::payout::ZeroConfChangeMode::DepthLimited,
        "bridge_owned_recursive" => {
            crate::goldcoin::payout::ZeroConfChangeMode::BridgeOwnedRecursive
        }
        other => {
            return Err(ConfigError::Invalid {
                field: "goldcoin.zero_conf_change_mode",
                detail: format!(
                    "must be \"depth_limited\" or \"bridge_owned_recursive\", got \"{other}\""
                ),
            });
        }
    };
    // The node's -limitancestorcount mempool policy rejects a transaction
    // whose in-mempool ancestor count (including itself) reaches 25
    // (Goldcoin Core v0.17, `too-long-mempool-chain`) — the configured
    // budget must leave room for the new transaction itself, so 24 is the
    // absolute ceiling and the shipped default of 20 keeps real margin.
    if !(1..=24).contains(&raw.goldcoin.zero_conf_change_recursive_chain_limit) {
        return Err(ConfigError::Invalid {
            field: "goldcoin.zero_conf_change_recursive_chain_limit",
            detail: format!(
                "must be within 1..=24 (node -limitancestorcount is 25), got {}",
                raw.goldcoin.zero_conf_change_recursive_chain_limit
            ),
        });
    }
    if raw.goldcoin.utxo_pool_warning_count < raw.goldcoin.utxo_pool_min_available_count {
        return Err(ConfigError::Invalid {
            field: "goldcoin.utxo_pool_warning_count",
            detail: format!(
                "must be >= utxo_pool_min_available_count ({}), got {}",
                raw.goldcoin.utxo_pool_min_available_count, raw.goldcoin.utxo_pool_warning_count
            ),
        });
    }
    // Confirmed-liquidity admission safety buffer: a reopen threshold
    // below the close threshold cannot express hysteresis at all — the
    // gate would close and immediately reopen on one unchanged headroom,
    // which is precisely the flapping the buffer exists to prevent. Fail
    // closed at load time rather than persisting an incoherent pair that
    // `Ledger::set_admission_liquidity_thresholds` would reject at
    // startup anyway.
    if raw.goldcoin.admission_reopen_headroom_atomic < raw.goldcoin.admission_safety_buffer_atomic {
        return Err(ConfigError::Invalid {
            field: "goldcoin.admission_reopen_headroom_atomic",
            detail: format!(
                "must be >= admission_safety_buffer_atomic ({}), got {} — the reopen threshold \
                 is what stops the gate flapping, so it can never sit below the close threshold",
                raw.goldcoin.admission_safety_buffer_atomic,
                raw.goldcoin.admission_reopen_headroom_atomic
            ),
        });
    }
    // Both thresholds are compared against an i64 headroom figure
    // (`reserve_ledger.total_reserve_balance` and friends are INTEGER);
    // refuse anything that could not round-trip rather than silently
    // wrapping into a negative threshold that would disable the gate.
    if raw.goldcoin.admission_safety_buffer_atomic > i64::MAX as u64
        || raw.goldcoin.admission_reopen_headroom_atomic > i64::MAX as u64
    {
        return Err(ConfigError::Invalid {
            field: "goldcoin.admission_safety_buffer_atomic",
            detail: "admission thresholds must fit in a signed 64-bit ledger amount".to_string(),
        });
    }
    // Automatic UTXO liquidity shaping (goldcoin::liquidity): resolve the
    // split-candidate size floor, defaulting to 4x the canonical chunk
    // target, and refuse a value that could never plan a valid split
    // (plan_split requires at least 2 whole chunks) — fail closed at load
    // time rather than erroring on every shaping tick.
    let utxo_shaping_min_source_atomic = raw
        .goldcoin
        .utxo_shaping_min_source_atomic
        .unwrap_or_else(|| raw.goldcoin.change_fanout_target_atomic.saturating_mul(4));
    if utxo_shaping_min_source_atomic < raw.goldcoin.change_fanout_target_atomic.saturating_mul(2) {
        return Err(ConfigError::Invalid {
            field: "goldcoin.utxo_shaping_min_source_atomic",
            detail: format!(
                "must be >= 2 * change_fanout_target_atomic ({}), got {} — a split candidate \
                 must be worth at least two whole chunks",
                raw.goldcoin.change_fanout_target_atomic.saturating_mul(2),
                utxo_shaping_min_source_atomic
            ),
        });
    }
    if raw.goldcoin.utxo_shaping_max_outputs_per_split < 2 {
        return Err(ConfigError::Invalid {
            field: "goldcoin.utxo_shaping_max_outputs_per_split",
            detail: format!(
                "must be at least 2 (a split with fewer outputs is not a split), got {}",
                raw.goldcoin.utxo_shaping_max_outputs_per_split
            ),
        });
    }

    // Goldcoin initial-checkpoint bootstrap: only structural validation
    // here (no chain connection at config-load time) — see
    // `goldcoin::indexer::InitialCheckpoint`'s own docs for the live
    // getblockhash/above-tip checks made at indexer bootstrap time.
    // `height`/`hash` must be given together or not at all: a partial
    // pair (one set, the other absent) is exactly the "malformed config"
    // case that must fail closed here rather than silently either being
    // ignored or accepted with a missing half.
    let initial_checkpoint = match (
        raw.goldcoin.initial_checkpoint_height,
        raw.goldcoin.initial_checkpoint_hash,
    ) {
        (None, None) => None,
        (Some(_), None) => {
            return Err(ConfigError::Invalid {
                field: "goldcoin.initial_checkpoint_hash",
                detail: "initial_checkpoint_height is set but initial_checkpoint_hash is not — \
                         both must be configured together, or neither"
                    .to_string(),
            })
        }
        (None, Some(_)) => {
            return Err(ConfigError::Invalid {
                field: "goldcoin.initial_checkpoint_height",
                detail: "initial_checkpoint_hash is set but initial_checkpoint_height is not — \
                         both must be configured together, or neither"
                    .to_string(),
            })
        }
        (Some(height), Some(hash)) => {
            if height < 0 {
                return Err(ConfigError::Invalid {
                    field: "goldcoin.initial_checkpoint_height",
                    detail: format!("must be >= 0, got {height}"),
                });
            }
            if crate::goldcoin::hex::decode_exact::<32>(&hash).is_err() {
                return Err(ConfigError::Invalid {
                    field: "goldcoin.initial_checkpoint_hash",
                    detail: format!("{hash:?} is not exactly 32 bytes of hex"),
                });
            }
            Some(crate::goldcoin::indexer::InitialCheckpoint {
                height,
                hash,
                operator_acknowledged_no_prior_deposits: raw
                    .goldcoin
                    .initial_checkpoint_operator_acknowledged_no_prior_deposits,
            })
        }
    };

    // Absent section -> all four Robinhood flags false, which is also what
    // `RoutesConfig::default()` already holds; written out rather than
    // short-circuited so the "absent means disabled" rule is one visible
    // expression instead of an implicit consequence of the default impl.
    let routes = {
        let (glc_to_rhn, rhn_to_glc, sol_to_rhn, rhn_to_sol) = match &raw.robinhood {
            Some(r) => (
                r.glc_to_rhn_enabled,
                r.rhn_to_glc_enabled,
                r.sol_to_rhn_enabled,
                r.rhn_to_sol_enabled,
            ),
            None => (false, false, false, false),
        };
        crate::routes::RoutesConfig::default()
            .with_robinhood(glc_to_rhn, rhn_to_glc, sol_to_rhn, rhn_to_sol)
    };

    // The indexer is resolved independently of those flags: watching a
    // chain and being allowed to transact on it are separate decisions.
    // Absent section -> `None` -> no client, no task, no RPC call.
    let robinhood_bounds = match raw.reserve.robinhood.as_ref() {
        None => None,
        Some(bounds) => Some(resolve_bounds(bounds, "reserve.robinhood")?),
    };
    let robinhood_indexer = match raw.robinhood.as_ref().and_then(|r| r.indexer.as_ref()) {
        None => None,
        Some(raw_indexer) => Some(resolve_robinhood_indexer(raw_indexer)?),
    };
    // Resolved AFTER the indexer, and given it, because three of its
    // validity checks are agreements BETWEEN the two sections — an
    // agreement checked somewhere else is one that can be forgotten.
    let robinhood_settlement = match raw.robinhood.as_ref().and_then(|r| r.settlement.as_ref()) {
        None => None,
        Some(raw_settlement) => Some(resolve_robinhood_settlement(
            raw_settlement,
            robinhood_indexer.as_ref(),
        )?),
    };
    let robinhood_dev_signer_key_paths = raw
        .robinhood
        .as_ref()
        .and_then(|r| r.settlement.as_ref())
        .map(|s| s.dev_signer_key_paths.clone())
        .unwrap_or_default();
    let robinhood_auth_remote_signers = resolve_robinhood_auth_signers(
        mode,
        raw.robinhood.as_ref().and_then(|r| r.settlement.as_ref()),
        robinhood_settlement.as_ref(),
    )?;
    // Resolved independently of the indexer, the settlement section and
    // the route flags: what a chain's launch policy IS does not depend on
    // whether this process observes that chain, can settle on it, or is
    // allowed to open a route to it.
    let chain_policies =
        resolve_chain_policies(raw.robinhood.as_ref().and_then(|r| r.policy.as_ref()))?;
    // Materialised here, once, so nothing downstream ever needs a
    // fallback: after this line every executable route has exactly one
    // rate, and asking for a route that has none is an error.
    let route_fees = resolve_route_fees(raw.fees.as_ref(), &chain_policies, &routes)?;
    let rapid_burst = resolve_rapid_burst(raw.rapid_burst)?;

    Ok(Config {
        solana: SolanaConfig {
            rpc_url: raw.solana.rpc_url,
            reserve_token_mint,
        },
        goldcoin: GoldcoinConfig {
            network,
            rpc_url: raw.goldcoin.rpc_url,
            rpc_user: raw.goldcoin.rpc_user,
            rpc_password: raw.goldcoin.rpc_password,
            rpc_connect_timeout_ms: raw.goldcoin.rpc_connect_timeout_ms,
            rpc_read_timeout_ms: raw.goldcoin.rpc_read_timeout_ms,
            confirmation_depth: raw.goldcoin.confirmation_depth,
            max_reorg_depth: raw.goldcoin.max_reorg_depth,
            required_payout_confirmations: raw.goldcoin.required_payout_confirmations,
            vault_min_confirmations: raw.goldcoin.vault_min_confirmations,
            fee_rate_per_kb: raw.goldcoin.fee_rate_per_kb,
            dust_threshold: raw.goldcoin.dust_threshold,
            max_inputs: raw.goldcoin.max_inputs,
            change_fanout_target_atomic: raw.goldcoin.change_fanout_target_atomic,
            change_fanout_max_outputs: raw.goldcoin.change_fanout_max_outputs,
            zero_conf_change_max_depth: raw.goldcoin.zero_conf_change_max_depth,
            zero_conf_change_mode,
            zero_conf_change_recursive_chain_limit: raw
                .goldcoin
                .zero_conf_change_recursive_chain_limit,
            utxo_pool_min_available_count: raw.goldcoin.utxo_pool_min_available_count,
            utxo_pool_warning_count: raw.goldcoin.utxo_pool_warning_count,
            admission_safety_buffer_atomic: raw.goldcoin.admission_safety_buffer_atomic,
            admission_reopen_headroom_atomic: raw.goldcoin.admission_reopen_headroom_atomic,
            max_auto_resumes_per_tick: raw.goldcoin.max_auto_resumes_per_tick,
            utxo_shaping_enabled: raw.goldcoin.utxo_shaping_enabled,
            utxo_shaping_target_available_count: raw.goldcoin.utxo_shaping_target_available_count,
            utxo_shaping_min_source_atomic,
            utxo_shaping_max_outputs_per_split: raw.goldcoin.utxo_shaping_max_outputs_per_split,
            initial_checkpoint,
        },
        reserve: ReserveConfig {
            reconciliation_tolerance: raw.reserve.reconciliation_tolerance,
            solana: solana_bounds,
            goldcoin: goldcoin_bounds,
            robinhood: robinhood_bounds,
        },
        operators: OperatorsConfig {
            mode,
            admin_pubkey,
            attestation_threshold: raw.operators.attestation_threshold,
            attestation_pubkeys,
            attestation_key_paths: raw.operators.attestation_key_paths,
            attestation_remote_signers,
            vault_threshold: raw.operators.vault_threshold,
            vault_pubkeys,
            vault_key_paths: raw.operators.vault_key_paths,
            vault_remote_signers,
            submitter_key_path: raw.operators.submitter_key_path,
        },
        service: ServiceConfig {
            db_path: raw.service.db_path,
            tick_interval_ms: raw.service.tick_interval_ms,
            health_bind_addr,
            api_bind_addr,
            admin_bind_addr,
            admin_operators,
            reservation_ttl_secs: raw.service.reservation_ttl_secs,
            alert_webhook_url,
            alert_poll_interval_secs: raw.service.alert_poll_interval_secs,
            signer_timeout_ms: raw.service.signer_timeout_ms,
        },
        routes,
        robinhood_indexer,
        robinhood_settlement,
        robinhood_dev_signer_key_paths,
        robinhood_auth_remote_signers,
        chain_policies,
        route_fees,
        rapid_burst,
    })
}

/// Resolves `[rapid_burst]` — absent means disabled with the defaults
/// (so `rapid-burst-policy-show` still prints what WOULD apply).
fn resolve_rapid_burst(
    raw: Option<RawRapidBurst>,
) -> Result<crate::ledger::RapidBurstPolicy, ConfigError> {
    let raw = raw.unwrap_or(RawRapidBurst {
        enabled: false,
        window_secs: default_rapid_burst_window_secs(),
        max_per_source_wallet: default_rapid_burst_max_per_wallet(),
        max_per_destination_wallet: default_rapid_burst_max_per_wallet(),
        max_per_pair: default_rapid_burst_max_per_pair(),
        minimum_review_hold_secs: default_rapid_burst_minimum_review_hold_secs(),
    });
    let invalid = |field: &'static str, detail: String| ConfigError::Invalid { field, detail };
    if raw.window_secs <= 0 {
        return Err(invalid(
            "rapid_burst.window_secs",
            format!("must be > 0 (got {})", raw.window_secs),
        ));
    }
    for (field, value) in [
        (
            "rapid_burst.max_per_source_wallet",
            raw.max_per_source_wallet,
        ),
        (
            "rapid_burst.max_per_destination_wallet",
            raw.max_per_destination_wallet,
        ),
        ("rapid_burst.max_per_pair", raw.max_per_pair),
    ] {
        if value == 0 {
            return Err(invalid(
                field,
                "must be >= 1 (the maximum INCLUDES the request being folded)".to_string(),
            ));
        }
    }
    if raw.minimum_review_hold_secs < 0 {
        return Err(invalid(
            "rapid_burst.minimum_review_hold_secs",
            format!("must be >= 0 (got {})", raw.minimum_review_hold_secs),
        ));
    }
    Ok(crate::ledger::RapidBurstPolicy {
        enabled: raw.enabled,
        window_secs: raw.window_secs,
        max_per_source_wallet: raw.max_per_source_wallet,
        max_per_destination_wallet: raw.max_per_destination_wallet,
        max_per_pair: raw.max_per_pair,
        minimum_review_hold_secs: raw.minimum_review_hold_secs,
    })
}

/// Resolves `[fees]` into one rate per executable route.
///
/// # Two modes, and only two
///
/// **`[fees]` present.** It is authoritative and must be COMPLETE: every
/// route `crate::fees::executable_routes()` yields must appear, and
/// nothing else may. A key that is not a route name, names a
/// non-executable route, or carries a rate the protocol has never charged
/// is a config error naming itself. Partial tables are refused rather
/// than topped up from the fallback below — a table listing three of four
/// routes is far likelier to be an unfinished edit than a deliberate one,
/// and the failure mode of guessing wrong is mispriced money.
///
/// **`[fees]` absent.** The MIGRATION FALLBACK, which reproduces exactly
/// what this service charged before per-route fees existed, so an
/// unmodified production config keeps loading and keeps pricing
/// identically:
///
/// | route | rate before this change | fallback |
/// |---|---|---|
/// | `GlcToSol`, `SolToGlc` | compiled-in `BRIDGE_FEE_BPS` | same |
/// | `GlcToRhn`, `RhnToGlc` | `[robinhood.policy].fee_bps`, via `ChainPolicies::fee_bps_for` | same, and `BRIDGE_FEE_BPS` when that section is absent |
///
/// The fallback is a *load-time* convenience with a deliberate shelf
/// life, not a runtime rule. It exists so this change breaks no running
/// deployment; the moment a config states `[fees]`, the fallback is out
/// of the picture entirely.
///
/// # Why the Robinhood fallback reads the chain policy
///
/// Because that is where the Robinhood rate already lives, and reading it
/// from anywhere else would CHANGE production economics on upgrade — the
/// one thing this must not do. `[robinhood.policy].fee_bps` keeps its
/// existing meaning for every tool that reads it; once `[fees]` is
/// present, `[fees]` is what prices requests, and `glc-admin fees-show`
/// reports any disagreement between the two rather than silently
/// preferring one.
fn resolve_route_fees(
    fees: Option<&std::collections::BTreeMap<String, u64>>,
    chain_policies: &crate::chain_policy::ChainPolicies,
    routes: &crate::routes::RoutesConfig,
) -> Result<crate::fees::RouteFees, ConfigError> {
    use crate::fees::{executable_routes, RouteFees};
    use crate::routes::Route;

    let mut resolved = RouteFees::new();

    let Some(raw) = fees else {
        // Migration fallback. Every rate here is one the service was
        // already charging for that route a moment before the upgrade —
        // which is why the two Solana<->Robinhood routes are NOT carried
        // forward: nothing was ever charged on them, and there is no
        // pre-existing rate to reproduce. They can be priced only by an
        // explicit `[fees]` entry, and a config that enables one without
        // a `[fees]` table is refused below.
        for route in executable_routes().filter(|route| !route.is_solana_robinhood()) {
            let chain = if route.source_chain() == crate::routes::Chain::Goldcoin {
                route.destination_chain()
            } else {
                route.source_chain()
            };
            let fee_bps = chain_policies.fee_bps_for(chain);
            resolved
                .insert(route, fee_bps)
                .map_err(|e| ConfigError::Invalid {
                    field: "fees",
                    detail: format!(
                        "no [fees] section, and the pre-existing rate for {} could not be                          carried forward: {e}",
                        route.as_str()
                    ),
                })?;
        }
        resolved
            .covers_required_routes(routes)
            .map_err(|e| ConfigError::Invalid {
                field: "fees",
                detail: format!(
                    "{e}\n\nA Solana<->Robinhood route enabled in [robinhood] must be priced by \
                     an explicit [fees] entry; there is no pre-existing rate to carry forward \
                     for it."
                ),
            })?;
        return Ok(resolved);
    };

    for (name, fee_bps) in raw {
        let route: Route = name.parse().map_err(|_| ConfigError::Invalid {
            field: "fees",
            detail: format!(
                "{name:?} is not a route this bridge models. Keys are route names — expected                  one of: {}",
                executable_routes()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })?;
        resolved
            .insert(route, *fee_bps)
            .map_err(|e| ConfigError::Invalid {
                field: "fees",
                detail: e.to_string(),
            })?;
    }

    // Completeness is checked AFTER every entry, so an operator sees
    // their own typo before they see "you also forgot RhnToGlc".
    resolved
        .covers_required_routes(routes)
        .map_err(|e| ConfigError::Invalid {
            field: "fees",
            detail: format!(
                "{e}\n\nA [fees] section is authoritative and must name every executable \
                 route (a Solana<->Robinhood route may be omitted only while it is disabled). \
                 Remove the section entirely to keep the pre-existing rates, or complete it."
            ),
        })?;

    Ok(resolved)
}

/// Resolves the configured per-chain launch policies.
///
/// Today there is exactly one policy-bearing section,
/// `[robinhood.policy]`. Adding a chain means adding its section and one
/// more call here plus an entry in `chain_policy::
/// POLICY_GOVERNED_CHAINS` — deliberately three explicit edits rather
/// than a loop over every `Chain` variant, so a chain cannot acquire a
/// configurable fee by being added to an enum.
fn resolve_chain_policies(
    robinhood: Option<&RawChainPolicy>,
) -> Result<crate::chain_policy::ChainPolicies, ConfigError> {
    use crate::amount_conversion::CanonicalAtomic;
    use crate::chain_policy::{ChainPolicies, ChainPolicy};

    let mut policies = ChainPolicies::new();
    let Some(raw) = robinhood else {
        return Ok(policies);
    };

    let policy = ChainPolicy::new(
        crate::routes::Chain::Robinhood,
        raw.fee_bps,
        CanonicalAtomic(raw.per_transfer_limit),
        CanonicalAtomic(raw.rolling_daily_limit),
    )
    .map_err(|e| ConfigError::Invalid {
        field: "robinhood.policy",
        detail: e.to_string(),
    })?;
    policies.insert(policy).map_err(|e| ConfigError::Invalid {
        field: "robinhood.policy",
        detail: e.to_string(),
    })?;

    // Refused HERE rather than at first use: a policy that no `setLimits`
    // call could ever install on the deployed contract — one whose strict
    // rolling ceiling does not halve exactly, or whose implied on-chain
    // rolling limit would sit below its own per-transfer maximum and be
    // rejected by `_validateLimits` — is a config error, not a runtime
    // surprise for whoever runs preflight months later.
    crate::robinhood::RobinhoodPolicyBinding::new(policy).map_err(|e| ConfigError::Invalid {
        field: "robinhood.policy",
        detail: e.to_string(),
    })?;

    Ok(policies)
}

/// Cross-checks `operators.mode` against which Robinhood authorization
/// signer list is populated, and resolves the production one.
///
/// The same mode/field discipline `resolve()` already applies to the
/// attestation and vault signer lists, for the same reason: a deployment
/// is either entirely dev-posture or entirely production-posture, and a
/// reviewer must be able to answer "is this a real deployment" by reading
/// `operators.mode` rather than by auditing every signer entry.
///
/// The additional check here, which the v1 lists do not need, is that
/// every declared signer address is one of the contract's own
/// `authorized_signers`. A configured endpoint controlling an address the
/// contract does not know would produce signatures the contract refuses,
/// after gas was spent — so it is refused at config load instead.
fn resolve_robinhood_auth_signers(
    mode: SignerMode,
    raw: Option<&RawRobinhoodSettlement>,
    settlement: Option<&crate::robinhood::RobinhoodSettlementConfig>,
) -> Result<Vec<(RemoteSignerConfig, crate::evm::EvmAddress)>, ConfigError> {
    const FIELD: &str = "robinhood.settlement.auth_remote_signers";
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    match mode {
        SignerMode::Dev => {
            if !raw.auth_remote_signers.is_empty() {
                return Err(ConfigError::DevModeForbidsRemoteSigners { field: FIELD });
            }
            return Ok(Vec::new());
        }
        SignerMode::Production => {
            if !raw.dev_signer_key_paths.is_empty() {
                return Err(ConfigError::ProductionModeForbidsLocalSigners {
                    field: "robinhood.settlement.dev_signer_key_paths",
                });
            }
        }
    }
    let Some(settlement) = settlement else {
        // Unreachable in practice — `raw` being present is what produced
        // `settlement` — but resolving signers against a signer set that
        // does not exist would be checking nothing.
        return Ok(Vec::new());
    };

    let mut resolved = Vec::with_capacity(raw.auth_remote_signers.len());
    let mut addresses = Vec::with_capacity(raw.auth_remote_signers.len());
    for (index, entry) in raw.auth_remote_signers.iter().enumerate() {
        let address: crate::evm::EvmAddress =
            entry
                .expected_public_key
                .parse()
                .map_err(|e| ConfigError::Invalid {
                    field: "robinhood.settlement.auth_remote_signers.expected_public_key",
                    detail: format!("entry {index}: {e}"),
                })?;
        if !settlement.authorized_signers.contains(&address) {
            return Err(ConfigError::Invalid {
                field: FIELD,
                detail: format!(
                    "entry {index} declares {}, which is not one of the configured \
                     authorized_signers — a quorum containing it would be refused on-chain",
                    address.to_checksum_string()
                ),
            });
        }
        // The submitter pays gas and authorizes nothing. An endpoint
        // controlling it in the authorization pool would collapse the two
        // roles the settlement config exists to keep apart — and
        // `RobinhoodSettlementConfig::new` already refuses the reverse
        // spelling of the same mistake.
        if address == settlement.submitter_address {
            return Err(ConfigError::Invalid {
                field: FIELD,
                detail: format!(
                    "entry {index} declares the submitter address {} — the account that pays \
                     gas must not authorize transfers",
                    address.to_checksum_string()
                ),
            });
        }
        addresses.push(address);
        resolved.push((raw_remote_signer_to_config(entry), address));
    }
    if let Some((first_index, dup_index)) = find_duplicate(&addresses) {
        return Err(ConfigError::DuplicateRemoteSignerPubkey {
            field: FIELD,
            pubkey: addresses[dup_index].to_checksum_string(),
            first_index,
            dup_index,
        });
    }
    let endpoints: Vec<&str> = raw
        .auth_remote_signers
        .iter()
        .map(|s| s.endpoint_url.as_str())
        .collect();
    if let Some((first_index, dup_index)) = find_duplicate(&endpoints) {
        return Err(ConfigError::DuplicateRemoteSignerEndpoint {
            field: FIELD,
            endpoint_url: endpoints[dup_index].to_string(),
            first_index,
            dup_index,
        });
    }
    Ok(resolved)
}

/// Resolves and validates a `[robinhood.settlement]` section.
///
/// Every structural refusal from `crate::robinhood::settlement_config` is
/// mapped onto the field it actually concerns, so an operator gets
/// "`robinhood.settlement.tx_envelope` is invalid" rather than one opaque
/// section-level error.
fn resolve_robinhood_settlement(
    raw: &RawRobinhoodSettlement,
    indexer: Option<&crate::robinhood::RobinhoodIndexerConfig>,
) -> Result<crate::robinhood::RobinhoodSettlementConfig, ConfigError> {
    use crate::robinhood::settlement_config::RobinhoodSettlementConfigError as E;

    let chain_id = crate::evm::EvmChainId::new(raw.chain_id).map_err(|e| ConfigError::Invalid {
        field: "robinhood.settlement.chain_id",
        detail: e.to_string(),
    })?;
    let bridge_contract: crate::evm::EvmAddress =
        raw.bridge_contract
            .parse()
            .map_err(|e: crate::evm::EvmAddressError| ConfigError::Invalid {
                field: "robinhood.settlement.bridge_contract",
                detail: e.to_string(),
            })?;
    let tx_envelope: crate::evm::TxEnvelope =
        raw.tx_envelope
            .parse()
            .map_err(|e: String| ConfigError::Invalid {
                field: "robinhood.settlement.tx_envelope",
                detail: e,
            })?;
    let submitter_address: crate::evm::EvmAddress =
        raw.submitter_address
            .parse()
            .map_err(|e| ConfigError::Invalid {
                field: "robinhood.settlement.submitter_address",
                detail: format!("{e}"),
            })?;
    if raw.authorized_signers.len() != 3 {
        return Err(ConfigError::Invalid {
            field: "robinhood.settlement.authorized_signers",
            detail: format!(
                "the contract's signer set is exactly three addresses, got {}",
                raw.authorized_signers.len()
            ),
        });
    }
    let mut authorized_signers = [crate::evm::EvmAddress::ZERO; 3];
    for (i, raw_address) in raw.authorized_signers.iter().enumerate() {
        authorized_signers[i] = raw_address.parse().map_err(|e| ConfigError::Invalid {
            field: "robinhood.settlement.authorized_signers",
            detail: format!("entry {i}: {e}"),
        })?;
    }

    // Wei values arrive as decimal STRINGS: TOML has no unsigned 128-bit
    // integer, and a fee ceiling that silently truncated would sign
    // transactions at a price the operator did not choose.
    let parse_wei = |value: &str, field: &'static str| -> Result<u128, ConfigError> {
        value
            .trim()
            .parse::<u128>()
            .map_err(|e| ConfigError::Invalid {
                field,
                detail: format!("{value:?} is not a decimal wei amount: {e}"),
            })
    };
    let max_fee_per_gas_wei = parse_wei(
        &raw.max_fee_per_gas_wei,
        "robinhood.settlement.max_fee_per_gas_wei",
    )?;
    let priority_fee_wei = parse_wei(
        &raw.priority_fee_wei,
        "robinhood.settlement.priority_fee_wei",
    )?;
    let min_submitter_balance_wei = parse_wei(
        &raw.min_submitter_balance_wei,
        "robinhood.settlement.min_submitter_balance_wei",
    )?;

    crate::robinhood::RobinhoodSettlementConfig::new(
        indexer,
        chain_id,
        bridge_contract,
        tx_envelope,
        raw.submitter_key_env.clone(),
        submitter_address,
        authorized_signers,
        raw.authorization_ttl_secs,
        raw.required_confirmations,
        raw.gas_limit_margin_percent,
        raw.max_gas_limit,
        max_fee_per_gas_wei,
        priority_fee_wei,
        raw.rebroadcast_after_secs,
        raw.max_replacements,
        min_submitter_balance_wei,
    )
    .map_err(|e| ConfigError::Invalid {
        field: match e {
            E::IndexerMissing => "robinhood.settlement",
            E::ChainIdMismatch { .. } => "robinhood.settlement.chain_id",
            E::BridgeContractMismatch { .. } => "robinhood.settlement.bridge_contract",
            E::EmptySubmitterKeyEnv | E::SubmitterKeyEnvLooksLikeASecret { .. } => {
                "robinhood.settlement.submitter_key_env"
            }
            E::ZeroSubmitterAddress
            | E::SubmitterIsAnAuthorizedSigner { .. }
            | E::BridgeContractReused { .. } => "robinhood.settlement.submitter_address",
            E::ZeroAuthorizedSigner { .. } | E::DuplicateAuthorizedSigner { .. } => {
                "robinhood.settlement.authorized_signers"
            }
            E::AuthorizationTtlOutOfRange { .. } => "robinhood.settlement.authorization_ttl_secs",
            E::ZeroRequiredConfirmations => "robinhood.settlement.required_confirmations",
            E::GasMarginOutOfRange { .. } => "robinhood.settlement.gas_limit_margin_percent",
            E::MaxGasLimitTooLow { .. } => "robinhood.settlement.max_gas_limit",
            E::ZeroMaxFee | E::PriorityFeeExceedsMaxFee { .. } => {
                "robinhood.settlement.max_fee_per_gas_wei"
            }
            E::ZeroRebroadcastInterval => "robinhood.settlement.rebroadcast_after_secs",
            E::TooManyReplacements { .. } => "robinhood.settlement.max_replacements",
        },
        detail: e.to_string(),
    })
}

/// Resolves and validates a `[robinhood.indexer]` section.
///
/// Each structural refusal from `crate::robinhood::config` is mapped onto
/// the field it actually concerns, so an operator gets
/// "`robinhood.indexer.confirmation_depth` is invalid" rather than one
/// opaque section-level error.
fn resolve_robinhood_indexer(
    raw: &RawRobinhoodIndexer,
) -> Result<crate::robinhood::RobinhoodIndexerConfig, ConfigError> {
    use crate::robinhood::config::RobinhoodConfigError;

    let chain_id = crate::evm::EvmChainId::new(raw.chain_id).map_err(|e| ConfigError::Invalid {
        field: "robinhood.indexer.chain_id",
        detail: e.to_string(),
    })?;
    let bridge_contract: crate::evm::EvmAddress =
        raw.bridge_contract
            .parse()
            .map_err(|e| ConfigError::Invalid {
                field: "robinhood.indexer.bridge_contract",
                detail: format!("{e}"),
            })?;
    let expected_token: crate::evm::EvmAddress =
        raw.expected_token
            .parse()
            .map_err(|e| ConfigError::Invalid {
                field: "robinhood.indexer.expected_token",
                detail: format!("{e}"),
            })?;

    crate::robinhood::RobinhoodIndexerConfig::new(
        raw.rpc_url.clone(),
        chain_id,
        bridge_contract,
        expected_token,
        raw.start_block,
        raw.confirmation_depth,
        raw.poll_interval_ms,
        raw.request_timeout_ms,
        raw.max_log_block_range,
    )
    .map_err(|e| {
        let field = match e {
            RobinhoodConfigError::EmptyRpcUrl
            | RobinhoodConfigError::UnsupportedRpcScheme { .. } => "robinhood.indexer.rpc_url",
            RobinhoodConfigError::ZeroBridgeContract
            | RobinhoodConfigError::BridgeIsToken { .. } => "robinhood.indexer.bridge_contract",
            RobinhoodConfigError::ZeroExpectedToken => "robinhood.indexer.expected_token",
            RobinhoodConfigError::ZeroConfirmationDepth => "robinhood.indexer.confirmation_depth",
            RobinhoodConfigError::ZeroPollInterval => "robinhood.indexer.poll_interval_ms",
            RobinhoodConfigError::ZeroRequestTimeout => "robinhood.indexer.request_timeout_ms",
            RobinhoodConfigError::ZeroMaxLogBlockRange => "robinhood.indexer.max_log_block_range",
        };
        ConfigError::Invalid {
            field,
            detail: e.to_string(),
        }
    })
}

fn raw_remote_signer_to_config(raw: &RawRemoteSigner) -> RemoteSignerConfig {
    RemoteSignerConfig {
        endpoint_url: raw.endpoint_url.clone(),
        auth_token_env: raw.auth_token_env.clone(),
        timeout: Duration::from_millis(raw.timeout_ms),
    }
}

fn plural_ies(n: usize) -> &'static str {
    if n == 1 {
        "y"
    } else {
        "ies"
    }
}

/// Returns `Some((first_index, dup_index))` for the first repeated value
/// in `items`, or `None` if every value is distinct. Used to reject
/// remote-signer entries that share an `expected_public_key` or
/// `endpoint_url` — production custody domains must be genuinely
/// independent, and either kind of duplicate silently collapses two
/// configured domains into one.
fn find_duplicate<T: Eq + std::hash::Hash>(items: &[T]) -> Option<(usize, usize)> {
    let mut seen: std::collections::HashMap<&T, usize> = std::collections::HashMap::new();
    for (i, item) in items.iter().enumerate() {
        if let Some(&first) = seen.get(item) {
            return Some((first, i));
        }
        seen.insert(item, i);
    }
    None
}

fn resolve_bounds(
    raw: &RawReserveBounds,
    field: &'static str,
) -> Result<ReserveBounds, ConfigError> {
    // Mirrors `Ledger::configure_reserve`'s own assertion exactly, so a
    // config that would panic deep inside ledger setup is instead rejected
    // here with a clear message before anything opens the database.
    if raw.critical_reserve <= raw.protected_minimum {
        return Err(ConfigError::Invalid {
            field,
            detail: format!(
                "critical_reserve ({}) must exceed protected_minimum ({}) — \
                 docs/05-reserve-accounting.md",
                raw.critical_reserve, raw.protected_minimum
            ),
        });
    }
    Ok(ReserveBounds {
        protected_minimum: raw.protected_minimum,
        target_reserve: raw.target_reserve,
        warning_reserve: raw.warning_reserve,
        critical_reserve: raw.critical_reserve,
    })
}

#[cfg(test)]
// `pub(crate)` so `chain_policy::edit`'s tests can build a REAL, loadable
// config file with the same fixture this module validates against. An
// edit test that invented its own minimal TOML would be proving something
// about a file shape production never sees.
pub(crate) mod tests;
