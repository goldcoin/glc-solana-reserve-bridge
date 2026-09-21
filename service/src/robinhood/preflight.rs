//! The startup verification gate: everything that must be true about the
//! deployed contracts before any Robinhood route may be enabled.
//!
//! # What this proves, and what it emphatically does not
//!
//! It proves that the addresses in the config file are the contracts this
//! code was written against, on the network this deployment expects:
//!
//! - there is contract CODE at the bridge address at all;
//! - `eth_chainId` is the configured chain id;
//! - the bridge's `bridgeProtocolId()` is this protocol family;
//! - the bridge's `TOKEN()` is the configured expected token;
//! - the token's `decimals()` is exactly 18;
//! - the bridge's `signers()` is the configured authorized set;
//! - the bridge's `domainSeparator()` equals the one this service
//!   computes, so every authorization it mints will be checked against
//!   the domain it was built for;
//! - each route's `routeChains()` pair, recorded for use in every
//!   authorization;
//! - the configured transaction envelope matches the chain's own fee
//!   market.
//!
//! It does **not** prove anything about the token beyond its decimals. In
//! particular it says nothing about whether the token has a mint
//! authority, a blocklist, a transfer hook, a fee on transfer, a pause,
//! or an upgradeable proxy behind it. Those are properties of the token's
//! CODE and its governance, not of any value this service can read, and
//! establishing them is a separate mainnet token review that this
//! function neither performs nor substitutes for. Claiming otherwise
//! because a `decimals()` call succeeded would be the worst possible
//! outcome of writing this module.
//!
//! # Why every check is a REFUSAL rather than a warning
//!
//! Each of these is a value the whole settlement path depends on being
//! true, and none of them can be "mostly" right. A wrong token address
//! means paying out a different asset; a wrong decimals means every
//! amount is off by ten orders of magnitude; a wrong domain separator
//! means every signature is worthless; a wrong signer set means every
//! quorum is rejected on-chain after gas was spent.
//!
//! # When it runs
//!
//! At startup, before the orchestrator's first tick, and it is the gate
//! [`crate::chains::robinhood::RobinhoodAdapter`] consults: an adapter
//! constructed from an UNVERIFIED configuration reports
//! [`crate::chains::Capability::Unavailable`], so a deployment that
//! skipped or failed preflight cannot open a route no matter what its
//! config file or its `bridge_routes` table say.

use crate::amount_conversion::robinhood::{ensure_robinhood_decimals, ROBINHOOD_DECIMALS};
use crate::evm::{EvmAddress, EvmChainId, TxEnvelope};
use crate::routes::Route;

use super::auth::ProtocolChainPair;
use super::calls::{self, BridgeReader, ContractReadError, TokenReader};
use super::config::RobinhoodIndexerConfig;
use super::policy::RobinhoodPolicyBinding;
use super::rpc::{EvmBlockTag, EvmCallRpc, EvmRpc, EvmRpcError, EvmSubmitRpc};
use super::settlement_config::RobinhoodSettlementConfig;
use crate::chain_policy::ChainPolicy;

#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    #[error("Robinhood RPC while {doing}: {source}")]
    Rpc {
        doing: &'static str,
        #[source]
        source: EvmRpcError,
    },
    #[error(transparent)]
    Read(#[from] ContractReadError),
    #[error(
        "the endpoint reports chain id {actual}, but this deployment is configured for {expected} \
         — refusing to settle on a different network than the one it was configured for"
    )]
    WrongChain { expected: u64, actual: u64 },
    #[error(
        "there is NO CONTRACT CODE at {address} on this chain — the configured bridge address is \
         either wrong, or names a contract that has not been deployed on this network"
    )]
    NoContractCode { address: String },
    #[error(
        "there is NO CONTRACT CODE at the configured token address {address} — an `eth_call` to \
         an address with no code returns empty data rather than failing, so this is checked \
         directly"
    )]
    NoTokenCode { address: String },
    #[error(
        "the contract at {address} reports bridgeProtocolId() = {actual}, not this protocol \
         family's {expected} — it is not a GLC reserve bridge"
    )]
    WrongProtocol {
        address: String,
        expected: String,
        actual: String,
    },
    #[error(
        "the bridge custodies token {actual}, but robinhood.indexer.expected_token says {expected} \
         — paying out against this contract would move a DIFFERENT asset than the one configured"
    )]
    WrongToken { expected: String, actual: String },
    #[error(
        "the reserve token reports {actual} decimals, but this bridge's amount model is built for \
         exactly {expected}: {detail}"
    )]
    WrongDecimals {
        expected: u32,
        actual: u8,
        detail: String,
    },
    #[error(
        "the bridge's signer set is {actual:?}, but robinhood.settlement.authorized_signers says \
         {expected:?} — every quorum this service assembled would be refused on-chain"
    )]
    WrongSignerSet {
        expected: Vec<String>,
        actual: Vec<String>,
    },
    #[error(
        "the bridge's own domainSeparator() is {actual} but this service computes {expected} — \
         every authorization it minted would be signed over the wrong domain and rejected. The \
         cross-language golden fixture proves the FORMULA; this proves the DEPLOYMENT."
    )]
    DomainSeparatorMismatch { expected: String, actual: String },
    #[error(
        "the configured transaction envelope is {envelope}, but the chain's latest block \
         {evidence}. This is the one chain property this repository had no evidence for, so it \
         is configured AND verified — a configured value nothing checks is still a guess."
    )]
    EnvelopeMismatch {
        envelope: &'static str,
        evidence: &'static str,
    },
    #[error(
        "the bridge has MIGRATED to a successor contract: every value-moving call against this \
         address reverts permanently. Point this deployment at the successor."
    )]
    AlreadyMigrated,
    #[error(
        "route {route} resolves to protocol chain pair ({source_chain}, {dest}) on the \
         contract, but the two legs must be distinct — refusing to bind a self-referential route"
    )]
    DegenerateRouteChains {
        route: &'static str,
        // Named `source_chain` rather than `source` because `thiserror`
        // treats a field literally named `source` as the error's CAUSE.
        source_chain: u64,
        dest: u64,
    },
}

/// What preflight established, and which every later step reads rather
/// than re-deriving.
///
/// Constructing one is [`verify`]'s job and there is no other
/// constructor, so possessing a `VerifiedDeployment` IS the evidence that
/// every check above passed against this exact deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDeployment {
    pub chain_id: EvmChainId,
    pub bridge_contract: EvmAddress,
    /// The token the bridge actually custodies, read from the contract —
    /// not the configured expectation, which was only used to check it.
    pub token: EvmAddress,
    pub token_decimals: u8,
    pub signers: [EvmAddress; 3],
    /// The contract's own EIP-712 domain separator, confirmed equal to
    /// this service's computed one.
    pub domain_separator: [u8; 32],
    /// The `(protocolSourceChainId, protocolDestChainId)` pair for each
    /// route the contract models, read from the contract's immutables.
    ///
    /// Read once here rather than before every authorization: they are
    /// immutable at the contract's construction, so a per-operation read
    /// would be a round trip that can only ever return the same answer.
    /// Carrying them means an authorization is built from a value that
    /// was proven against the deployment, not from configuration.
    ///
    /// All four are read, including the two Solana<->Robinhood pairs:
    /// the contract binds every leg at construction whether or not the
    /// route is enabled, and an authorization on a cross route must be
    /// built from the pair the DEPLOYMENT holds, not one this service
    /// assumed.
    pub glc_to_rhn_chains: ProtocolChainPair,
    pub rhn_to_glc_chains: ProtocolChainPair,
    pub sol_to_rhn_chains: ProtocolChainPair,
    pub rhn_to_sol_chains: ProtocolChainPair,
    pub tx_envelope: TxEnvelope,
    /// Whether the chain's latest header carries a `baseFeePerGas` — the
    /// evidence the envelope was checked against, carried so it can be
    /// logged rather than re-derived.
    pub chain_has_base_fee: bool,
}

impl VerifiedDeployment {
    /// The chain pair for one contract route.
    ///
    /// Returns `None` for the two Solana<->Goldcoin routes: the custody
    /// contract does not model them, nothing about them is verified
    /// here, and handing back a pair would suggest otherwise.
    pub fn chains_for(&self, route: Route) -> Option<ProtocolChainPair> {
        match route {
            Route::GlcToRhn => Some(self.glc_to_rhn_chains),
            Route::RhnToGlc => Some(self.rhn_to_glc_chains),
            Route::SolToRhn => Some(self.sol_to_rhn_chains),
            Route::RhnToSol => Some(self.rhn_to_sol_chains),
            Route::GlcToSol | Route::SolToGlc => None,
        }
    }

    /// The EIP-712 domain, from the verified deployment identity.
    pub fn domain(&self) -> super::auth::BridgeDomain {
        super::auth::BridgeDomain::new(self.chain_id, self.bridge_contract)
    }
}

/// Runs every check and returns the verified deployment, or the first
/// refusal.
///
/// Every read is pinned to `Latest` rather than a specific block: this is
/// a startup check about the deployment as it exists now, and a pinned
/// historical block would answer a question nobody asked.
pub async fn verify<R>(
    rpc: &R,
    indexer: &RobinhoodIndexerConfig,
    settlement: &RobinhoodSettlementConfig,
) -> Result<VerifiedDeployment, PreflightError>
where
    R: EvmRpc + EvmCallRpc + EvmSubmitRpc,
{
    let block = EvmBlockTag::Latest;

    // ---- the network ----
    let chain_id = rpc.chain_id().await.map_err(|source| PreflightError::Rpc {
        doing: "reading the chain id",
        source,
    })?;
    if chain_id != settlement.chain_id {
        return Err(PreflightError::WrongChain {
            expected: settlement.chain_id.get(),
            actual: chain_id.get(),
        });
    }

    // ---- there is something deployed at all ----
    //
    // FIRST, because an `eth_call` to an address with no code does not
    // fail — it returns empty data, which every subsequent decoder
    // reports as a malformed return. Checking code presence directly is
    // what turns "the configured address is wrong" into its own message
    // rather than a confusing ABI error.
    let bridge_code = rpc
        .code_at(settlement.bridge_contract, block)
        .await
        .map_err(|source| PreflightError::Rpc {
            doing: "reading the bridge contract's code",
            source,
        })?;
    if bridge_code.is_empty() {
        return Err(PreflightError::NoContractCode {
            address: settlement.bridge_contract.to_checksum_string(),
        });
    }

    let reader = BridgeReader::new(settlement.bridge_contract);

    // ---- it is a bridge of THIS protocol family ----
    let protocol_id = reader.bridge_protocol_id(rpc, block).await?;
    let expected_protocol = calls::bridge_protocol_id();
    if protocol_id != expected_protocol {
        return Err(PreflightError::WrongProtocol {
            address: settlement.bridge_contract.to_checksum_string(),
            expected: hex32(&expected_protocol),
            actual: hex32(&protocol_id),
        });
    }

    // ---- it has not already handed its reserve to a successor ----
    if reader.migrated(rpc, block).await? {
        return Err(PreflightError::AlreadyMigrated);
    }

    // ---- the token ----
    let token = reader.token(rpc, block).await?;
    if token != indexer.expected_token {
        return Err(PreflightError::WrongToken {
            expected: indexer.expected_token.to_checksum_string(),
            actual: token.to_checksum_string(),
        });
    }
    let token_code = rpc
        .code_at(token, block)
        .await
        .map_err(|source| PreflightError::Rpc {
            doing: "reading the token contract's code",
            source,
        })?;
    if token_code.is_empty() {
        return Err(PreflightError::NoTokenCode {
            address: token.to_checksum_string(),
        });
    }
    let decimals = TokenReader::new(token).decimals(rpc, block).await?;
    ensure_robinhood_decimals(decimals).map_err(|e| PreflightError::WrongDecimals {
        expected: ROBINHOOD_DECIMALS,
        actual: decimals,
        detail: e.to_string(),
    })?;

    // ---- the signer set ----
    let signers = reader.signers(rpc, block).await?;
    // Compared as SETS, not as ordered lists: the contract stores an
    // array but `_isSigner` is a mapping, and a rotation may legitimately
    // reorder. Requiring an order would make a correct configuration fail.
    let mut on_chain: Vec<EvmAddress> = signers.to_vec();
    let mut configured: Vec<EvmAddress> = settlement.authorized_signers.to_vec();
    on_chain.sort_by_key(|a| a.to_bytes());
    configured.sort_by_key(|a| a.to_bytes());
    if on_chain != configured {
        return Err(PreflightError::WrongSignerSet {
            expected: configured.iter().map(|a| a.to_checksum_string()).collect(),
            actual: on_chain.iter().map(|a| a.to_checksum_string()).collect(),
        });
    }

    // ---- the EIP-712 domain ----
    //
    // The cross-language golden fixture proves this service's FORMULA
    // matches the contract's. This proves the formula, applied to THIS
    // deployment's address and chain id, produces the separator the
    // deployed contract actually uses.
    let on_chain_domain = reader.domain_separator(rpc, block).await?;
    let computed_domain = settlement.domain().separator();
    if on_chain_domain != computed_domain {
        return Err(PreflightError::DomainSeparatorMismatch {
            expected: hex32(&computed_domain),
            actual: hex32(&on_chain_domain),
        });
    }

    // ---- the route topology ----
    //
    // Every route the contract models, in `ROUTE_*` order. A pair is
    // verified non-degenerate for all four even though a deployment may
    // never open the Solana ones: the immutables exist regardless, and a
    // degenerate pair on ANY route is evidence this is not the contract
    // this service expects.
    let mut pairs = Vec::with_capacity(4);
    for route in [
        Route::GlcToRhn,
        Route::RhnToGlc,
        Route::SolToRhn,
        Route::RhnToSol,
    ] {
        let route_byte = route
            .contract_route_id()
            .expect("every Robinhood route has a contract discriminator");
        let chains = reader.route_chains(rpc, route_byte, block).await?;
        if chains.source == chains.dest {
            return Err(PreflightError::DegenerateRouteChains {
                route: route.as_str(),
                source_chain: chains.source,
                dest: chains.dest,
            });
        }
        pairs.push(chains);
    }

    // ---- the fee market ----
    //
    // The one chain property this repository had no evidence for. It is
    // configured, and it is verified here against the chain's own header:
    // a `baseFeePerGas` proves London is active, and its absence proves
    // it is not.
    let base_fee = rpc
        .latest_base_fee()
        .await
        .map_err(|source| PreflightError::Rpc {
            doing: "reading the chain's base fee to verify the transaction envelope",
            source,
        })?;
    let chain_has_base_fee = base_fee.is_some();
    match (settlement.tx_envelope, chain_has_base_fee) {
        (TxEnvelope::Eip1559, false) => {
            return Err(PreflightError::EnvelopeMismatch {
                envelope: "eip1559",
                evidence: "carries NO baseFeePerGas, so this chain has no EIP-1559 fee market \
                           and would not recognise a type-0x02 transaction",
            })
        }
        (TxEnvelope::Legacy, true) => {
            return Err(PreflightError::EnvelopeMismatch {
                envelope: "legacy",
                evidence: "DOES carry a baseFeePerGas, so this chain has an EIP-1559 fee market \
                           and a legacy transaction's gasPrice is not the price it will be \
                           charged",
            })
        }
        _ => {}
    }

    Ok(VerifiedDeployment {
        chain_id,
        bridge_contract: settlement.bridge_contract,
        token,
        token_decimals: decimals,
        signers,
        domain_separator: on_chain_domain,
        glc_to_rhn_chains: pairs[0],
        rhn_to_glc_chains: pairs[1],
        sol_to_rhn_chains: pairs[2],
        rhn_to_sol_chains: pairs[3],
        tx_envelope: settlement.tx_envelope,
        chain_has_base_fee,
    })
}

fn hex32(bytes: &[u8; 32]) -> String {
    format!(
        "0x{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

// ===================================================================
// The operator-facing preflight
// ===================================================================

/// What a single preflight check established.
///
/// The third variant is the point of this whole type. A report with only
/// PASS and FAIL forces every check into a claim, and the most dangerous
/// thing this module could do is answer "pass" to a question it did not
/// ask — `verify`'s own module docs already say so about the token, and
/// an operator reading a green preflight would reasonably assume
/// otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The check ran against the live deployment and held.
    Pass,
    /// The check ran and did not hold.
    Fail,
    /// The check did NOT establish its property. Either it could not run
    /// (an earlier check failed and this one was never reached), or the
    /// property is not one an RPC read can establish at all.
    Unverified,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Unverified => "UNVERIFIED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightCheck {
    pub name: &'static str,
    pub verdict: Verdict,
    pub detail: String,
}

impl PreflightCheck {
    fn new(name: &'static str, verdict: Verdict, detail: impl Into<String>) -> PreflightCheck {
        PreflightCheck {
            name,
            verdict,
            detail: detail.into(),
        }
    }
}

/// The full operator preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReport {
    pub checks: Vec<PreflightCheck>,
    /// Present only when [`verify`] itself succeeded.
    pub deployment: Option<VerifiedDeployment>,
}

impl PreflightReport {
    /// Whether any check FAILED. Unverified checks are not failures —
    /// they are gaps, and conflating the two would either block a
    /// deployment on a question this tool cannot answer or hide one.
    pub fn any_failed(&self) -> bool {
        self.checks.iter().any(|c| c.verdict == Verdict::Fail)
    }

    /// Checks that could not be established. Never empty in practice: the
    /// token's security properties are permanently in here.
    pub fn unverified(&self) -> Vec<&PreflightCheck> {
        self.checks
            .iter()
            .filter(|c| c.verdict == Verdict::Unverified)
            .collect()
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let mut pass = 0;
        let mut fail = 0;
        let mut unverified = 0;
        for check in &self.checks {
            match check.verdict {
                Verdict::Pass => pass += 1,
                Verdict::Fail => fail += 1,
                Verdict::Unverified => unverified += 1,
            }
        }
        (pass, fail, unverified)
    }
}

/// The checks [`verify`] performs, in the order it performs them.
///
/// The order is load-bearing: `verify` short-circuits on the first
/// failure, so everything after the failing check genuinely was not run
/// and is reported UNVERIFIED rather than assumed either way.
const VERIFY_CHECKS: &[&str] = &[
    "rpc_reachable",
    "chain_id",
    "bridge_contract_code",
    "bridge_protocol_id",
    "not_migrated",
    "bridge_token",
    "token_contract_code",
    "token_decimals",
    "bridge_signer_set",
    "eip712_domain_separator",
    "route_chains",
    "tx_envelope",
];

/// Which check a `verify` failure belongs to.
///
/// Derived from the error rather than from a counter, so a reordering of
/// `verify` cannot silently attribute a failure to the wrong check —
/// every arm names the condition it actually describes.
fn failing_check(error: &PreflightError) -> &'static str {
    match error {
        PreflightError::Rpc { doing, .. } => match *doing {
            "reading the chain id" => "rpc_reachable",
            "reading the bridge contract's code" => "bridge_contract_code",
            "reading the token contract's code" => "token_contract_code",
            _ => "tx_envelope",
        },
        PreflightError::WrongChain { .. } => "chain_id",
        PreflightError::NoContractCode { .. } => "bridge_contract_code",
        PreflightError::WrongProtocol { .. } => "bridge_protocol_id",
        PreflightError::AlreadyMigrated => "not_migrated",
        PreflightError::WrongToken { .. } => "bridge_token",
        PreflightError::NoTokenCode { .. } => "token_contract_code",
        PreflightError::WrongDecimals { .. } => "token_decimals",
        PreflightError::WrongSignerSet { .. } => "bridge_signer_set",
        PreflightError::DomainSeparatorMismatch { .. } => "eip712_domain_separator",
        PreflightError::DegenerateRouteChains { .. } => "route_chains",
        PreflightError::EnvelopeMismatch { .. } => "tx_envelope",
        // A contract read that failed at the RPC or decode layer names
        // the function it was reading, which maps to the check that
        // depends on it.
        PreflightError::Read(read) => match read {
            calls::ContractReadError::Rpc { what, .. }
            | calls::ContractReadError::Decode { what, .. } => match *what {
                "bridgeProtocolId()" => "bridge_protocol_id",
                "migrated()" => "not_migrated",
                "token()" => "bridge_token",
                "decimals()" => "token_decimals",
                "signers()" => "bridge_signer_set",
                "domainSeparator()" => "eip712_domain_separator",
                "routeChains(uint8)" => "route_chains",
                _ => "rpc_reachable",
            },
            calls::ContractReadError::UnexpectedObligationStatus { .. } => "rpc_reachable",
        },
    }
}

/// What the operator expects the contract's four route flags to be.
///
/// Defaults to "all four closed", which is how this ships and what a
/// preflight before launch should find. An operator deliberately checking
/// a deployment mid-rollout names the ones they expect open, so that an
/// UNEXPECTEDLY open route is a FAIL rather than something nobody looked
/// at.
#[derive(Debug, Clone, Default)]
pub struct ExpectedRoutes {
    pub expect_enabled: Vec<Route>,
}

/// Everything the operator preflight needs that [`verify`] does not read.
pub struct OperatorPreflightInputs<'a> {
    pub indexer: &'a RobinhoodIndexerConfig,
    pub settlement: &'a RobinhoodSettlementConfig,
    pub expected_routes: ExpectedRoutes,
    /// Authorization signers this process could actually load or connect,
    /// and how many a quorum needs. The caller supplies them because
    /// connecting is `Config`'s job, and because a preflight that
    /// constructed its own signers would be checking something other than
    /// what the daemon will use.
    pub signers_available: usize,
    pub signers_required: usize,
    /// The approved Robinhood launch policy, if one is configured.
    ///
    /// `None` is a real state and not a hole: a deployment with no
    /// `[robinhood.policy]` section has stated no backend limits, so the
    /// two policy checks report UNVERIFIED. Reporting them as PASS
    /// because there was nothing to disagree with would be exactly the
    /// "answering pass to a question it did not ask" failure this
    /// report's `Verdict::Unverified` variant exists to prevent.
    pub policy: Option<&'a ChainPolicy>,
    /// Every executable route's configured fee, for the source-minimum
    /// deliverability check.
    ///
    /// Needed because the destination floor the policy requires is a
    /// function of the ROUTE's rate, not of the chain's: the same
    /// `outboundMin` serves `GlcToRhn` and `SolToRhn`, and if those two
    /// price differently they need different floors. `None` reports the
    /// check UNVERIFIED rather than assuming a rate.
    pub route_fees: Option<&'a crate::fees::RouteFees>,
}

/// Runs the full operator preflight and reports every check as PASS, FAIL
/// or UNVERIFIED.
///
/// # What it adds over [`verify`]
///
/// `verify` is the STARTUP GATE: it returns a `VerifiedDeployment` or the
/// first refusal, because a daemon needs a yes/no. An operator running a
/// preflight by hand needs the opposite — the whole picture, including
/// the parts that failed after the first one and the parts nothing here
/// can establish.
///
/// So this calls `verify` (never a second copy of its logic), expands its
/// single answer into the ordered check list it actually performed, and
/// then adds the checks a startup gate does not make:
///
/// - the contract's four route flags against what the operator expects;
/// - the submitter account's reachability and gas balance;
/// - whether an authorization quorum could form at all.
///
/// # And the checks it can never make
///
/// Every token security property is reported UNVERIFIED, permanently and
/// by construction. `decimals()` returning 18 says nothing about a mint
/// authority, a blocklist, a transfer hook, a pause, or an upgradeable
/// proxy — those are properties of the token's CODE and its governance,
/// not of any value an `eth_call` returns. Reporting them as PASS because
/// a preflight ran would be the single most harmful thing this function
/// could do.
pub async fn operator_preflight<R>(rpc: &R, inputs: &OperatorPreflightInputs<'_>) -> PreflightReport
where
    R: EvmRpc + EvmCallRpc + EvmSubmitRpc,
{
    let mut checks = Vec::new();
    let outcome = verify(rpc, inputs.indexer, inputs.settlement).await;

    let deployment = match &outcome {
        Ok(deployment) => {
            for name in VERIFY_CHECKS {
                checks.push(PreflightCheck::new(
                    name,
                    Verdict::Pass,
                    describe_verified(name, deployment),
                ));
            }
            Some(deployment.clone())
        }
        Err(error) => {
            let failed = failing_check(error);
            let mut reached = true;
            for name in VERIFY_CHECKS {
                if *name == failed {
                    checks.push(PreflightCheck::new(name, Verdict::Fail, error.to_string()));
                    reached = false;
                    continue;
                }
                if reached {
                    checks.push(PreflightCheck::new(
                        name,
                        Verdict::Pass,
                        "established before the failure below",
                    ));
                } else {
                    // Not "assumed bad" and not "assumed fine": the check
                    // never ran, because `verify` stopped.
                    checks.push(PreflightCheck::new(
                        name,
                        Verdict::Unverified,
                        "not reached — an earlier check failed and preflight stopped there",
                    ));
                }
            }
            None
        }
    };

    // ---- the contract's own route flags ----
    //
    // Read even when `verify` failed: an unexpectedly OPEN route is
    // exactly the thing an operator most needs to know about, and
    // suppressing the read because some other check failed would hide it.
    let reader = calls::BridgeReader::new(inputs.settlement.bridge_contract);
    for route in [
        Route::GlcToRhn,
        Route::RhnToGlc,
        Route::SolToRhn,
        Route::RhnToSol,
    ] {
        let name: &'static str = match route {
            Route::GlcToRhn => "contract_route_glc_to_rhn",
            Route::RhnToGlc => "contract_route_rhn_to_glc",
            Route::SolToRhn => "contract_route_sol_to_rhn",
            Route::RhnToSol => "contract_route_rhn_to_sol",
            _ => unreachable!("only the four Robinhood routes are listed"),
        };
        let expected_enabled = inputs.expected_routes.expect_enabled.contains(&route);
        let discriminator = route
            .contract_route_id()
            .expect("every Robinhood route has a contract discriminator");
        match reader
            .route_enabled(rpc, discriminator, EvmBlockTag::Latest)
            .await
        {
            Ok(actual) => checks.push(PreflightCheck::new(
                name,
                if actual == expected_enabled {
                    Verdict::Pass
                } else {
                    Verdict::Fail
                },
                format!(
                    "contract reports routeEnabled({}) = {actual}; expected {expected_enabled}{}",
                    route.as_str(),
                    if actual && !expected_enabled {
                        " — this route is OPEN on-chain and was not expected to be"
                    } else {
                        ""
                    }
                ),
            )),
            Err(e) => checks.push(PreflightCheck::new(
                name,
                Verdict::Unverified,
                format!("could not read routeEnabled({}): {e}", route.as_str()),
            )),
        }
    }

    // ---- the submitter ----
    match rpc.balance(inputs.settlement.submitter_address).await {
        Ok(balance) => {
            checks.push(PreflightCheck::new(
                "submitter_reachable",
                Verdict::Pass,
                format!(
                    "submitter {} balance read successfully",
                    inputs.settlement.submitter_address.to_checksum_string()
                ),
            ));
            let minimum =
                crate::evm::EvmU256::from_u128(inputs.settlement.min_submitter_balance_wei);
            checks.push(PreflightCheck::new(
                "submitter_funded",
                if balance >= minimum {
                    Verdict::Pass
                } else {
                    Verdict::Fail
                },
                format!(
                    "balance {balance} against the configured min_submitter_balance_wei of \
                     {minimum} — checked BEFORE a nonce is allocated, so an underfunded \
                     submitter produces no half-built operation"
                ),
            ));
        }
        Err(e) => {
            checks.push(PreflightCheck::new(
                "submitter_reachable",
                Verdict::Fail,
                format!("could not read the submitter's balance: {e}"),
            ));
            checks.push(PreflightCheck::new(
                "submitter_funded",
                Verdict::Unverified,
                "not reached — the submitter's balance could not be read",
            ));
        }
    }

    // ---- the approved launch policy against the contract's own limits ----
    //
    // The contract is the enforcement layer: it refuses a transfer above
    // `{in,out}boundMax` and refuses either direction once the
    // fixed-bucket window would pass `{in,out}boundRollingLimit`. A
    // configured backend limit can therefore only be the same number or a
    // lie, and this is where the two are told apart.
    //
    // Read even when `verify` failed, for the same reason the route flags
    // are: a limit disagreement is a thing an operator most needs to see,
    // and suppressing the read because some other check failed would hide
    // it.
    push_policy_checks(rpc, inputs, &mut checks).await;

    // ---- treasury withdrawal capability and migration state ----
    //
    // Both are facts about WHICH deployment this is, not gates on whether
    // it may settle: the first deployment has no `treasury()` and no
    // withdrawal entry point at all, and an operator running this to
    // find out why `robinhood-treasury-withdraw` refuses needs that said
    // plainly rather than as a decode error. A pending migration is the
    // single most consequential state a bridge can be in and is reported
    // whenever it exists.
    match reader.treasury(rpc, EvmBlockTag::Latest).await {
        Ok(treasury) if !treasury.is_zero() => checks.push(PreflightCheck::new(
            "treasury_withdraw_capability",
            Verdict::Pass,
            format!(
                "treasury() = {} — the ONE address executeTreasuryWithdraw may pay; compare it \
                 against the deployment record before any withdrawal is proposed",
                treasury.to_checksum_string()
            ),
        )),
        Ok(_) => checks.push(PreflightCheck::new(
            "treasury_withdraw_capability",
            Verdict::Fail,
            "treasury() = 0x0 — this deployment was constructed WITHOUT a treasury and \
             executeTreasuryWithdraw reverts TreasuryNotConfigured; a reserve withdrawal needs a \
             successor deployment",
        )),
        Err(e) => checks.push(PreflightCheck::new(
            "treasury_withdraw_capability",
            Verdict::Fail,
            format!(
                "the contract does not answer treasury() ({e}) — a deployment that predates the \
                 treasury withdrawal (it has no executeTreasuryWithdraw either). A reserve \
                 withdrawal needs a successor deployment reached through commitMigration / \
                 finalizeMigration; see docs/34-robinhood-reserve-withdrawal.md"
            ),
        )),
    }
    match reader.migration_committed(rpc, EvmBlockTag::Latest).await {
        Ok(false) => checks.push(PreflightCheck::new(
            "no_pending_migration",
            Verdict::Pass,
            "no migration is committed; every route can still be governed",
        )),
        Ok(true) => {
            let successor = reader
                .migration_successor(rpc, EvmBlockTag::Latest)
                .await
                .map(|a| a.to_checksum_string())
                .unwrap_or_else(|e| format!("(unreadable: {e})"));
            let finalizable_at = reader
                .migration_finalizable_at(rpc, EvmBlockTag::Latest)
                .await
                .map(|t| t.to_string())
                .unwrap_or_else(|e| format!("(unreadable: {e})"));
            checks.push(PreflightCheck::new(
                "no_pending_migration",
                Verdict::Fail,
                format!(
                    "a migration to {successor} is COMMITTED (finalizable from unix time \
                     {finalizable_at}). Every inbound route is permanently closed on this \
                     contract; the only exits are finalizeMigration or a guardian's \
                     vetoMigration"
                ),
            ));
        }
        Err(e) => checks.push(PreflightCheck::new(
            "no_pending_migration",
            Verdict::Unverified,
            format!("could not read migrationCommitted(): {e}"),
        )),
    }

    // ---- the quorum ----
    checks.push(PreflightCheck::new(
        "signer_quorum_available",
        if inputs.signers_required > 0 && inputs.signers_available >= inputs.signers_required {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "{} authorization signer(s) available, {} required for a quorum",
            inputs.signers_available, inputs.signers_required
        ),
    ));

    // ---- and everything a preflight cannot establish ----
    for property in crate::chains::robinhood::UNVERIFIED_TOKEN_PROPERTIES {
        checks.push(PreflightCheck::new(
            "token_security_property",
            Verdict::Unverified,
            format!(
                "{property} — NOT established by any check here. This needs a separate mainnet \
                 token review against the token's SOURCE and governance, not against any value \
                 an eth_call can return"
            ),
        ));
    }

    PreflightReport { checks, deployment }
}

/// The two checks that compare the approved launch policy against the
/// deployed contract's `limits()`.
///
/// Separate from the checks `verify` makes because they answer a
/// different kind of question. `verify` asks whether this is the right
/// contract; these ask whether the right contract is configured to
/// enforce the policy an operator approved. A deployment can pass every
/// identity check and still be holding last month's limits.
const POLICY_CHECK_PER_TRANSFER: &str = "policy_per_transfer_limit";
const POLICY_CHECK_ROLLING: &str = "policy_rolling_daily_limit";
/// Whether a policy-minimum transfer can actually be DELIVERED on the
/// routes this contract pays out.
///
/// The one check that catches the failure mode the source minimum was
/// introduced to end: a user hands over exactly the minimum, this service
/// accepts and prices it, and the contract then refuses to pay out what
/// is left after the fee. On `GlcToRhn` that happens AFTER an
/// irreversible Goldcoin deposit, so it must be caught at a launch gate
/// rather than at settlement.
const POLICY_CHECK_SOURCE_MINIMUM: &str = "policy_source_minimum_deliverable";

async fn push_policy_checks<R>(
    rpc: &R,
    inputs: &OperatorPreflightInputs<'_>,
    checks: &mut Vec<PreflightCheck>,
) where
    R: EvmRpc + EvmCallRpc + EvmSubmitRpc,
{
    let Some(policy) = inputs.policy else {
        for name in [POLICY_CHECK_PER_TRANSFER, POLICY_CHECK_ROLLING] {
            checks.push(PreflightCheck::new(
                name,
                Verdict::Unverified,
                "no [robinhood.policy] section is configured, so this deployment states no \
                 backend limit for the contract's to be checked against. The contract's own \
                 limits still govern; nothing here has confirmed they are the ones an operator \
                 approved",
            ));
        }
        return;
    };

    // Refused at config load too, so this is unreachable for a
    // `Config`-sourced policy — stated rather than assumed, because this
    // function also serves callers that built a policy by hand.
    let binding = match RobinhoodPolicyBinding::new(*policy) {
        Ok(binding) => binding,
        Err(e) => {
            for name in [POLICY_CHECK_PER_TRANSFER, POLICY_CHECK_ROLLING] {
                checks.push(PreflightCheck::new(
                    name,
                    Verdict::Fail,
                    format!("the configured policy cannot be expressed on this contract: {e}"),
                ));
            }
            return;
        }
    };

    let reader = calls::BridgeReader::new(inputs.settlement.bridge_contract);
    let limits = match reader.limits(rpc, EvmBlockTag::Latest).await {
        Ok(limits) => limits,
        Err(e) => {
            for name in [POLICY_CHECK_PER_TRANSFER, POLICY_CHECK_ROLLING] {
                checks.push(PreflightCheck::new(
                    name,
                    Verdict::Unverified,
                    format!("could not read the contract's limits(): {e}"),
                ));
            }
            return;
        }
    };

    let mismatches = binding.compare(&limits);
    let per_transfer: Vec<String> = mismatches
        .iter()
        .filter(|m| {
            matches!(
                m,
                super::policy::PolicyMismatch::PerTransferAboveChainMax { .. }
                    | super::policy::PolicyMismatch::PerTransferBelowChainMax { .. }
            )
        })
        .map(|m| m.to_string())
        .collect();
    let rolling: Vec<String> = mismatches
        .iter()
        .filter(|m| {
            !matches!(
                m,
                super::policy::PolicyMismatch::PerTransferAboveChainMax { .. }
                    | super::policy::PolicyMismatch::PerTransferBelowChainMax { .. }
            )
        })
        .map(|m| m.to_string())
        .collect();

    checks.push(PreflightCheck::new(
        POLICY_CHECK_PER_TRANSFER,
        if per_transfer.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        if per_transfer.is_empty() {
            format!(
                "configured inbound_per_transfer_limit {} canonical 8dp ({} at 18dp) equals \
                 the contract's inboundMax and outbound_per_transfer_limit {} ({} at 18dp) \
                 equals its outboundMax",
                policy.inbound_per_transfer_limit().0,
                binding.inbound_max().get(),
                policy.outbound_per_transfer_limit().0,
                binding.outbound_max().get()
            )
        } else {
            per_transfer.join(" | ")
        },
    ));

    checks.push(PreflightCheck::new(
        POLICY_CHECK_ROLLING,
        if rolling.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        if rolling.is_empty() {
            format!(
                "approved strict 24h policy {} canonical 8dp; the contract holds {} at 18dp in \
                 both directions — exactly half, so the fixed bucket's reachable 2x worst case \
                 equals the policy rather than doubling it",
                policy.rolling_daily_limit().0,
                binding.expected_onchain_rolling_limit().get()
            )
        } else {
            rolling.join(" | ")
        },
    ));

    push_source_minimum_check(inputs, &limits, checks);
}

/// Whether `outboundMin` leaves room for a policy-minimum transfer to be
/// delivered, on every route this contract pays out.
///
/// # Why it is per route and not per contract
///
/// `outboundMin` bounds `executePayout`'s `req.amount`, which is the NET
/// this service pays after the ROUTE's fee. One contract field, two
/// routes that may price differently: at 300 bps a 100 GLC minimum needs
/// `outboundMin <= 97 GLC`, at 600 bps it needs `<= 94`. The requirement
/// is therefore computed from each route's own rate
/// ([`crate::min_transfer::required_destination_floor`]) and never from a
/// constant — writing 97 here would be correct today and silently wrong
/// the first time a rate moved.
///
/// The inbound routes are not checked here: `inboundMin` bounds the
/// deposit itself, which IS the gross the policy is a statement about, so
/// there is no fee in between and nothing to be squeezed out by one.
fn push_source_minimum_check(
    inputs: &OperatorPreflightInputs<'_>,
    limits: &calls::BridgeLimits,
    checks: &mut Vec<PreflightCheck>,
) {
    let Some(fees) = inputs.route_fees else {
        checks.push(PreflightCheck::new(
            POLICY_CHECK_SOURCE_MINIMUM,
            Verdict::Unverified,
            "no route fee table was supplied, so the net of a minimum transfer is unknown and              the contract's outboundMin cannot be judged against it",
        ));
        return;
    };

    // 18dp -> canonical, EXACTLY. `_validateLimits` requires every limit
    // to be a whole multiple of `CANONICAL_SCALE`, so an inexact
    // `outboundMin` is not a rounding question — it is a contract holding
    // something this bridge's amount model cannot represent, and saying
    // so is more useful than picking a direction to round it.
    let chain_floor = match limits
        .outbound_min
        .try_to_u128()
        .map_err(|e| e.to_string())
        .and_then(|raw| {
            crate::amount_conversion::robinhood::RobinhoodAtomic::new(raw)
                .to_canonical()
                .map_err(|e| e.to_string())
        }) {
        Ok(canonical) => canonical,
        Err(e) => {
            checks.push(PreflightCheck::new(
                POLICY_CHECK_SOURCE_MINIMUM,
                Verdict::Unverified,
                format!("the contract's outboundMin could not be read in canonical units: {e}"),
            ));
            return;
        }
    };

    let mut violations = Vec::new();
    let mut satisfied = Vec::new();
    for route in [
        crate::routes::Route::GlcToRhn,
        crate::routes::Route::SolToRhn,
    ] {
        let Some(fee_bps) = fees.get(route) else {
            // An unpriced route folds nothing and quotes nothing, so it
            // cannot deliver a sub-minimum payout either. Reported rather
            // than silently skipped: "not checked" is not "fine".
            satisfied.push(format!(
                "{} is unpriced, so nothing is checked",
                route.as_str()
            ));
            continue;
        };
        let required = match crate::min_transfer::required_destination_floor(fee_bps) {
            Ok(required) => required,
            Err(e) => {
                violations.push(format!("{}: {e}", route.as_str()));
                continue;
            }
        };
        let check = crate::min_transfer::DestinationFloorCheck {
            route,
            fee_bps,
            required_at_most: required,
            chain_floor,
        };
        match check.violation() {
            Some(v) => violations.push(v),
            None => satisfied.push(format!(
                "{} at {fee_bps} bps needs outboundMin <= {} and it is {}",
                route.as_str(),
                required.0,
                chain_floor.0
            )),
        }
    }

    checks.push(PreflightCheck::new(
        POLICY_CHECK_SOURCE_MINIMUM,
        if violations.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        if violations.is_empty() {
            satisfied.join(" | ")
        } else {
            violations.join(" | ")
        },
    ));
}

/// The value a passing `verify` check actually established, for display.
fn describe_verified(name: &str, d: &VerifiedDeployment) -> String {
    match name {
        "rpc_reachable" => "the endpoint answered eth_chainId".to_string(),
        "chain_id" => format!("chain id {}", d.chain_id.get()),
        "bridge_contract_code" => format!(
            "contract code present at {}",
            d.bridge_contract.to_checksum_string()
        ),
        "bridge_protocol_id" => "bridgeProtocolId() is this protocol family".to_string(),
        "not_migrated" => "the bridge has not migrated to a successor".to_string(),
        "bridge_token" => format!("custodies {}", d.token.to_checksum_string()),
        "token_contract_code" => "contract code present at the token address".to_string(),
        "token_decimals" => format!("decimals() is {}", d.token_decimals),
        "bridge_signer_set" => format!(
            "signers() is the configured set: {}",
            d.signers
                .iter()
                .map(|a| a.to_checksum_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        "eip712_domain_separator" => {
            format!("domainSeparator() is {}", hex32(&d.domain_separator))
        }
        "route_chains" => format!(
            "GlcToRhn ({}, {}), RhnToGlc ({}, {})",
            d.glc_to_rhn_chains.source,
            d.glc_to_rhn_chains.dest,
            d.rhn_to_glc_chains.source,
            d.rhn_to_glc_chains.dest
        ),
        "tx_envelope" => format!(
            "{} matches the chain's fee market (baseFeePerGas present: {})",
            d.tx_envelope.as_str(),
            d.chain_has_base_fee
        ),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests;
