//! End-to-end tests for the ROUTE-SCOPED admission axis (schema v25).
//!
//! # What this axis is for
//!
//! `SolToGlc` and `RhnToGlc` both settle out of `GoldcoinReserve`, so
//! before v25 the only admission control either had was shared between
//! them: `reserve_ledger.paused` and `reserve_ledger.admission_closed`.
//! Closing either shut BOTH routes, and there was no supported way to
//! hold one open while the other was closed —
//! `Route::is_operator_settable` refuses a `bridge_routes` write for
//! `SolToGlc` outright, and the config file has no legacy-route surface.
//!
//! v25 adds `route_admission`, a per-route gate for exactly those two
//! routes, ANDed with the reserve-wide gates rather than replacing them.
//!
//! # What these tests pin, and why at this level
//!
//! `ledger::admission::tests` already pins the AND and the ranking as
//! pure functions. These go through the REAL folds and the REAL audited
//! command path instead, because the property that matters in production
//! is not "the evaluator ANDs two booleans" but "a deposit that arrives
//! while one route is closed parks, with a recoverable reason, while the
//! other route keeps settling normally".
//!
//! Every test drives state through the same audited entry point the CLI
//! uses (`admin_api::audited_set_route_admission`) — never a direct
//! SQLite write — so what is proven here is what an operator can
//! actually reach.

use glc_reserve_bridge_service::admin_api::{
    audited_set_admission, audited_set_local_pause, audited_set_route_admission,
};
use glc_reserve_bridge_service::amount_conversion::{compute_fee, CanonicalAtomic};
use glc_reserve_bridge_service::ledger::{
    CreateRequestOutcome, Direction, InboundAdmissionBlocker, Ledger, RequestAmounts, RequestState,
    ReserveDirection, RobinhoodDepositObservation, RobinhoodFinality, RobinhoodObservationRow,
    SolFoldOutcome,
};
use glc_reserve_bridge_service::robinhood::fold::FoldOutcome;
use glc_reserve_bridge_service::routes::Route;

const GLC: u64 = 100_000_000;
/// Canonical (8dp) -> Robinhood-native (18dp).
const CANONICAL_SCALE: u128 =
    glc_reserve_bridge_service::amount_conversion::robinhood::CANONICAL_TO_ROBINHOOD_SCALE;

/// A ledger with both reserves funded, every gate open, and the v25
/// migration applied — i.e. exactly what a freshly migrated production
/// ledger looks like.
fn setup() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    for direction in [
        ReserveDirection::SolanaReserve,
        ReserveDirection::GoldcoinReserve,
    ] {
        ledger
            .configure_reserve(
                direction,
                10_000 * GLC,
                100 * GLC,
                5_000 * GLC,
                500 * GLC,
                200 * GLC,
                1_000,
            )
            .unwrap();
    }
    ledger
}

/// A distinct, payout-valid Goldcoin testnet P2PKH address per `tag`, so
/// no test can trip the per-recipient rolling-24h window by accident.
fn glc_address(tag: u8) -> String {
    // Deterministic, valid testnet addresses used elsewhere in this
    // suite; the exact bytes do not matter, only that they differ.
    const ADDRESSES: [&str; 6] = [
        "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef",
        "mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn",
        "mxosQ4CvQR8ipfWdRktyB3u16tauEdamGc",
        "n1ZCYg9YXtB5XCZazLxSmPDa8iwJRZHhGx",
        "myoqcgYiehufrsnnkqdqbp69dddVUMKUnh",
        "mgnucj8nYqdrPFh2JfZSB1NmUThUGnmsqe",
    ];
    ADDRESSES[tag as usize % ADDRESSES.len()].to_string()
}

fn amounts(gross_canonical: u64) -> RequestAmounts {
    let fb = compute_fee(CanonicalAtomic(gross_canonical)).unwrap();
    RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
        quote: None,
    }
}

/// Folds one real `SolToGlc` deposit and reports the resulting state and
/// `manual_review_note`.
fn fold_sol(ledger: &mut Ledger, index: u64, tag: u8) -> (RequestState, Option<String>) {
    let outcome = ledger
        .fold_sol_deposit(
            index,
            amounts(10 * GLC),
            [tag; 32],
            glc_address(tag).as_bytes(),
            None,
            1_000,
        )
        .unwrap();
    let request_id = match outcome {
        SolFoldOutcome::FoldedFinalized { request_id }
        | SolFoldOutcome::FoldedManualReview { request_id, .. } => request_id,
        other => panic!("unexpected SolToGlc fold outcome: {other:?}"),
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    (request.state, request.manual_review_note)
}

fn observation(index: u64, canonical: u64, destination: String) -> RobinhoodObservationRow {
    let robinhood = u128::from(canonical) * CANONICAL_SCALE;
    RobinhoodObservationRow {
        id: index as i64 + 1,
        observation: RobinhoodDepositObservation {
            source_contract: [0x77; 20],
            obligation_index: index,
            route: Route::RhnToGlc,
            depositor: {
                let mut d = [0x33; 20];
                d[0] = index as u8;
                d
            },
            destination: destination.into_bytes(),
            amount_robinhood_atomic: glc_reserve_bridge_service::evm::EvmU256::from_u128(robinhood)
                .to_be_bytes(),
            amount_canonical_atomic: canonical,
            tx_hash: {
                let mut h = [0xaa; 32];
                h[0] = index as u8;
                h
            },
            log_index: 0,
            block_number: 500,
            block_hash: [0xbb; 32],
        },
        finality: RobinhoodFinality::Final,
        observed_at: 100,
        finalized_at: Some(200),
        reorged_at: None,
    }
}

/// Folds one real `RhnToGlc` deposit with its ENABLEMENT gate open, so
/// the only thing under test is admission.
fn fold_rhn(ledger: &mut Ledger, index: u64, tag: u8) -> (RequestState, Option<String>) {
    let gross = 10 * GLC;
    let fb = compute_fee(CanonicalAtomic(gross)).unwrap();
    let destination = glc_address(tag);
    let row = observation(index, gross, destination.clone());
    let outcome = ledger
        .fold_robinhood_deposit(
            &row,
            glc_reserve_bridge_service::ledger::RequestAmounts {
                gross_atomic: fb.gross.0,
                fee_bps: fb.fee_bps,
                fee_atomic: fb.fee.0,
                net_atomic: fb.net.0,
                net_destination_atomic: fb.net.0,
                quote: None,
            },
            Some(destination.as_bytes()),
            true, // route ENABLEMENT open — a different axis
            None,
            1_000,
        )
        .unwrap();
    let request_id = match outcome {
        FoldOutcome::FoldedFinalized { request_id }
        | FoldOutcome::FoldedManualReview { request_id } => request_id,
        other => panic!("unexpected RhnToGlc fold outcome: {other:?}"),
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    (request.state, request.manual_review_note)
}

fn close_route(ledger: &mut Ledger, route: Route) {
    audited_set_route_admission(ledger, route, true, "test closure", "cli:test").unwrap();
}

fn open_route(ledger: &mut Ledger, route: Route) {
    audited_set_route_admission(ledger, route, false, "test reopen", "cli:test").unwrap();
}

// ---------------------------------------------------------------------
// The four independence scenarios
// ---------------------------------------------------------------------

/// SolToGlc closed, RhnToGlc open — through the real folds.
///
/// The whole point of the axis: one inbound route parks while the other
/// keeps settling, out of the SAME reserve, with no reserve-wide flag
/// touched.
#[test]
fn sol_to_glc_closed_while_rhn_to_glc_stays_open() {
    let mut ledger = setup();
    close_route(&mut ledger, Route::SolToGlc);

    let (sol_state, sol_note) = fold_sol(&mut ledger, 0, 1);
    assert_eq!(sol_state, RequestState::ManualReview);
    assert_eq!(sol_note.as_deref(), Some("route_admission_closed_at_fold"));

    let (rhn_state, rhn_note) = fold_rhn(&mut ledger, 0, 2);
    assert_eq!(
        rhn_state,
        RequestState::SourceFinalized,
        "closing SolToGlc must not close RhnToGlc — they share a reserve, not a gate"
    );
    assert_eq!(rhn_note, None);

    // And the reserve itself was never touched.
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
    assert!(!ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

/// RhnToGlc closed, SolToGlc open — the mirror image.
#[test]
fn rhn_to_glc_closed_while_sol_to_glc_stays_open() {
    let mut ledger = setup();
    close_route(&mut ledger, Route::RhnToGlc);

    let (rhn_state, rhn_note) = fold_rhn(&mut ledger, 0, 1);
    assert_eq!(rhn_state, RequestState::ManualReview);
    assert_eq!(rhn_note.as_deref(), Some("route_admission_closed_at_fold"));

    let (sol_state, sol_note) = fold_sol(&mut ledger, 0, 2);
    assert_eq!(
        sol_state,
        RequestState::SourceFinalized,
        "closing RhnToGlc must not close SolToGlc"
    );
    assert_eq!(sol_note, None);
}

/// The reserve-wide pause remains the emergency stop: it closes BOTH
/// routes regardless of their own gates.
#[test]
fn reserve_wide_pause_blocks_both_inbound_routes() {
    let mut ledger = setup();
    // Both route gates explicitly OPEN, so nothing here can be mistaken
    // for a route-level closure.
    open_route(&mut ledger, Route::SolToGlc);
    open_route(&mut ledger, Route::RhnToGlc);

    audited_set_local_pause(
        &mut ledger,
        ReserveDirection::GoldcoinReserve,
        true,
        "reserve-wide emergency stop",
        "cli:test",
    )
    .unwrap();

    for direction in [Direction::SolToGlc, Direction::RhnToGlc] {
        assert_eq!(
            ledger.route_admission_blocker(direction).unwrap(),
            Some(InboundAdmissionBlocker::ReservePaused),
            "{direction:?} must be blocked by the reserve-wide pause"
        );
    }

    let (sol_state, sol_note) = fold_sol(&mut ledger, 0, 1);
    assert_eq!(sol_state, RequestState::ManualReview);
    assert_eq!(sol_note.as_deref(), Some("reserve_paused_at_fold"));

    let (rhn_state, rhn_note) = fold_rhn(&mut ledger, 0, 2);
    assert_eq!(rhn_state, RequestState::ManualReview);
    assert_eq!(rhn_note.as_deref(), Some("reserve_paused_at_fold"));
}

/// Reopening the reserve-wide gates does NOT override a route-specific
/// closed gate.
///
/// This is the operator-facing property most likely to be assumed the
/// other way round ("I unpaused, why is it still shut"), and the one a
/// future refactor is most likely to break by treating the reserve flag
/// as authoritative.
#[test]
fn reopening_reserve_wide_pause_does_not_override_a_closed_route() {
    let mut ledger = setup();

    close_route(&mut ledger, Route::RhnToGlc);
    audited_set_local_pause(
        &mut ledger,
        ReserveDirection::GoldcoinReserve,
        true,
        "maintenance",
        "cli:test",
    )
    .unwrap();
    audited_set_admission(
        &mut ledger,
        ReserveDirection::GoldcoinReserve,
        true,
        "maintenance",
        "cli:test",
    )
    .unwrap();

    // Reserve-wide gates fully reopened, one at a time.
    audited_set_local_pause(
        &mut ledger,
        ReserveDirection::GoldcoinReserve,
        false,
        "maintenance over",
        "cli:test",
    )
    .unwrap();
    assert_eq!(
        ledger.route_admission_blocker(Direction::RhnToGlc).unwrap(),
        Some(InboundAdmissionBlocker::RouteAdmissionClosed)
    );

    audited_set_admission(
        &mut ledger,
        ReserveDirection::GoldcoinReserve,
        false,
        "maintenance over",
        "cli:test",
    )
    .unwrap();
    assert_eq!(
        ledger.route_admission_blocker(Direction::RhnToGlc).unwrap(),
        Some(InboundAdmissionBlocker::RouteAdmissionClosed),
        "a fully reopened reserve must not open a route an operator closed"
    );

    // A real deposit still parks, with the ROUTE reason — not the
    // reserve one.
    let (state, note) = fold_rhn(&mut ledger, 0, 1);
    assert_eq!(state, RequestState::ManualReview);
    assert_eq!(note.as_deref(), Some("route_admission_closed_at_fold"));

    // The sibling route, never closed, was admitted throughout.
    assert_eq!(
        ledger.route_admission_blocker(Direction::SolToGlc).unwrap(),
        None
    );

    // Only the route's own command reopens it.
    open_route(&mut ledger, Route::RhnToGlc);
    assert_eq!(
        ledger.route_admission_blocker(Direction::RhnToGlc).unwrap(),
        None
    );
    let (state, note) = fold_rhn(&mut ledger, 1, 2);
    assert_eq!(state, RequestState::SourceFinalized);
    assert_eq!(note, None);
}

// ---------------------------------------------------------------------
// Migration and scope
// ---------------------------------------------------------------------

/// A freshly migrated ledger admits exactly what it admitted before v25
/// (and v27, and v38): every seeded row open, no gate closed by a
/// migration. Since v38 that is one row per route — all six.
#[test]
fn migration_preserves_existing_behaviour_every_route_open() {
    let ledger = setup();
    let state = ledger.route_admission_rows().unwrap().unwrap();

    assert_eq!(state.rows.len(), Route::ALL.len());
    assert_eq!(
        state.rows.iter().map(|r| r.route).collect::<Vec<_>>(),
        Route::ALL.to_vec(),
        "one row per route, in registry order"
    );
    assert!(state.unknown_route_ids.is_empty());
    for row in &state.rows {
        assert!(
            !row.admission_closed,
            "{} must be seeded OPEN — the migration must change no behaviour",
            row.route.as_str()
        );
        assert!(row.admission_closed_reason.is_none());
    }

    // ...and both Goldcoin-bound routes actually admit, exactly as before,
    // as does the Goldcoin-sourced route that draws on the configured
    // Solana reserve.
    for direction in [
        Direction::SolToGlc,
        Direction::RhnToGlc,
        Direction::GlcToSol,
    ] {
        assert_eq!(ledger.route_admission_blocker(direction).unwrap(), None);
    }
    // The Solana-bound cross route admits against the configured Solana
    // reserve; the Robinhood-bound one has no reserve row here and so
    // admits nothing — the fail-closed answer, never "open".
    assert_eq!(
        ledger.route_admission_blocker(Direction::RhnToSol).unwrap(),
        None
    );
    assert!(ledger.route_admission_blocker(Direction::SolToRhn).is_err());
}

/// An operator's closure survives a reopen of the ledger — the flag is
/// durable state, not process state.
#[test]
fn a_closed_route_survives_reopening_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");

    {
        let mut ledger = Ledger::open(&path).unwrap();
        for direction in [
            ReserveDirection::SolanaReserve,
            ReserveDirection::GoldcoinReserve,
        ] {
            ledger
                .configure_reserve(
                    direction,
                    10_000 * GLC,
                    100 * GLC,
                    5_000 * GLC,
                    500 * GLC,
                    200 * GLC,
                    1_000,
                )
                .unwrap();
        }
        close_route(&mut ledger, Route::SolToGlc);
    }

    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(
        ledger.route_admission_blocker(Direction::SolToGlc).unwrap(),
        Some(InboundAdmissionBlocker::RouteAdmissionClosed)
    );
    assert_eq!(
        ledger.route_admission_blocker(Direction::RhnToGlc).unwrap(),
        None
    );
    // The reason an operator recorded is durable too.
    let row = ledger
        .route_admission_rows()
        .unwrap()
        .unwrap()
        .row(Route::SolToGlc)
        .cloned()
        .unwrap();
    assert_eq!(row.admission_closed_reason.as_deref(), Some("test closure"));
}

/// Since v38 the two Goldcoin-SOURCED routes carry a route-level gate
/// too, and it is a REAL gate, not merely a writable row: closing
/// `GlcToSol` through the AUDITED entry point refuses the next
/// `POST /transfers`-style request on that route before any row or
/// reservation exists, leaves `GlcToRhn` (same Goldcoin source) and
/// `RhnToSol` (same Solana destination reserve) admitting, writes one
/// audit row, and re-opening restores admission. A route an operator can
/// close but no admission path consults would be the worst of both
/// worlds; this pins that neither half drifts.
#[test]
fn the_audited_path_closes_a_goldcoin_sourced_route_and_the_request_path_honours_it() {
    let mut ledger = setup();
    let create = |ledger: &mut Ledger, tag: u8| {
        ledger
            .create_request(
                Direction::GlcToSol,
                amounts(10 * GLC),
                &[tag; 32],
                None,
                3600,
                1_000,
            )
            .unwrap()
    };
    assert!(matches!(
        create(&mut ledger, 1),
        CreateRequestOutcome::Reserved { .. }
    ));
    let capacity_before = ledger
        .available_capacity(ReserveDirection::SolanaReserve)
        .unwrap();

    for route in [Route::GlcToSol, Route::GlcToRhn] {
        let receipt = audited_set_route_admission(&mut ledger, route, true, "nope", "cli:test")
            .expect("every route carries a route-level admission gate since v38");
        assert_eq!(receipt.action, "route_admission_close");
        assert_eq!(receipt.target, route.as_str());
        assert_eq!(receipt.old_value.as_deref(), Some("admission_closed=false"));
        assert_eq!(receipt.new_value.as_deref(), Some("admission_closed=true"));
    }

    // Both rows recorded, every other route untouched.
    let state = ledger.route_admission_rows().unwrap().unwrap();
    assert_eq!(state.rows.len(), Route::ALL.len());
    for row in &state.rows {
        assert_eq!(
            row.admission_closed,
            matches!(row.route, Route::GlcToSol | Route::GlcToRhn),
            "{}",
            row.route.as_str()
        );
    }

    // The gate is consulted where a Goldcoin-sourced route is admitted:
    // the request is refused, nothing is reserved.
    assert_eq!(
        create(&mut ledger, 2),
        CreateRequestOutcome::RouteAdmissionClosed
    );
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        capacity_before
    );
    assert_eq!(
        ledger.route_admission_blocker(Direction::GlcToSol).unwrap(),
        Some(InboundAdmissionBlocker::RouteAdmissionClosed)
    );
    // The sibling on the same destination reserve still admits.
    assert_eq!(
        ledger.route_admission_blocker(Direction::RhnToSol).unwrap(),
        None
    );

    // Re-opening `GlcToSol` alone is the whole remedy for that route;
    // `GlcToRhn` stays closed.
    open_route(&mut ledger, Route::GlcToSol);
    assert!(matches!(
        create(&mut ledger, 3),
        CreateRequestOutcome::Reserved { .. }
    ));
    assert!(ledger.route_admission_closed(Route::GlcToRhn).unwrap());
}

/// Each Solana<->Robinhood route carries its OWN admission gate (v27):
/// closing one parks that route's new deposits and leaves the other
/// cross route, and both Goldcoin-bound routes, exactly as they were.
#[test]
fn a_cross_route_can_be_closed_alone() {
    let mut ledger = setup();
    for route in [Route::SolToRhn, Route::RhnToSol] {
        assert!(route.as_direction().is_some(), "{}", route.as_str());
        assert!(route.is_admission_settable(), "{}", route.as_str());
        assert!(route.is_operator_settable(), "{}", route.as_str());
        assert!(ledger
            .route_admission_rows()
            .unwrap()
            .unwrap()
            .row(route)
            .is_some());
    }

    audited_set_route_admission(&mut ledger, Route::RhnToSol, true, "incident", "cli:test")
        .expect("a cross route's admission is operator-settable");
    assert_eq!(
        ledger.route_admission_blocker(Direction::RhnToSol).unwrap(),
        Some(InboundAdmissionBlocker::RouteAdmissionClosed)
    );
    // The reserve it draws on (Solana) still admits the OTHER route that
    // draws on it, and the Goldcoin-bound routes are untouched.
    for direction in [Direction::SolToGlc, Direction::RhnToGlc] {
        assert_eq!(ledger.route_admission_blocker(direction).unwrap(), None);
    }
    let sol_to_rhn = ledger
        .route_admission_rows()
        .unwrap()
        .unwrap()
        .row(Route::SolToRhn)
        .cloned()
        .unwrap();
    assert!(
        !sol_to_rhn.admission_closed,
        "closing RhnToSol must not touch SolToRhn"
    );

    audited_set_route_admission(&mut ledger, Route::RhnToSol, false, "resolved", "cli:test")
        .unwrap();
    assert_eq!(
        ledger.route_admission_blocker(Direction::RhnToSol).unwrap(),
        None
    );
}

// ---------------------------------------------------------------------
// The open path is guarded exactly like reserve-wide open-admission
// ---------------------------------------------------------------------

/// `route-admission-open` refuses while the destination reserve's hard
/// invariant is broken — the same unconditional refusal `open-admission`
/// gives, so a route-scoped command cannot be used to route around it.
#[test]
fn opening_a_route_refuses_while_the_reserve_invariant_is_broken() {
    let mut ledger = setup();
    close_route(&mut ledger, Route::SolToGlc);

    // Break the invariant: protected_minimum above the balance.
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            10_000 * GLC,
            50_000 * GLC,
            60_000 * GLC,
            55_000 * GLC,
            51_000 * GLC,
            1_000,
        )
        .unwrap();
    assert!(ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .is_err());

    let err = audited_set_route_admission(
        &mut ledger,
        Route::SolToGlc,
        false,
        "reopen please",
        "cli:test",
    )
    .expect_err("must refuse to open a route onto a broken reserve");
    assert!(
        err.to_string().contains("invariant"),
        "the refusal must name the invariant, got: {err}"
    );

    // The refusal changed nothing: the route is still closed.
    assert_eq!(
        ledger.route_admission_blocker(Direction::SolToGlc).unwrap(),
        Some(InboundAdmissionBlocker::RouteAdmissionClosed)
    );
}

/// Closing is always allowed, even on a reserve too broken to reopen —
/// refusing to stop taking deposits is not a safety property.
#[test]
fn closing_a_route_is_always_allowed() {
    let mut ledger = setup();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            10_000 * GLC,
            50_000 * GLC,
            60_000 * GLC,
            55_000 * GLC,
            51_000 * GLC,
            1_000,
        )
        .unwrap();

    audited_set_route_admission(&mut ledger, Route::RhnToGlc, true, "stop it", "cli:test").unwrap();
    assert_eq!(
        ledger.route_admission_blocker(Direction::RhnToGlc).unwrap(),
        Some(InboundAdmissionBlocker::RouteAdmissionClosed)
    );
}

// ---------------------------------------------------------------------
// Parked deposits keep their exits
// ---------------------------------------------------------------------

/// A deposit parked by the route gate is recoverable and refundable on
/// the same terms as any other fold-time park.
///
/// Without this, closing a route would create the one park with no exit
/// — strictly worse than the others, because the parking was an
/// operator's own deliberate act and may persist indefinitely.
#[test]
fn a_route_parked_deposit_keeps_both_exits() {
    assert!(Ledger::RECOVERABLE_MANUAL_REVIEW_REASONS.contains(&"route_admission_closed_at_fold"));
    assert!(Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS.contains(&"route_admission_closed_at_fold"));
    assert!(Ledger::is_recoverable_manual_review_reason(Some(
        "route_admission_closed_at_fold"
    )));

    // And the real resume path agrees, on a real parked request.
    let mut ledger = setup();
    close_route(&mut ledger, Route::SolToGlc);
    let (state, note) = fold_sol(&mut ledger, 0, 1);
    assert_eq!(state, RequestState::ManualReview);
    assert_eq!(note.as_deref(), Some("route_admission_closed_at_fold"));

    // Resume works even while the route stays CLOSED: like reserve-wide
    // admission, resuming never admits anything new — it only unblocks
    // something already accepted.
    let request_id = ledger
        .requests_by_state(Direction::SolToGlc, RequestState::ManualReview)
        .unwrap()[0]
        .id;
    ledger
        .resume_manual_review_sol_to_glc(request_id, "recovered", "operator", 5_000)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );
    assert_eq!(
        ledger.route_admission_blocker(Direction::SolToGlc).unwrap(),
        Some(InboundAdmissionBlocker::RouteAdmissionClosed),
        "resuming a parked request must not reopen the route"
    );
}
