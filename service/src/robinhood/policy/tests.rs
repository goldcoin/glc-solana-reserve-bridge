use super::*;

use crate::amount_conversion::robinhood::CANONICAL_TO_ROBINHOOD_SCALE;
use crate::chain_policy::ChainPolicy;

/// 1 GLC in canonical 8-decimal units.
const ONE_GLC: u64 = 100_000_000;

/// The approved launch policy: 6.00%, 20,000 GLC per transfer,
/// 10,000,000 GLC strict per 24h.
fn approved() -> ChainPolicy {
    ChainPolicy::new_symmetric(
        Chain::Robinhood,
        600,
        CanonicalAtomic(20_000 * ONE_GLC),
        CanonicalAtomic(10_000_000 * ONE_GLC),
    )
    .expect("the approved policy is valid")
}

fn robinhood_u256(glc: u128) -> EvmU256 {
    EvmU256::from_u128(glc * 1_000_000_000_000_000_000)
}

/// Limits that match [`approved`] exactly: the max equal to the
/// per-transfer ceiling and the rolling limit at HALF the strict policy.
fn matching_limits() -> BridgeLimits {
    BridgeLimits {
        inbound_min: robinhood_u256(1),
        inbound_max: robinhood_u256(20_000),
        inbound_rolling_limit: robinhood_u256(5_000_000),
        outbound_min: robinhood_u256(1),
        outbound_max: robinhood_u256(20_000),
        outbound_rolling_limit: robinhood_u256(5_000_000),
        protected_min_reserve: EvmU256::ZERO,
    }
}

#[test]
fn the_on_chain_rolling_limit_is_exactly_half_the_strict_policy() {
    // The single most important number in this change. The contract's
    // window is a fixed bucket whose reachable worst case is 2x, so a
    // 10,000,000 GLC/24h policy is installed on chain as 5,000,000 GLC.
    let binding = RobinhoodPolicyBinding::new(approved()).unwrap();
    assert_eq!(
        binding.expected_onchain_rolling_limit_canonical(),
        CanonicalAtomic(5_000_000 * ONE_GLC)
    );
    assert_eq!(
        binding.expected_onchain_rolling_limit().get(),
        5_000_000u128 * 1_000_000_000_000_000_000
    );
    // And it is genuinely NOT the policy figure, which is the mistake
    // this derivation exists to prevent.
    assert_ne!(
        binding.expected_onchain_rolling_limit(),
        binding.rolling_daily_policy()
    );
    assert_eq!(
        binding.rolling_daily_policy().get(),
        binding.expected_onchain_rolling_limit().get() * 2
    );
}

#[test]
fn canonical_amounts_widen_to_the_tokens_eighteen_decimals_exactly() {
    let binding = RobinhoodPolicyBinding::new(approved()).unwrap();
    assert_eq!(
        binding.inbound_max().get(),
        u128::from(20_000 * ONE_GLC) * CANONICAL_TO_ROBINHOOD_SCALE
    );
    assert_eq!(
        binding.inbound_max().get(),
        20_000u128 * 1_000_000_000_000_000_000
    );
    assert_eq!(binding.outbound_max(), binding.inbound_max());
}

#[test]
fn a_contract_holding_the_approved_policy_reports_no_mismatch() {
    let binding = RobinhoodPolicyBinding::new(approved()).unwrap();
    assert_eq!(binding.compare(&matching_limits()), Vec::new());
}

#[test]
fn a_backend_limit_above_the_contracts_max_is_an_over_claim_in_both_directions() {
    let binding = RobinhoodPolicyBinding::new(approved()).unwrap();
    let mut limits = matching_limits();
    limits.inbound_max = robinhood_u256(10_000);
    limits.outbound_max = robinhood_u256(10_000);

    let mismatches = binding.compare(&limits);
    let over: Vec<_> = mismatches
        .iter()
        .filter(|m| matches!(m, PolicyMismatch::PerTransferAboveChainMax { .. }))
        .collect();
    assert_eq!(over.len(), 2, "both directions reported: {mismatches:?}");
    for m in &mismatches {
        assert!(is_backend_over_claim(m), "{m:?}");
    }
    assert!(
        over[0].to_string().contains("inboundMax") || over[0].to_string().contains("outboundMax")
    );
}

#[test]
fn a_contract_max_above_the_approved_limit_is_reported_but_is_not_an_over_claim() {
    let binding = RobinhoodPolicyBinding::new(approved()).unwrap();
    let mut limits = matching_limits();
    limits.outbound_max = robinhood_u256(50_000);

    let mismatches = binding.compare(&limits);
    assert_eq!(mismatches.len(), 1, "{mismatches:?}");
    assert!(matches!(
        mismatches[0],
        PolicyMismatch::PerTransferBelowChainMax { .. }
    ));
    assert!(!is_backend_over_claim(&mismatches[0]));
}

#[test]
fn a_contract_rolling_limit_set_to_the_policy_figure_is_caught() {
    // The exact operational mistake the contract's `_consumeWindow` docs
    // warn about: putting 10,000,000 on chain doubles the real ceiling to
    // 20,000,000 per 24h.
    let binding = RobinhoodPolicyBinding::new(approved()).unwrap();
    let mut limits = matching_limits();
    limits.inbound_rolling_limit = robinhood_u256(10_000_000);
    limits.outbound_rolling_limit = robinhood_u256(10_000_000);

    let mismatches = binding.compare(&limits);
    assert_eq!(mismatches.len(), 2, "{mismatches:?}");
    for m in &mismatches {
        assert!(matches!(
            m,
            PolicyMismatch::RollingAboveApprovedPolicy { .. }
        ));
        // The chain is the permissive side here, so this is not the
        // backend over-claim a daemon refuses to start on.
        assert!(!is_backend_over_claim(m));
        // The message must state the doubled worst case in canonical
        // units, because that is the number an operator has to compare
        // against the approved policy.
        assert!(
            m.to_string()
                .contains(&(20_000_000u128 * u128::from(ONE_GLC)).to_string()),
            "{m}"
        );
    }
}

#[test]
fn a_contract_rolling_limit_below_the_implied_value_is_a_backend_over_claim() {
    let binding = RobinhoodPolicyBinding::new(approved()).unwrap();
    let mut limits = matching_limits();
    limits.inbound_rolling_limit = robinhood_u256(1_000_000);
    limits.outbound_rolling_limit = robinhood_u256(1_000_000);

    let mismatches = binding.compare(&limits);
    assert_eq!(mismatches.len(), 2, "{mismatches:?}");
    for m in &mismatches {
        assert!(matches!(
            m,
            PolicyMismatch::RollingBelowApprovedPolicy { .. }
        ));
        assert!(is_backend_over_claim(m));
    }
}

#[test]
fn every_direction_is_compared_independently() {
    let binding = RobinhoodPolicyBinding::new(approved()).unwrap();
    let mut limits = matching_limits();
    limits.inbound_max = robinhood_u256(10_000); // backend over-claims inbound
    limits.outbound_rolling_limit = robinhood_u256(9_000_000); // chain looser outbound

    let mismatches = binding.compare(&limits);
    assert_eq!(mismatches.len(), 2, "{mismatches:?}");
    assert!(mismatches
        .iter()
        .any(|m| matches!(m, PolicyMismatch::PerTransferAboveChainMax { direction, .. } if *direction == "inbound")));
    assert!(mismatches
        .iter()
        .any(|m| matches!(m, PolicyMismatch::RollingAboveApprovedPolicy { direction, .. } if *direction == "outbound")));
}

#[test]
fn a_contract_limit_too_large_for_the_amount_model_is_reported_not_truncated() {
    let binding = RobinhoodPolicyBinding::new(approved()).unwrap();
    let mut limits = matching_limits();
    limits.inbound_max = EvmU256::MAX;

    let mismatches = binding.compare(&limits);
    assert_eq!(mismatches.len(), 1, "{mismatches:?}");
    assert!(matches!(
        mismatches[0],
        PolicyMismatch::ChainLimitUnrepresentable { .. }
    ));
    // Fails closed: an uncomparable limit is treated as the dangerous
    // case, never as "probably fine".
    assert!(is_backend_over_claim(&mismatches[0]));
}

#[test]
fn a_policy_for_another_chain_is_refused_by_this_binding() {
    // Unreachable through `ChainPolicy::new`, which already refuses every
    // non-policy-governed chain — asserted so the binding stays honest if
    // another chain is ever added to POLICY_GOVERNED_CHAINS.
    assert!(ChainPolicy::new_symmetric(
        Chain::Solana,
        600,
        CanonicalAtomic(ONE_GLC),
        CanonicalAtomic(ONE_GLC)
    )
    .is_err());
}

#[test]
fn a_strict_rolling_policy_that_does_not_halve_exactly_is_refused() {
    let odd = ChainPolicy::new_symmetric(
        Chain::Robinhood,
        600,
        CanonicalAtomic(1_000),
        CanonicalAtomic(1_000_001),
    )
    .unwrap();
    assert!(matches!(
        RobinhoodPolicyBinding::new(odd),
        Err(RobinhoodPolicyError::RollingPolicyNotHalvable { .. })
    ));
}

#[test]
fn a_policy_the_contract_could_never_hold_is_refused_before_any_chain_read() {
    // `_validateLimits` rejects any Limits whose rollingLimit is below
    // its max. A strict policy of 30,000 GLC/24h implies an on-chain
    // rolling limit of 15,000 — below a 20,000 GLC per-transfer maximum —
    // so no `setLimits` call could ever install it.
    let impossible = ChainPolicy::new_symmetric(
        Chain::Robinhood,
        600,
        CanonicalAtomic(20_000 * ONE_GLC),
        CanonicalAtomic(30_000 * ONE_GLC),
    )
    .expect("valid as a bare policy: rolling >= per-transfer");
    assert!(matches!(
        RobinhoodPolicyBinding::new(impossible),
        Err(RobinhoodPolicyError::ImpliedRollingBelowPerTransfer { .. })
    ));

    // Exactly 2x the per-transfer limit is the boundary and is
    // installable: the implied on-chain rolling limit equals the max.
    let boundary = ChainPolicy::new_symmetric(
        Chain::Robinhood,
        600,
        CanonicalAtomic(20_000 * ONE_GLC),
        CanonicalAtomic(40_000 * ONE_GLC),
    )
    .unwrap();
    let binding = RobinhoodPolicyBinding::new(boundary).unwrap();
    assert_eq!(
        binding.expected_onchain_rolling_limit(),
        binding.inbound_max()
    );
    assert_eq!(
        binding.expected_onchain_rolling_limit(),
        binding.outbound_max()
    );
}

#[test]
fn the_largest_canonical_policy_still_widens_without_overflow() {
    // u64::MAX canonical is ~1.8e19 atomic units; widened by 10^10 that
    // is ~1.8e29, comfortably inside u128. Asserted so a decimals change
    // that broke this would fail here rather than in a settlement path.
    let huge = ChainPolicy::new_symmetric(
        Chain::Robinhood,
        600,
        CanonicalAtomic(u64::MAX / 2),
        CanonicalAtomic(u64::MAX - 1),
    )
    .unwrap();
    let binding = RobinhoodPolicyBinding::new(huge).expect("widening must not overflow");
    assert_eq!(
        binding.rolling_daily_policy().get(),
        u128::from(u64::MAX - 1) * CANONICAL_TO_ROBINHOOD_SCALE
    );
}

// ------------------------------------ inbound vs outbound (2026-09-21) --

/// An asymmetric policy binds each direction to its own contract field:
/// `inboundMax` from the inbound limit, `outboundMax` from the outbound
/// one; the one rolling bucket must cover the larger; and `compare`
/// reports each direction against its own figure — a chain holding the
/// old symmetric 20_000/20_000 against an asymmetric 20_000/2_000_000
/// policy reports ONLY outbound as below the policy.
#[test]
fn an_asymmetric_policy_binds_each_direction_to_its_own_contract_field() {
    let policy = ChainPolicy::new(
        Chain::Robinhood,
        600,
        CanonicalAtomic(20_000 * ONE_GLC),
        CanonicalAtomic(2_000_000 * ONE_GLC),
        CanonicalAtomic(10_000_000 * ONE_GLC),
    )
    .unwrap();
    let binding = RobinhoodPolicyBinding::new(policy).unwrap();
    assert_eq!(binding.inbound_max().get(), 20_000u128 * 10u128.pow(18));
    assert_eq!(binding.outbound_max().get(), 2_000_000u128 * 10u128.pow(18));
    assert_eq!(
        binding.per_transfer_limit(LimitDirection::Inbound),
        binding.inbound_max()
    );
    assert_eq!(
        binding.per_transfer_limit(LimitDirection::Outbound),
        binding.outbound_max()
    );
    // Bucket 5_000_000 ≥ 2_000_000: bindable. Daily 3_000_000 → bucket
    // 1_500_000 < outbound 2_000_000: not bindable, named for outbound.
    let too_small = ChainPolicy::new(
        Chain::Robinhood,
        600,
        CanonicalAtomic(20_000 * ONE_GLC),
        CanonicalAtomic(2_000_000 * ONE_GLC),
        CanonicalAtomic(3_000_000 * ONE_GLC),
    )
    .unwrap();
    assert!(matches!(
        RobinhoodPolicyBinding::new(too_small),
        Err(RobinhoodPolicyError::ImpliedRollingBelowPerTransfer {
            field: "outbound_per_transfer_limit",
            ..
        })
    ));

    // Today's chain (20_000 both ways) against the asymmetric policy.
    let mismatches = binding.compare(&matching_limits());
    assert_eq!(mismatches.len(), 1, "{mismatches:?}");
    assert!(
        matches!(
            &mismatches[0],
            PolicyMismatch::PerTransferBelowChainMax { direction: "outbound", field: "outboundMax", backend_canonical, .. }
                if *backend_canonical == 2_000_000 * ONE_GLC
        ) || matches!(
            &mismatches[0],
            PolicyMismatch::PerTransferAboveChainMax { direction: "outbound", field: "outboundMax", backend_canonical, .. }
                if *backend_canonical == 2_000_000 * ONE_GLC
        ),
        "{mismatches:?}"
    );
    // The reconciled chain: no mismatch at all.
    let reconciled = BridgeLimits {
        outbound_max: robinhood_u256(2_000_000),
        ..matching_limits()
    };
    assert!(binding.compare(&reconciled).is_empty());
}
