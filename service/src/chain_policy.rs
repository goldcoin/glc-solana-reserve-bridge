//! Per-chain launch policy: the fee rate and the transfer ceilings an
//! operator approved for ONE chain, stated in the ledger's canonical
//! 8-decimal unit.
//!
//! # Why policy is per chain
//!
//! Until this module existed the bridge had exactly one fee rate
//! ([`crate::amount_conversion::BRIDGE_FEE_BPS`], compiled in) and no
//! configured transfer ceilings at all — the Solana program's own
//! `min_transfer_amount`/`per_transfer_limit`/`rolling_volume_limit` are
//! read FROM the chain (`crate::solana::accounts`), never mirrored here.
//! That worked while there was one destination chain.
//!
//! It stops working the moment a second chain launches under different
//! commercial terms. A single compiled-in rate cannot say "6% to
//! Robinhood, 3% to Solana", and a single global ceiling cannot say
//! "20,000 GLC per Robinhood transfer" without also saying it about
//! Solana. So policy becomes a value keyed by chain.
//!
//! # What this is NOT
//!
//! It is **not** an enforcement layer, and reading it as one would be the
//! most dangerous mistake available here. Both bridges enforce their own
//! per-transfer and rolling limits ON CHAIN — the Solana program in
//! `programs/glc-reserve-bridge/src/limits.rs`, the Robinhood contract in
//! `GlcRobinhoodBridge._consumeWindow` and its `deposit`/`executePayout`
//! bounds checks. Those are the hard limits. A value here can only ever
//! be a STATEMENT of what the operator believes the chain is configured
//! to allow, which is why the Robinhood binding
//! ([`crate::robinhood::policy`]) exists solely to compare this statement
//! against the deployed contract's `limits()` and report every
//! disagreement.
//!
//! # Solana can never acquire a policy through this mechanism
//!
//! [`POLICY_GOVERNED_CHAINS`] lists the chains a configured policy may
//! name, and [`ChainPolicies::insert`] refuses every other chain. Today
//! that list holds Robinhood and nothing else, so no configuration file,
//! however written, can change the Goldcoin<->Solana fee or limits: that
//! route keeps pricing at the compiled-in [`BRIDGE_FEE_BPS`] and keeps
//! reading its ceilings off the Solana program account, exactly as
//! before. Adding a chain to this list is a deliberate, reviewable edit,
//! not a config-file consequence.
//!
//! [`BRIDGE_FEE_BPS`]: crate::amount_conversion::BRIDGE_FEE_BPS

use std::collections::BTreeMap;

use crate::amount_conversion::{CanonicalAtomic, BRIDGE_FEE_BPS};
use crate::routes::{Chain, Route};

pub mod edit;
pub mod human;
pub mod inspect;

/// The chains a configured [`ChainPolicy`] may name.
///
/// A deliberate allow-list rather than "any chain the enum can spell".
/// `Chain` names every chain this deployment knows, including the two
/// whose policy is not configurable at all, and letting a config file
/// name one of those would be exactly the silent Solana behaviour change
/// this module must make impossible.
pub const POLICY_GOVERNED_CHAINS: &[Chain] = &[Chain::Robinhood];

/// Every chain a bridge route reaches, other than the home chain — the
/// networks an operator can be asked about.
///
/// DERIVED from [`Route::ALL`] rather than listed: a route added to that
/// enum brings its network into every menu, report and tool that calls
/// this, with no second list to keep in step. Goldcoin is excluded
/// because it is the home chain, not a network the bridge has a policy
/// TOWARDS — every route either starts or ends there, or (the two
/// Solana<->Robinhood routes) does not involve it at all.
///
/// Sorted, so the order an operator sees is stable across runs.
pub fn bridge_networks() -> Vec<Chain> {
    let mut networks = Vec::new();
    for route in Route::ALL {
        for chain in [route.source_chain(), route.destination_chain()] {
            if chain != Chain::Goldcoin && !networks.contains(&chain) {
                networks.push(chain);
            }
        }
    }
    networks.sort();
    networks
}

/// How one chain's fee and limits are actually governed.
///
/// Every field is prose meant to be shown to an operator, because the
/// honest answer differs per chain and pretending otherwise is the
/// specific failure this type exists to prevent: a policy tool that
/// offered "change the fee" for a chain whose fee is a compile-time
/// constant would be lying about what pressing the key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Governance {
    /// Whether `[<chain>.policy]` governs this chain at all.
    pub configurable: bool,
    /// Where this chain's fee rate actually comes from.
    pub fee: &'static str,
    /// Where this chain's transfer and rolling limits are enforced, and
    /// what changes them.
    pub limits: &'static str,
    /// Shown when an operator tries to change a non-configurable chain.
    /// Always a complete sentence-fragment naming the real mechanism, so
    /// the refusal points somewhere.
    pub why_not_configurable: &'static str,
}

/// How `chain`'s policy is governed.
pub fn governance(chain: Chain) -> Governance {
    match chain {
        Chain::Robinhood => Governance {
            configurable: true,
            fee: "[robinhood.policy].fee_bps in this config file. Applied to NEW requests at \
                  fold time and snapshotted onto each one, so in-flight requests keep settling \
                  at the rate they were created under",
            limits: "STATED here and ENFORCED on chain. GlcRobinhoodBridge holds inboundMax / \
                     outboundMax and inboundRollingLimit / outboundRollingLimit in its `Limits` \
                     storage struct; they are changed by setLimits(...) under a 2-of-3 signer \
                     quorum (ACTION_SET_LIMITS), with no redeployment. This tool never sends \
                     that transaction — it only reports whether the two agree",
            why_not_configurable: "",
        },
        Chain::Solana => Governance {
            configurable: false,
            fee: "[fees] in this config file, per ROUTE — GlcToSol and SolToGlc each carry \
                  their own rate and are changed with `glc-admin fees-set --route <ROUTE>`. No \
                  rebuild is involved; the daemon picks the change up on its next restart",
            limits: "the on-chain program's own config account (min_transfer_amount, \
                     per_transfer_limit, rolling_volume_limit). The service READS them and never \
                     mirrors them; they are changed with `glc-admin set-limit` under the Solana \
                     admin authority",
            why_not_configurable: "its fee is a compiled-in constant and its limits live in the \
                                   Solana program's config account, read from the chain rather \
                                   than configured — use `glc-admin set-limit` for the limits, \
                                   and a code change for the fee",
        },
        Chain::Goldcoin => Governance {
            configurable: false,
            fee: "not applicable — Goldcoin is the home chain, not a destination the bridge \
                  prices a route towards",
            limits: "not applicable — see above",
            why_not_configurable: "it is the bridge's home chain, not a network the bridge \
                                   holds a policy towards",
        },
    }
}

/// Why a per-chain policy was refused.
///
/// Every variant names the chain, because a policy error with no chain in
/// it is unreadable the moment there is more than one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChainPolicyError {
    #[error(
        "{chain} is not a policy-governed chain — its fee and limits are not configurable. \
         Goldcoin<->Solana reads its transfer and rolling ceilings from the Solana program \
         account, and its FEES are per route in [fees] — this per-chain policy section governs \
         neither"
    )]
    ChainNotPolicyGoverned { chain: &'static str },
    #[error("{chain}: a policy for this chain was already configured — declare it exactly once")]
    DuplicatePolicy { chain: &'static str },
    #[error(
        "{chain}: fee_bps {fee_bps} is not a usable rate — 9999 basis points (99.99%) is the \
         maximum. At 10000 bps (100%) the fee consumes the whole gross amount and every \
         transfer delivers nothing, and above that the net entitlement would be negative. Any \
         rate from 0 to 9999 is accepted; there is no list of previously-charged rates"
    )]
    FeeBpsOutOfRange { chain: &'static str, fee_bps: u64 },
    #[error(
        "{chain}: {field} must not be zero — a zero ceiling closes the direction silently, \
         and closing a route is what the route flags and the pause gates are for"
    )]
    ZeroPerTransferLimit {
        chain: &'static str,
        field: &'static str,
    },
    #[error(
        "{chain}: rolling_daily_limit must not be zero, for the same reason as a per-transfer limit"
    )]
    ZeroRollingDailyLimit { chain: &'static str },
    #[error(
        "{chain}: rolling_daily_limit {rolling} is below {field} {per_transfer} \
         (canonical 8dp) — a single legal transfer could not fit in a whole day's budget, so the \
         per-transfer ceiling would be unreachable and the stated policy self-contradictory"
    )]
    RollingBelowPerTransfer {
        chain: &'static str,
        field: &'static str,
        per_transfer: u64,
        rolling: u64,
    },
}

/// The two per-transfer limits a policy states, by the direction of the
/// transfer relative to the chain (docs/40-destination-bound-admission.md,
/// "SOURCE TRANSFER LIMIT vs DESTINATION PAYOUT CAP"): the INBOUND limit
/// bounds what a user may deposit INTO the chain's custody contract (a
/// source-side, user-facing ceiling), the OUTBOUND limit bounds what one
/// settlement may pay OUT of it (destination settlement capacity, sized
/// for the elastic payouts a source-side maximum can produce). They are
/// separate figures on the contract (`inboundMax` / `outboundMax`) and
/// separate figures here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Inbound,
    Outbound,
}

impl TransferDirection {
    pub const ALL: [TransferDirection; 2] =
        [TransferDirection::Inbound, TransferDirection::Outbound];

    /// The config key.
    pub fn field(self) -> &'static str {
        match self {
            TransferDirection::Inbound => "inbound_per_transfer_limit",
            TransferDirection::Outbound => "outbound_per_transfer_limit",
        }
    }
}

/// One chain's approved launch policy, validated at construction.
///
/// Fields are private and there is exactly one constructor, so possessing
/// a `ChainPolicy` IS the evidence that every check in
/// [`ChainPolicy::new`] passed — the same discipline
/// `crate::robinhood::preflight::VerifiedDeployment` uses.
///
/// Amounts are [`CanonicalAtomic`] (8 decimals), the unit every ledger
/// figure in this service already uses. Converting them to a chain's
/// native precision is the job of that chain's binding module, never of
/// the caller reading these values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainPolicy {
    chain: Chain,
    fee_bps: u64,
    inbound_per_transfer_limit: CanonicalAtomic,
    outbound_per_transfer_limit: CanonicalAtomic,
    rolling_daily_limit: CanonicalAtomic,
}

impl ChainPolicy {
    /// Validates and constructs.
    ///
    /// Every check refuses a value that would be silently harmful rather
    /// than obviously wrong: a zero ceiling closes a chain without saying
    /// so, and a fee rate outside the configurable range
    /// ([`crate::fees::MIN_FEE_BPS`]`..=`[`crate::fees::MAX_FEE_BPS`])
    /// prices requests that can never settle.
    ///
    /// The fee validated here is the LEGACY per-chain one. It still prices
    /// Robinhood routes for a config with no `[fees]` section (the
    /// documented migration fallback), so it is held to exactly the same
    /// range rule as a per-route fee — one rule, in one place, reused.
    ///
    /// The two per-transfer limits are stated separately
    /// ([`TransferDirection`]); the ONE strict daily ceiling must cover
    /// the larger of them. [`ChainPolicy::new_symmetric`] is the legacy
    /// one-figure form.
    pub fn new(
        chain: Chain,
        fee_bps: u64,
        inbound_per_transfer_limit: CanonicalAtomic,
        outbound_per_transfer_limit: CanonicalAtomic,
        rolling_daily_limit: CanonicalAtomic,
    ) -> Result<ChainPolicy, ChainPolicyError> {
        let name = chain.as_str();
        if !POLICY_GOVERNED_CHAINS.contains(&chain) {
            return Err(ChainPolicyError::ChainNotPolicyGoverned { chain: name });
        }
        if !(crate::fees::MIN_FEE_BPS..=crate::fees::MAX_FEE_BPS).contains(&fee_bps) {
            return Err(ChainPolicyError::FeeBpsOutOfRange {
                chain: name,
                fee_bps,
            });
        }
        for (direction, limit) in [
            (TransferDirection::Inbound, inbound_per_transfer_limit),
            (TransferDirection::Outbound, outbound_per_transfer_limit),
        ] {
            if limit.0 == 0 {
                return Err(ChainPolicyError::ZeroPerTransferLimit {
                    chain: name,
                    field: direction.field(),
                });
            }
        }
        if rolling_daily_limit.0 == 0 {
            return Err(ChainPolicyError::ZeroRollingDailyLimit { chain: name });
        }
        for (direction, limit) in [
            (TransferDirection::Inbound, inbound_per_transfer_limit),
            (TransferDirection::Outbound, outbound_per_transfer_limit),
        ] {
            if rolling_daily_limit.0 < limit.0 {
                return Err(ChainPolicyError::RollingBelowPerTransfer {
                    chain: name,
                    field: direction.field(),
                    per_transfer: limit.0,
                    rolling: rolling_daily_limit.0,
                });
            }
        }
        Ok(ChainPolicy {
            chain,
            fee_bps,
            inbound_per_transfer_limit,
            outbound_per_transfer_limit,
            rolling_daily_limit,
        })
    }

    /// The legacy one-figure form: the same limit in both directions —
    /// what a config carrying only `per_transfer_limit` states.
    pub fn new_symmetric(
        chain: Chain,
        fee_bps: u64,
        per_transfer_limit: CanonicalAtomic,
        rolling_daily_limit: CanonicalAtomic,
    ) -> Result<ChainPolicy, ChainPolicyError> {
        ChainPolicy::new(
            chain,
            fee_bps,
            per_transfer_limit,
            per_transfer_limit,
            rolling_daily_limit,
        )
    }

    pub fn chain(&self) -> Chain {
        self.chain
    }

    /// The rate NEW requests on this chain price at, and which is
    /// snapshotted onto each request. Settlement of an in-flight request
    /// always uses that snapshot, never this value, so changing a
    /// configured rate cannot re-price anything already created.
    pub fn fee_bps(&self) -> u64 {
        self.fee_bps
    }

    /// The largest single deposit INTO the chain this policy approves,
    /// canonical 8dp — the user-facing, source-side ceiling
    /// (`inboundMax` on the contract).
    pub fn inbound_per_transfer_limit(&self) -> CanonicalAtomic {
        self.inbound_per_transfer_limit
    }

    /// The largest single payout OUT of the chain this policy approves,
    /// canonical 8dp — destination settlement capacity (`outboundMax` on
    /// the contract), sized for the elastic payouts a source-side maximum
    /// can produce; never a user limit.
    pub fn outbound_per_transfer_limit(&self) -> CanonicalAtomic {
        self.outbound_per_transfer_limit
    }

    /// The per-transfer limit for `direction`.
    pub fn per_transfer_limit(&self, direction: TransferDirection) -> CanonicalAtomic {
        match direction {
            TransferDirection::Inbound => self.inbound_per_transfer_limit,
            TransferDirection::Outbound => self.outbound_per_transfer_limit,
        }
    }

    /// Whether the two per-transfer limits are one figure (the legacy
    /// symmetric policy).
    pub fn is_symmetric(&self) -> bool {
        self.inbound_per_transfer_limit == self.outbound_per_transfer_limit
    }

    /// The STRICT 24-hour ceiling this policy approves, canonical 8dp.
    ///
    /// "Strict" is load-bearing and is not the same number as the value
    /// configured on a chain whose rolling window is a fixed bucket — see
    /// [`crate::robinhood::policy`], which derives the on-chain figure
    /// from this one rather than passing it through.
    pub fn rolling_daily_limit(&self) -> CanonicalAtomic {
        self.rolling_daily_limit
    }
}

/// Every configured per-chain policy, keyed by chain.
///
/// A map rather than a struct of named options: adding a chain is then a
/// config section plus an entry in [`POLICY_GOVERNED_CHAINS`], not a
/// change to this type and every match on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainPolicies {
    entries: BTreeMap<Chain, ChainPolicy>,
}

impl ChainPolicies {
    pub fn new() -> ChainPolicies {
        ChainPolicies::default()
    }

    /// Records one chain's policy, refusing a chain that is not
    /// policy-governed and refusing a second policy for a chain that
    /// already has one.
    pub fn insert(&mut self, policy: ChainPolicy) -> Result<(), ChainPolicyError> {
        let chain = policy.chain();
        if !POLICY_GOVERNED_CHAINS.contains(&chain) {
            return Err(ChainPolicyError::ChainNotPolicyGoverned {
                chain: chain.as_str(),
            });
        }
        if self.entries.contains_key(&chain) {
            return Err(ChainPolicyError::DuplicatePolicy {
                chain: chain.as_str(),
            });
        }
        self.entries.insert(chain, policy);
        Ok(())
    }

    /// This chain's configured policy, or `None`.
    ///
    /// `None` is a real answer, not a hole to fill with another chain's
    /// numbers: it means this chain is governed by whatever it was
    /// governed by before any policy existed.
    pub fn get(&self, chain: Chain) -> Option<&ChainPolicy> {
        self.entries.get(&chain)
    }

    /// The fee rate NEW requests on `chain` price at.
    ///
    /// Falls back to the compiled-in [`BRIDGE_FEE_BPS`] for a chain with
    /// no configured policy — which is every chain today except
    /// Robinhood, and is precisely the behaviour that existed before this
    /// module. The fallback is the GLOBAL constant and never another
    /// chain's configured rate: one chain's commercial terms must never
    /// leak into another's.
    pub fn fee_bps_for(&self, chain: Chain) -> u64 {
        match self.entries.get(&chain) {
            Some(policy) => policy.fee_bps(),
            None => BRIDGE_FEE_BPS,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every configured policy, in a stable chain order.
    pub fn iter(&self) -> impl Iterator<Item = &ChainPolicy> {
        self.entries.values()
    }
}

#[cfg(test)]
mod tests;
