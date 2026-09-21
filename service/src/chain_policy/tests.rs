use super::*;

fn policy(fee_bps: u64, per_transfer: u64, rolling: u64) -> Result<ChainPolicy, ChainPolicyError> {
    ChainPolicy::new_symmetric(
        Chain::Robinhood,
        fee_bps,
        CanonicalAtomic(per_transfer),
        CanonicalAtomic(rolling),
    )
}

/// The exact launch policy this change exists to make configurable:
/// 6.00%, 20,000 GLC per transfer, 10,000,000 GLC per strict 24h.
#[test]
fn robinhood_launch_policy_is_accepted_exactly_as_specified() {
    let p = policy(600, 2_000_000_000_000, 1_000_000_000_000_000).expect("the approved policy");
    assert_eq!(p.chain(), Chain::Robinhood);
    assert_eq!(p.fee_bps(), 600);
    assert_eq!(
        p.inbound_per_transfer_limit(),
        CanonicalAtomic(2_000_000_000_000)
    );
    assert_eq!(
        p.outbound_per_transfer_limit(),
        CanonicalAtomic(2_000_000_000_000)
    );
    assert!(p.is_symmetric());
    assert_eq!(
        p.rolling_daily_limit(),
        CanonicalAtomic(1_000_000_000_000_000)
    );
    // Sanity on the human figures the runbook states, so a typo in the
    // atomic values cannot pass review.
    assert_eq!(p.inbound_per_transfer_limit().0 / 100_000_000, 20_000);
    assert_eq!(p.rolling_daily_limit().0 / 100_000_000, 10_000_000);
}

#[test]
fn solana_can_never_be_given_a_configured_policy() {
    // The whole point of POLICY_GOVERNED_CHAINS: no config file, however
    // written, can change the Goldcoin<->Solana fee or limits.
    assert!(!POLICY_GOVERNED_CHAINS.contains(&Chain::Solana));
    assert!(!POLICY_GOVERNED_CHAINS.contains(&Chain::Goldcoin));
    for chain in [Chain::Solana, Chain::Goldcoin] {
        assert!(matches!(
            ChainPolicy::new_symmetric(
                chain,
                600,
                CanonicalAtomic(2_000_000_000_000),
                CanonicalAtomic(1_000_000_000_000_000)
            ),
            Err(ChainPolicyError::ChainNotPolicyGoverned { .. })
        ));
    }
}

#[test]
fn a_robinhood_policy_does_not_change_any_other_chains_fee() {
    let mut policies = ChainPolicies::new();
    policies
        .insert(policy(600, 2_000_000_000_000, 1_000_000_000_000_000).unwrap())
        .unwrap();

    assert_eq!(policies.fee_bps_for(Chain::Robinhood), 600);
    // Solana and Goldcoin keep the compiled-in rate. Asserted against
    // BRIDGE_FEE_BPS rather than against `300` so this test keeps meaning
    // the right thing if the global rate ever changes.
    assert_eq!(policies.fee_bps_for(Chain::Solana), BRIDGE_FEE_BPS);
    assert_eq!(policies.fee_bps_for(Chain::Goldcoin), BRIDGE_FEE_BPS);
    assert_ne!(policies.fee_bps_for(Chain::Solana), 600);
    assert!(policies.get(Chain::Solana).is_none());
    assert!(policies.get(Chain::Goldcoin).is_none());
}

#[test]
fn an_empty_policy_set_leaves_every_chain_on_the_compiled_in_rate() {
    let policies = ChainPolicies::new();
    assert!(policies.is_empty());
    for chain in Chain::ALL {
        assert_eq!(policies.fee_bps_for(chain), BRIDGE_FEE_BPS);
        assert!(policies.get(chain).is_none());
    }
}

#[test]
fn duplicate_policies_for_one_chain_are_refused() {
    let mut policies = ChainPolicies::new();
    policies
        .insert(policy(600, 2_000_000_000_000, 1_000_000_000_000_000).unwrap())
        .unwrap();
    assert!(matches!(
        policies.insert(policy(300, 1_000_000_000_000, 1_000_000_000_000_000).unwrap()),
        Err(ChainPolicyError::DuplicatePolicy { .. })
    ));
    // The first policy stands; a refused insert never half-applies.
    assert_eq!(policies.fee_bps_for(Chain::Robinhood), 600);
}

#[test]
fn a_zero_fee_rate_is_accepted_because_a_free_route_is_a_real_choice() {
    // Was refused, on the reasoning that 0 was "a rate the protocol has
    // never charged". That reasoning went with the allowlist: fee = 0 and
    // net = gross is arithmetically fine and settles end to end.
    let policy = policy(0, 2_000_000_000_000, 1_000_000_000_000_000).unwrap();
    assert_eq!(policy.fee_bps(), 0);
}

#[test]
fn a_fee_rate_at_or_above_one_hundred_percent_is_refused() {
    // 10,000 bps leaves the user nothing on every transfer; above it the
    // net entitlement would be negative. Both are range refusals, and the
    // message says which.
    for fee_bps in [
        crate::amount_conversion::BPS_DENOMINATOR,
        crate::amount_conversion::BPS_DENOMINATOR + 1,
        u64::MAX,
    ] {
        assert!(matches!(
            policy(fee_bps, 2_000_000_000_000, 1_000_000_000_000_000),
            Err(ChainPolicyError::FeeBpsOutOfRange { .. })
        ));
    }
}

#[test]
fn any_rate_in_range_is_accepted_with_no_reference_to_what_was_charged_before() {
    // The point of removing the allowlist: 450 bps was refused purely for
    // being new. 400 (4%) is the case an operator actually asked for.
    for rate in [
        crate::fees::MIN_FEE_BPS,
        1,
        137,
        300,
        400,
        450,
        600,
        1_234,
        crate::fees::MAX_FEE_BPS,
    ] {
        let policy = policy(rate, 2_000_000_000_000, 1_000_000_000_000_000)
            .unwrap_or_else(|e| panic!("{rate} bps must be configurable: {e}"));
        assert_eq!(policy.fee_bps(), rate);
    }
}

#[test]
fn a_zero_transfer_limit_is_refused() {
    assert!(matches!(
        policy(600, 0, 1_000_000_000_000_000),
        Err(ChainPolicyError::ZeroPerTransferLimit { .. })
    ));
    assert!(matches!(
        policy(600, 2_000_000_000_000, 0),
        Err(ChainPolicyError::ZeroRollingDailyLimit { .. })
    ));
}

#[test]
fn a_rolling_limit_below_the_per_transfer_limit_is_refused() {
    assert!(matches!(
        policy(600, 2_000_000_000_000, 1_999_999_999_999),
        Err(ChainPolicyError::RollingBelowPerTransfer { .. })
    ));
    // Equal is the boundary and is allowed: exactly one maximum-size
    // transfer per day is a coherent, if strict, policy.
    assert!(policy(600, 2_000_000_000_000, 2_000_000_000_000).is_ok());
}

#[test]
fn the_largest_representable_limits_are_accepted() {
    // No arbitrary ceiling of this module's own invention: the canonical
    // unit's own range is the range.
    let p = policy(600, u64::MAX, u64::MAX).expect("u64::MAX canonical is a valid statement");
    assert_eq!(p.rolling_daily_limit(), CanonicalAtomic(u64::MAX));
}

#[test]
fn every_error_message_names_the_chain() {
    let errors = [
        ChainPolicyError::ChainNotPolicyGoverned { chain: "solana" },
        ChainPolicyError::DuplicatePolicy { chain: "robinhood" },
        ChainPolicyError::FeeBpsOutOfRange {
            chain: "robinhood",
            fee_bps: 10_000,
        },
        ChainPolicyError::ZeroPerTransferLimit {
            chain: "robinhood",
            field: "inbound_per_transfer_limit",
        },
        ChainPolicyError::ZeroRollingDailyLimit { chain: "robinhood" },
        ChainPolicyError::RollingBelowPerTransfer {
            chain: "robinhood",
            field: "outbound_per_transfer_limit",
            per_transfer: 2,
            rolling: 1,
        },
    ];
    for error in errors {
        let text = error.to_string();
        assert!(
            text.contains("robinhood") || text.contains("solana"),
            "error does not name its chain: {text}"
        );
    }
}

// ------------------------------------ inbound vs outbound (2026-09-21) --

/// The two per-transfer limits are separate figures: the legacy
/// one-figure form is exactly the symmetric case, an asymmetric policy
/// keeps both, and the strict daily ceiling must cover the LARGER one —
/// the error names the direction it fails for.
#[test]
fn inbound_and_outbound_limits_are_separate_and_the_daily_ceiling_covers_the_larger() {
    let asymmetric = ChainPolicy::new(
        Chain::Robinhood,
        600,
        CanonicalAtomic(2_000_000_000_000),   // 20_000 GLC in
        CanonicalAtomic(200_000_000_000_000), // 2_000_000 GLC out
        CanonicalAtomic(1_000_000_000_000_000),
    )
    .unwrap();
    assert!(!asymmetric.is_symmetric());
    assert_eq!(asymmetric.inbound_per_transfer_limit().0, 2_000_000_000_000);
    assert_eq!(
        asymmetric.outbound_per_transfer_limit().0,
        200_000_000_000_000
    );
    assert_eq!(
        asymmetric.per_transfer_limit(TransferDirection::Inbound),
        asymmetric.inbound_per_transfer_limit()
    );
    assert_eq!(
        asymmetric.per_transfer_limit(TransferDirection::Outbound),
        asymmetric.outbound_per_transfer_limit()
    );
    let symmetric = ChainPolicy::new_symmetric(
        Chain::Robinhood,
        600,
        CanonicalAtomic(2_000_000_000_000),
        CanonicalAtomic(1_000_000_000_000_000),
    )
    .unwrap();
    assert!(symmetric.is_symmetric());
    assert_eq!(
        symmetric,
        ChainPolicy::new(
            Chain::Robinhood,
            600,
            CanonicalAtomic(2_000_000_000_000),
            CanonicalAtomic(2_000_000_000_000),
            CanonicalAtomic(1_000_000_000_000_000),
        )
        .unwrap()
    );
    // Zero in either direction is refused and named.
    for (inbound, outbound, field) in [
        (0, 1, "inbound_per_transfer_limit"),
        (1, 0, "outbound_per_transfer_limit"),
    ] {
        assert_eq!(
            ChainPolicy::new(
                Chain::Robinhood,
                600,
                CanonicalAtomic(inbound),
                CanonicalAtomic(outbound),
                CanonicalAtomic(10),
            ),
            Err(ChainPolicyError::ZeroPerTransferLimit {
                chain: "robinhood",
                field
            })
        );
    }
    // A daily ceiling below the OUTBOUND limit is refused for outbound.
    assert_eq!(
        ChainPolicy::new(
            Chain::Robinhood,
            600,
            CanonicalAtomic(2_000_000_000_000),
            CanonicalAtomic(200_000_000_000_000),
            CanonicalAtomic(100_000_000_000_000),
        ),
        Err(ChainPolicyError::RollingBelowPerTransfer {
            chain: "robinhood",
            field: "outbound_per_transfer_limit",
            per_transfer: 200_000_000_000_000,
            rolling: 100_000_000_000_000,
        })
    );
    assert_eq!(
        TransferDirection::Inbound.field(),
        "inbound_per_transfer_limit"
    );
    assert_eq!(
        TransferDirection::Outbound.field(),
        "outbound_per_transfer_limit"
    );
}
