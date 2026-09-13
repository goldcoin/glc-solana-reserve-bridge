//! The shared admission evaluator's ranking and its route-level form.
//!
//! These are pure-function tests over [`InboundAdmissionGates`]: no
//! database, no reserve row. What they pin is the part that used to be
//! duplicated — the ranking, and the exact relationship between the
//! per-request decision a fold makes and the amount-independent one the
//! public API reports.

use super::*;

/// Every gate open, comfortable headroom, no floor and no buffer.
fn healthy() -> InboundAdmissionGates {
    InboundAdmissionGates {
        route_admission_closed: false,
        paused: false,
        admission_closed: false,
        liquidity_admission_closed: false,
        confirmed_headroom_atomic: 1_000_000,
        admission_buffer_atomic: 0,
        min_available_utxo_count: 0,
        available_utxo_count: 0,
    }
}

#[test]
fn a_healthy_reserve_admits() {
    assert_eq!(healthy().blocker(1_000, InboundRateLimits::default()), None);
    assert_eq!(healthy().route_blocker(), None);
}

/// The ranking, stated once as data. Each row sets exactly one gate on
/// top of `healthy()` and asserts the blocker it must produce.
#[test]
fn each_gate_produces_its_own_blocker_and_note() {
    let cases: [(
        InboundAdmissionGates,
        InboundRateLimits,
        InboundAdmissionBlocker,
        &str,
    ); 7] = [
        (
            InboundAdmissionGates {
                route_admission_closed: true,
                ..healthy()
            },
            InboundRateLimits::default(),
            InboundAdmissionBlocker::RouteAdmissionClosed,
            "route_admission_closed_at_fold",
        ),
        (
            InboundAdmissionGates {
                admission_closed: true,
                ..healthy()
            },
            InboundRateLimits::default(),
            InboundAdmissionBlocker::AdmissionClosed,
            "admission_closed_at_fold",
        ),
        (
            InboundAdmissionGates {
                paused: true,
                ..healthy()
            },
            InboundRateLimits::default(),
            InboundAdmissionBlocker::ReservePaused,
            "reserve_paused_at_fold",
        ),
        (
            healthy(),
            InboundRateLimits {
                source_wallet_rate_limited: true,
                recipient_rate_limited: false,
            },
            InboundAdmissionBlocker::SourceWalletRateLimited,
            "wallet_source_24h_limit",
        ),
        (
            healthy(),
            InboundRateLimits {
                source_wallet_rate_limited: false,
                recipient_rate_limited: true,
            },
            InboundAdmissionBlocker::RecipientRateLimited,
            "wallet_destination_24h_limit",
        ),
        (
            InboundAdmissionGates {
                min_available_utxo_count: 5,
                available_utxo_count: 5,
                ..healthy()
            },
            InboundRateLimits::default(),
            InboundAdmissionBlocker::UtxoLiquidityLow,
            "utxo_liquidity_low_at_fold",
        ),
        (
            InboundAdmissionGates {
                liquidity_admission_closed: true,
                ..healthy()
            },
            InboundRateLimits::default(),
            InboundAdmissionBlocker::LiquidityBufferLow,
            "liquidity_buffer_low_at_fold",
        ),
    ];
    for (gates, limits, expected, note) in cases {
        assert_eq!(
            gates.blocker(1_000, limits),
            Some(expected),
            "wrong blocker for {expected:?}"
        );
        assert_eq!(expected.manual_review_note(), note);
    }
}

/// Plain capacity exhaustion is the LAST resort, reached only when every
/// more specific gate is open.
#[test]
fn insufficient_capacity_is_the_fallback() {
    let gates = InboundAdmissionGates {
        confirmed_headroom_atomic: 500,
        ..healthy()
    };
    assert_eq!(
        gates.blocker(1_000, InboundRateLimits::default()),
        Some(InboundAdmissionBlocker::InsufficientCapacity)
    );
    assert_eq!(
        InboundAdmissionBlocker::InsufficientCapacity.manual_review_note(),
        "insufficient_capacity_at_fold"
    );
    // ...but the reserve is not closed to ALL demand: a small enough
    // deposit still fits, which is exactly what `route_blocker` reports.
    assert_eq!(gates.blocker(500, InboundRateLimits::default()), None);
    assert_eq!(gates.route_blocker(), None);
}

/// The ranking order itself: with EVERY gate closed at once, the most
/// specific one wins, and removing it reveals the next. This is the
/// property that used to live in two hand-written `else if` chains.
#[test]
fn the_ranking_is_most_specific_first() {
    let all_closed = InboundAdmissionGates {
        route_admission_closed: true,
        paused: true,
        admission_closed: true,
        liquidity_admission_closed: true,
        confirmed_headroom_atomic: -1,
        admission_buffer_atomic: 10,
        min_available_utxo_count: 5,
        available_utxo_count: 0,
    };
    let both_limits = InboundRateLimits {
        source_wallet_rate_limited: true,
        recipient_rate_limited: true,
    };
    let expected = [
        InboundAdmissionBlocker::RouteAdmissionClosed,
        InboundAdmissionBlocker::AdmissionClosed,
        InboundAdmissionBlocker::ReservePaused,
        InboundAdmissionBlocker::SourceWalletRateLimited,
        InboundAdmissionBlocker::RecipientRateLimited,
        InboundAdmissionBlocker::UtxoLiquidityLow,
        InboundAdmissionBlocker::LiquidityBufferLow,
        InboundAdmissionBlocker::InsufficientCapacity,
    ];
    let mut gates = all_closed;
    let mut limits = both_limits;
    for step in expected {
        assert_eq!(
            gates.blocker(1_000, limits),
            Some(step),
            "expected {step:?} at this point in the ranking"
        );
        match step {
            InboundAdmissionBlocker::RouteAdmissionClosed => gates.route_admission_closed = false,
            InboundAdmissionBlocker::AdmissionClosed => gates.admission_closed = false,
            InboundAdmissionBlocker::ReservePaused => gates.paused = false,
            InboundAdmissionBlocker::SourceWalletRateLimited => {
                limits.source_wallet_rate_limited = false
            }
            InboundAdmissionBlocker::RecipientRateLimited => limits.recipient_rate_limited = false,
            InboundAdmissionBlocker::UtxoLiquidityLow => gates.min_available_utxo_count = 0,
            InboundAdmissionBlocker::LiquidityBufferLow => {
                gates.liquidity_admission_closed = false;
                gates.admission_buffer_atomic = 0;
            }
            InboundAdmissionBlocker::InsufficientCapacity => {}
        }
    }
}

/// `route_blocker` is not a re-statement of the amount-dependent gates,
/// it is literally the real decision at the smallest amount that can
/// exist. Pinned so nobody "optimises" it into a second formula.
#[test]
fn route_blocker_is_the_real_decision_at_one_atomic_unit() {
    for headroom in [-5i64, 0, 1, 2, 99, 100, 101, 1_000] {
        for buffer in [0i64, 1, 100] {
            for (min_count, count) in [(0i64, 0i64), (5, 5), (5, 6)] {
                let gates = InboundAdmissionGates {
                    confirmed_headroom_atomic: headroom,
                    admission_buffer_atomic: buffer,
                    min_available_utxo_count: min_count,
                    available_utxo_count: count,
                    ..healthy()
                };
                assert_eq!(
                    gates.route_blocker(),
                    gates.blocker(1, InboundRateLimits::default()),
                    "route_blocker must BE blocker(1, no limits) — headroom {headroom}, \
                     buffer {buffer}, utxo {count}/{min_count}"
                );
            }
        }
    }
}

/// The weakest-form identities the route-level answer relies on:
/// `available` is `headroom > 0` when no buffer is configured, and
/// `headroom > buffer` when one is. Asserted against `route_blocker`'s
/// actual output rather than assumed.
#[test]
fn route_availability_is_the_weakest_form_of_the_amount_gates() {
    for headroom in [-1i64, 0, 1, 2] {
        let gates = InboundAdmissionGates {
            confirmed_headroom_atomic: headroom,
            ..healthy()
        };
        assert_eq!(
            gates.route_blocker().is_none(),
            headroom > 0,
            "with no buffer, any admission at all requires headroom > 0 (headroom {headroom})"
        );
    }
    for headroom in [99i64, 100, 101] {
        let gates = InboundAdmissionGates {
            confirmed_headroom_atomic: headroom,
            admission_buffer_atomic: 100,
            ..healthy()
        };
        assert_eq!(
            gates.route_blocker().is_none(),
            headroom > 100,
            "with a buffer of 100, any admission at all requires headroom > 100 \
             (headroom {headroom})"
        );
    }
}

/// A disabled floor (`min_available_utxo_count == 0`) never blocks, no
/// matter how empty the pool is — the short-circuit every caller relies
/// on for reserves that have no vault pool at all.
#[test]
fn a_disabled_utxo_floor_never_blocks() {
    let gates = InboundAdmissionGates {
        min_available_utxo_count: 0,
        available_utxo_count: 0,
        ..healthy()
    };
    assert_eq!(gates.route_blocker(), None);
}

// ------------------------------------------- route-scoped admission --

/// The core AND, as a truth table: a route admits only when BOTH its own
/// gate and the reserve-wide gates are open, and the two never cancel.
///
/// This is the property the whole route-scoped axis exists to provide,
/// so it is pinned as data rather than as four separate tests.
#[test]
fn route_and_reserve_gates_are_anded_never_substituted() {
    let cases = [
        // (route closed, reserve paused, reserve admission closed, expected)
        (false, false, false, None),
        (
            true,
            false,
            false,
            Some(InboundAdmissionBlocker::RouteAdmissionClosed),
        ),
        (
            false,
            true,
            false,
            Some(InboundAdmissionBlocker::ReservePaused),
        ),
        (
            false,
            false,
            true,
            Some(InboundAdmissionBlocker::AdmissionClosed),
        ),
        // Both closed: the route gate is the more specific statement and
        // wins the ranking, but the point is that NEITHER opens.
        (
            true,
            true,
            false,
            Some(InboundAdmissionBlocker::RouteAdmissionClosed),
        ),
        (
            true,
            false,
            true,
            Some(InboundAdmissionBlocker::RouteAdmissionClosed),
        ),
        (
            true,
            true,
            true,
            Some(InboundAdmissionBlocker::RouteAdmissionClosed),
        ),
    ];
    for (route_closed, paused, admission_closed, expected) in cases {
        let gates = InboundAdmissionGates {
            route_admission_closed: route_closed,
            paused,
            admission_closed,
            ..healthy()
        };
        assert_eq!(
            gates.route_blocker(),
            expected,
            "route_closed={route_closed} paused={paused} admission_closed={admission_closed}"
        );
    }
}

/// Reopening the RESERVE-wide gates does not override a route whose own
/// gate an operator closed — the explicit non-override property.
///
/// Stated separately from the truth table above because it is the one an
/// operator's mental model gets wrong: "I unpaused the reserve, why is
/// the route still shut".
#[test]
fn reopening_the_reserve_does_not_open_a_closed_route() {
    let mut gates = InboundAdmissionGates {
        route_admission_closed: true,
        paused: true,
        admission_closed: true,
        ..healthy()
    };
    assert_eq!(
        gates.route_blocker(),
        Some(InboundAdmissionBlocker::RouteAdmissionClosed)
    );

    // Every reserve-wide gate reopened, one at a time. The route stays
    // shut throughout: nothing about the reserve can clear its flag.
    gates.paused = false;
    assert_eq!(
        gates.route_blocker(),
        Some(InboundAdmissionBlocker::RouteAdmissionClosed)
    );
    gates.admission_closed = false;
    assert_eq!(
        gates.route_blocker(),
        Some(InboundAdmissionBlocker::RouteAdmissionClosed),
        "a fully open reserve must not open a route an operator closed"
    );

    // ...and only clearing the route's own flag opens it.
    gates.route_admission_closed = false;
    assert_eq!(gates.route_blocker(), None);
}

/// The converse: an OPEN route gate grants nothing on its own. Opening a
/// route can never admit onto a paused or closed reserve.
#[test]
fn an_open_route_does_not_override_the_reserve_wide_stop() {
    let gates = InboundAdmissionGates {
        route_admission_closed: false,
        paused: true,
        ..healthy()
    };
    assert_eq!(
        gates.route_blocker(),
        Some(InboundAdmissionBlocker::ReservePaused),
        "reserve-wide pause must remain the emergency stop"
    );
}

/// The route gate is amount-independent, exactly like the reserve-wide
/// operator switches: it refuses the smallest representable deposit and
/// the largest alike.
#[test]
fn a_closed_route_refuses_every_amount() {
    let gates = InboundAdmissionGates {
        route_admission_closed: true,
        ..healthy()
    };
    for amount in [1i64, 1_000, 999_999] {
        assert_eq!(
            gates.blocker(amount, InboundRateLimits::default()),
            Some(InboundAdmissionBlocker::RouteAdmissionClosed),
            "amount {amount}"
        );
    }
}

/// Every blocker's operator-facing `as_str` is distinct, and distinct
/// from every `manual_review_note` — the two namespaces must never be
/// confused, because the notes are durable column values that resume and
/// refund allowlists match exactly.
#[test]
fn blocker_display_names_are_distinct_and_not_manual_review_notes() {
    let all = [
        InboundAdmissionBlocker::RouteAdmissionClosed,
        InboundAdmissionBlocker::AdmissionClosed,
        InboundAdmissionBlocker::ReservePaused,
        InboundAdmissionBlocker::SourceWalletRateLimited,
        InboundAdmissionBlocker::RecipientRateLimited,
        InboundAdmissionBlocker::UtxoLiquidityLow,
        InboundAdmissionBlocker::LiquidityBufferLow,
        InboundAdmissionBlocker::InsufficientCapacity,
    ];
    let mut names: Vec<&str> = all.iter().map(|b| b.as_str()).collect();
    names.sort_unstable();
    let count = names.len();
    names.dedup();
    assert_eq!(names.len(), count, "blocker display names must be distinct");

    // The two route/reserve admission gates in particular must not share
    // a name in either namespace: telling them apart is the entire point
    // of the axis.
    assert_ne!(
        InboundAdmissionBlocker::RouteAdmissionClosed.as_str(),
        InboundAdmissionBlocker::AdmissionClosed.as_str()
    );
    assert_ne!(
        InboundAdmissionBlocker::RouteAdmissionClosed.manual_review_note(),
        InboundAdmissionBlocker::AdmissionClosed.manual_review_note()
    );
}

// ------------------------------------------------ probing at a real size --

/// `route_blocker_at(n)` IS `blocker(n, no limits)` — the same identity
/// `route_blocker_is_the_real_decision_at_one_atomic_unit` pins for the
/// one-unit form, at every probe size.
#[test]
fn route_blocker_at_is_the_real_decision_at_that_size() {
    for headroom in [-5i64, 0, 1, 46_999, 47_000, 47_001, 297_000, 1_000_000] {
        for buffer in [0i64, 1, 250_000] {
            for probe in [1i64, 47_000, 1_000_000] {
                let gates = InboundAdmissionGates {
                    confirmed_headroom_atomic: headroom,
                    admission_buffer_atomic: buffer,
                    ..healthy()
                };
                assert_eq!(
                    gates.route_blocker_at(probe),
                    gates.blocker(probe, InboundRateLimits::default()),
                    "headroom {headroom}, buffer {buffer}, probe {probe}"
                );
            }
        }
    }
    let g = healthy();
    assert_eq!(g.route_blocker_at(1), g.route_blocker());
}

/// The 2026-09-12 production shape, in the evaluator's own units: every
/// gate open, 280,252 GLC of headroom against a 250,000 GLC buffer,
/// 47,000 GLC net per normal deposit. The one-unit probe says "open"
/// (`headroom > buffer`); a normal deposit is refused
/// (`headroom - net < buffer`). A public verdict computed from the
/// former advertised a route every deposit parked on.
#[test]
fn a_tiny_probe_can_say_open_while_a_normal_transfer_is_refused() {
    let gates = InboundAdmissionGates {
        confirmed_headroom_atomic: 280_252,
        admission_buffer_atomic: 250_000,
        ..healthy()
    };
    assert_eq!(gates.route_blocker(), None, "the weakest form is satisfied");
    assert_eq!(
        gates.route_blocker_at(47_000),
        Some(InboundAdmissionBlocker::LiquidityBufferLow),
        "a normal deposit is not"
    );
    assert_eq!(
        InboundAdmissionBlocker::LiquidityBufferLow.as_str(),
        "liquidity_buffer_low"
    );
    // One settlement's worth of headroom later (+47,000) the same probe
    // clears — the exact one-in-one-out rhythm the incident showed.
    let recovered = InboundAdmissionGates {
        confirmed_headroom_atomic: 297_000,
        ..gates
    };
    assert_eq!(recovered.route_blocker_at(47_000), None);
    assert_eq!(
        recovered.route_blocker_at(47_001),
        Some(InboundAdmissionBlocker::LiquidityBufferLow)
    );
}

/// `max_admissible_net_destination_atomic` is the exact boundary of the
/// decision it summarizes — `blocker(max)` admits and `blocker(max + 1)`
/// refuses — and collapses to `0` whenever any amount-independent gate
/// is closed or headroom is already inside the buffer.
#[test]
fn max_admissible_net_is_the_exact_boundary_of_the_decision() {
    for headroom in [
        -1i64, 0, 1, 2, 249_999, 250_000, 250_001, 280_252, 1_000_000,
    ] {
        for buffer in [0i64, 1, 250_000] {
            let gates = InboundAdmissionGates {
                confirmed_headroom_atomic: headroom,
                admission_buffer_atomic: buffer,
                ..healthy()
            };
            let max = gates.max_admissible_net_destination_atomic();
            assert!(max >= 0);
            if max > 0 {
                assert_eq!(
                    gates.route_blocker_at(max),
                    None,
                    "headroom {headroom}, buffer {buffer}: max {max} must be admitted"
                );
                assert_ne!(
                    gates.route_blocker_at(max + 1),
                    None,
                    "headroom {headroom}, buffer {buffer}: max {max} must be maximal"
                );
            } else {
                assert_ne!(
                    gates.route_blocker_at(1),
                    None,
                    "headroom {headroom}, buffer {buffer}: a zero max means nothing is admitted"
                );
            }
        }
    }
    // The incident figures: 30,252 is what fits, not 47,000.
    let incident = InboundAdmissionGates {
        confirmed_headroom_atomic: 280_252,
        admission_buffer_atomic: 250_000,
        ..healthy()
    };
    assert_eq!(incident.max_admissible_net_destination_atomic(), 30_252);

    // Every amount-independent gate zeroes it, however large the headroom.
    for closed in [
        InboundAdmissionGates {
            route_admission_closed: true,
            ..healthy()
        },
        InboundAdmissionGates {
            admission_closed: true,
            ..healthy()
        },
        InboundAdmissionGates {
            paused: true,
            ..healthy()
        },
        InboundAdmissionGates {
            liquidity_admission_closed: true,
            ..healthy()
        },
        InboundAdmissionGates {
            min_available_utxo_count: 5,
            available_utxo_count: 5,
            ..healthy()
        },
    ] {
        assert_eq!(
            closed.max_admissible_net_destination_atomic(),
            0,
            "{closed:?}"
        );
    }
}
