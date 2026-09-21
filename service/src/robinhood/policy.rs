//! Binding a [`ChainPolicy`] to the deployed `GlcRobinhoodBridge`: what
//! the contract's `limits()` must hold for the approved policy to be the
//! policy users actually get, and every way the two can disagree.
//!
//! # The contract is the enforcement layer; this is the reconciliation
//!
//! `GlcRobinhoodBridge` refuses a deposit above `inboundMax` and a payout
//! above `outboundMax`, and refuses either once the direction's
//! fixed-bucket window would exceed `inbound`/`outboundRollingLimit`
//! (`_consumeWindow`). Those checks run on the chain, under 2-of-3
//! governance, and nothing in this service can weaken them.
//!
//! So a configured backend limit can only be one of two things: the same
//! number the contract holds, or a lie. This module's entire job is to
//! tell those apart and name the difference — `crate::robinhood::calls`'s
//! `limits()` docs already record why the values themselves are READ from
//! the contract rather than mirrored, and that reasoning is unchanged: a
//! configured copy is a second opinion, and the only safe thing to do
//! with a second opinion is check it.
//!
//! # The fixed-bucket doubling rule, which is why two numbers differ
//!
//! The contract's rolling window is a FIXED BUCKET that resets wholesale,
//! not a sliding one. Its own `_consumeWindow` documentation states the
//! consequence and the operational rule that follows, and this module
//! implements that rule rather than restating it as a comment:
//!
//! > Fill a bucket at `t0`, then at exactly `t0 + 24h` the reset
//! > condition holds and the full limit is available again: 2x the
//! > configured limit moves within a span of 86,400 seconds.
//! >
//! > THEREFORE: the configured on-chain rolling limit MUST be set to ONE
//! > HALF of the intended strict 24-hour policy limit.
//!
//! [`ChainPolicy::rolling_daily_limit`] is the STRICT policy — the most
//! GLC that may move in any 86,400-second span. The value that belongs in
//! the contract's `Limits` struct is therefore HALF of it, and
//! [`RobinhoodPolicyBinding::expected_onchain_rolling_limit`] is where
//! that halving happens exactly once. Configuring the policy number
//! directly on-chain would silently double the real ceiling; that is the
//! single most likely way this launch could go wrong, so it is a
//! mechanical derivation here and a checked comparison in
//! [`RobinhoodPolicyBinding::compare`], not an instruction in a runbook.
//!
//! # Direction
//!
//! The contract's limits are per DIRECTION (inbound = deposits, outbound
//! = payouts) and shared by both routes in that direction. One policy
//! therefore has to hold for both directions, and every comparison below
//! is made twice and reported separately: an operator needs to know WHICH
//! direction disagrees.

use crate::amount_conversion::robinhood::{RobinhoodAtomic, RobinhoodConversionError};
use crate::amount_conversion::CanonicalAtomic;
use crate::chain_policy::ChainPolicy;
use crate::evm::EvmU256;
use crate::routes::Chain;

use super::calls::BridgeLimits;

/// One of the contract's two limit directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LimitDirection {
    /// Deposits into the contract — `inboundMax`, `inboundRollingLimit`.
    Inbound,
    /// Payouts and refunds out of it — `outboundMax`,
    /// `outboundRollingLimit`.
    Outbound,
}

impl LimitDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitDirection::Inbound => "inbound",
            LimitDirection::Outbound => "outbound",
        }
    }

    pub const ALL: [LimitDirection; 2] = [LimitDirection::Inbound, LimitDirection::Outbound];

    /// The policy-side name of the same direction.
    pub fn policy(self) -> crate::chain_policy::TransferDirection {
        match self {
            LimitDirection::Inbound => crate::chain_policy::TransferDirection::Inbound,
            LimitDirection::Outbound => crate::chain_policy::TransferDirection::Outbound,
        }
    }

    fn max(self, limits: &BridgeLimits) -> EvmU256 {
        match self {
            LimitDirection::Inbound => limits.inbound_max,
            LimitDirection::Outbound => limits.outbound_max,
        }
    }

    fn rolling(self, limits: &BridgeLimits) -> EvmU256 {
        match self {
            LimitDirection::Inbound => limits.inbound_rolling_limit,
            LimitDirection::Outbound => limits.outbound_rolling_limit,
        }
    }

    fn max_field(self) -> &'static str {
        match self {
            LimitDirection::Inbound => "inboundMax",
            LimitDirection::Outbound => "outboundMax",
        }
    }

    fn rolling_field(self) -> &'static str {
        match self {
            LimitDirection::Inbound => "inboundRollingLimit",
            LimitDirection::Outbound => "outboundRollingLimit",
        }
    }
}

/// Why a policy could not be expressed against this contract at all.
///
/// Distinct from [`PolicyMismatch`], and the distinction matters: these
/// are policies that no `setLimits` call could ever install, so they are
/// refused before any chain read. A mismatch, by contrast, is a policy
/// the contract COULD hold and currently does not.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RobinhoodPolicyError {
    #[error(
        "a {chain} policy was offered to the Robinhood binding — this binding expresses a policy \
         against the GlcRobinhoodBridge contract and against nothing else"
    )]
    WrongChain { chain: &'static str },
    #[error(
        "converting {field} ({canonical} canonical 8dp) to the token's 18-decimal unit: {source}"
    )]
    Conversion {
        field: &'static str,
        canonical: u64,
        #[source]
        source: RobinhoodConversionError,
    },
    #[error(
        "rolling_daily_limit {rolling} (canonical 8dp) is odd, so the on-chain fixed-bucket \
         limit it implies — exactly half of it, per GlcRobinhoodBridge._consumeWindow — is not a \
         whole canonical unit. A policy whose on-chain expression has to be rounded is a policy \
         nobody can state exactly; raise or lower it by one atomic unit"
    )]
    RollingPolicyNotHalvable { rolling: u64 },
    #[error(
        "the on-chain rolling limit this policy implies ({implied} canonical 8dp — half of the \
         strict rolling_daily_limit {rolling}) is below {field} {per_transfer}. \
         GlcRobinhoodBridge._validateLimits rejects any Limits struct whose rollingLimit is below \
         its max, so no setLimits call could install this policy: the contract would revert with \
         InvalidLimits"
    )]
    ImpliedRollingBelowPerTransfer {
        field: &'static str,
        per_transfer: u64,
        rolling: u64,
        implied: u64,
    },
}

/// One disagreement between the approved policy and the deployed
/// contract's `limits()`.
///
/// Split by which side is the more permissive one, because the two are
/// different failures with different consequences:
///
/// - the BACKEND being more permissive means this service would admit,
///   price and promise a transfer the contract will revert — a user is
///   told a limit that is not real;
/// - the CONTRACT being more permissive means the chain would accept
///   value movement beyond what an operator approved.
///
/// Both are refusals at a launch gate. Only the first is the one
/// requirement "backend must never claim a larger usable limit than the
/// contract allows" names, and it is reported first.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyMismatch {
    #[error(
        "{direction}: the configured per_transfer_limit is {backend_canonical} canonical 8dp \
         ({backend_robinhood} at 18dp) but the contract's {field} is {chain_robinhood} — this \
         service would admit a transfer the contract REVERTS with AmountAboveMaximum. The \
         backend must never claim a larger usable limit than the deployed contract allows"
    )]
    PerTransferAboveChainMax {
        direction: &'static str,
        field: &'static str,
        backend_canonical: u64,
        backend_robinhood: u128,
        chain_robinhood: String,
    },
    #[error(
        "{direction}: the contract's {field} is {chain_robinhood} (18dp) but the approved \
         per_transfer_limit is only {backend_canonical} canonical 8dp ({backend_robinhood} at \
         18dp) — the chain would accept a single transfer larger than the operator approved"
    )]
    PerTransferBelowChainMax {
        direction: &'static str,
        field: &'static str,
        backend_canonical: u64,
        backend_robinhood: u128,
        chain_robinhood: String,
    },
    #[error(
        "{direction}: the approved strict 24h policy is {policy_canonical} canonical 8dp, which \
         requires {field} = {expected_robinhood} (18dp, exactly half — GlcRobinhoodBridge's \
         window is a fixed bucket, so 2x the configured limit can move in one 86,400s span). The \
         contract holds {chain_robinhood}, whose worst case is {chain_worst_case_canonical} \
         canonical 8dp — MORE than the approved policy"
    )]
    RollingAboveApprovedPolicy {
        direction: &'static str,
        field: &'static str,
        policy_canonical: u64,
        expected_robinhood: u128,
        chain_robinhood: String,
        chain_worst_case_canonical: String,
    },
    #[error(
        "{direction}: the approved strict 24h policy is {policy_canonical} canonical 8dp, which \
         requires {field} = {expected_robinhood} (18dp, exactly half). The contract holds \
         {chain_robinhood}, which is LOWER — this service would admit volume the contract \
         REVERTS with ExceedsRollingLimit once the bucket fills"
    )]
    RollingBelowApprovedPolicy {
        direction: &'static str,
        field: &'static str,
        policy_canonical: u64,
        expected_robinhood: u128,
        chain_robinhood: String,
    },
    #[error(
        "{direction}: the contract's {field} is {value}, which does not fit a u128 — no amount \
         this bridge's 18-decimal model can represent could ever reach it, so the configured \
         policy cannot be compared against it. Treat this deployment's limits as unreviewed"
    )]
    ChainLimitUnrepresentable {
        direction: &'static str,
        field: &'static str,
        value: String,
    },
}

/// An approved [`ChainPolicy`] expressed in the deployed contract's own
/// units and window semantics.
///
/// Constructing one proves the policy is installable: it converts
/// exactly, it halves exactly, and the resulting `Limits` struct would
/// pass the contract's own `_validateLimits`. Comparing it against a live
/// `limits()` read is then pure arithmetic with no further failure modes
/// of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RobinhoodPolicyBinding {
    policy: ChainPolicy,
    /// `inboundMax`: the source-side deposit ceiling.
    inbound_max: RobinhoodAtomic,
    /// `outboundMax`: destination settlement capacity — never a user limit.
    outbound_max: RobinhoodAtomic,
    /// The strict 24-hour ceiling, in Robinhood atomic units. NOT the
    /// value that belongs on chain.
    rolling_daily_policy: RobinhoodAtomic,
    /// Half the strict ceiling: the value `setLimits` must install for
    /// the fixed-bucket worst case to equal the approved policy.
    expected_onchain_rolling_limit: RobinhoodAtomic,
    expected_onchain_rolling_limit_canonical: CanonicalAtomic,
}

impl RobinhoodPolicyBinding {
    /// Expresses `policy` against the Robinhood contract, or explains why
    /// it cannot be.
    pub fn new(policy: ChainPolicy) -> Result<RobinhoodPolicyBinding, RobinhoodPolicyError> {
        if policy.chain() != Chain::Robinhood {
            return Err(RobinhoodPolicyError::WrongChain {
                chain: policy.chain().as_str(),
            });
        }

        let inbound_canonical = policy.inbound_per_transfer_limit();
        let outbound_canonical = policy.outbound_per_transfer_limit();
        let rolling_canonical = policy.rolling_daily_limit();

        // The halving happens in the CANONICAL unit, before widening, so
        // "not exactly halvable" is caught as the policy statement it is
        // rather than surfacing later as an 18-decimal remainder.
        if !rolling_canonical.0.is_multiple_of(2) {
            return Err(RobinhoodPolicyError::RollingPolicyNotHalvable {
                rolling: rolling_canonical.0,
            });
        }
        let implied_canonical = CanonicalAtomic(rolling_canonical.0 / 2);

        // `_validateLimits` refuses a Limits struct whose rollingLimit is
        // below its max, so a policy implying that is one no governance
        // action could ever install.
        // Both directions: the contract validates each rolling limit
        // against its own max, and this binding installs one bucket for
        // both, so the bucket must cover the larger of the two.
        for (field, limit) in [
            ("inbound_per_transfer_limit", inbound_canonical),
            ("outbound_per_transfer_limit", outbound_canonical),
        ] {
            if implied_canonical.0 < limit.0 {
                return Err(RobinhoodPolicyError::ImpliedRollingBelowPerTransfer {
                    field,
                    per_transfer: limit.0,
                    rolling: rolling_canonical.0,
                    implied: implied_canonical.0,
                });
            }
        }

        let inbound_max = widen(inbound_canonical, "inbound_per_transfer_limit")?;
        let outbound_max = widen(outbound_canonical, "outbound_per_transfer_limit")?;
        let rolling_daily_policy = widen(rolling_canonical, "rolling_daily_limit")?;
        let expected_onchain_rolling_limit =
            widen(implied_canonical, "implied on-chain rolling limit")?;

        Ok(RobinhoodPolicyBinding {
            policy,
            inbound_max,
            outbound_max,
            rolling_daily_policy,
            expected_onchain_rolling_limit,
            expected_onchain_rolling_limit_canonical: implied_canonical,
        })
    }

    pub fn policy(&self) -> &ChainPolicy {
        &self.policy
    }

    /// The value `inboundMax` must hold (18dp): the source-side deposit
    /// ceiling on `RhnToGlc`/`RhnToSol`.
    pub fn inbound_max(&self) -> RobinhoodAtomic {
        self.inbound_max
    }

    /// The value `outboundMax` must hold (18dp): destination settlement
    /// capacity for `GlcToRhn`/`SolToRhn` payouts — never a user limit.
    pub fn outbound_max(&self) -> RobinhoodAtomic {
        self.outbound_max
    }

    /// The per-transfer limit the contract must hold for `direction`.
    pub fn per_transfer_limit(&self, direction: LimitDirection) -> RobinhoodAtomic {
        match direction {
            LimitDirection::Inbound => self.inbound_max,
            LimitDirection::Outbound => self.outbound_max,
        }
    }

    /// The approved STRICT 24-hour ceiling, 18dp. Never the value to put
    /// on chain — see [`Self::expected_onchain_rolling_limit`].
    pub fn rolling_daily_policy(&self) -> RobinhoodAtomic {
        self.rolling_daily_policy
    }

    /// The value `inboundRollingLimit` and `outboundRollingLimit` must
    /// both hold: exactly half the strict policy, so the fixed bucket's
    /// reachable 2x worst case equals the policy rather than doubling it.
    pub fn expected_onchain_rolling_limit(&self) -> RobinhoodAtomic {
        self.expected_onchain_rolling_limit
    }

    /// The same value in canonical 8dp, for operator-facing reporting.
    pub fn expected_onchain_rolling_limit_canonical(&self) -> CanonicalAtomic {
        self.expected_onchain_rolling_limit_canonical
    }

    /// Every disagreement between this policy and a live `limits()` read.
    ///
    /// An empty result means the deployed contract enforces exactly the
    /// approved policy in both directions. Nothing is short-circuited:
    /// all four comparisons run, because an operator fixing one limit
    /// needs to see the others in the same report.
    pub fn compare(&self, limits: &BridgeLimits) -> Vec<PolicyMismatch> {
        let mut mismatches = Vec::new();
        for direction in LimitDirection::ALL {
            self.compare_per_transfer(direction, limits, &mut mismatches);
            self.compare_rolling(direction, limits, &mut mismatches);
        }
        mismatches
    }

    fn compare_per_transfer(
        &self,
        direction: LimitDirection,
        limits: &BridgeLimits,
        out: &mut Vec<PolicyMismatch>,
    ) {
        let word = direction.max(limits);
        let field = direction.max_field();
        let Some(chain) = narrow(word) else {
            out.push(PolicyMismatch::ChainLimitUnrepresentable {
                direction: direction.as_str(),
                field,
                value: word.to_string(),
            });
            return;
        };
        let backend = self.per_transfer_limit(direction).get();
        let backend_canonical = self.policy.per_transfer_limit(direction.policy()).0;
        if backend > chain {
            out.push(PolicyMismatch::PerTransferAboveChainMax {
                direction: direction.as_str(),
                field,
                backend_canonical,
                backend_robinhood: backend,
                chain_robinhood: chain.to_string(),
            });
        } else if backend < chain {
            out.push(PolicyMismatch::PerTransferBelowChainMax {
                direction: direction.as_str(),
                field,
                backend_canonical,
                backend_robinhood: backend,
                chain_robinhood: chain.to_string(),
            });
        }
    }

    fn compare_rolling(
        &self,
        direction: LimitDirection,
        limits: &BridgeLimits,
        out: &mut Vec<PolicyMismatch>,
    ) {
        let word = direction.rolling(limits);
        let field = direction.rolling_field();
        let Some(chain) = narrow(word) else {
            out.push(PolicyMismatch::ChainLimitUnrepresentable {
                direction: direction.as_str(),
                field,
                value: word.to_string(),
            });
            return;
        };
        let expected = self.expected_onchain_rolling_limit.get();
        if chain > expected {
            // The chain's reachable worst case over one 86,400s span is
            // twice what it holds. Reported in canonical units because
            // that is the unit the approved policy is stated in, and as a
            // string because the doubling can exceed what a canonical u64
            // could hold.
            let worst_case = chain.saturating_mul(2) / CANONICAL_SCALE_U128;
            out.push(PolicyMismatch::RollingAboveApprovedPolicy {
                direction: direction.as_str(),
                field,
                policy_canonical: self.policy.rolling_daily_limit().0,
                expected_robinhood: expected,
                chain_robinhood: chain.to_string(),
                chain_worst_case_canonical: worst_case.to_string(),
            });
        } else if chain < expected {
            out.push(PolicyMismatch::RollingBelowApprovedPolicy {
                direction: direction.as_str(),
                field,
                policy_canonical: self.policy.rolling_daily_limit().0,
                expected_robinhood: expected,
                chain_robinhood: chain.to_string(),
            });
        }
    }
}

/// Whether a mismatch is the case where the BACKEND is the more
/// permissive side — the one a running daemon must refuse to start on,
/// because it would price and promise transfers the contract reverts.
pub fn is_backend_over_claim(mismatch: &PolicyMismatch) -> bool {
    matches!(
        mismatch,
        PolicyMismatch::PerTransferAboveChainMax { .. }
            | PolicyMismatch::RollingBelowApprovedPolicy { .. }
            | PolicyMismatch::ChainLimitUnrepresentable { .. }
    )
}

/// `10^10`, as a `u128`, for reporting an 18-decimal figure in canonical
/// units. Deliberately the same constant the amount model uses rather
/// than a second literal.
const CANONICAL_SCALE_U128: u128 =
    crate::amount_conversion::robinhood::CANONICAL_TO_ROBINHOOD_SCALE;

fn widen(
    canonical: CanonicalAtomic,
    field: &'static str,
) -> Result<RobinhoodAtomic, RobinhoodPolicyError> {
    RobinhoodAtomic::from_canonical(canonical).map_err(|source| RobinhoodPolicyError::Conversion {
        field,
        canonical: canonical.0,
        source,
    })
}

/// A contract limit narrowed to the amount model's `u128`, or `None` if
/// it is larger than any amount this bridge can represent. Never
/// truncates.
fn narrow(word: EvmU256) -> Option<u128> {
    RobinhoodAtomic::try_from_u256(word).ok().map(|a| a.get())
}

#[cfg(test)]
mod tests;
