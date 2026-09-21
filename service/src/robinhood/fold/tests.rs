//! Fold tests: turning a FINALIZED Robinhood observation into exactly one
//! bridge request.

use super::*;
use crate::amount_conversion::BRIDGE_FEE_BPS;
use crate::ledger::{
    Direction, RequestState, RobinhoodDepositObservation, RobinhoodFinality,
    RobinhoodObservationRow, WalletRole,
};
use crate::robinhood::testkit::BRIDGE;

const CANONICAL_SCALE: u128 = 10_000_000_000;

fn ledger() -> Ledger {
    let mut ledger = Ledger::open_in_memory().expect("an in-memory ledger");
    ledger
        .configure_reserve(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            1_000_000_000_000,
            0,
            1_000_000_000_000,
            500_000_000_000,
            250_000_000_000,
            100,
        )
        .expect("configures the Goldcoin reserve");
    ledger
}

/// A Goldcoin testnet P2PKH address the payout builder would accept.
pub(super) fn destination() -> String {
    crate::goldcoin::address::encode_p2pkh(&[0x42; 20], crate::goldcoin::address::Network::Testnet)
}

/// The Phase 2A book: a fixed unit rate at the default quote lifetime.
pub(super) fn unit_book() -> crate::bridge_rate::RateBook {
    crate::bridge_rate::RateBook::fixed_unit(crate::bridge_rate::DEFAULT_QUOTE_LIFETIME_SECS)
}

pub(super) fn observation(
    index: u64,
    canonical: u64,
    destination_bytes: Vec<u8>,
) -> RobinhoodObservationRow {
    let robinhood = u128::from(canonical) * CANONICAL_SCALE;
    RobinhoodObservationRow {
        id: index as i64 + 1,
        observation: RobinhoodDepositObservation {
            source_contract: BRIDGE.to_bytes(),
            obligation_index: index,
            route: Route::RhnToGlc,
            depositor: [0x33; 20],
            destination: destination_bytes,
            amount_robinhood_atomic: crate::evm::EvmU256::from_u128(robinhood).to_be_bytes(),
            amount_canonical_atomic: canonical,
            // Distinct per obligation, because `ux_robinhood_log_identity`
            // guards `(tx_hash, log_index)` independently of the durable
            // identity — one log can never claim two obligation indexes.
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

/// Inserts the observation so the fold has a row to link back to.
pub(super) fn store(ledger: &Ledger, row: &RobinhoodObservationRow) {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_deposit_observations
                (id, source_chain, source_contract, source_obligation_index, contract_route_id,
                 route, depositor, destination, amount_robinhood_atomic,
                 amount_canonical_atomic, tx_hash, log_index, block_number, block_hash,
                 finality, observed_at, finalized_at)
             VALUES (?1, 'robinhood', ?2, ?3, 2, 'RhnToGlc', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                     'Final', 100, 200)",
            rusqlite::params![
                row.id,
                &row.observation.source_contract[..],
                row.observation.obligation_index as i64,
                &row.observation.depositor[..],
                row.observation.destination,
                &row.observation.amount_robinhood_atomic[..],
                row.observation.amount_canonical_atomic as i64,
                &row.observation.tx_hash[..],
                row.observation.log_index as i64,
                row.observation.block_number as i64,
                &row.observation.block_hash[..],
            ],
        )
        .expect("stores the observation");
}

pub(super) fn network() -> crate::goldcoin::address::Network {
    crate::goldcoin::address::Network::Testnet
}

#[test]
fn a_finalized_deposit_folds_exactly_once() {
    let mut ledger = ledger();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);

    let first = fold_observation(
        &mut ledger,
        &row,
        network(),
        BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        true,
        300,
    )
    .unwrap();
    let request_id = match first {
        FoldOutcome::FoldedFinalized { request_id } => request_id,
        other => panic!("expected a payable fold, got {other:?}"),
    };

    // Every subsequent attempt resumes rather than duplicating — the
    // durable identity guard, not a prior read.
    for _ in 0..3 {
        assert_eq!(
            fold_observation(
                &mut ledger,
                &row,
                network(),
                BRIDGE_FEE_BPS,
                crate::amount_conversion::CanonicalAtomic(1),
                true,
                400
            )
            .unwrap(),
            FoldOutcome::AlreadyFolded { request_id }
        );
    }
    let count: i64 = ledger
        .conn_for_tests()
        .query_row("SELECT COUNT(*) FROM bridge_requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.direction, Direction::RhnToGlc);
    assert_eq!(request.state, RequestState::SourceFinalized);
    assert_eq!(request.source_obligation_index, Some(0));
    assert_eq!(
        request.source_contract.as_deref(),
        Some(&BRIDGE.to_bytes()[..])
    );
    // The recipient is the destination address bytes, exactly as
    // `SolToGlc` stores them.
    assert_eq!(request.recipient, destination().into_bytes());
    // The normal fee policy in canonical units.
    assert_eq!(request.fee_bps, crate::amount_conversion::BRIDGE_FEE_BPS);
    assert_eq!(
        request.gross_amount_atomic,
        request.fee_amount_atomic + request.net_amount_atomic
    );
    assert_eq!(request.net_destination_atomic, request.net_amount_atomic);
}

#[test]
fn a_deposit_on_a_closed_route_is_recorded_and_parked_rather_than_dropped() {
    // The most important decision in this module: a deposit that already
    // landed cannot be un-landed by a flag on this side. Declining to
    // record it would leave real money in the contract with no ledger
    // row, no accounting and no refund path.
    let mut ledger = ledger();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);

    let outcome = fold_observation(
        &mut ledger,
        &row,
        network(),
        BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        false,
        300,
    )
    .unwrap();
    let request_id = match outcome {
        FoldOutcome::FoldedManualReview { request_id } => request_id,
        other => panic!("expected a parked fold, got {other:?}"),
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::ManualReview);
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some("route_disabled_at_fold")
    );
    // Nothing is reserved for a parked request.
    let (_, _, reserved, pending) = ledger
        .reserve_snapshot(crate::ledger::ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!((reserved, pending), (0, 0));
}

#[test]
fn an_undeliverable_destination_is_folded_and_parked_with_an_explicit_reason() {
    // The deposit is real and irreversible; it must be refunded on
    // Robinhood rather than paid out to a guess.
    let mut ledger = ledger();
    let row = observation(0, 1_000_000_000, b"not a goldcoin address".to_vec());
    store(&ledger, &row);

    let outcome = fold_observation(
        &mut ledger,
        &row,
        network(),
        BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        true,
        300,
    )
    .unwrap();
    let request_id = match outcome {
        FoldOutcome::FoldedManualReview { request_id } => request_id,
        other => panic!("expected a parked fold, got {other:?}"),
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert!(request
        .manual_review_note
        .unwrap()
        .contains("undeliverable destination"));
    // The RAW payload is kept: it is the evidence a refund decision rests
    // on, including when it is unusable.
    assert_eq!(request.recipient, b"not a goldcoin address".to_vec());
}

#[test]
fn a_mainnet_address_is_undeliverable_on_a_testnet_deployment() {
    let mut ledger = ledger();
    let mainnet = crate::goldcoin::address::encode_p2pkh(
        &[0x42; 20],
        crate::goldcoin::address::Network::Mainnet,
    );
    let row = observation(0, 1_000_000_000, mainnet.into_bytes());
    store(&ledger, &row);
    assert!(matches!(
        fold_observation(
            &mut ledger,
            &row,
            network(),
            BRIDGE_FEE_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            true,
            300
        )
        .unwrap(),
        FoldOutcome::FoldedManualReview { .. }
    ));
}

#[test]
fn a_provisional_observation_is_refused() {
    // A provisional deposit can still be reorged away, and folding one
    // would create an obligation against a deposit that never happened.
    let mut ledger = ledger();
    let mut row = observation(0, 1_000_000_000, destination().into_bytes());
    row.finality = RobinhoodFinality::Provisional;
    assert!(matches!(
        fold_observation(
            &mut ledger,
            &row,
            network(),
            BRIDGE_FEE_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            true,
            300
        ),
        Err(FoldError::NotFinal { .. })
    ));
}

#[test]
fn a_non_executable_route_is_refused() {
    let mut ledger = ledger();
    let mut row = observation(0, 1_000_000_000, destination().into_bytes());
    row.observation.route = Route::RhnToSol;
    assert!(matches!(
        fold_observation(
            &mut ledger,
            &row,
            network(),
            BRIDGE_FEE_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            true,
            300
        ),
        Err(FoldError::UnsupportedRoute { .. })
    ));
}

// -------------------------------------------------------- amounts ----

#[test]
fn an_amount_that_is_not_an_exact_multiple_of_the_scale_is_refused() {
    // The contract refuses a non-canonical deposit on-chain, so observing
    // one means this service and the contract disagree about what was
    // deposited — not a rounding decision to make.
    let mut row = observation(0, 1_000_000_000, destination().into_bytes());
    let inexact = u128::from(1_000_000_000u64) * CANONICAL_SCALE + 1;
    row.observation.amount_robinhood_atomic = crate::evm::EvmU256::from_u128(inexact).to_be_bytes();
    assert!(matches!(
        resolve_amounts(&row, BRIDGE_FEE_BPS, &unit_book(), 0, 1),
        Err(FoldError::NotCanonical { .. })
    ));
}

#[test]
fn the_two_recorded_amounts_must_agree() {
    // The event carries both, and the decoder already cross-checked them.
    // Deriving again HERE is what makes a single stored number
    // trustworthy at the moment it becomes an entitlement.
    let mut row = observation(0, 1_000_000_000, destination().into_bytes());
    row.observation.amount_canonical_atomic = 999_999_999;
    assert!(matches!(
        resolve_amounts(&row, BRIDGE_FEE_BPS, &unit_book(), 0, 1),
        Err(FoldError::AmountDisagreement {
            recorded: 999_999_999,
            derived: 1_000_000_000,
            ..
        })
    ));
}

#[test]
fn a_word_too_large_for_the_amount_model_is_refused_rather_than_truncated() {
    let mut row = observation(0, 1_000_000_000, destination().into_bytes());
    row.observation.amount_robinhood_atomic = crate::evm::EvmU256::MAX.to_be_bytes();
    assert!(matches!(
        resolve_amounts(&row, BRIDGE_FEE_BPS, &unit_book(), 0, 1),
        Err(FoldError::NotCanonical { .. })
    ));
}

#[test]
fn the_conversion_is_exact_across_a_range_of_real_amounts() {
    for whole in [1u64, 100, 20_000] {
        let canonical = whole * 100_000_000;
        let row = observation(0, canonical, destination().into_bytes());
        let amounts =
            resolve_amounts(&row, BRIDGE_FEE_BPS, &unit_book(), 0, 1).expect("an exact amount");
        assert_eq!(amounts.gross_canonical, canonical);
        assert_eq!(
            amounts.gross_canonical,
            amounts.fee_canonical + amounts.net_canonical,
            "gross == fee + net is structural"
        );
        assert_eq!(amounts.fee_bps, crate::amount_conversion::BRIDGE_FEE_BPS);
        // And the net widens back to Robinhood units exactly.
        crate::amount_conversion::CanonicalAtomic(amounts.net_canonical)
            .to_robinhood()
            .expect("the net must be exactly representable at 18dp");
    }
}

#[test]
fn folding_links_the_observation_to_its_request_and_only_one_can_claim_it() {
    let mut ledger = ledger();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);
    let request_id = fold_observation(
        &mut ledger,
        &row,
        network(),
        BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        true,
        300,
    )
    .unwrap()
    .request_id();

    let linked = ledger
        .robinhood_observation_for_request(request_id)
        .unwrap()
        .expect("the observation links back to its request");
    assert_eq!(linked.observation.obligation_index, 0);

    // A second observation cannot claim the same request.
    let other = observation(1, 1_000_000_000, destination().into_bytes());
    store(&ledger, &other);
    let forced = ledger.conn_for_tests().execute(
        "UPDATE robinhood_deposit_observations SET folded_request_id = ?1 WHERE id = ?2",
        rusqlite::params![request_id, other.id],
    );
    assert!(
        forced.is_err(),
        "two observations must not be able to claim one request"
    );
}

#[test]
fn only_unfolded_final_observations_are_offered_to_the_fold_phase() {
    let mut ledger = ledger();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);
    assert_eq!(
        ledger
            .unfolded_final_robinhood_observations()
            .unwrap()
            .len(),
        1
    );
    fold_observation(
        &mut ledger,
        &row,
        network(),
        BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        true,
        300,
    )
    .unwrap();
    assert_eq!(
        ledger
            .unfolded_final_robinhood_observations()
            .unwrap()
            .len(),
        0,
        "a folded observation is not offered again"
    );
}

#[test]
fn a_thin_reserve_parks_the_deposit_instead_of_refusing_it() {
    // Same posture as `fold_sol_deposit`: the deposit is recorded and
    // made visible rather than dropped, and pays out nothing.
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            1,
            0,
            10,
            5,
            2,
            100,
        )
        .unwrap();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);
    let outcome = fold_observation(
        &mut ledger,
        &row,
        network(),
        BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        true,
        300,
    )
    .unwrap();
    match outcome {
        FoldOutcome::FoldedManualReview { request_id } => {
            let request = ledger.get_request(request_id).unwrap().unwrap();
            assert_eq!(
                request.manual_review_note.as_deref(),
                Some("insufficient_capacity_at_fold")
            );
        }
        other => panic!("expected a parked fold, got {other:?}"),
    }
}

#[test]
fn a_paused_goldcoin_reserve_parks_the_deposit() {
    let mut ledger = ledger();
    ledger
        .set_paused(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            true,
            Some("incident"),
        )
        .unwrap();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);
    match fold_observation(
        &mut ledger,
        &row,
        network(),
        BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        true,
        300,
    )
    .unwrap()
    {
        FoldOutcome::FoldedManualReview { request_id } => {
            assert_eq!(
                ledger
                    .get_request(request_id)
                    .unwrap()
                    .unwrap()
                    .manual_review_note
                    .as_deref(),
                Some("reserve_paused_at_fold")
            );
        }
        other => panic!("expected a parked fold, got {other:?}"),
    }
}

/// The Robinhood launch rate is applied to a Robinhood deposit, and the
/// snapshot stored on the request is that rate — not the compiled-in one.
#[test]
fn a_robinhood_deposit_prices_at_the_rate_it_is_given() {
    const ROBINHOOD_FEE_BPS: u64 = 600;
    // 100 GLC in canonical 8-decimal units.
    let row = observation(1, 10_000_000_000, destination().into_bytes());
    let at_robinhood =
        resolve_amounts(&row, ROBINHOOD_FEE_BPS, &unit_book(), 0, 1).expect("an exact amount");
    let at_global =
        resolve_amounts(&row, BRIDGE_FEE_BPS, &unit_book(), 0, 1).expect("an exact amount");

    assert_eq!(at_robinhood.fee_bps, ROBINHOOD_FEE_BPS);
    assert_eq!(at_global.fee_bps, BRIDGE_FEE_BPS);
    assert_eq!(at_robinhood.gross_canonical, at_global.gross_canonical);

    // 6% of the gross, floored, and net derived by subtraction.
    assert_eq!(
        at_robinhood.fee_canonical,
        at_robinhood.gross_canonical * ROBINHOOD_FEE_BPS / 10_000
    );
    assert_eq!(
        at_robinhood.net_canonical,
        at_robinhood.gross_canonical - at_robinhood.fee_canonical
    );

    // And it is genuinely a different, larger fee than the global rate —
    // the whole point of a per-chain policy.
    const _: () = assert!(BRIDGE_FEE_BPS < ROBINHOOD_FEE_BPS);
    assert!(at_robinhood.fee_canonical > at_global.fee_canonical);
}

/// An out-of-range rate fails closed at fold time rather than creating a
/// request that could never settle.
///
/// "Out of range" now means exactly that — above 100%, where the net
/// entitlement would be negative. It used to also mean "a rate no release
/// has shipped", and 450 bps was the fixture; 450 folds perfectly well
/// now, which is the point, so it is asserted here alongside the refusal.
#[test]
fn an_out_of_range_rate_refuses_to_fold_and_an_in_range_one_does_not() {
    let row = observation(1, 10_000_000_000, destination().into_bytes());

    for bps in [10_001u64, 20_000, u64::MAX] {
        assert!(
            matches!(
                resolve_amounts(&row, bps, &unit_book(), 0, 1),
                Err(FoldError::Fee { .. })
            ),
            "{bps} bps must refuse to fold"
        );
    }

    // Rates that are merely NEW are ordinary.
    for bps in [0u64, 137, 400, 450, 9_999] {
        let amounts = resolve_amounts(&row, bps, &unit_book(), 0, 1)
            .unwrap_or_else(|e| panic!("{bps} bps must fold: {e:?}"));
        assert_eq!(amounts.fee_bps, bps);
    }
}

// ---------------------------------------------------------------------------
// Anti-abuse rate limits on RhnToGlc
//
// The rule these pin, in full:
//
//   * ONE Goldcoin L1 destination address may receive at most one bridge
//     payout per rolling 86_400 seconds — GLOBALLY, across every
//     inbound-to-Goldcoin route. A recent `SolToGlc` payout to address X
//     blocks `RhnToGlc` to X, and vice versa.
//   * ONE source wallet may make at most one qualifying deposit per rolling
//     86_400 seconds ON ITS OWN SOURCE NETWORK. A Solana pubkey's window and
//     an EVM address's window are independent and never pooled.
//
// Both are enforced by the SAME ledger functions `fold_sol_deposit` uses, so
// these tests are as much a pin on "Robinhood did not get its own policy" as
// on the policy itself.
// ---------------------------------------------------------------------------

use crate::ledger::{LedgerError, ResumeManualReviewOutcome, SolFoldOutcome};

const WINDOW: i64 = 86_400;
const T0: i64 = 1_000_000;

/// `ledger()` under a name that cannot be shadowed by a local `ledger`
/// binding — several tests below deliberately build a SECOND, fresh
/// ledger partway through.
fn fresh_ledger() -> Ledger {
    ledger()
}

/// `observation`, with the depositor as an explicit input — the source
/// wallet is what half of these tests vary.
fn observation_from(
    index: u64,
    canonical: u64,
    destination_bytes: Vec<u8>,
    depositor: [u8; 20],
) -> RobinhoodObservationRow {
    let mut row = observation(index, canonical, destination_bytes);
    row.observation.depositor = depositor;
    row
}

/// A distinct, payout-valid Goldcoin testnet P2PKH address per `tag`.
fn glc_address(tag: u8) -> String {
    crate::goldcoin::address::encode_p2pkh(&[tag; 20], crate::goldcoin::address::Network::Testnet)
}

const DEPOSIT: u64 = 1_000_000_000;

/// Folds one Robinhood deposit end to end through the real fold path.
fn fold_rhn(
    ledger: &mut Ledger,
    index: u64,
    address: &str,
    depositor: [u8; 20],
    now: i64,
) -> FoldOutcome {
    let row = observation_from(index, DEPOSIT, address.as_bytes().to_vec(), depositor);
    store(ledger, &row);
    fold_observation(
        ledger,
        &row,
        network(),
        BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        true,
        now,
    )
    .unwrap()
}

/// Folds one Solana deposit into the same ledger, so the cross-route
/// destination rule can be exercised against real rows on both routes.
fn fold_sol(
    ledger: &mut Ledger,
    index: u64,
    address: &str,
    requester: [u8; 32],
    now: i64,
) -> SolFoldOutcome {
    ledger
        .fold_sol_deposit(
            index,
            crate::ledger::RequestAmounts {
                gross_atomic: DEPOSIT,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: DEPOSIT,
                net_destination_atomic: DEPOSIT,
                quote: None,
            },
            requester,
            address.as_bytes(),
            None,
            now,
        )
        .unwrap()
}

fn note_of(ledger: &Ledger, request_id: i64) -> Option<String> {
    ledger
        .get_request(request_id)
        .unwrap()
        .unwrap()
        .manual_review_note
}

fn parked(outcome: FoldOutcome) -> i64 {
    match outcome {
        FoldOutcome::FoldedManualReview { request_id } => request_id,
        other => panic!("expected a parked fold, got {other:?}"),
    }
}

fn admitted(outcome: FoldOutcome) -> i64 {
    match outcome {
        FoldOutcome::FoldedFinalized { request_id } => request_id,
        other => panic!("expected a payable fold, got {other:?}"),
    }
}

// -------------------------------------------------- the destination window --

#[test]
fn a_second_rhn_deposit_to_the_same_goldcoin_address_inside_24h_is_parked() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);
    admitted(fold_rhn(&mut ledger, 0, &address, [0x01; 20], T0));

    // A DIFFERENT depositor, so only the destination rule can be doing
    // the work here.
    let second = parked(fold_rhn(&mut ledger, 1, &address, [0x02; 20], T0 + 3_600));
    assert_eq!(
        note_of(&ledger, second).as_deref(),
        Some("wallet_destination_24h_limit")
    );
}

#[test]
fn different_goldcoin_destinations_remain_independent_on_rhn_to_glc() {
    let mut ledger = fresh_ledger();
    admitted(fold_rhn(&mut ledger, 0, &glc_address(0x42), [0x01; 20], T0));
    admitted(fold_rhn(
        &mut ledger,
        1,
        &glc_address(0x43),
        [0x02; 20],
        T0 + 10,
    ));
}

#[test]
fn the_rhn_destination_window_is_exactly_86_400_seconds() {
    // Each side gets its own ledger deliberately: a park is ITSELF a
    // blocker (see `a_parked_row_is_itself_a_blocker_...` below), so
    // reusing one ledger would measure the parked row's window rather
    // than the boundary under test.
    let address = glc_address(0x42);

    // One second short of the boundary: still blocked.
    let mut ledger = fresh_ledger();
    admitted(fold_rhn(&mut ledger, 0, &address, [0x01; 20], T0));
    parked(fold_rhn(
        &mut ledger,
        1,
        &address,
        [0x02; 20],
        T0 + WINDOW - 1,
    ));

    // At exactly `created_at + WINDOW` the blocker has aged out —
    // `retry_after` is the FIRST eligible second, not the last blocked
    // one. Identical boundary semantics to `fold_sol_deposit`.
    let mut ledger = fresh_ledger();
    admitted(fold_rhn(&mut ledger, 0, &address, [0x01; 20], T0));
    admitted(fold_rhn(&mut ledger, 1, &address, [0x03; 20], T0 + WINDOW));
}

/// A row PARKED by a rate limit still occupies the window itself, exactly
/// as on `SolToGlc`: the exclude-list names terminal no-payout states, and
/// `ManualReview` is deliberately not one of them. This is what makes a
/// backlog to one address drain strictly oldest-first (each row waits for
/// its own predecessor) instead of every queued row becoming eligible at
/// the same instant.
#[test]
fn a_parked_row_is_itself_a_blocker_exactly_as_on_sol_to_glc() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);
    admitted(fold_rhn(&mut ledger, 0, &address, [0x01; 20], T0));
    parked(fold_rhn(&mut ledger, 1, &address, [0x02; 20], T0 + 10));

    // The FIRST deposit's window has now elapsed, but the second one's
    // has not — so a third arrival is still blocked, by the park.
    parked(fold_rhn(&mut ledger, 2, &address, [0x03; 20], T0 + WINDOW));
    // Only once the parked row's own window elapses is the address free.
    admitted(fold_rhn(
        &mut ledger,
        3,
        &address,
        [0x04; 20],
        T0 + WINDOW + WINDOW,
    ));
}

// ------------------------------------------------- the source-wallet window --

#[test]
fn a_second_rhn_deposit_from_the_same_wallet_to_a_different_address_is_parked() {
    let mut ledger = fresh_ledger();
    let wallet = [0x77; 20];
    admitted(fold_rhn(&mut ledger, 0, &glc_address(0x42), wallet, T0));

    // The exact bypass this limit closes: one wallet spreading deposits
    // across many different Goldcoin recipients, each individually fresh.
    let second = parked(fold_rhn(
        &mut ledger,
        1,
        &glc_address(0x43),
        wallet,
        T0 + 3_600,
    ));
    assert_eq!(
        note_of(&ledger, second).as_deref(),
        Some("wallet_source_24h_limit")
    );
}

#[test]
fn a_different_rhn_wallet_to_a_fresh_address_is_unaffected() {
    let mut ledger = fresh_ledger();
    admitted(fold_rhn(&mut ledger, 0, &glc_address(0x42), [0x77; 20], T0));
    admitted(fold_rhn(
        &mut ledger,
        1,
        &glc_address(0x43),
        [0x78; 20],
        T0 + 10,
    ));
}

#[test]
fn the_rhn_source_wallet_window_is_exactly_86_400_seconds() {
    let wallet = [0x77; 20];

    let mut ledger = fresh_ledger();
    admitted(fold_rhn(&mut ledger, 0, &glc_address(0x42), wallet, T0));
    parked(fold_rhn(
        &mut ledger,
        1,
        &glc_address(0x43),
        wallet,
        T0 + WINDOW - 1,
    ));

    let mut ledger = fresh_ledger();
    admitted(fold_rhn(&mut ledger, 0, &glc_address(0x42), wallet, T0));
    admitted(fold_rhn(
        &mut ledger,
        1,
        &glc_address(0x43),
        wallet,
        T0 + WINDOW,
    ));
}

#[test]
fn the_source_wallet_reason_outranks_the_recipient_reason() {
    let mut ledger = fresh_ledger();
    let wallet = [0x77; 20];
    let address = glc_address(0x42);
    admitted(fold_rhn(&mut ledger, 0, &address, wallet, T0));
    // Both limits apply. `fold_sol_deposit` reports the wallet first;
    // this must too, or one situation would read differently per route.
    let both = parked(fold_rhn(&mut ledger, 1, &address, wallet, T0 + 10));
    assert_eq!(
        note_of(&ledger, both).as_deref(),
        Some("wallet_source_24h_limit")
    );
}

// ------------------------------------------------------ the cross-route rule --

#[test]
fn a_sol_to_glc_payout_blocks_rhn_to_glc_to_the_same_address_for_24h() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);
    let SolFoldOutcome::FoldedFinalized { .. } = fold_sol(&mut ledger, 0, &address, [0x11; 32], T0)
    else {
        panic!("the Solana deposit must be admitted")
    };

    let blocked = parked(fold_rhn(&mut ledger, 0, &address, [0x77; 20], T0 + 3_600));
    assert_eq!(
        note_of(&ledger, blocked).as_deref(),
        Some("wallet_destination_24h_limit"),
        "a Goldcoin address may take ONE bridge payout per 24h, whatever chain funds it"
    );

    // And the cross-route block clears on exactly the same boundary as a
    // same-route one. Fresh ledger, so the park above is not itself the
    // thing being measured.
    let mut ledger = fresh_ledger();
    let SolFoldOutcome::FoldedFinalized { .. } = fold_sol(&mut ledger, 0, &address, [0x11; 32], T0)
    else {
        panic!()
    };
    parked(fold_rhn(
        &mut ledger,
        0,
        &address,
        [0x77; 20],
        T0 + WINDOW - 1,
    ));
    let mut ledger = fresh_ledger();
    let SolFoldOutcome::FoldedFinalized { .. } = fold_sol(&mut ledger, 0, &address, [0x11; 32], T0)
    else {
        panic!()
    };
    admitted(fold_rhn(&mut ledger, 0, &address, [0x78; 20], T0 + WINDOW));
}

#[test]
fn an_rhn_to_glc_payout_blocks_sol_to_glc_to_the_same_address_for_24h() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);
    admitted(fold_rhn(&mut ledger, 0, &address, [0x77; 20], T0));

    let SolFoldOutcome::FoldedManualReview { request_id } =
        fold_sol(&mut ledger, 0, &address, [0x11; 32], T0 + 3_600)
    else {
        panic!("the Solana deposit must be parked by the Robinhood payout's window")
    };
    assert_eq!(
        note_of(&ledger, request_id).as_deref(),
        Some("wallet_destination_24h_limit"),
        "the destination window is global in BOTH directions, not just one"
    );

    // Fresh ledger for the clearing half, so the park above is not itself
    // the blocker being measured.
    let mut ledger = fresh_ledger();
    admitted(fold_rhn(&mut ledger, 0, &address, [0x77; 20], T0));
    let SolFoldOutcome::FoldedFinalized { .. } =
        fold_sol(&mut ledger, 0, &address, [0x12; 32], T0 + WINDOW)
    else {
        panic!("must be admitted once the Robinhood blocker ages out")
    };
}

#[test]
fn a_cross_route_block_never_reaches_a_different_goldcoin_destination() {
    let mut ledger = fresh_ledger();
    let SolFoldOutcome::FoldedFinalized { .. } =
        fold_sol(&mut ledger, 0, &glc_address(0x42), [0x11; 32], T0)
    else {
        panic!()
    };
    // Different destination address: completely unaffected by the
    // Solana payout above, on either route.
    admitted(fold_rhn(
        &mut ledger,
        0,
        &glc_address(0x43),
        [0x77; 20],
        T0 + 10,
    ));
}

#[test]
fn source_wallet_windows_are_never_pooled_across_source_networks() {
    let mut ledger = fresh_ledger();
    // A Solana wallet deposits; a *Robinhood* wallet then deposits to a
    // different address. Nothing about the Solana wallet's window may
    // touch the EVM wallet's, and vice versa — they are different kinds
    // of identity on different chains.
    let SolFoldOutcome::FoldedFinalized { .. } =
        fold_sol(&mut ledger, 0, &glc_address(0x42), [0x11; 32], T0)
    else {
        panic!()
    };
    admitted(fold_rhn(
        &mut ledger,
        0,
        &glc_address(0x43),
        [0x77; 20],
        T0 + 10,
    ));

    // The mirror: a Robinhood wallet deposits, then a Solana wallet does.
    admitted(fold_rhn(
        &mut ledger,
        1,
        &glc_address(0x44),
        [0x78; 20],
        T0 + 20,
    ));
    let SolFoldOutcome::FoldedFinalized { .. } =
        fold_sol(&mut ledger, 1, &glc_address(0x45), [0x12; 32], T0 + 30)
    else {
        panic!("an EVM wallet's window must never rate-limit a Solana wallet")
    };
}

#[test]
fn a_20_byte_evm_wallet_never_collides_with_a_32_byte_solana_requester() {
    let mut ledger = fresh_ledger();
    // Byte-identical prefixes on purpose: the two limiters read different
    // columns in different tables, so even a deliberately confusable pair
    // must not interact.
    let SolFoldOutcome::FoldedFinalized { .. } =
        fold_sol(&mut ledger, 0, &glc_address(0x42), [0xAB; 32], T0)
    else {
        panic!()
    };
    admitted(fold_rhn(
        &mut ledger,
        0,
        &glc_address(0x43),
        [0xAB; 20],
        T0 + 10,
    ));
}

// ----------------------------------------- which states consume the window --

/// Test-only: forces a terminal state production code does not currently
/// set, purely to exercise the shared exclude-list. The direct mirror of
/// `ledger::tests::force_state`.
fn force_state(ledger: &Ledger, request_id: i64, state: RequestState) {
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![state.as_str(), request_id],
        )
        .unwrap();
}

#[test]
fn a_failed_or_cancelled_rhn_deposit_never_counts_against_its_destination() {
    for terminal in [
        RequestState::Failed,
        RequestState::Cancelled,
        RequestState::Expired,
        RequestState::Reorged,
        RequestState::DestinationSubmissionFailed,
        RequestState::InsufficientReserveAtSettlement,
    ] {
        let mut ledger = fresh_ledger();
        let address = glc_address(0x42);
        let first = admitted(fold_rhn(&mut ledger, 0, &address, [0x01; 20], T0));
        force_state(&ledger, first, terminal);
        admitted(fold_rhn(&mut ledger, 1, &address, [0x02; 20], T0 + 10));
    }
}

#[test]
fn a_failed_or_cancelled_rhn_deposit_never_counts_against_its_source_wallet() {
    let mut ledger = fresh_ledger();
    let wallet = [0x77; 20];
    let first = admitted(fold_rhn(&mut ledger, 0, &glc_address(0x42), wallet, T0));
    force_state(&ledger, first, RequestState::Failed);
    admitted(fold_rhn(
        &mut ledger,
        1,
        &glc_address(0x43),
        wallet,
        T0 + 10,
    ));
}

/// The behaviour `SolToGlc` has always had, preserved verbatim: a REFUND
/// still consumes the full 24-hour window, for both the destination and
/// the source wallet.
///
/// The refund lifecycle states are deliberately absent from the shared
/// exclude-list. A refund means the service declined to complete the
/// transfer — not that the deposit never happened — and letting one reset
/// the window would hand an abuser a free retry on demand.
#[test]
fn a_refunded_rhn_deposit_still_consumes_both_windows_exactly_as_sol_to_glc_does() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);
    let wallet = [0x77; 20];

    // Park it for a reason that is NOT a rate limit (a closed route), so
    // this test measures the refunded row itself and nothing else.
    let row = observation_from(0, DEPOSIT, address.as_bytes().to_vec(), wallet);
    store(&ledger, &row);
    let request_id = parked(
        fold_observation(
            &mut ledger,
            &row,
            network(),
            BRIDGE_FEE_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            false,
            T0,
        )
        .unwrap(),
    );
    assert_eq!(
        note_of(&ledger, request_id).as_deref(),
        Some("route_disabled_at_fold")
    );

    ledger
        .mark_robinhood_refund_pending(request_id, T0 + 1)
        .unwrap();
    ledger
        .mark_robinhood_refund_confirmed(request_id, T0 + 2)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Refunded
    );

    // Destination window: still consumed.
    let blocked = parked(fold_rhn(&mut ledger, 1, &address, [0x99; 20], T0 + 10));
    assert_eq!(
        note_of(&ledger, blocked).as_deref(),
        Some("wallet_destination_24h_limit")
    );
    // Source-wallet window: still consumed.
    let blocked = parked(fold_rhn(
        &mut ledger,
        2,
        &glc_address(0x43),
        wallet,
        T0 + 20,
    ));
    assert_eq!(
        note_of(&ledger, blocked).as_deref(),
        Some("wallet_source_24h_limit")
    );
    // And it is a WINDOW, not a permanent ban.
    let mut ledger = fresh_ledger();
    let row = observation_from(0, DEPOSIT, address.as_bytes().to_vec(), wallet);
    store(&ledger, &row);
    let request_id = parked(
        fold_observation(
            &mut ledger,
            &row,
            network(),
            BRIDGE_FEE_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            false,
            T0,
        )
        .unwrap(),
    );
    ledger
        .mark_robinhood_refund_pending(request_id, T0 + 1)
        .unwrap();
    ledger
        .mark_robinhood_refund_confirmed(request_id, T0 + 2)
        .unwrap();
    admitted(fold_rhn(&mut ledger, 1, &address, wallet, T0 + WINDOW));
}

#[test]
fn a_rate_limited_rhn_park_holds_no_reserve_capacity() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);
    admitted(fold_rhn(&mut ledger, 0, &address, [0x01; 20], T0));
    let after_one = ledger
        .available_capacity(crate::ledger::ReserveDirection::GoldcoinReserve)
        .unwrap();

    parked(fold_rhn(&mut ledger, 1, &address, [0x02; 20], T0 + 10));
    assert_eq!(
        ledger
            .available_capacity(crate::ledger::ReserveDirection::GoldcoinReserve)
            .unwrap(),
        after_one,
        "a rate-limited park must reserve nothing — same posture as fold_sol_deposit"
    );
}

#[test]
fn replaying_the_same_rhn_obligation_is_never_reinterpreted_as_a_rate_limit_hit() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);
    let wallet = [0x77; 20];
    let row = observation_from(0, DEPOSIT, address.as_bytes().to_vec(), wallet);
    store(&ledger, &row);
    let request_id = admitted(
        fold_observation(
            &mut ledger,
            &row,
            network(),
            BRIDGE_FEE_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            true,
            T0,
        )
        .unwrap(),
    );

    // The very same observation, re-offered inside its own window. The
    // durable-identity guard must answer first — a replay is not a second
    // deposit, and must never be reported as rate limited.
    for now in [T0 + 1, T0 + 3_600, T0 + WINDOW - 1] {
        assert_eq!(
            fold_observation(
                &mut ledger,
                &row,
                network(),
                BRIDGE_FEE_BPS,
                crate::amount_conversion::CanonicalAtomic(1),
                true,
                now
            )
            .unwrap(),
            FoldOutcome::AlreadyFolded { request_id }
        );
    }
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized,
        "a replay must not disturb the original request"
    );
}

// ------------------------------------------------------------- the resume --

#[test]
fn a_rate_limited_rhn_park_resumes_once_its_window_expires() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);
    admitted(fold_rhn(&mut ledger, 0, &address, [0x01; 20], T0));
    let blocked = parked(fold_rhn(&mut ledger, 1, &address, [0x02; 20], T0 + 10));

    // Too early: refused, with no mutation.
    let err = ledger
        .resume_manual_review_rhn_to_glc(blocked, "too early", "operator", T0 + 20)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::WalletWindowActive { request_id, role: WalletRole::Destination, .. } if request_id == blocked),
        "got {err}"
    );
    assert_eq!(
        ledger.get_request(blocked).unwrap().unwrap().state,
        RequestState::ManualReview
    );

    // Exactly at the blocker's `retry_after`: resumes normally, in place.
    let outcome = ledger
        .resume_manual_review_rhn_to_glc(blocked, "window cleared", "operator", T0 + WINDOW)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
    let request = ledger.get_request(blocked).unwrap().unwrap();
    assert_eq!(request.state, RequestState::SourceFinalized);
    assert_eq!(request.manual_review_note, None);
    assert_eq!(
        request.source_obligation_index,
        Some(1),
        "resume transitions the EXISTING row — never a second obligation"
    );
}

#[test]
fn resuming_an_rhn_park_reserves_capacity_exactly_once() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);
    admitted(fold_rhn(&mut ledger, 0, &address, [0x01; 20], T0));
    let blocked = parked(fold_rhn(&mut ledger, 1, &address, [0x02; 20], T0 + 10));
    let parked_capacity = ledger
        .available_capacity(crate::ledger::ReserveDirection::GoldcoinReserve)
        .unwrap();
    // The NET destination amount — what a successful fold would have
    // reserved — read from the request itself rather than restated here,
    // so the assertion cannot drift with the fee policy.
    let net = ledger
        .get_request(blocked)
        .unwrap()
        .unwrap()
        .net_destination_atomic as i64;

    ledger
        .resume_manual_review_rhn_to_glc(blocked, "first", "operator", T0 + WINDOW)
        .unwrap();
    let after_resume = ledger
        .available_capacity(crate::ledger::ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(
        after_resume,
        parked_capacity - net,
        "a resume reserves exactly what the fold would have"
    );

    // Idempotent: a repeat is a no-op, never a second reservation.
    for _ in 0..3 {
        assert_eq!(
            ledger
                .resume_manual_review_rhn_to_glc(blocked, "again", "operator", T0 + WINDOW + 1)
                .unwrap(),
            ResumeManualReviewOutcome::AlreadyResumed {
                state: RequestState::SourceFinalized
            }
        );
        assert_eq!(
            ledger
                .available_capacity(crate::ledger::ReserveDirection::GoldcoinReserve)
                .unwrap(),
            after_resume
        );
    }
}

#[test]
fn a_manual_rhn_resume_can_never_bypass_a_live_source_wallet_window() {
    let mut ledger = fresh_ledger();
    let wallet = [0x77; 20];
    admitted(fold_rhn(&mut ledger, 0, &glc_address(0x42), wallet, T0));
    let blocked = parked(fold_rhn(
        &mut ledger,
        1,
        &glc_address(0x43),
        wallet,
        T0 + 10,
    ));

    let err = ledger
        .resume_manual_review_rhn_to_glc(blocked, "operator override attempt", "operator", T0 + 20)
        .unwrap_err();
    assert!(
        matches!(
            err,
            LedgerError::WalletWindowActive { request_id, role: WalletRole::Source, .. }
                if request_id == blocked
        ),
        "got {err}"
    );
    assert_eq!(
        ledger.get_request(blocked).unwrap().unwrap().state,
        RequestState::ManualReview
    );

    assert_eq!(
        ledger
            .resume_manual_review_rhn_to_glc(blocked, "cleared", "operator", T0 + WINDOW)
            .unwrap(),
        ResumeManualReviewOutcome::Resumed
    );
}

/// The re-check is UNCONDITIONAL — it does not care what the request was
/// originally parked for. A request parked for a closed route, whose
/// destination has meanwhile been paid by another deposit, must still be
/// refused; otherwise "parked for a different reason" would be a bypass.
#[test]
fn an_rhn_resume_rechecks_the_windows_even_when_it_was_parked_for_another_reason() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);

    // An ordinary payout takes the address's window first.
    admitted(fold_rhn(&mut ledger, 0, &address, [0x02; 20], T0));

    // A later deposit to the same address is parked for a DIFFERENT
    // reason — the reserve was paused when it landed.
    ledger
        .set_paused(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            true,
            Some("incident"),
        )
        .unwrap();
    let blocked = parked(fold_rhn(&mut ledger, 1, &address, [0x01; 20], T0 + 10));
    assert_eq!(
        note_of(&ledger, blocked).as_deref(),
        Some("reserve_paused_at_fold")
    );
    ledger
        .set_paused(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            false,
            Some("resolved"),
        )
        .unwrap();

    // The incident is over. The resume must STILL refuse: the rate-limit
    // re-check does not care what the request's own note says.
    let err = ledger
        .resume_manual_review_rhn_to_glc(blocked, "incident resolved", "operator", T0 + 20)
        .unwrap_err();
    assert!(
        matches!(
            err,
            LedgerError::WalletWindowActive {
                role: WalletRole::Destination,
                ..
            }
        ),
        "got {err}"
    );

    // Once the predecessor's window clears, the same call succeeds.
    assert_eq!(
        ledger
            .resume_manual_review_rhn_to_glc(blocked, "cleared", "operator", T0 + WINDOW)
            .unwrap(),
        ResumeManualReviewOutcome::Resumed
    );
}

/// A refund is one-way and permanent: once begun, no resume — by an
/// operator or by automatic recovery — may ever re-open the request.
#[test]
fn an_rhn_request_with_a_refund_lifecycle_can_never_be_resumed() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);
    let row = observation_from(0, DEPOSIT, address.as_bytes().to_vec(), [0x01; 20]);
    store(&ledger, &row);
    let request_id = parked(
        fold_observation(
            &mut ledger,
            &row,
            network(),
            BRIDGE_FEE_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            false,
            T0,
        )
        .unwrap(),
    );
    ledger
        .mark_robinhood_refund_pending(request_id, T0 + 1)
        .unwrap();

    let err = ledger
        .resume_manual_review_rhn_to_glc(request_id, "attempt", "operator", T0 + WINDOW * 10)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::ManualReviewNotRecoverable { .. }),
        "got {err}"
    );
}

#[test]
fn the_two_resume_entry_points_each_refuse_the_other_route() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);
    admitted(fold_rhn(&mut ledger, 0, &address, [0x01; 20], T0));
    let rhn_parked = parked(fold_rhn(&mut ledger, 1, &address, [0x02; 20], T0 + 10));

    let err = ledger
        .resume_manual_review_sol_to_glc(rhn_parked, "wrong command", "operator", T0 + WINDOW)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::NotASolToGlcRequest { .. }),
        "got {err}"
    );

    let mut ledger = fresh_ledger();
    let SolFoldOutcome::FoldedFinalized { .. } = fold_sol(&mut ledger, 0, &address, [0x11; 32], T0)
    else {
        panic!()
    };
    let SolFoldOutcome::FoldedManualReview {
        request_id: sol_parked,
    } = fold_sol(&mut ledger, 1, &address, [0x12; 32], T0 + 10)
    else {
        panic!()
    };
    let err = ledger
        .resume_manual_review_rhn_to_glc(sol_parked, "wrong command", "operator", T0 + WINDOW)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::NotARhnToGlcRequest { .. }),
        "got {err}"
    );
}

/// Oldest-first draining, across a backlog to ONE destination and across
/// BOTH routes. Only a strict predecessor by `(created_at, id)` may block
/// a candidate, so a later arrival can never shadow-block an earlier one.
#[test]
fn a_mixed_route_backlog_to_one_address_drains_strictly_oldest_first() {
    let mut ledger = fresh_ledger();
    let address = glc_address(0x42);

    // A (Robinhood, t=T0) is admitted and owns the window.
    admitted(fold_rhn(&mut ledger, 0, &address, [0x01; 20], T0));
    // B (Solana, t=T0+10) and C (Robinhood, t=T0+20) both park behind it.
    let SolFoldOutcome::FoldedManualReview { request_id: b } =
        fold_sol(&mut ledger, 0, &address, [0x11; 32], T0 + 10)
    else {
        panic!()
    };
    let c = parked(fold_rhn(&mut ledger, 1, &address, [0x02; 20], T0 + 20));

    // At A's boundary, B (the oldest parked) becomes eligible. C does not:
    // its own predecessor, B, is still inside its window.
    assert!(ledger
        .resume_manual_review_rhn_to_glc(c, "too early", "operator", T0 + WINDOW)
        .is_err());
    assert_eq!(
        ledger
            .resume_manual_review_sol_to_glc(b, "A cleared", "operator", T0 + WINDOW)
            .unwrap(),
        ResumeManualReviewOutcome::Resumed
    );

    // C waits for B's own window, then drains — never before, never out of
    // order, and never blocked by anything newer than itself.
    assert!(ledger
        .resume_manual_review_rhn_to_glc(c, "still too early", "operator", T0 + WINDOW + 9)
        .is_err());
    assert_eq!(
        ledger
            .resume_manual_review_rhn_to_glc(c, "B cleared", "operator", T0 + WINDOW + 10)
            .unwrap(),
        ResumeManualReviewOutcome::Resumed
    );
}

// ------------------------------------------------------- routes unaffected --

#[test]
fn the_inbound_rate_limits_never_touch_glc_to_sol_or_glc_to_rhn() {
    // No outbound direction is in the destination set the limit is scoped
    // to, so no `GlcToSol`/`GlcToRhn` row can ever be a blocker or be
    // blocked. Pinned structurally rather than by fold: those directions
    // never call either check at all.
    for direction in Direction::ALL {
        assert_eq!(
            direction.destination_is_goldcoin(),
            matches!(direction, Direction::SolToGlc | Direction::RhnToGlc),
            "{direction:?}"
        );
    }
    // The two cross routes are executable (Phase H) but neither is
    // Goldcoin-bound, so neither is touched by these limits either.
    assert!(!Direction::SolToRhn.destination_is_goldcoin());
    assert!(!Direction::RhnToSol.destination_is_goldcoin());
}

// ------------------------------- API availability <-> fold equivalence --
//
// The production launch-blocker these tests exist for: `GET /chains`
// reported `RhnToGlc` as usable while `reserve_ledger.admission_closed`
// was set on `GoldcoinReserve`, so every newly observed Robinhood deposit
// folded straight into `ManualReview` with `admission_closed_at_fold`.
// Users made irreversible on-chain deposits against that answer.
//
// The fix is that both answers now come from ONE evaluator
// (`crate::ledger::admission`). These tests are what makes that
// structural rather than merely current: they drive a REAL fold against a
// matrix of reserve states and assert, for each, that the read-only
// answer the API publishes agreed with what the fold then did.

/// The read-only availability answer the public API computes, taken
/// through the exact function `api::route_availability` calls.
fn api_says_available(ledger: &Ledger) -> bool {
    ledger
        .route_admission_blocker(crate::ledger::Direction::RhnToGlc)
        .expect("the Goldcoin reserve is configured in these fixtures")
        .is_none()
}

/// One matrix row: a mutation to apply to a freshly configured ledger,
/// and a label for failure output.
struct GateCase {
    label: &'static str,
    apply: fn(&mut Ledger),
}

/// Every runtime gate, one row each, plus the healthy baseline.
fn gate_cases() -> Vec<GateCase> {
    vec![
        GateCase {
            label: "healthy",
            apply: |_| {},
        },
        GateCase {
            label: "operator admission closed",
            apply: |l| {
                l.set_admission(
                    crate::ledger::ReserveDirection::GoldcoinReserve,
                    true,
                    Some("test"),
                )
                .unwrap()
            },
        },
        GateCase {
            label: "reserve paused",
            apply: |l| {
                l.set_paused(
                    crate::ledger::ReserveDirection::GoldcoinReserve,
                    true,
                    Some("test"),
                )
                .unwrap()
            },
        },
        GateCase {
            label: "capacity exhausted (protected minimum takes the whole balance)",
            // `configure_reserve` updates the thresholds, never the
            // cached balance, so raising `protected_minimum` to the
            // fixture's full balance is what drives headroom to zero.
            apply: |l| {
                l.configure_reserve(
                    crate::ledger::ReserveDirection::GoldcoinReserve,
                    0,
                    1_000_000_000_000,
                    2_000_000_000_000,
                    1_500_000_000_000,
                    1_000_000_000_001,
                    100,
                )
                .unwrap()
            },
        },
        GateCase {
            label: "confirmed-liquidity buffer above headroom",
            apply: |l| {
                l.set_admission_liquidity_thresholds(
                    crate::ledger::ReserveDirection::GoldcoinReserve,
                    u64::MAX / 4,
                    u64::MAX / 2,
                )
                .unwrap()
            },
        },
        GateCase {
            label: "mature UTXO pool floor engaged on an empty pool",
            apply: |l| {
                l.set_utxo_pool_thresholds(crate::ledger::ReserveDirection::GoldcoinReserve, 3, 5)
                    .unwrap()
            },
        },
    ]
}

/// **The equivalence.** For every gate, the answer the API would have
/// published before the deposit must match what the fold then did with
/// it — and when both say "no", the recorded `manual_review_note` must be
/// the one the shared evaluator named.
#[test]
fn api_availability_matches_what_the_fold_actually_does() {
    // Counted so a bug that made EVERY row agree trivially (all
    // available, or all parked) fails instead of passing quietly.
    let mut admitted_rows = 0usize;
    let mut parked_rows = 0usize;
    for case in gate_cases() {
        let mut ledger = ledger();
        (case.apply)(&mut ledger);
        // The daemon settles the confirmed-liquidity hysteresis once per
        // tick so the state a read-only surface reports is current; do
        // the same here, since that is the operational precondition the
        // API's read-only answer is specified against.
        ledger
            .evaluate_liquidity_admission_gate(
                crate::ledger::ReserveDirection::GoldcoinReserve,
                299,
            )
            .unwrap();

        let predicted = api_says_available(&ledger);
        let blocker = ledger
            .route_admission_blocker(crate::ledger::Direction::RhnToGlc)
            .unwrap();

        // A deliberately tiny deposit, so the ONLY thing that can park it
        // is a gate rather than its own size — which is exactly the
        // amount-independent question `available` answers.
        let row = observation(0, 10_000, destination().into_bytes());
        store(&ledger, &row);
        let outcome = fold_observation(
            &mut ledger,
            &row,
            network(),
            BRIDGE_FEE_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            true,
            300,
        )
        .unwrap();

        let admitted = matches!(outcome, FoldOutcome::FoldedFinalized { .. });
        if admitted {
            admitted_rows += 1;
        } else {
            parked_rows += 1;
        }
        assert_eq!(
            predicted,
            admitted,
            "[{}] the API published available={predicted} but the fold {}",
            case.label,
            if admitted { "admitted" } else { "parked" }
        );

        let request = ledger.get_request(outcome.request_id()).unwrap().unwrap();
        match blocker {
            None => assert_eq!(
                request.state,
                RequestState::SourceFinalized,
                "[{}] an available route must fold to SourceFinalized",
                case.label
            ),
            Some(b) => {
                assert_eq!(
                    request.state,
                    RequestState::ManualReview,
                    "[{}] an unavailable route must park",
                    case.label
                );
                assert_eq!(
                    request.manual_review_note.as_deref(),
                    Some(b.manual_review_note()),
                    "[{}] the parked note must be the one the shared evaluator named",
                    case.label
                );
            }
        }
    }
    assert_eq!(admitted_rows, 1, "exactly the healthy row must be admitted");
    assert_eq!(
        parked_rows,
        gate_cases().len() - 1,
        "every gate row must actually park — otherwise this matrix agrees for the wrong reason"
    );
}

/// The specific production shape, spelled out on its own so a regression
/// names itself: admission closed by an operator, route wide open, and a
/// deposit that arrives anyway.
#[test]
fn admission_closed_makes_the_route_unavailable_and_parks_the_deposit() {
    let mut ledger = ledger();
    assert!(
        api_says_available(&ledger),
        "the fixture must start available, or this test proves nothing"
    );

    ledger
        .set_admission(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            true,
            Some("incident"),
        )
        .unwrap();
    assert!(
        !api_says_available(&ledger),
        "a closed admission gate must make the route unavailable BEFORE any deposit"
    );

    // `route_open` is `true` — the route gate is wide open, exactly as
    // `/chains` reported `enabled: true` in production.
    let row = observation(1, 10_000, destination().into_bytes());
    store(&ledger, &row);
    let outcome = fold_observation(
        &mut ledger,
        &row,
        network(),
        BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        true,
        300,
    )
    .unwrap();
    let id = outcome.request_id();
    assert!(matches!(outcome, FoldOutcome::FoldedManualReview { .. }));
    assert_eq!(
        ledger
            .get_request(id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("admission_closed_at_fold")
    );

    // And re-opening admission restores availability, with no other
    // change — the operator remedy is the one thing that moves it.
    ledger
        .set_admission(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            false,
            Some("reopened"),
        )
        .unwrap();
    assert!(api_says_available(&ledger));
}

/// The Robinhood reserve's own pause — the figure `glc-admin
/// robinhood-status` prints — must NOT affect `RhnToGlc`, which pays out
/// of the GOLDCOIN reserve. Mistaking one for the other is what made the
/// production incident hard to read.
#[test]
fn the_robinhood_reserve_pause_does_not_gate_rhn_to_glc() {
    let mut ledger = ledger();
    ledger
        .configure_reserve(
            crate::ledger::ReserveDirection::RobinhoodReserve,
            1_000_000_000,
            0,
            1_000_000_000,
            500_000_000,
            250_000_000,
            100,
        )
        .unwrap();
    ledger
        .set_paused(
            crate::ledger::ReserveDirection::RobinhoodReserve,
            true,
            Some("outbound incident"),
        )
        .unwrap();

    assert!(
        api_says_available(&ledger),
        "RhnToGlc draws on the Goldcoin reserve; the Robinhood reserve's pause is a \
         different route's gate"
    );
    let row = observation(2, 10_000, destination().into_bytes());
    store(&ledger, &row);
    assert!(matches!(
        fold_observation(
            &mut ledger,
            &row,
            network(),
            BRIDGE_FEE_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            true,
            300
        )
        .unwrap(),
        FoldOutcome::FoldedFinalized { .. }
    ));
}

mod bridge_quote;
mod cross_route;

/// The SOURCE transfer maximum (`min_transfer::SOURCE_MAXIMUM_CANONICAL`,
/// 50 000 GLC) at the fold: a deposit the contract accepted above it is
/// parked with an explicit reason, reserves nothing and stays
/// refundable — exactly as a sub-minimum one — while one at exactly the
/// maximum folds payable. The policy is on what was SENT; the ceiling is
/// the constant, not the caller's (test-tunable) floor.
#[test]
fn a_deposit_above_the_source_maximum_is_parked_and_one_at_it_is_payable() {
    // The Robinhood-sourced maximum (20 000 GLC), not the 50 000 GLC of
    // the Goldcoin/Solana-sourced routes.
    let max = crate::min_transfer::source_maximum(Route::RhnToGlc).0;
    assert_eq!(
        max,
        crate::min_transfer::SOURCE_MAXIMUM_ROBINHOOD_CANONICAL.0
    );
    for (canonical, parked) in [(max, false), (max + 1, true)] {
        let mut ledger = Ledger::open_in_memory().expect("an in-memory ledger");
        // Deep enough that liquidity is not the reason.
        ledger
            .configure_reserve(
                crate::ledger::ReserveDirection::GoldcoinReserve,
                100_000_000_000_000,
                0,
                50_000_000_000_000,
                20_000_000_000_000,
                10_000_000_000_000,
                100,
            )
            .unwrap();
        let row = observation(0, canonical, destination().into_bytes());
        store(&ledger, &row);
        let outcome = fold_observation(
            &mut ledger,
            &row,
            network(),
            BRIDGE_FEE_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            true,
            300,
        )
        .unwrap();
        let request_id = match outcome {
            FoldOutcome::FoldedFinalized { request_id }
            | FoldOutcome::FoldedManualReview { request_id } => request_id,
            other => panic!("{canonical}: {other:?}"),
        };
        let request = ledger.get_request(request_id).unwrap().unwrap();
        assert_eq!(request.gross_amount_atomic, canonical);
        if parked {
            assert!(matches!(outcome, FoldOutcome::FoldedManualReview { .. }));
            assert_eq!(request.state, RequestState::ManualReview);
            let note = request.manual_review_note.as_deref().unwrap();
            assert!(note.starts_with("above source maximum"), "{note}");
            assert!(note.contains("amount SENT"), "{note}");
            let (_, _, reserved, _) = ledger
                .reserve_snapshot(crate::ledger::ReserveDirection::GoldcoinReserve)
                .unwrap();
            assert_eq!(reserved, 0, "nothing is held for a parked deposit");
        } else {
            assert!(matches!(outcome, FoldOutcome::FoldedFinalized { .. }));
            assert_eq!(request.state, RequestState::SourceFinalized);
        }
    }
}
