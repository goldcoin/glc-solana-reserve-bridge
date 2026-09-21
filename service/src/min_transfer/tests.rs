use super::*;
use crate::routes::Route;

/// Whole GLC as a canonical 8dp amount.
fn glc(whole: u64) -> CanonicalAtomic {
    CanonicalAtomic(whole * CANONICAL_SCALE)
}

/// The policy, stated as a test rather than inferred from a chain.
#[test]
fn the_source_minimum_is_exactly_one_hundred_glc() {
    assert_eq!(SOURCE_MINIMUM_CANONICAL, glc(100));
    assert_eq!(SOURCE_MINIMUM_CANONICAL.0, 10_000_000_000);
}

/// Every route, the same floor — including the two cross routes, which
/// have a chain floor on BOTH legs and could most easily have been given
/// a different rule by accident.
#[test]
fn every_route_carries_the_same_source_minimum() {
    for route in Route::ALL {
        assert_eq!(
            source_minimum(route),
            SOURCE_MINIMUM_CANONICAL,
            "{} must carry the one policy floor",
            route.as_str()
        );
    }
}

/// The boundary, on every route, stated in the exact units the policy is
/// written in: 100.00000000 passes, 99.99999999 does not.
#[test]
fn exactly_one_hundred_passes_and_one_atomic_unit_below_fails() {
    for route in Route::ALL {
        assert!(
            enforce_source_minimum(route, CanonicalAtomic(10_000_000_000)).is_ok(),
            "{}: exactly 100.00000000 GLC must be accepted",
            route.as_str()
        );
        let err = enforce_source_minimum(route, CanonicalAtomic(9_999_999_999))
            .expect_err("99.99999999 GLC must be refused");
        assert_eq!(
            err,
            MinTransferError::BelowSourceMinimum {
                route: route.as_str(),
                gross: 9_999_999_999,
                minimum: 10_000_000_000,
            }
        );
    }
}

#[test]
fn anything_above_the_minimum_passes_and_zero_does_not() {
    for route in Route::ALL {
        assert!(enforce_source_minimum(route, glc(20_000)).is_ok());
        assert!(enforce_source_minimum(route, CanonicalAtomic(0)).is_err());
        assert!(enforce_source_minimum(route, CanonicalAtomic(1)).is_err());
    }
}

/// The fee comes off AFTER the check, so the destination figure is
/// expected to be below the minimum. This is the part of the policy most
/// likely to be "fixed" by someone who reads a 97 GLC payout as a bug.
#[test]
fn a_minimum_transfer_delivers_less_than_the_minimum_and_that_is_correct() {
    let breakdown = compute_fee_at_bps(SOURCE_MINIMUM_CANONICAL, 300).unwrap();
    assert_eq!(breakdown.gross, glc(100));
    assert_eq!(breakdown.fee, glc(3));
    assert_eq!(breakdown.net, glc(97));
    assert!(breakdown.net.0 < SOURCE_MINIMUM_CANONICAL.0);
    // And the gross that was checked is untouched by the fee: the policy
    // is a statement about what was SENT.
    assert!(enforce_source_minimum(Route::GlcToSol, breakdown.gross).is_ok());
}

/// The destination floor a route needs is a function of its fee, which is
/// the whole reason the POLICY is not.
#[test]
fn the_required_destination_floor_tracks_the_fee() {
    assert_eq!(required_destination_floor(0).unwrap(), glc(100));
    assert_eq!(required_destination_floor(300).unwrap(), glc(97));
    assert_eq!(required_destination_floor(600).unwrap(), glc(94));
    assert_eq!(required_destination_floor(1_000).unwrap(), glc(90));
}

/// Fee rounding is floored in the user's favour, so the required floor
/// must be the real computed net rather than continuous algebra — or a
/// preflight would demand a floor the chain need not have.
#[test]
fn the_required_floor_uses_the_real_floored_fee_never_algebra() {
    // 333 bps of 100 GLC = 3.33 GLC, exact at 8dp.
    assert_eq!(
        required_destination_floor(333).unwrap(),
        CanonicalAtomic(9_667_000_000)
    );
    // 1 bp of 100 GLC = 0.01 GLC.
    assert_eq!(
        required_destination_floor(1).unwrap(),
        CanonicalAtomic(9_999_000_000)
    );
    // Every rate the config allows must at least be computable, and none
    // may demand a floor above the policy itself.
    for fee_bps in [0, 1, 250, 300, 600, 900, crate::fees::MAX_FEE_BPS] {
        let net = required_destination_floor(fee_bps).unwrap();
        assert!(net.0 <= SOURCE_MINIMUM_CANONICAL.0);
        let recomputed = compute_fee_at_bps(SOURCE_MINIMUM_CANONICAL, fee_bps).unwrap();
        assert_eq!(net, recomputed.net);
    }
}

#[test]
fn a_satisfied_destination_floor_reports_no_violation() {
    let check = DestinationFloorCheck {
        route: Route::GlcToSol,
        fee_bps: 300,
        required_at_most: glc(97),
        chain_floor: glc(97),
    };
    assert!(check.is_satisfied(), "the exact boundary is satisfied");
    assert!(check.violation().is_none());

    let lower = DestinationFloorCheck {
        chain_floor: glc(50),
        ..check
    };
    assert!(lower.is_satisfied());
}

/// The production situation this whole module exists to surface: a 99 GLC
/// net floor against a 300 bps route needs to be 97 or lower.
#[test]
fn a_floor_above_the_requirement_is_reported_with_the_value_to_set() {
    let check = DestinationFloorCheck {
        route: Route::GlcToSol,
        fee_bps: 300,
        required_at_most: glc(97),
        chain_floor: glc(99),
    };
    assert!(!check.is_satisfied());
    let violation = check.violation().expect("a violation");
    assert!(violation.contains("GlcToSol"), "{violation}");
    // The value an operator must set, spelled out rather than implied.
    assert!(violation.contains("9700000000"), "{violation}");
    assert!(violation.contains("9900000000"), "{violation}");
}

/// One atomic unit over the requirement still breaks the policy. Pinned
/// because an off-by-one here reads as "close enough" and is not.
#[test]
fn one_atomic_unit_above_the_requirement_is_a_violation() {
    let check = DestinationFloorCheck {
        route: Route::GlcToRhn,
        fee_bps: 300,
        required_at_most: glc(97),
        chain_floor: CanonicalAtomic(9_700_000_001),
    };
    assert!(!check.is_satisfied());
    assert!(check.violation().is_some());
}

// ----------------------------- fold-time parking, at the ledger boundary --

/// A deposit below the source minimum is RECORDED, never dropped.
///
/// The value has already moved on the source chain by the time a fold
/// runs, so "refuse" cannot mean "discard". These pin the shape every
/// fold path relies on: a request exists, it holds no destination
/// capacity, it sits in `ManualReview`, and its note says why — which is
/// what makes it visible to an operator and refundable through the
/// ordinary path.
mod fold_parking {
    use crate::ledger::{Ledger, RequestAmounts, RequestState, SolFoldOutcome};

    fn amounts(gross: u64, fee_bps: u64) -> RequestAmounts {
        let fb = crate::amount_conversion::compute_fee_at_bps(
            crate::amount_conversion::CanonicalAtomic(gross),
            fee_bps,
        )
        .unwrap();
        RequestAmounts {
            gross_atomic: fb.gross.0,
            fee_bps: fb.fee_bps,
            fee_atomic: fb.fee.0,
            net_atomic: fb.net.0,
            net_destination_atomic: fb.net.0,
            quote: None,
        }
    }

    /// Ample capacity, so nothing below can be explained by the reserve
    /// being too small — the refusal under test must be the minimum.
    fn ledger_with_capacity() -> Ledger {
        let mut ledger = Ledger::open_in_memory().unwrap();
        ledger
            .configure_reserve(
                crate::ledger::ReserveDirection::GoldcoinReserve,
                1_000_000_000_000,
                0,
                500_000_000_000,
                200_000_000_000,
                100_000_000_000,
                0,
            )
            .unwrap();
        ledger
    }

    /// The `SolToGlc` source leg: exactly the case that opens up once the
    /// Solana program's `min_transfer_amount` is lowered far enough for a
    /// minimum transfer to be deliverable. 99.99999999 GLC clears the
    /// chain's floor and must still be parked here.
    #[test]
    fn a_sub_minimum_sol_to_glc_deposit_is_parked_with_its_reason() {
        let mut ledger = ledger_with_capacity();

        let outcome = ledger
            .fold_sol_deposit(
                0,
                amounts(9_999_999_999, 300),
                [7u8; 32],
                b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef",
                Some("below source minimum: test"),
                0,
            )
            .unwrap();
        let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
            panic!("a sub-minimum deposit must fold to ManualReview, got {outcome:?}")
        };

        let request = ledger.get_request(request_id).unwrap().expect("a row");
        assert_eq!(
            request.state,
            RequestState::ManualReview,
            "a sub-minimum deposit must be parked, never payable"
        );
        assert!(
            request
                .manual_review_note
                .as_deref()
                .unwrap_or_default()
                .contains("below source minimum"),
            "the note must say why: {:?}",
            request.manual_review_note
        );
        // The deposit itself is recorded in full — nothing was discarded.
        assert_eq!(request.gross_amount_atomic, 9_999_999_999);
    }

    /// The refusal outranks a capacity blocker: the two have different
    /// remedies, and "the reserve is too small" would send an operator to
    /// fix the wrong thing.
    #[test]
    fn the_minimum_refusal_outranks_a_capacity_blocker() {
        // A configured reserve holding NOTHING, so a capacity blocker is
        // certainly present and competing with the explicit refusal.
        let mut ledger = Ledger::open_in_memory().unwrap();
        ledger
            .configure_reserve(
                crate::ledger::ReserveDirection::GoldcoinReserve,
                0,
                0,
                500_000_000_000,
                200_000_000_000,
                100_000_000_000,
                0,
            )
            .unwrap();
        let outcome = ledger
            .fold_sol_deposit(
                0,
                amounts(9_999_999_999, 300),
                [7u8; 32],
                b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef",
                Some("below source minimum: test"),
                0,
            )
            .unwrap();
        let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
            panic!("expected a park, got {outcome:?}")
        };
        let note = ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .unwrap_or_default();
        assert!(
            note.contains("below source minimum"),
            "the explicit refusal must win over the capacity note: {note}"
        );
    }

    /// And an ordinary deposit is unaffected: no refusal, no park.
    #[test]
    fn a_deposit_at_or_above_the_minimum_folds_payable_as_before() {
        let mut ledger = ledger_with_capacity();
        let outcome = ledger
            .fold_sol_deposit(
                0,
                amounts(10_000_000_000, 300),
                [7u8; 32],
                b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef",
                None,
                0,
            )
            .unwrap();
        let SolFoldOutcome::FoldedFinalized { request_id } = outcome else {
            panic!("exactly 100 GLC must fold payable, got {outcome:?}")
        };
        let request = ledger.get_request(request_id).unwrap().unwrap();
        assert_eq!(request.state, RequestState::SourceFinalized);
        assert!(request.manual_review_note.is_none());
    }
}

// ------------------------------------------------ the source MAXIMUM --

#[test]
fn the_source_maximum_is_by_source_chain_and_inclusive() {
    use crate::routes::Chain;
    assert_eq!(SOURCE_MAXIMUM_CANONICAL.0, 50_000 * 100_000_000);
    assert_eq!(SOURCE_MAXIMUM_ROBINHOOD_CANONICAL.0, 20_000 * 100_000_000);
    for route in Route::ALL {
        let expected = match route.source_chain() {
            Chain::Robinhood => SOURCE_MAXIMUM_ROBINHOOD_CANONICAL,
            Chain::Goldcoin | Chain::Solana => SOURCE_MAXIMUM_CANONICAL,
        };
        assert_eq!(source_maximum(route), expected, "{}", route.as_str());
        assert!(enforce_source_maximum(route, expected).is_ok());
        assert!(enforce_source_maximum(route, CanonicalAtomic(1)).is_ok());
        let err = enforce_source_maximum(route, CanonicalAtomic(expected.0 + 1)).unwrap_err();
        assert_eq!(
            err,
            MaxTransferError::AboveSourceMaximum {
                route: route.as_str(),
                gross: expected.0 + 1,
                maximum: expected.0,
            }
        );
        assert!(err.to_string().contains("amount SENT"));
    }
    // The two Robinhood-sourced routes are the 20_000 ones, no others.
    assert_eq!(source_maximum(Route::RhnToGlc).0, 20_000 * 100_000_000);
    assert_eq!(source_maximum(Route::RhnToSol).0, 20_000 * 100_000_000);
    assert_eq!(source_maximum(Route::GlcToRhn).0, 50_000 * 100_000_000);
    assert_eq!(source_maximum(Route::SolToRhn).0, 50_000 * 100_000_000);
    // The explicit-ceiling form is the same comparison.
    assert!(
        enforce_source_maximum_at(Route::GlcToSol, CanonicalAtomic(10), CanonicalAtomic(10))
            .is_ok()
    );
    assert!(
        enforce_source_maximum_at(Route::GlcToSol, CanonicalAtomic(11), CanonicalAtomic(10))
            .is_err()
    );
    // Minimum and maximum bracket a real range.
    const { assert!(SOURCE_MINIMUM_CANONICAL.0 < SOURCE_MAXIMUM_CANONICAL.0) };
}
