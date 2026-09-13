use super::*;

/// A fee-free `RequestAmounts` for structural/lifecycle tests that predate
/// the bridge fee and don't care about fee math (docs/20-bridge-fee.md) —
/// dedicated fee/accounting behavior is covered separately. The ledger
/// itself never validates `fee_bps`/computes a fee, so this is a legitimate
/// (if unrealistic) input from the ledger's point of view.
fn amounts(gross: u64) -> RequestAmounts {
    RequestAmounts {
        gross_atomic: gross,
        fee_bps: 0,
        fee_atomic: 0,
        net_atomic: gross,
        net_destination_atomic: gross,
    }
}

fn setup() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            1_000_000,
            100_000,
            500_000,
            200_000,
            150_000,
            1_000,
        )
        .unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            1_000_000,
            100_000,
            500_000,
            200_000,
            150_000,
            1_000,
        )
        .unwrap();
    ledger
}

#[test]
fn available_capacity_is_balance_minus_minimum_minus_reserved() {
    let ledger = setup();
    // 1_000_000 - 100_000 - 0
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        900_000
    );
}

#[test]
fn create_request_reserves_capacity_and_never_exceeds_it() {
    let mut ledger = setup();
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    assert!(matches!(outcome, CreateRequestOutcome::Reserved { .. }));
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000
    );
    ledger
        .check_invariant(ReserveDirection::SolanaReserve)
        .unwrap();
}

#[test]
fn create_request_capacity_check_is_based_on_net_destination_not_gross_amount() {
    // available capacity for SolanaReserve is 900_000 (balance 1_000_000 -
    // protected_minimum 100_000, see `setup`). A gross far beyond that
    // must still succeed as long as the fee-adjusted NET destination
    // payout fits exactly — proving the capacity check is against
    // `net_destination_atomic`, not `gross_atomic` (docs/20-bridge-fee.md).
    let mut ledger = setup();
    let net_at_capacity = RequestAmounts {
        gross_atomic: 5_000_000, // far beyond 900_000 if checked against gross
        fee_bps: 100,
        fee_atomic: 4_100_000,
        net_atomic: 900_000,
        net_destination_atomic: 900_000, // exactly at available capacity
    };
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            net_at_capacity,
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    assert!(
        matches!(outcome, CreateRequestOutcome::Reserved { .. }),
        "a huge gross must still be accepted when its net destination payout fits capacity"
    );
}

#[test]
fn create_request_rejects_when_net_destination_exceeds_capacity_even_for_a_small_gross() {
    // Inverse of the test above: a small gross whose net_destination_atomic
    // exceeds capacity must be rejected — a small gross figure alone
    // guarantees nothing about whether the destination reserve can
    // actually cover the release (docs/20-bridge-fee.md).
    let mut ledger = setup();
    let net_over_capacity = RequestAmounts {
        gross_atomic: 1_000,
        fee_bps: 0,
        fee_atomic: 0,
        net_atomic: 1_000,
        net_destination_atomic: 950_000, // exceeds the 900_000 available
    };
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            net_over_capacity,
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    assert_eq!(
        outcome,
        CreateRequestOutcome::InsufficientLiquidity {
            available_capacity: 900_000
        },
        "insufficient destination reserve must be judged on the net payout, not the gross amount"
    );
}

#[test]
fn create_request_rejects_when_capacity_insufficient_never_creates_a_row() {
    let mut ledger = setup();
    // available is 900_000; ask for more than that.
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(950_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    assert_eq!(
        outcome,
        CreateRequestOutcome::InsufficientLiquidity {
            available_capacity: 900_000
        }
    );
    // Never accept a transfer that cannot be fulfilled: no capacity touched.
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        900_000
    );
    let none = ledger
        .requests_by_state(Direction::GlcToSol, RequestState::AwaitingDeposit)
        .unwrap();
    assert!(none.is_empty());
}

#[test]
fn create_request_rejects_when_direction_is_paused() {
    let mut ledger = setup();
    ledger
        .set_paused(ReserveDirection::SolanaReserve, true, Some("test"))
        .unwrap();
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(1_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    assert_eq!(outcome, CreateRequestOutcome::Paused);
}

#[test]
fn concurrent_reservations_never_double_spend_the_same_capacity() {
    // Sequential calls stand in for "concurrent" here since sqlite
    // serializes writers DB-wide (module docs) — the property under test is
    // that two reservations summing to more than available capacity cannot
    // both succeed, regardless of arrival order.
    let mut ledger = setup();
    let available = ledger
        .available_capacity(ReserveDirection::SolanaReserve)
        .unwrap();
    let half = available / 2 + 1; // two of these exceed capacity
    let first = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(half as u64),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    let second = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(half as u64),
            &[2u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    assert!(matches!(first, CreateRequestOutcome::Reserved { .. }));
    assert!(matches!(
        second,
        CreateRequestOutcome::InsufficientLiquidity { .. }
    ));
    ledger
        .check_invariant(ReserveDirection::SolanaReserve)
        .unwrap();
}

#[test]
fn expire_reservations_releases_capacity_and_is_idempotent() {
    let mut ledger = setup();
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            10,
            1_000,
        )
        .unwrap();
    let CreateRequestOutcome::Reserved { request_id } = outcome else {
        panic!()
    };

    // Not yet expired.
    assert_eq!(ledger.expire_reservations(1_005).unwrap(), 0);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000
    );

    // Past expiry.
    let expired = ledger.expire_reservations(1_020).unwrap();
    assert_eq!(expired, 1);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        900_000
    );
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::Expired);

    // Idempotent: running again finds nothing more to expire.
    assert_eq!(ledger.expire_reservations(1_030).unwrap(), 0);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        900_000
    );
    ledger
        .check_invariant(ReserveDirection::SolanaReserve)
        .unwrap();
}

#[test]
fn cancel_request_releases_capacity() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .cancel_request(request_id, 1_001, "user requested")
        .unwrap();
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        900_000
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Cancelled
    );
}

// -------------------------------------------------------------- Goldcoin leg --

#[test]
fn glc_deposit_flows_from_awaiting_through_confirming_to_finalized() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };

    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::Recorded);
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Confirming
    );

    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::SourceFinalized);
    assert_eq!(req.source_txid, Some([0xAA; 32]));

    // pending_obligations now holds the committed amount.
    let pending: i64 = ledger
        .raw()
        .query_row(
            "SELECT pending_obligations FROM reserve_ledger WHERE direction = 'SolanaReserve'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 100_000);
    ledger
        .check_invariant(ReserveDirection::SolanaReserve)
        .unwrap();
}

#[test]
fn available_vault_utxos_excludes_a_utxo_backing_a_not_yet_finalized_glc_to_sol_deposit() {
    // Regression: a concurrent SolToGlc payout's coin selection could pick
    // the vault UTXO backing a GlcToSol deposit before that deposit
    // reached SourceFinalized, permanently stranding the GlcToSol request
    // in Confirming once its own backing output turned up already spent.
    // Prevention: available_vault_utxos must never offer such a UTXO as a
    // payout candidate in the first place.
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Confirming
    );

    // Two vault UTXOs are now visible on-chain: the one backing the
    // still-Confirming GlcToSol deposit above, and an unrelated one (e.g.
    // vault change from an earlier settlement) with nothing pending
    // against it.
    let backing_deposit = crate::goldcoin::coin::VaultUtxo {
        txid: [0xAA; 32],
        vout: 0,
        amount_atomic: 100_000,
        script_pubkey_hex: "51".to_string(),
    };
    let unrelated = crate::goldcoin::coin::VaultUtxo {
        txid: [0xCC; 32],
        vout: 1,
        amount_atomic: 250_000,
        script_pubkey_hex: "51".to_string(),
    };
    ledger
        .sync_vault_utxos(
            &[
                (backing_deposit.clone(), 6, "51".to_string()),
                (unrelated.clone(), 6, "51".to_string()),
            ],
            1,
            1_150,
        )
        .unwrap();

    let available = ledger.available_vault_utxos().unwrap();
    assert!(
        !available
            .iter()
            .any(|u| u.txid == backing_deposit.txid && u.vout == backing_deposit.vout),
        "must exclude the UTXO backing a not-yet-SourceFinalized GlcToSol deposit: {available:?}"
    );
    assert!(
        available
            .iter()
            .any(|u| u.txid == unrelated.txid && u.vout == unrelated.vout),
        "must still offer an unrelated, unencumbered UTXO: {available:?}"
    );

    // Once the deposit reaches SourceFinalized, its UTXO becomes a
    // legitimate payout candidate again (nothing left to strand).
    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();
    let available_after = ledger.available_vault_utxos().unwrap();
    assert!(
        available_after
            .iter()
            .any(|u| u.txid == backing_deposit.txid && u.vout == backing_deposit.vout),
        "must offer the UTXO once its backing deposit is SourceFinalized: {available_after:?}"
    );
}

#[test]
fn immature_vault_utxo_total_sums_only_unconfirmed_utxos() {
    let mut ledger = setup();

    let mature = crate::goldcoin::coin::VaultUtxo {
        txid: [0xAAu8; 32],
        vout: 0,
        amount_atomic: 250_000,
        script_pubkey_hex: "51".to_string(),
    };
    let immature = crate::goldcoin::coin::VaultUtxo {
        txid: [0xBBu8; 32],
        vout: 0,
        amount_atomic: 9_010_000,
        script_pubkey_hex: "51".to_string(),
    };

    // Only 9 confirmations against a required minimum of 20 — mirrors the
    // production incident's large, still-maturing change output.
    ledger
        .sync_vault_utxos(
            &[
                (mature.clone(), 20, "51".to_string()),
                (immature.clone(), 9, "51".to_string()),
            ],
            20,
            1_000,
        )
        .unwrap();

    assert_eq!(ledger.immature_vault_utxo_total().unwrap(), 9_010_000);

    // The mature UTXO is a normal payout candidate; the immature one is
    // invisible to coin selection until it matures.
    let available = ledger.available_vault_utxos().unwrap();
    assert!(available.iter().any(|u| u.txid == mature.txid));
    assert!(!available.iter().any(|u| u.txid == immature.txid));

    // Once it matures, it stops counting as immature and becomes a normal
    // candidate.
    ledger
        .sync_vault_utxos(
            &[
                (mature.clone(), 21, "51".to_string()),
                (immature.clone(), 21, "51".to_string()),
            ],
            20,
            1_100,
        )
        .unwrap();
    assert_eq!(ledger.immature_vault_utxo_total().unwrap(), 0);
    let available_after = ledger.available_vault_utxos().unwrap();
    assert!(available_after.iter().any(|u| u.txid == immature.txid));
}

#[test]
fn available_vault_utxos_excludes_a_utxo_already_reserved_for_another_payout() {
    // A UTXO `reserve_vault_utxos` has already claimed for one SolToGlc
    // payout must never be offered to coin selection for a second, distinct
    // payout — the reservation itself (not merely `state != 'Spent'`) is
    // what coin selection must respect, since the reserving payout has not
    // broadcast (or even necessarily been signed) yet.
    let mut ledger = setup();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };

    let utxo = crate::goldcoin::coin::VaultUtxo {
        txid: [0xEEu8; 32],
        vout: 0,
        amount_atomic: 500_000,
        script_pubkey_hex: "51".to_string(),
    };
    ledger
        .sync_vault_utxos(&[(utxo.clone(), 20, "51".to_string())], 1, 1_000)
        .unwrap();
    assert!(
        ledger
            .available_vault_utxos()
            .unwrap()
            .iter()
            .any(|u| u.txid == utxo.txid),
        "must be a normal candidate before anything reserves it"
    );

    ledger
        .reserve_vault_utxos(request_id, std::slice::from_ref(&utxo), 0, 1_100)
        .unwrap();

    let available = ledger.available_vault_utxos().unwrap();
    assert!(
        !available.iter().any(|u| u.txid == utxo.txid),
        "a UTXO already reserved by another in-flight payout must never be offered again: {available:?}"
    );
}

#[test]
fn reserve_vault_utxos_is_safe_under_genuine_concurrent_writers() {
    // Real OS threads, each with its own connection to the SAME file-backed
    // ledger, racing to reserve the SAME single vault UTXO for two different
    // payout requests — not the sequential-call "concurrency" stand-in used
    // elsewhere in this crate's test suite (module docs on those tests
    // explain why sequential calls are an adequate substitute for capacity
    // accounting; the outpoint-level reservation guard below is exactly the
    // mechanism that substitution would fail to exercise). A busy timeout is
    // set on each connection so SQLite's writer serialization produces a
    // deterministic winner rather than a flaky `SQLITE_BUSY` on either side.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let (request_a, request_b, utxo) = {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                1_000_000,
                0,
                500_000,
                200_000,
                150_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                1_000_000,
                0,
                500_000,
                200_000,
                150_000,
                0,
            )
            .unwrap();
        let SolFoldOutcome::FoldedFinalized { request_id: a } = ledger
            .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 0)
            .unwrap()
        else {
            panic!()
        };
        let SolFoldOutcome::FoldedFinalized { request_id: b } = ledger
            .fold_sol_deposit(1, amounts(100_000), [3u8; 32], &[4u8; 32], None, 0)
            .unwrap()
        else {
            panic!()
        };
        let utxo = crate::goldcoin::coin::VaultUtxo {
            txid: [0xFFu8; 32],
            vout: 0,
            amount_atomic: 500_000,
            script_pubkey_hex: "51".to_string(),
        };
        ledger
            .sync_vault_utxos(&[(utxo.clone(), 20, "51".to_string())], 1, 0)
            .unwrap();
        (a, b, utxo)
    };

    let run = |request_id: i64| {
        let path = path.clone();
        let utxo = utxo.clone();
        std::thread::spawn(move || {
            // The busy timeout this test used to set by hand is now
            // applied by `Ledger::open` itself, for every connection.
            let mut ledger = Ledger::open(&path).unwrap();
            ledger.reserve_vault_utxos(request_id, &[utxo], 0, 10)
        })
    };
    let ta = run(request_a);
    let tb = run(request_b);
    let ra = ta.join().unwrap();
    let rb = tb.join().unwrap();

    let outcomes = [ra.is_ok(), rb.is_ok()];
    assert_eq!(
        outcomes.iter().filter(|ok| **ok).count(),
        1,
        "exactly one of the two concurrent reservations for the same UTXO must win: {ra:?} / {rb:?}"
    );

    let ledger = Ledger::open(&path).unwrap();
    assert!(
        ledger.available_vault_utxos().unwrap().is_empty(),
        "the contested UTXO must be Reserved, not offered to a third payout"
    );
}

#[test]
fn mark_release_confirmed_decrements_total_reserve_balance_immediately() {
    // Regression: a real-node run against a real solana-test-validator
    // paused the reserve permanently right after a completely legitimate
    // settlement. Root cause: `total_reserve_balance` was only ever
    // refreshed by reconciliation's own periodic live read, never by the
    // settlement path itself. So the very next reconciliation after a
    // confirmed release compared a *stale* cached balance (pre-settlement)
    // against the real, already-lower on-chain balance, saw an
    // "unexplained" drop exactly equal to the amount this service itself
    // just released, and latched a one-way pause
    // (docs/05-reserve-accounting.md's never-auto-unpause design) even
    // though nothing anomalous had happened. `mark_release_confirmed` must
    // keep the cache self-consistent with settlements it causes, not leave
    // that to the next reconcile.
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();
    ledger
        .record_release_submitted(request_id, [0xCC; 64], 1_300)
        .unwrap();

    let balance_before: i64 = ledger
        .raw()
        .query_row(
            "SELECT total_reserve_balance FROM reserve_ledger WHERE direction = 'SolanaReserve'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    ledger.mark_release_confirmed(request_id, 1_400).unwrap();

    let balance_after: i64 = ledger
        .raw()
        .query_row(
            "SELECT total_reserve_balance FROM reserve_ledger WHERE direction = 'SolanaReserve'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        balance_after,
        balance_before - 100_000,
        "confirming a release must immediately decrement the cached reserve balance by the \
         settled amount, so the very next reconciliation sees a matching (not stale) baseline"
    );
}

#[test]
fn replaying_the_same_glc_observation_after_restart_is_a_no_op() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    // Indexer restarts and re-processes the same block.
    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_150)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::AlreadyRecorded);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000,
        "no double reservation"
    );
}

#[test]
fn glc_deposit_with_no_matching_request_is_never_silently_dropped() {
    let mut ledger = setup();
    let outcome = ledger
        .record_glc_deposit_observed(99999, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::NoMatchingRequest);
    ledger
        .record_unmatched_goldcoin_deposit([0xAA; 32], 0, 100_000, 10, "no_matching_request", 1_100)
        .unwrap();
    let count: i64 = ledger
        .raw()
        .query_row(
            "SELECT count(*) FROM unmatched_goldcoin_deposits",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn late_glc_deposit_after_expiry_auto_recreates_when_capacity_available() {
    // docs/04-state-machines.md "Open design item: late deposits after
    // expiry": a deposit that arrives after the reservation TTL elapsed
    // must not be treated the same as an uncorrelated payment when
    // capacity is still available — it should re-reserve and continue.
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            10,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(ledger.expire_reservations(1_020).unwrap(), 1);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        900_000,
        "expiry must have released the reservation"
    );

    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::LateDepositRecreated);

    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        req.state,
        RequestState::Confirming,
        "late deposit continues the flow normally from DepositObserved, same as an on-time one"
    );
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000,
        "capacity must be re-reserved, not double-counted or left released"
    );

    let log = ledger.state_log(request_id).unwrap();
    let transitions: Vec<(Option<RequestState>, RequestState)> =
        log.iter().map(|e| (e.0, e.1)).collect();
    assert!(transitions.contains(&(Some(RequestState::Expired), RequestState::LiquidityReserved)));
    assert!(transitions.contains(&(
        Some(RequestState::LiquidityReserved),
        RequestState::AwaitingDeposit
    )));
    assert!(transitions.contains(&(
        Some(RequestState::AwaitingDeposit),
        RequestState::DepositObserved
    )));

    // Idempotent on replay, same as an on-time deposit.
    let replay = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_150)
        .unwrap();
    assert_eq!(replay, GlcObservationOutcome::AlreadyRecorded);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000,
        "replay must not re-reserve capacity a second time"
    );
}

#[test]
fn late_glc_deposit_after_expiry_routes_to_manual_review_when_no_capacity() {
    // Same design item, other branch: if capacity is no longer available to
    // re-reserve, the real (irreversible) deposit must route to
    // ManualReview rather than being silently recorded as unmatched.
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved {
        request_id: stale_id,
    } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(900_000),
            &[1u8; 32],
            None,
            10,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(ledger.expire_reservations(1_020).unwrap(), 1);

    // A different request now consumes all the capacity the stale
    // reservation released.
    let CreateRequestOutcome::Reserved {
        request_id: other_id,
    } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(900_000),
            &[2u8; 32],
            None,
            3600,
            1_020,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        0
    );

    let outcome = ledger
        .record_glc_deposit_observed(stale_id, [0xAA; 32], 0, 900_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::LateDepositNoCapacity);

    let req = ledger.get_request(stale_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::ManualReview);
    assert!(req.manual_review_note.as_deref() == Some("late_deposit_no_capacity"));

    // The other, unrelated request is untouched.
    let other = ledger.get_request(other_id).unwrap().unwrap();
    assert_eq!(other.state, RequestState::AwaitingDeposit);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        0,
        "no capacity was fabricated for the stale request"
    );
}

#[test]
fn glc_deposit_amount_mismatch_routes_to_manual_review_not_silent_accept() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 50_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert_eq!(
        outcome,
        GlcObservationOutcome::AmountMismatch {
            expected: 100_000,
            observed: 50_000
        }
    );
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::ManualReview);
    // Reserved capacity is untouched — no release, no advancement toward
    // settlement while under review.
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000
    );
}

#[test]
fn pre_finality_reorg_clears_source_binding_and_returns_to_awaiting_deposit() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    ledger.mark_glc_reorged(request_id, 1_150).unwrap();
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::AwaitingDeposit);
    assert_eq!(req.source_txid, None);
    // Reservation itself is untouched — still live, just unbound.
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000
    );

    // A fresh observation (possibly a different mined block) can re-bind.
    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 20, [0xCC; 32], 1_200)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::Recorded);
}

#[test]
#[should_panic(expected = "post-finality")]
fn reorg_after_finality_must_never_be_called_it_is_a_caller_bug() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();
    // This must panic rather than silently reverting an irreversible claim
    // (docs/10-threat-model.md's post-finality-reorg section).
    let _ = ledger.mark_glc_reorged(request_id, 1_300);
}

// ---------------------------------------------------------------- Solana leg --

#[test]
fn sol_deposit_folds_directly_to_source_finalized_when_capacity_available() {
    let mut ledger = setup();
    let outcome = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap();
    let SolFoldOutcome::FoldedFinalized { request_id } = outcome else {
        panic!("{outcome:?}")
    };
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::SourceFinalized);
    assert_eq!(req.direction, Direction::SolToGlc);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        800_000
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

/// The production write paths record a COMPLETE source identity, not just
/// an obligation index (schema v21). This is what makes
/// `ux_bridge_requests_obligation_source` load-bearing rather than
/// vacuous: an index written without its chain and issuing contract would
/// be rejected outright by the table's CHECKs.
#[test]
fn folding_a_solana_deposit_records_the_chain_and_the_issuing_program() {
    let mut ledger = setup();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!("expected a finalized fold")
    };

    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.source_chain, SourceChain::Solana);
    assert_eq!(
        req.source_contract.as_deref(),
        Some(&glc_reserve_bridge_shared::PROGRAM_ID_BYTES[..]),
        "the obligation index is local to the program that issued it, so the deployed \
         program id travels with it"
    );
    assert_eq!(req.source_obligation_index, Some(0));
}

/// A `GlcToSol` request's source leg is Goldcoin from creation — before
/// any deposit is observed — and Goldcoin has no contract identity at all.
#[test]
fn creating_a_glc_to_sol_request_records_goldcoin_as_the_source_chain() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[9u8; 32],
            None,
            600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected a created request")
    };

    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.source_chain, SourceChain::Goldcoin);
    assert_eq!(req.source_contract, None);
    assert_eq!(req.source_obligation_index, None);
}

/// A row migrated from a pre-v21 database carries the legacy marker
/// instead of a known program id, so this ledger cannot prove it came from
/// a DIFFERENT program than the one running now. Re-observing its
/// obligation index must therefore still be refused — cleanly, as
/// `AlreadyFolded`, exactly as it was before v21 — never folded a second
/// time and never surfaced as a raw constraint error.
#[test]
fn re_observing_a_migrated_legacy_obligation_is_still_already_folded_never_double_paid() {
    let mut ledger = setup();
    // Stand in for a row the v21 migration brought across: Solana chain,
    // obligation 5, contract unknown.
    ledger
        .raw()
        .execute(
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, created_at,
                 source_chain, source_contract, source_obligation_index)
             VALUES (900, 'SolToGlc', 'SourceFinalized', 100, X'AA', 0, 'solana', ?1, 5)",
            rusqlite::params![LEGACY_SOLANA_SOURCE_CONTRACT],
        )
        .unwrap();

    let outcome = ledger
        .fold_sol_deposit(5, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap();
    assert!(
        matches!(outcome, SolFoldOutcome::AlreadyFolded { request_id: 900 }),
        "got {outcome:?}"
    );
    // Nothing new was written, and no capacity was committed.
    let n: i64 = ledger
        .raw()
        .query_row(
            "SELECT COUNT(*) FROM bridge_requests WHERE source_obligation_index = 5",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);

    // An index no legacy row holds still folds normally, and records the
    // EXACT current program id — the legacy marker never spreads to a new
    // row.
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(6, amounts(100_000), [1u8; 32], &[3u8; 32], None, 2_000)
        .unwrap()
    else {
        panic!("expected a finalized fold")
    };
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        req.source_contract.as_deref(),
        Some(&glc_reserve_bridge_shared::PROGRAM_ID_BYTES[..])
    );
    assert_ne!(
        req.source_contract.as_deref(),
        Some(LEGACY_SOLANA_SOURCE_CONTRACT)
    );
}

/// Re-observing the SAME obligation is still exactly one request — the
/// identity-qualified pre-check is behaviourally identical to the
/// index-only one it replaced, for every source that exists today.
#[test]
fn refolding_the_same_solana_obligation_is_still_idempotent() {
    let mut ledger = setup();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(7, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!("expected a finalized fold")
    };
    let again = ledger
        .fold_sol_deposit(7, amounts(100_000), [1u8; 32], &[2u8; 32], None, 2_000)
        .unwrap();
    assert!(matches!(
        again,
        SolFoldOutcome::AlreadyFolded { request_id: id } if id == request_id
    ));
}

#[test]
fn sol_deposit_beyond_capacity_is_recorded_in_manual_review_never_dropped() {
    let mut ledger = setup();
    // available is 900_000
    let outcome = ledger
        .fold_sol_deposit(0, amounts(950_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
        panic!("{outcome:?}")
    };
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::ManualReview);
    // Capacity untouched — the deposit is real (Solana-side, irreversible)
    // but does not commit reserve capacity it doesn't have.
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        900_000
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

// ------------------------------------------------------- admission control --
//
// `admission_closed` (docs/09-runbook.md "Admission control
// (Solana->Goldcoin)") is a separate axis from `paused` — see
// `Ledger::set_admission`/`is_admission_closed`. It is checked ONLY by
// `fold_sol_deposit`'s capacity_ok computation; nothing else in this crate
// reads it, and nothing in this crate ever sets it automatically.

#[test]
fn closed_admission_routes_a_new_deposit_to_manual_review_even_with_capacity_and_no_pause() {
    let mut ledger = setup();
    ledger
        .set_admission(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("operator note"),
        )
        .unwrap();
    // Proves the two flags are genuinely independent: pause is untouched
    // (still false), and there is ample capacity — admission_closed alone
    // must still be what blocks this.
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    let outcome = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
        panic!("expected admission-closed to route to ManualReview, got {outcome:?}")
    };
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::ManualReview);
    assert_eq!(
        req.manual_review_note.as_deref(),
        Some("admission_closed_at_fold")
    );
    // The deposit is real and irreversible on Solana, but must not commit
    // reserve capacity it was never granted.
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        900_000
    );
}

#[test]
fn admission_is_open_by_default_and_folding_is_unaffected() {
    let ledger_open = setup();
    assert!(!ledger_open
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

#[test]
fn pause_still_blocks_admission_independent_of_the_new_admission_flag() {
    let mut ledger = setup();
    // Existing pause logic, unchanged: admission stays open (the new
    // flag), but the pre-existing `paused` gate alone must still be
    // enough to route a new deposit to ManualReview, exactly as before
    // this feature existed.
    ledger
        .set_paused(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("reconciliation breach"),
        )
        .unwrap();
    assert!(!ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    let outcome = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
        panic!("expected paused to still route to ManualReview, got {outcome:?}")
    };
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        req.manual_review_note.as_deref(),
        Some("reserve_paused_at_fold")
    );
}

#[test]
fn closing_admission_never_touches_an_already_accepted_request() {
    let mut ledger = setup();
    // Accept a request BEFORE admission is ever closed.
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    let before = ledger.get_request(request_id).unwrap().unwrap();
    let capacity_before = ledger
        .available_capacity(ReserveDirection::GoldcoinReserve)
        .unwrap();

    ledger
        .set_admission(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("closing admission"),
        )
        .unwrap();

    // The already-accepted request and its committed capacity are
    // completely unaffected — closing admission only ever gates NEW folds.
    let after = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(after.state, before.state);
    assert_eq!(after.net_destination_atomic, before.net_destination_atomic);
    assert_eq!(after.state, RequestState::SourceFinalized);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        capacity_before
    );
}

#[test]
fn set_admission_never_reopens_automatically() {
    let mut ledger = setup();
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    // Nothing else in this crate ever calls `set_admission` — closing it
    // once must leave it closed indefinitely, with no code path that
    // implicitly reopens it. Simulate the passage of time/other ledger
    // activity and confirm it's still closed.
    ledger
        .fold_sol_deposit(9, amounts(1), [3u8; 32], &[4u8; 32], None, 2_000)
        .unwrap();
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

#[test]
fn check_invariant_fails_on_a_genuine_breach_the_same_check_open_admission_relies_on() {
    // `glc-admin open-admission` refuses unless `Ledger::check_invariant`
    // holds — this proves that check itself actually fails closed on a
    // real breach (balance below protected_minimum + reserved_liquidity),
    // not just that it passes on a healthy fixture.
    let mut ledger = setup();
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    // reserved_liquidity is now 100_000 against protected_minimum 100_000
    // -> the invariant requires balance >= 200_000. Drop the observed
    // balance below that via a live reconciliation-style refresh.
    ledger
        .refresh_reserve_balance(ReserveDirection::GoldcoinReserve, 150_000, 2_000)
        .unwrap();
    let err = ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap_err();
    assert!(matches!(err, LedgerError::InvariantViolated { .. }));
}

// ---------------------------------------------------- resume manual review --

#[test]
fn resumes_a_request_parked_by_admission_closed_and_reserves_capacity() {
    let mut ledger = setup();
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!("expected admission-closed to route to ManualReview")
    };
    let before = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(before.state, RequestState::ManualReview);
    assert_eq!(
        before.manual_review_note.as_deref(),
        Some("admission_closed_at_fold")
    );
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        900_000,
        "a parked request must not have committed capacity"
    );

    // Admission may remain CLOSED — resuming never touches it.
    let outcome = ledger
        .resume_manual_review_sol_to_glc(
            request_id,
            "operator resuming after incident",
            "operator",
            2_000,
        )
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    let after = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(after.id, before.id, "same request id preserved");
    assert_eq!(after.state, RequestState::SourceFinalized);
    assert!(after.manual_review_note.is_none());
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        800_000,
        "resuming must reserve capacity, exactly as a successful fold would have"
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

#[test]
fn resumes_a_request_parked_by_pause_even_while_still_paused() {
    let mut ledger = setup();
    ledger
        .set_paused(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("reconciliation breach"),
        )
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("reserve_paused_at_fold")
    );

    // Resuming does not require unpausing first — processing has never
    // been gated by `paused` either.
    let outcome = ledger
        .resume_manual_review_sol_to_glc(
            request_id,
            "resuming while still paused",
            "operator",
            2_000,
        )
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
    assert!(ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );
}

#[test]
fn resumes_a_request_parked_by_insufficient_capacity_once_capacity_recovers() {
    let mut ledger = setup();
    // available is 900_000 -> this exceeds it.
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(950_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("insufficient_capacity_at_fold")
    );

    // Still insufficient -> refused.
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "trying too early", "operator", 1_500)
        .unwrap_err();
    assert!(matches!(err, LedgerError::InvariantViolated { .. }));
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview,
        "a refused resume attempt must not mutate the request"
    );

    // Capacity recovers (e.g. a rebalance deposit).
    ledger
        .refresh_reserve_balance(ReserveDirection::GoldcoinReserve, 2_000_000, 1_800)
        .unwrap();
    let outcome = ledger
        .resume_manual_review_sol_to_glc(request_id, "capacity has recovered", "operator", 2_000)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
}

#[test]
fn resume_is_idempotent_and_never_double_reserves() {
    let mut ledger = setup();
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    ledger
        .resume_manual_review_sol_to_glc(request_id, "first resume", "operator", 2_000)
        .unwrap();
    let capacity_after_first = ledger
        .available_capacity(ReserveDirection::GoldcoinReserve)
        .unwrap();

    let outcome = ledger
        .resume_manual_review_sol_to_glc(request_id, "second resume attempt", "operator", 3_000)
        .unwrap();
    assert_eq!(
        outcome,
        ResumeManualReviewOutcome::AlreadyResumed {
            state: RequestState::SourceFinalized
        }
    );
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        capacity_after_first,
        "a repeat call must never reserve capacity a second time"
    );
}

#[test]
fn refuses_a_request_that_reached_source_finalized_without_ever_being_in_manual_review() {
    let mut ledger = setup();
    // Capacity is available and admission is open -> folds directly to
    // SourceFinalized, never touching ManualReview at all.
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "mistaken call", "operator", 2_000)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::ManualReviewNotRecoverable { .. }),
        "a request never in ManualReview must be refused, not reported as already-resumed: {err}"
    );
}

#[test]
fn refuses_a_glc_to_sol_request() {
    let mut ledger = setup();
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[2u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    let CreateRequestOutcome::Reserved { request_id } = outcome else {
        panic!("{outcome:?}")
    };
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "wrong direction", "operator", 2_000)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::NotASolToGlcRequest { id, .. } if id == request_id
    ));
}

#[test]
fn refuses_an_unknown_manual_review_reason() {
    let mut ledger = setup();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(950_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    // Simulate a ManualReview row parked for some unrelated reason (e.g. a
    // future code path this command was never meant to touch).
    ledger
        .conn
        .execute(
            "UPDATE bridge_requests SET manual_review_note = 'some_future_unrelated_reason' WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "should be refused", "operator", 2_000)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::ManualReviewNotRecoverable { .. }
    ));
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview
    );
}

#[test]
fn refuses_a_request_that_already_has_a_goldcoin_payout() {
    let mut ledger = setup();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(950_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    // A payout row existing at all for a ManualReview request should never
    // happen in practice, but this command must fail closed rather than
    // assume it can't.
    ledger
        .conn
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at)
             VALUES (?1, X'ab', 1, 0, 0, X'cd', 'Built', 1000)",
            [request_id],
        )
        .unwrap();
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "should be refused", "operator", 2_000)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::ManualReviewNotRecoverable { .. }
    ));
}

#[test]
fn refuses_an_unknown_request_id() {
    let mut ledger = setup();
    let err = ledger
        .resume_manual_review_sol_to_glc(999_999, "does not exist", "operator", 2_000)
        .unwrap_err();
    assert!(matches!(err, LedgerError::RequestNotFound(id) if id == 999_999));
}

#[test]
fn resume_writes_the_operator_note_to_the_audit_trail() {
    let mut ledger = setup();
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    ledger
        .resume_manual_review_sol_to_glc(
            request_id,
            "verified with ops, safe to resume",
            "operator",
            2_000,
        )
        .unwrap();
    let log = ledger.state_log(request_id).unwrap();
    let resumed_entry = log
        .iter()
        .find(|(from, to, _, _)| {
            *from == Some(RequestState::ManualReview) && *to == RequestState::SourceFinalized
        })
        .expect("expected a ManualReview -> SourceFinalized log entry");
    assert_eq!(
        resumed_entry.3.as_deref(),
        Some("verified with ops, safe to resume")
    );
}

// ---- SolToGlc recipient rate limit (docs/09-runbook.md) ----

/// Directly sets a request's `state`, bypassing every ledger safety check —
/// legitimate ONLY in tests, to reach states
/// (`DestinationSubmitted`/`Settled`/etc.) the public API has no single-call
/// path to for a bare `SolToGlc` fold without also standing up the full
/// Goldcoin payout-signing pipeline. `tests` is a descendant module of
/// `ledger`, so it may access the private `conn` field directly.
fn force_state(ledger: &mut Ledger, request_id: i64, state: RequestState) {
    ledger
        .conn
        .execute(
            "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![state, request_id],
        )
        .unwrap();
}

#[test]
fn second_deposit_to_the_same_recipient_inside_24h_is_parked_recipient_rate_limited() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap()
    else {
        panic!("first deposit to a fresh recipient must fold straight through")
    };

    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(
            1,
            amounts(50_000),
            [2u8; 32],
            &recipient,
            None,
            1_000 + 3_600,
        )
        .unwrap()
    else {
        panic!("a second deposit to the SAME recipient inside the window must be parked")
    };
    let parked = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        parked.manual_review_note.as_deref(),
        Some("wallet_destination_24h_limit")
    );
    assert_eq!(
        parked.state,
        RequestState::ManualReview,
        "rate-limited fold must never reserve capacity"
    );
}

#[test]
fn deposit_to_the_same_recipient_after_the_window_ages_out_is_accepted_normally() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap();

    // created_at(1_000) + 86_400 == 87_400: the window has fully elapsed by
    // this exact instant (strictly-greater-than in the query), so this must
    // fold straight through, not park.
    let outcome = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, None, 87_400)
        .unwrap();
    assert!(
        matches!(outcome, SolFoldOutcome::FoldedFinalized { .. }),
        "expected a normal fold once the 24h window has aged out, got {outcome:?}"
    );
}

#[test]
fn different_recipients_are_completely_independent() {
    let mut ledger = setup();
    let outcome_a = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &[1u8; 32], None, 1_000)
        .unwrap();
    let outcome_b = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &[2u8; 32], None, 1_000)
        .unwrap();
    assert!(matches!(outcome_a, SolFoldOutcome::FoldedFinalized { .. }));
    assert!(matches!(outcome_b, SolFoldOutcome::FoldedFinalized { .. }));
}

#[test]
fn replaying_the_same_obligation_after_restart_is_not_treated_as_rate_limited() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    // Simulated restart: the exact same obligation is observed again. The
    // pre-existing `source_obligation_index` idempotency check must win
    // BEFORE the rate-limit check ever runs — this must never be
    // reinterpreted as "this recipient hit its own limit."
    let outcome2 = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 2_000)
        .unwrap();
    assert_eq!(outcome2, SolFoldOutcome::AlreadyFolded { request_id });
}

#[test]
fn an_in_flight_manual_review_obligation_still_counts_against_its_recipient() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    // Parked for an UNRELATED reason (insufficient capacity), never
    // resumed — still a live obligation that can result in a payout, so it
    // must still count against this recipient.
    let SolFoldOutcome::FoldedManualReview { .. } = ledger
        .fold_sol_deposit(0, amounts(950_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap()
    else {
        panic!()
    };

    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, None, 1_000 + 10)
        .unwrap()
    else {
        panic!("a second obligation to a recipient with a live ManualReview obligation must also be parked")
    };
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("wallet_destination_24h_limit")
    );
}

#[test]
fn a_settled_obligation_still_counts_against_its_recipient_until_the_window_elapses() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    force_state(&mut ledger, request_id, RequestState::Settled);

    let outcome = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, None, 1_000 + 10)
        .unwrap();
    assert!(
        matches!(outcome, SolFoldOutcome::FoldedManualReview { .. }),
        "a fully Settled payout to this recipient is still inside the window \
         and must still count, got {outcome:?}"
    );
}

#[test]
fn a_destination_submitted_obligation_counts_against_its_recipient() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    force_state(&mut ledger, request_id, RequestState::DestinationSubmitted);

    let outcome = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, None, 1_000 + 10)
        .unwrap();
    assert!(matches!(outcome, SolFoldOutcome::FoldedManualReview { .. }));
}

#[test]
fn a_cancelled_or_failed_obligation_never_counts_against_its_recipient() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    // `Failed` is defined but never set anywhere in production code today
    // (docs/09-runbook.md) — forced directly here purely to exercise the
    // exclude-list, defensively, in case that ever changes.
    force_state(&mut ledger, request_id, RequestState::Failed);

    let outcome = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, None, 1_000 + 10)
        .unwrap();
    assert!(
        matches!(outcome, SolFoldOutcome::FoldedFinalized { .. }),
        "a Failed request must never count against its recipient, got {outcome:?}"
    );
}

#[test]
fn manual_resume_refuses_while_the_recipient_is_still_inside_the_window() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    // First obligation: settles the window.
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap();
    // Second obligation to the same recipient: parked recipient_rate_limited.
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, None, 1_000 + 10)
        .unwrap()
    else {
        panic!()
    };

    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "trying too early", "operator", 1_000 + 20)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::WalletWindowActive { request_id: rid, role: WalletRole::Destination, .. } if rid == request_id
    ));
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview,
        "a refused resume attempt must not mutate the request"
    );

    // Once the FIRST request's window has aged out, the resume succeeds
    // normally — self-clearing, no operator override needed.
    let outcome = ledger
        .resume_manual_review_sol_to_glc(request_id, "window has elapsed", "operator", 87_401)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
}

#[test]
fn manual_resume_checks_the_window_unconditionally_even_when_parked_for_a_different_reason() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    // A live obligation to this recipient, still within its own window.
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap();

    // A second obligation to the SAME recipient, but parked for a
    // completely different, unrelated reason (admission closed).
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, None, 1_000 + 10)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("admission_closed_at_fold")
    );

    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopening"))
        .unwrap();
    // Admission is open again, but the recipient's window (from the FIRST
    // request) has not elapsed yet — the resume must still be refused,
    // proving the window check is unconditional, not gated on this
    // request's own `manual_review_note`.
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "admission reopened", "operator", 1_000 + 20)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::WalletWindowActive {
            role: WalletRole::Destination,
            ..
        }
    ));
}

#[test]
fn manual_resume_self_excludes_so_a_request_never_blocks_its_own_resume() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap()
    else {
        panic!("parked for admission_closed, not rate limiting")
    };
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopening"))
        .unwrap();
    // With no OTHER request to this recipient, the rate-limit re-check must
    // never treat this request's own row as a blocker of itself.
    let outcome = ledger
        .resume_manual_review_sol_to_glc(request_id, "admission reopened", "operator", 1_500)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
}

#[test]
fn a_second_glc_to_sol_request_to_the_same_solana_recipient_inside_24h_is_refused() {
    let mut ledger = setup();
    // The rolling-24h destination window applies on `GlcToSol` too
    // (`ledger::wallet_window`): the second request to one Solana pubkey
    // inside a day is refused BEFORE any capacity is reserved — nothing
    // is on-chain yet, so a refusal (not a park) is the right shape.
    let outcome_a = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(50_000),
            &[7u8; 32],
            None,
            3_600,
            1_000,
        )
        .unwrap();
    let CreateRequestOutcome::Reserved { .. } = outcome_a else {
        panic!("the first request to a fresh recipient must be reserved, got {outcome_a:?}")
    };
    let reserved_before: i64 = ledger
        .conn_for_tests()
        .query_row(
            "SELECT reserved_liquidity FROM reserve_ledger WHERE direction = 'SolanaReserve'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let outcome_b = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(50_000),
            &[7u8; 32],
            None,
            3_600,
            1_010,
        )
        .unwrap();
    let CreateRequestOutcome::WalletLimited { eligibility } = outcome_b else {
        panic!(
            "a second request to the SAME recipient inside 24h must be refused, got {outcome_b:?}"
        )
    };
    assert_eq!(eligibility.destination_retry_after, Some(1_000 + 86_400));
    assert_eq!(eligibility.source_retry_after, None);
    assert_eq!(
        eligibility.manual_review_note(),
        Some("wallet_destination_24h_limit")
    );
    let reserved_after: i64 = ledger
        .conn_for_tests()
        .query_row(
            "SELECT reserved_liquidity FROM reserve_ledger WHERE direction = 'SolanaReserve'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        reserved_after, reserved_before,
        "a refusal reserves nothing"
    );
    let rows: i64 = ledger
        .conn_for_tests()
        .query_row("SELECT COUNT(*) FROM bridge_requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1, "a refusal leaves no row behind");

    // A DIFFERENT recipient at the same instant is unaffected.
    let outcome_c = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(50_000),
            &[8u8; 32],
            None,
            3_600,
            1_010,
        )
        .unwrap();
    assert!(matches!(outcome_c, CreateRequestOutcome::Reserved { .. }));
}

// ---- Solana-source-wallet rate limit (dual key alongside the recipient one) --
//
// Mirrors the recipient-rate-limit tests above exactly (same window, same
// state exclude-list, same strict-predecessor resume semantics) — see
// `Ledger::source_wallet_rate_limit_blocker_created_at`'s doc comment for
// why the two are deliberately near-identical, keyed on `requester`
// instead of `recipient`. Additional tests here cover the two limits'
// INDEPENDENCE from each other (same wallet/different recipient and
// different wallet/same recipient must each still block, on their own).

#[test]
fn second_deposit_from_the_same_wallet_to_a_different_recipient_is_parked_source_wallet_rate_limited(
) {
    let mut ledger = setup();
    let wallet = [7u8; 32];
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(0, amounts(50_000), wallet, &[1u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!("first deposit from a fresh wallet must fold straight through")
    };

    // Same wallet, but a DIFFERENT recipient — the recipient-only rule
    // would admit this; the source-wallet rule must still block it, this
    // is exactly the production bypass being closed.
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(1, amounts(50_000), wallet, &[2u8; 32], None, 1_000 + 3_600)
        .unwrap()
    else {
        panic!("a second deposit from the SAME wallet inside the window must be parked, even to a different recipient")
    };
    let parked = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        parked.manual_review_note.as_deref(),
        Some("wallet_source_24h_limit")
    );
    assert_eq!(parked.state, RequestState::ManualReview);
}

#[test]
fn a_different_wallet_to_the_same_recipient_is_still_blocked_by_the_recipient_rule() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap();

    // A DIFFERENT wallet, same recipient — the source-wallet rule alone
    // would admit this (this wallet has no history), but the pre-existing
    // recipient rule must still block it, unchanged.
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, None, 1_000 + 10)
        .unwrap()
    else {
        panic!("a different wallet to the SAME recipient inside the window must still be parked")
    };
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("wallet_destination_24h_limit")
    );
}

#[test]
fn a_different_wallet_and_a_different_recipient_is_completely_unaffected() {
    let mut ledger = setup();
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &[1u8; 32], None, 1_000)
        .unwrap();

    let outcome = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &[2u8; 32], None, 1_000 + 10)
        .unwrap();
    assert!(
        matches!(outcome, SolFoldOutcome::FoldedFinalized { .. }),
        "a fresh wallet to a fresh recipient must never be blocked by either limit, got {outcome:?}"
    );
}

#[test]
fn deposit_from_the_same_wallet_after_the_window_ages_out_is_accepted_normally() {
    let mut ledger = setup();
    let wallet = [7u8; 32];
    ledger
        .fold_sol_deposit(0, amounts(50_000), wallet, &[1u8; 32], None, 1_000)
        .unwrap();

    // created_at(1_000) + 86_400 == 87_400: the window has fully elapsed by
    // this exact instant (strictly-greater-than in the query), same
    // boundary semantics as the recipient limiter.
    let outcome = ledger
        .fold_sol_deposit(1, amounts(50_000), wallet, &[2u8; 32], None, 87_400)
        .unwrap();
    assert!(
        matches!(outcome, SolFoldOutcome::FoldedFinalized { .. }),
        "expected a normal fold once the 24h window has aged out, got {outcome:?}"
    );
}

#[test]
fn manual_resume_refuses_while_the_source_wallet_is_still_inside_the_window() {
    let mut ledger = setup();
    let wallet = [7u8; 32];
    ledger
        .fold_sol_deposit(0, amounts(50_000), wallet, &[1u8; 32], None, 1_000)
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(1, amounts(50_000), wallet, &[2u8; 32], None, 1_000 + 10)
        .unwrap()
    else {
        panic!()
    };

    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "trying too early", "operator", 1_000 + 20)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::WalletWindowActive { request_id: rid, role: WalletRole::Source, .. } if rid == request_id
    ));
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview,
        "a refused resume attempt must not mutate the request"
    );

    // Once the FIRST request's window has aged out, the resume succeeds.
    let outcome = ledger
        .resume_manual_review_sol_to_glc(request_id, "window has elapsed", "operator", 87_401)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
}

#[test]
fn manual_resume_checks_the_source_wallet_window_unconditionally_even_when_parked_for_a_different_reason(
) {
    let mut ledger = setup();
    let wallet = [7u8; 32];
    // A live obligation from this wallet, still within its own window.
    ledger
        .fold_sol_deposit(0, amounts(50_000), wallet, &[1u8; 32], None, 1_000)
        .unwrap();

    // A second obligation from the SAME wallet, but parked for a
    // completely different, unrelated reason (admission closed).
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(1, amounts(50_000), wallet, &[2u8; 32], None, 1_000 + 10)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("admission_closed_at_fold")
    );

    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopening"))
        .unwrap();
    // Admission is open again, but this wallet's window (from the FIRST
    // request) has not elapsed yet — the resume must still be refused,
    // proving the window check is unconditional, not gated on this
    // request's own `manual_review_note`.
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "admission reopened", "operator", 1_000 + 20)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::WalletWindowActive {
            role: WalletRole::Source,
            ..
        }
    ));
}

#[test]
fn manual_resume_self_excludes_the_source_wallet_check_too() {
    let mut ledger = setup();
    let wallet = [7u8; 32];
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), wallet, &[1u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!("parked for admission_closed, not rate limiting")
    };
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopening"))
        .unwrap();
    // With no OTHER request from this wallet, the rate-limit re-check must
    // never treat this request's own row as a blocker of itself.
    let outcome = ledger
        .resume_manual_review_sol_to_glc(request_id, "admission reopened", "operator", 1_500)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
}

#[test]
fn resuming_manually_never_bypasses_either_independent_limit() {
    // A single, combined regression covering the task's core requirement:
    // "manual resume must not bypass either timer" — parks one request
    // blocked by EACH limit and confirms both refuse a manual resume
    // attempt independently, in the same ledger, at the same instant.
    let mut ledger = setup();
    let wallet = [7u8; 32];
    let recipient = [9u8; 32];

    // Blocks future SolToGlc admissions from `wallet` for 24h.
    ledger
        .fold_sol_deposit(0, amounts(50_000), wallet, &[100u8; 32], None, 1_000)
        .unwrap();
    // Blocks future SolToGlc admissions to `recipient` for 24h.
    ledger
        .fold_sol_deposit(1, amounts(50_000), [200u8; 32], &recipient, None, 1_000)
        .unwrap();

    // Same wallet, different (fresh) recipient: parked by the wallet rule.
    let SolFoldOutcome::FoldedManualReview {
        request_id: wallet_blocked,
    } = ledger
        .fold_sol_deposit(2, amounts(50_000), wallet, &[101u8; 32], None, 1_000 + 10)
        .unwrap()
    else {
        panic!()
    };
    // Fresh wallet, same recipient: parked by the recipient rule.
    let SolFoldOutcome::FoldedManualReview {
        request_id: recipient_blocked,
    } = ledger
        .fold_sol_deposit(
            3,
            amounts(50_000),
            [201u8; 32],
            &recipient,
            None,
            1_000 + 10,
        )
        .unwrap()
    else {
        panic!()
    };

    let err_a = ledger
        .resume_manual_review_sol_to_glc(wallet_blocked, "too early", "operator", 1_000 + 20)
        .unwrap_err();
    assert!(matches!(
        err_a,
        LedgerError::WalletWindowActive {
            role: WalletRole::Source,
            ..
        }
    ));

    let err_b = ledger
        .resume_manual_review_sol_to_glc(recipient_blocked, "too early", "operator", 1_000 + 20)
        .unwrap_err();
    assert!(matches!(
        err_b,
        LedgerError::WalletWindowActive {
            role: WalletRole::Destination,
            ..
        }
    ));
}

#[test]
fn auto_resume_style_repeated_folds_never_create_a_second_row_for_one_obligation() {
    // A direct-admission "bypass attempt": replaying the exact same
    // on-chain obligation index (as `solana::indexer` would after a
    // restart, or as a malicious replay would) must hit the existing
    // `source_obligation_index` idempotency guard BEFORE either rate
    // limit is ever consulted — never silently accepted as a second,
    // distinct request.
    let mut ledger = setup();
    let wallet = [7u8; 32];
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), wallet, &[1u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    let replay = ledger
        .fold_sol_deposit(0, amounts(50_000), wallet, &[1u8; 32], None, 2_000)
        .unwrap();
    assert_eq!(replay, SolFoldOutcome::AlreadyFolded { request_id });
}

#[test]
fn a_cancelled_or_failed_obligation_never_counts_against_its_source_wallet() {
    let mut ledger = setup();
    let wallet = [7u8; 32];
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), wallet, &[1u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    force_state(&mut ledger, request_id, RequestState::Failed);

    let outcome = ledger
        .fold_sol_deposit(1, amounts(50_000), wallet, &[2u8; 32], None, 1_000 + 10)
        .unwrap();
    assert!(
        matches!(outcome, SolFoldOutcome::FoldedFinalized { .. }),
        "a Failed request must never count against its source wallet, got {outcome:?}"
    );
}

#[test]
fn a_declared_glc_to_sol_source_wallet_consumes_its_window_from_admission() {
    let mut ledger = setup();
    // A caller that declares the Goldcoin address it will fund from has
    // that address's window checked and consumed at creation — the
    // second request declaring the same source inside a day is refused,
    // even to a different recipient, and even though no deposit exists.
    let source = b"QdeclaredGoldcoinSourceAddress".to_vec();
    let outcome_a = ledger
        .create_request_from(
            Direction::GlcToSol,
            amounts(50_000),
            &[7u8; 32],
            None,
            Some(&source),
            3_600,
            1_000,
        )
        .unwrap();
    let CreateRequestOutcome::Reserved { request_id } = outcome_a else {
        panic!("{outcome_a:?}")
    };
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .source_wallet,
        Some(source.clone()),
        "the declared source is recorded on the row"
    );
    let outcome_b = ledger
        .create_request_from(
            Direction::GlcToSol,
            amounts(50_000),
            &[8u8; 32],
            None,
            Some(&source),
            3_600,
            1_010,
        )
        .unwrap();
    let CreateRequestOutcome::WalletLimited { eligibility } = outcome_b else {
        panic!("{outcome_b:?}")
    };
    assert_eq!(eligibility.source_retry_after, Some(1_000 + 86_400));
    assert_eq!(
        eligibility.manual_review_note(),
        Some("wallet_source_24h_limit")
    );

    // A request that declares nothing is not source-checked here (the
    // deposit observation is where the real funding wallet is enforced).
    let outcome_c = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(50_000),
            &[9u8; 32],
            None,
            3_600,
            1_010,
        )
        .unwrap();
    assert!(matches!(outcome_c, CreateRequestOutcome::Reserved { .. }));
}

/// Configures a `setup()`-equivalent reserve on a file-backed `Ledger` at
/// `path` — needed wherever a test must simulate a restart (`setup()`
/// itself is in-memory and cannot survive being dropped and reopened).
fn setup_at(path: &std::path::Path) -> Ledger {
    let mut ledger = Ledger::open(path).unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            1_000_000,
            100_000,
            500_000,
            200_000,
            150_000,
            1_000,
        )
        .unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            1_000_000,
            100_000,
            500_000,
            200_000,
            150_000,
            1_000,
        )
        .unwrap();
    ledger
}

/// Regression coverage for a real HIGH-severity finding: the resume-time
/// rate-limit check originally considered ANY other qualifying row to the
/// recipient as a potential blocker — including ones created AFTER the
/// candidate being resumed. For a recipient with 3+ queued rows, this let
/// a later-arriving (and itself still-parked) sibling shadow-block an
/// earlier, rightfully-next-in-line candidate, inverting oldest-first
/// draining. Fixed by restricting the blocker search to strict
/// predecessors — rows ordered `(created_at, id)` before the candidate's
/// own. These four tests exercise that fix directly.
///
/// Sets up A (accepted, anchors the window), B and C (parked, both
/// blocked at fold time). Returns their ids in creation order.
fn setup_three_requests_same_recipient(
    ledger: &mut Ledger,
    recipient: [u8; 32],
) -> (i64, i64, i64) {
    let SolFoldOutcome::FoldedFinalized { request_id: a } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap()
    else {
        panic!("A must fold straight through to establish the window")
    };
    let SolFoldOutcome::FoldedManualReview { request_id: b } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, None, 1_050)
        .unwrap()
    else {
        panic!("B must park, blocked by A")
    };
    let SolFoldOutcome::FoldedManualReview { request_id: c } = ledger
        .fold_sol_deposit(2, amounts(50_000), [3u8; 32], &recipient, None, 1_100)
        .unwrap()
    else {
        panic!("C must park too")
    };
    (a, b, c)
}

#[test]
fn oldest_first_ordering_holds_for_three_queued_requests_to_the_same_recipient() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let (_a, b, c) = setup_three_requests_same_recipient(&mut ledger, recipient);

    // now = 87_401: just past A's window (1_000 + 86_400 = 87_400).
    // B's only possible blocker is A (C is a later sibling and must be
    // structurally ineligible to block B at all) — B must resume.
    let now = 87_401;
    let b_outcome = ledger
        .resume_manual_review_sol_to_glc(b, "b turn", "operator", now)
        .unwrap();
    assert_eq!(
        b_outcome,
        ResumeManualReviewOutcome::Resumed,
        "B must resume once A's window clears"
    );

    // C's only possible blocker is B. B's own window (1_050 + 86_400 =
    // 87_450) has not elapsed yet at now = 87_401, so C must still be
    // refused — even though B itself JUST resumed in this same instant.
    let c_err = ledger
        .resume_manual_review_sol_to_glc(c, "c too early", "operator", now)
        .unwrap_err();
    assert!(
        matches!(
            c_err,
            LedgerError::WalletWindowActive {
                role: WalletRole::Destination,
                ..
            }
        ),
        "C must remain blocked by B until B's OWN window elapses, got {c_err:?}"
    );
    assert_eq!(
        ledger.get_request(c).unwrap().unwrap().state,
        RequestState::ManualReview
    );
}

#[test]
fn c_remains_blocked_until_bs_own_24h_window_expires_then_resumes() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let (_a, b, c) = setup_three_requests_same_recipient(&mut ledger, recipient);

    ledger
        .resume_manual_review_sol_to_glc(b, "b turn", "operator", 87_401)
        .unwrap();

    // Still inside B's window (1_050 + 86_400 = 87_450 is the exact
    // instant it elapses — one second before, it must still block).
    let err = ledger
        .resume_manual_review_sol_to_glc(c, "still too early", "operator", 87_449)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::WalletWindowActive {
            role: WalletRole::Destination,
            ..
        }
    ));

    // B's window has now fully elapsed (strictly-greater-than semantics:
    // at exactly created_at + 86_400 the window has already elapsed, same
    // boundary convention as every other rate-limit check in this file).
    let outcome = ledger
        .resume_manual_review_sol_to_glc(c, "b's window elapsed", "operator", 87_450)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
}

#[test]
fn continuous_newer_arrivals_can_never_starve_the_oldest_parked_request() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    let SolFoldOutcome::FoldedManualReview {
        request_id: oldest_parked,
    } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, None, 1_010)
        .unwrap()
    else {
        panic!()
    };

    // A steady trickle of NEW obligations to the SAME recipient, each
    // arriving shortly after the last — every one of them necessarily
    // parks too (the recipient is still within its own rolling window at
    // each arrival), continuously "renewing" admission-time rate limiting
    // for brand-new deposits. None of this may ever affect
    // `oldest_parked`'s own eligibility.
    for i in 2..30u64 {
        let outcome = ledger
            .fold_sol_deposit(
                i,
                amounts(50_000),
                [3u8; 32],
                &recipient,
                None,
                1_010 + (i as i64) * 10,
            )
            .unwrap();
        assert!(
            matches!(outcome, SolFoldOutcome::FoldedManualReview { .. }),
            "obligation {i}: expected a park (still inside the rolling window), got {outcome:?}"
        );
    }

    // `oldest_parked`'s only possible blocker is the very first
    // (accepted) request at created_at=1_000 — none of the 28 later
    // arrivals above may count, no matter how many piled up behind it.
    let outcome = ledger
        .resume_manual_review_sol_to_glc(
            oldest_parked,
            "unblocked by predecessor alone",
            "operator",
            87_401,
        )
        .unwrap();
    assert_eq!(
        outcome,
        ResumeManualReviewOutcome::Resumed,
        "a flood of newer same-recipient arrivals must never starve the oldest parked request"
    );
}

#[test]
fn restart_preserves_oldest_first_ordering_for_the_same_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let recipient = [9u8; 32];
    let (b, c) = {
        let mut ledger = setup_at(&path);
        let (_a, b, c) = setup_three_requests_same_recipient(&mut ledger, recipient);
        ledger
            .resume_manual_review_sol_to_glc(b, "b turn", "operator", 87_401)
            .unwrap();
        (b, c)
    };
    // Simulated restart: a brand-new `Ledger` handle over the same
    // on-disk database.
    let mut restarted = Ledger::open(&path).unwrap();
    assert_eq!(
        restarted.get_request(b).unwrap().unwrap().state,
        RequestState::SourceFinalized,
        "B's resume must have survived the restart"
    );

    // C must still be exactly as blocked by B (created_at=1_050) as it
    // was before the restart — ordering is a pure function of persisted
    // `created_at`/`id` values, not in-memory state.
    let err = restarted
        .resume_manual_review_sol_to_glc(c, "too early, post-restart", "operator", 87_449)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::WalletWindowActive {
            role: WalletRole::Destination,
            ..
        }
    ));

    let outcome = restarted
        .resume_manual_review_sol_to_glc(c, "b's window elapsed, post-restart", "operator", 87_450)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
}

#[test]
fn replaying_the_same_obligation_index_after_restart_is_a_no_op() {
    let mut ledger = setup();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(5, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    let outcome2 = ledger
        .fold_sol_deposit(5, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_050)
        .unwrap();
    assert_eq!(outcome2, SolFoldOutcome::AlreadyFolded { request_id });
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        800_000,
        "no double reservation"
    );
}

// The read-only view (`goldcoin_recipient_rate_limited_until`) the API's
// eligibility endpoint serves: it must answer exactly what
// `fold_sol_deposit` would decide for the next obligation naming these
// bytes — same shared query, so these tests pin the pairing from the
// read side.

#[test]
fn eligibility_view_reports_an_unused_recipient_as_not_rate_limited() {
    let ledger = setup();
    assert_eq!(
        ledger
            .goldcoin_recipient_rate_limited_until(&[9u8; 32], 1_000)
            .unwrap(),
        None
    );
}

#[test]
fn eligibility_view_reports_a_recently_paid_recipient_with_the_exact_reopen_time() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap();
    assert_eq!(
        ledger
            .goldcoin_recipient_rate_limited_until(&recipient, 1_000 + 3_600)
            .unwrap(),
        Some(1_000 + 86_400),
        "retry_after must be the blocking fold's created_at plus the 24h window"
    );
    // And a fold attempted now really would be parked — the view and the
    // authoritative admission check must agree.
    let SolFoldOutcome::FoldedManualReview { .. } = ledger
        .fold_sol_deposit(
            1,
            amounts(50_000),
            [2u8; 32],
            &recipient,
            None,
            1_000 + 3_600,
        )
        .unwrap()
    else {
        panic!("fold must park exactly when the view says rate-limited")
    };
}

#[test]
fn eligibility_view_clears_once_the_24h_window_has_elapsed() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap();
    // One second before the boundary: still blocked (`created_at > now -
    // window` — strictly-inside comparison).
    assert!(ledger
        .goldcoin_recipient_rate_limited_until(&recipient, 1_000 + 86_399)
        .unwrap()
        .is_some());
    // At exactly `created_at + window` — the very `retry_after` instant
    // reported above — the row no longer qualifies: retry_after is the
    // FIRST eligible second, not the last blocked one.
    assert_eq!(
        ledger
            .goldcoin_recipient_rate_limited_until(&recipient, 1_000 + 86_400)
            .unwrap(),
        None
    );
    // And the authoritative fold agrees: accepted normally.
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(
            1,
            amounts(50_000),
            [2u8; 32],
            &recipient,
            None,
            1_000 + 86_400,
        )
        .unwrap()
    else {
        panic!("fold must admit exactly when the view says eligible")
    };
}

#[test]
fn eligibility_view_is_per_recipient_a_different_address_is_unaffected() {
    let mut ledger = setup();
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &[9u8; 32], None, 1_000)
        .unwrap();
    assert_eq!(
        ledger
            .goldcoin_recipient_rate_limited_until(&[10u8; 32], 1_000 + 10)
            .unwrap(),
        None,
        "another recipient's payout must never rate-limit this one"
    );
}

#[test]
fn eligibility_view_counts_a_parked_manual_review_obligation_like_fold_does() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    // Oversized -> parked ManualReview, never paid — but it still counts
    // against the recipient, exactly as fold_sol_deposit counts it.
    ledger
        .fold_sol_deposit(0, amounts(950_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap();
    assert_eq!(
        ledger
            .goldcoin_recipient_rate_limited_until(&recipient, 1_000 + 10)
            .unwrap(),
        Some(1_000 + 86_400)
    );
}

#[test]
fn eligibility_view_ignores_terminal_never_paid_states_like_fold_does() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    force_state(&mut ledger, request_id, RequestState::Failed);
    assert_eq!(
        ledger
            .goldcoin_recipient_rate_limited_until(&recipient, 1_000 + 10)
            .unwrap(),
        None,
        "a Failed request produced no payout and must not block the recipient"
    );
}

// The read-only view (`sol_to_glc_source_wallet_rate_limited_until`) the
// API's eligibility endpoint serves for the source-wallet leg — same
// pairing discipline as the recipient view above.

#[test]
fn source_wallet_eligibility_view_reports_an_unused_wallet_as_not_rate_limited() {
    let ledger = setup();
    assert_eq!(
        ledger
            .sol_to_glc_source_wallet_rate_limited_until(&[9u8; 32], 1_000)
            .unwrap(),
        None
    );
}

#[test]
fn source_wallet_eligibility_view_reports_a_recent_deposit_with_the_exact_reopen_time() {
    let mut ledger = setup();
    let wallet = [9u8; 32];
    ledger
        .fold_sol_deposit(0, amounts(50_000), wallet, &[1u8; 32], None, 1_000)
        .unwrap();
    assert_eq!(
        ledger
            .sol_to_glc_source_wallet_rate_limited_until(&wallet, 1_000 + 3_600)
            .unwrap(),
        Some(1_000 + 86_400),
        "retry_after must be the blocking fold's created_at plus the 24h window"
    );
    // And a fold attempted now really would be parked — the view and the
    // authoritative admission check must agree.
    let SolFoldOutcome::FoldedManualReview { .. } = ledger
        .fold_sol_deposit(1, amounts(50_000), wallet, &[2u8; 32], None, 1_000 + 3_600)
        .unwrap()
    else {
        panic!("fold must park exactly when the view says rate-limited")
    };
}

#[test]
fn source_wallet_eligibility_view_clears_once_the_24h_window_has_elapsed() {
    let mut ledger = setup();
    let wallet = [9u8; 32];
    ledger
        .fold_sol_deposit(0, amounts(50_000), wallet, &[1u8; 32], None, 1_000)
        .unwrap();
    assert!(ledger
        .sol_to_glc_source_wallet_rate_limited_until(&wallet, 1_000 + 86_399)
        .unwrap()
        .is_some());
    assert_eq!(
        ledger
            .sol_to_glc_source_wallet_rate_limited_until(&wallet, 1_000 + 86_400)
            .unwrap(),
        None
    );
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(1, amounts(50_000), wallet, &[2u8; 32], None, 1_000 + 86_400)
        .unwrap()
    else {
        panic!("fold must admit exactly when the view says eligible")
    };
}

#[test]
fn source_wallet_eligibility_view_is_per_wallet_a_different_wallet_is_unaffected() {
    let mut ledger = setup();
    ledger
        .fold_sol_deposit(0, amounts(50_000), [9u8; 32], &[1u8; 32], None, 1_000)
        .unwrap();
    assert_eq!(
        ledger
            .sol_to_glc_source_wallet_rate_limited_until(&[10u8; 32], 1_000 + 10)
            .unwrap(),
        None,
        "another wallet's deposit must never rate-limit this one"
    );
}

#[test]
fn sol_indexer_progress_cursor_persists() {
    let mut ledger = setup();
    assert_eq!(ledger.last_synced_obligation_count().unwrap(), 0);
    ledger
        .set_last_synced_obligation_count(7, 12345, 1_000)
        .unwrap();
    assert_eq!(ledger.last_synced_obligation_count().unwrap(), 7);
}

#[test]
fn state_log_records_every_transition_in_order() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();
    let log = ledger.state_log(request_id).unwrap();
    let to_states: Vec<RequestState> = log.iter().map(|(_, to, _, _)| *to).collect();
    assert_eq!(
        to_states,
        vec![
            RequestState::LiquidityReserved,
            RequestState::AwaitingDeposit,
            RequestState::DepositObserved,
            RequestState::Confirming,
            RequestState::SourceFinalized,
        ]
    );
}

// -------------------------------------------------------------- rebalancing --

#[test]
fn rebalance_full_lifecycle_deposit_increases_balance_only_after_confirmed() {
    let mut ledger = setup();
    let before = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;

    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            50_000,
            "quarterly top-up",
            "ops-alice",
            2,
            1_000,
        )
        .unwrap();
    let rb = ledger.get_rebalance(id).unwrap().unwrap();
    assert_eq!(rb.state, RebalanceState::Proposed);
    assert_eq!(rb.amount_atomic, 50_000);

    // Below threshold: still Proposed.
    let outcome = ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    assert_eq!(
        outcome,
        RebalanceApprovalOutcome::Recorded {
            approvals: 1,
            required: 2
        }
    );
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Proposed
    );

    // Threshold reached: Approved.
    let outcome = ledger.approve_rebalance(id, "ops-bob", 1_002).unwrap();
    assert_eq!(outcome, RebalanceApprovalOutcome::ThresholdReached);
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Approved
    );

    // Balance untouched by proposal/approval alone.
    let mid = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    assert_eq!(
        mid, before,
        "no balance change before execution+confirmation"
    );

    ledger
        .record_rebalance_executed(id, "solana-sig-abc123", "ops-alice", 1_003)
        .unwrap();
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Executed
    );
    // Still untouched: executed only records evidence, not the effect.
    let still_mid = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    assert_eq!(still_mid, before);

    ledger
        .confirm_rebalance(id, 50_000, "ops-alice", 1_004)
        .unwrap();
    let rb = ledger.get_rebalance(id).unwrap().unwrap();
    assert_eq!(rb.state, RebalanceState::Confirmed);
    assert_eq!(rb.observed_amount_atomic, Some(50_000));

    let after = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    assert_eq!(
        after,
        before + 50_000,
        "a confirmed Deposit increases total_reserve_balance"
    );
}

#[test]
fn rebalance_withdraw_decreases_balance_only_after_confirmed() {
    let mut ledger = setup();
    let before = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Withdraw,
            10_000,
            "sweep surplus to cold storage",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    ledger
        .record_rebalance_executed(id, "solana-sig-withdraw-1", "ops-alice", 1_002)
        .unwrap();
    ledger
        .confirm_rebalance(id, 10_000, "ops-alice", 1_003)
        .unwrap();
    let after = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    assert_eq!(after, before - 10_000);
}

#[test]
fn rebalance_never_touches_reserved_liquidity_pending_obligations_or_bridge_requests() {
    let mut ledger = setup();
    let (_, _, reserved_before, pending_before) = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .unwrap();
    let requests_before = ledger
        .requests_by_state(Direction::GlcToSol, RequestState::AwaitingDeposit)
        .unwrap()
        .len();

    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            50_000,
            "top-up",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    ledger
        .record_rebalance_executed(id, "solana-sig-structural", "ops-alice", 1_002)
        .unwrap();
    ledger
        .confirm_rebalance(id, 50_000, "ops-alice", 1_003)
        .unwrap();

    let (_, _, reserved_after, pending_after) = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .unwrap();
    assert_eq!(reserved_before, reserved_after);
    assert_eq!(pending_before, pending_after);
    let requests_after = ledger
        .requests_by_state(Direction::GlcToSol, RequestState::AwaitingDeposit)
        .unwrap()
        .len();
    assert_eq!(requests_before, requests_after);
}

#[test]
fn rebalance_approving_twice_from_the_same_identity_does_not_double_count() {
    let mut ledger = setup();
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            50_000,
            "top-up",
            "ops-alice",
            2,
            1_000,
        )
        .unwrap();
    let o1 = ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    let o2 = ledger.approve_rebalance(id, "ops-alice", 1_002).unwrap();
    assert_eq!(
        o1, o2,
        "the same approver approving twice must not move the count"
    );
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Proposed,
        "still short of the real second distinct approver"
    );
}

#[test]
fn rebalance_duplicate_tx_reference_is_rejected_structurally() {
    let mut ledger = setup();
    let id1 = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up 1",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id1, "ops-alice", 1_001).unwrap();
    ledger
        .record_rebalance_executed(id1, "solana-sig-replay-target", "ops-alice", 1_002)
        .unwrap();

    let id2 = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            20_000,
            "top-up 2",
            "ops-alice",
            1,
            1_003,
        )
        .unwrap();
    ledger.approve_rebalance(id2, "ops-alice", 1_004).unwrap();
    let result =
        ledger.record_rebalance_executed(id2, "solana-sig-replay-target", "ops-alice", 1_005);
    assert!(
        result.is_err(),
        "the same real tx_reference must never be recorded against two rebalance requests"
    );
    // The first request is untouched by the rejected second attempt.
    assert_eq!(
        ledger.get_rebalance(id1).unwrap().unwrap().state,
        RebalanceState::Executed
    );
    assert_eq!(
        ledger.get_rebalance(id2).unwrap().unwrap().state,
        RebalanceState::Approved,
        "the second request must not be left partially executed by the rejected attempt"
    );
}

#[test]
fn rebalance_wrong_state_transitions_are_rejected() {
    let mut ledger = setup();
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();

    // Cannot execute before Approved.
    let result = ledger.record_rebalance_executed(id, "sig-1", "ops-alice", 1_001);
    assert!(matches!(
        result,
        Err(LedgerError::RebalanceWrongState { .. })
    ));

    // Cannot confirm before Executed.
    let result = ledger.confirm_rebalance(id, 10_000, "ops-alice", 1_001);
    assert!(matches!(
        result,
        Err(LedgerError::RebalanceWrongState { .. })
    ));

    ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    // Cannot approve again once Approved.
    let result = ledger.approve_rebalance(id, "ops-bob", 1_002);
    assert!(matches!(
        result,
        Err(LedgerError::RebalanceWrongState { .. })
    ));
}

#[test]
fn rebalance_reject_and_cancel_require_a_note_and_are_terminal() {
    let mut ledger = setup();
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up",
            "ops-alice",
            2,
            1_000,
        )
        .unwrap();
    assert!(ledger.reject_rebalance(id, "", "ops-bob", 1_001).is_err());
    ledger
        .reject_rebalance(id, "not needed right now", "ops-bob", 1_001)
        .unwrap();
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Rejected
    );
    // Terminal: cannot approve a rejected request.
    assert!(ledger.approve_rebalance(id, "ops-alice", 1_002).is_err());

    let id2 = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id2, "ops-alice", 1_001).unwrap();
    ledger
        .cancel_rebalance(id2, "plans changed", "ops-alice", 1_002)
        .unwrap();
    assert_eq!(
        ledger.get_rebalance(id2).unwrap().unwrap().state,
        RebalanceState::Cancelled
    );
}

#[test]
fn rebalance_fail_routes_to_manual_resolution_without_touching_balance() {
    let mut ledger = setup();
    let before = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    ledger
        .record_rebalance_executed(id, "sig-never-confirmed", "ops-alice", 1_002)
        .unwrap();
    ledger
        .fail_rebalance(
            id,
            "transaction never confirmed on-chain",
            "ops-alice",
            1_003,
        )
        .unwrap();
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Failed
    );
    let after = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    assert_eq!(
        after, before,
        "a Failed rebalance must never adjust the cached balance"
    );
}

#[test]
fn rebalance_list_filters_by_direction_and_open_state() {
    let mut ledger = setup();
    let goldcoin_id = ledger
        .propose_rebalance(
            ReserveDirection::GoldcoinReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up goldcoin",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    let solana_id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            20_000,
            "top-up solana",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger
        .reject_rebalance(solana_id, "not needed", "ops-bob", 1_001)
        .unwrap();

    let goldcoin_only = ledger
        .list_rebalances(Some(ReserveDirection::GoldcoinReserve), false)
        .unwrap();
    assert_eq!(goldcoin_only.len(), 1);
    assert_eq!(goldcoin_only[0].id, goldcoin_id);

    let all_open = ledger.list_rebalances(None, true).unwrap();
    assert_eq!(
        all_open.len(),
        1,
        "the rejected Solana request must not appear as open"
    );
    assert_eq!(all_open[0].id, goldcoin_id);

    let all = ledger.list_rebalances(None, false).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn rebalance_state_log_records_every_transition_in_order() {
    let mut ledger = setup();
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    ledger
        .record_rebalance_executed(id, "sig-log-order", "ops-alice", 1_002)
        .unwrap();
    ledger
        .confirm_rebalance(id, 10_000, "ops-alice", 1_003)
        .unwrap();
    let log = ledger.rebalance_state_log(id).unwrap();
    let to_states: Vec<RebalanceState> = log.iter().map(|(_, to, _, _, _)| *to).collect();
    assert_eq!(
        to_states,
        vec![
            RebalanceState::Proposed,
            RebalanceState::Approved,
            RebalanceState::Executed,
            RebalanceState::Confirmed,
        ]
    );
}

// -------------------------------------------------- post-finality reorg --

#[test]
fn detect_post_finality_reorg_finds_only_finalized_requests_above_the_fork_height() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved {
        request_id: finalized_id,
    } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(finalized_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    ledger
        .mark_glc_source_finalized(finalized_id, 1_200)
        .unwrap();

    let CreateRequestOutcome::Reserved {
        request_id: pre_finality_id,
    } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(50_000),
            &[2u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(
            pre_finality_id,
            [0xCC; 32],
            0,
            50_000,
            20,
            [0xDD; 32],
            1_100,
        )
        .unwrap();
    // Still Confirming — never finalized.

    // A rollback to height 5 orphans the finalized request's block (10)
    // but not the pre-finality one specifically — either way, only the
    // FINALIZED request must ever be returned here, since
    // `goldcoin_rollback_reorg` already handles pre-finality rows
    // correctly on its own.
    let affected = ledger.detect_post_finality_reorg(5).unwrap();
    assert_eq!(affected, vec![finalized_id]);

    // A rollback to height 15 (above the finalized request's block 10)
    // finds nothing — routine, no post-finality impact.
    let affected = ledger.detect_post_finality_reorg(15).unwrap();
    assert!(affected.is_empty());
}

#[test]
fn record_post_finality_reorg_pauses_both_reserves_and_writes_a_distinct_audit_event() {
    let mut ledger = setup();
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
    assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());

    let id = ledger
        .record_post_finality_reorg(5, 12, &[42, 43], 1_000)
        .unwrap();
    assert!(id > 0);

    assert!(
        ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        "post-finality reorg must pause the Goldcoin reserve"
    );
    assert!(
        ledger.is_paused(ReserveDirection::SolanaReserve).unwrap(),
        "post-finality reorg must pause the Solana reserve too (global, docs/10-threat-model.md)"
    );
    assert_eq!(ledger.post_finality_reorg_event_count().unwrap(), 1);

    let (fork_height, old_tip_height, ids_json): (i64, i64, String) = ledger
        .raw()
        .query_row(
            "SELECT fork_height, old_tip_height, affected_request_ids FROM \
             post_finality_reorg_events WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(fork_height, 5);
    assert_eq!(old_tip_height, 12);
    let ids: Vec<i64> = serde_json::from_str(&ids_json).unwrap();
    assert_eq!(ids, vec![42, 43]);

    // Never auto-cleared — same discipline as every other pause in this
    // codebase.
    let report = crate::reconciliation::reconcile(
        &mut ledger,
        ReserveDirection::GoldcoinReserve,
        1_000_000,
        1_000_000,
        2_000,
    )
    .unwrap();
    assert_eq!(
        report.classification,
        crate::reconciliation::Classification::WithinTolerance
    );
    assert!(
        ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        "a WithinTolerance reconciliation cycle must never clear an existing pause"
    );
}

// ------------------------------------------------- custody transitions --

#[test]
fn custody_transition_vault_sweep_full_lifecycle_requires_only_goldcoin_paused() {
    let mut ledger = setup();
    let id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-pubkey-1".to_string(), "old-pubkey-2".to_string()],
            &["new-pubkey-1".to_string(), "new-pubkey-2".to_string()],
            Some(2),
            "scheduled vault rotation",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    let ct = ledger.get_custody_transition(id).unwrap().unwrap();
    assert_eq!(ct.state, CustodyTransitionState::Proposed);
    assert_eq!(ct.new_threshold, Some(2));

    // Cannot approve before the new identity is verified.
    let result = ledger.approve_custody_transition(id, "ops-alice", 1_001);
    assert!(matches!(
        result,
        Err(LedgerError::CustodyTransitionWrongState { .. })
    ));

    ledger
        .verify_new_identity(id, "ops-verifier", 1_002)
        .unwrap();
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::IdentityVerified
    );

    let outcome = ledger
        .approve_custody_transition(id, "ops-alice", 1_003)
        .unwrap();
    assert_eq!(outcome, CustodyApprovalOutcome::ThresholdReached);
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::Approved
    );

    // Cannot execute while the Goldcoin reserve is still unpaused.
    let result =
        ledger.record_custody_transition_executed(id, "glc-sweep-txid-1", "ops-alice", 1_004);
    assert!(matches!(
        result,
        Err(LedgerError::CustodyTransitionRequiresPause { .. })
    ));

    ledger
        .set_paused(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("vault sweep in progress"),
        )
        .unwrap();
    ledger
        .record_custody_transition_executed(id, "glc-sweep-txid-1", "ops-alice", 1_005)
        .unwrap();
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::Executed
    );

    ledger
        .confirm_custody_transition(id, "ops-alice", 1_006)
        .unwrap();
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::Confirmed
    );
}

#[test]
fn custody_transition_attestation_rotation_requires_both_reserves_paused() {
    let mut ledger = setup();
    let id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::AttestationKeyRotation,
            &["old-signer-a".to_string()],
            &["new-signer-a".to_string(), "new-signer-b".to_string()],
            None,
            "scheduled attestation key rotation",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger
        .verify_new_identity(id, "ops-verifier", 1_001)
        .unwrap();
    ledger
        .approve_custody_transition(id, "ops-alice", 1_002)
        .unwrap();

    // Only Goldcoin paused: still not enough for an attestation rotation,
    // which authorizes BOTH bridge directions.
    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("rotation"))
        .unwrap();
    let result =
        ledger.record_custody_transition_executed(id, "attn-rotate-txid-1", "ops-alice", 1_003);
    assert!(matches!(
        result,
        Err(LedgerError::CustodyTransitionRequiresPause {
            direction: ReserveDirection::SolanaReserve,
            ..
        })
    ));

    ledger
        .set_paused(ReserveDirection::SolanaReserve, true, Some("rotation"))
        .unwrap();
    ledger
        .record_custody_transition_executed(id, "attn-rotate-txid-1", "ops-alice", 1_004)
        .unwrap();
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::Executed
    );
}

#[test]
fn custody_transition_new_threshold_is_rejected_for_attestation_rotation() {
    let mut ledger = setup();
    let result = ledger.propose_custody_transition(
        CustodyTransitionKind::AttestationKeyRotation,
        &["old-signer-a".to_string()],
        &["new-signer-a".to_string()],
        Some(2),
        "bad request",
        "ops-alice",
        1,
        1_000,
    );
    assert!(matches!(
        result,
        Err(LedgerError::InvalidCustodyTransition(_))
    ));
}

#[test]
fn custody_transition_approving_twice_from_the_same_identity_does_not_double_count() {
    let mut ledger = setup();
    let id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-1".to_string()],
            &["new-1".to_string()],
            Some(1),
            "top-up",
            "ops-alice",
            2,
            1_000,
        )
        .unwrap();
    ledger
        .verify_new_identity(id, "ops-verifier", 1_001)
        .unwrap();
    let o1 = ledger
        .approve_custody_transition(id, "ops-alice", 1_002)
        .unwrap();
    let o2 = ledger
        .approve_custody_transition(id, "ops-alice", 1_003)
        .unwrap();
    assert_eq!(
        o1, o2,
        "the same approver approving twice must not move the count"
    );
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::IdentityVerified,
        "still short of the real second distinct approver"
    );
}

#[test]
fn custody_transition_duplicate_tx_reference_is_rejected_structurally() {
    let mut ledger = setup();
    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("sweep"))
        .unwrap();

    let id1 = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-1".to_string()],
            &["new-1".to_string()],
            Some(1),
            "sweep 1",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger
        .verify_new_identity(id1, "ops-verifier", 1_001)
        .unwrap();
    ledger
        .approve_custody_transition(id1, "ops-alice", 1_002)
        .unwrap();
    ledger
        .record_custody_transition_executed(id1, "glc-sweep-replay-target", "ops-alice", 1_003)
        .unwrap();

    let id2 = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-2".to_string()],
            &["new-2".to_string()],
            Some(1),
            "sweep 2",
            "ops-alice",
            1,
            1_004,
        )
        .unwrap();
    ledger
        .verify_new_identity(id2, "ops-verifier", 1_005)
        .unwrap();
    ledger
        .approve_custody_transition(id2, "ops-alice", 1_006)
        .unwrap();
    let result = ledger.record_custody_transition_executed(
        id2,
        "glc-sweep-replay-target",
        "ops-alice",
        1_007,
    );
    assert!(
        result.is_err(),
        "the same real tx_reference must never be recorded against two custody transitions"
    );
    assert_eq!(
        ledger.get_custody_transition(id1).unwrap().unwrap().state,
        CustodyTransitionState::Executed
    );
    assert_eq!(
        ledger.get_custody_transition(id2).unwrap().unwrap().state,
        CustodyTransitionState::Approved,
        "the second transition must not be left partially executed by the rejected attempt"
    );
}

#[test]
fn custody_transition_reject_and_cancel_require_a_note_and_are_terminal() {
    let mut ledger = setup();
    let id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-1".to_string()],
            &["new-1".to_string()],
            Some(1),
            "sweep",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();

    let result = ledger.reject_custody_transition(id, "", "ops-alice", 1_001);
    assert!(matches!(
        result,
        Err(LedgerError::InvalidCustodyTransition(_))
    ));

    ledger
        .reject_custody_transition(id, "identity could not be verified", "ops-alice", 1_002)
        .unwrap();
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::Rejected
    );

    let id2 = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-2".to_string()],
            &["new-2".to_string()],
            Some(1),
            "sweep 2",
            "ops-alice",
            1,
            1_003,
        )
        .unwrap();
    ledger
        .cancel_custody_transition(id2, "no longer needed", "ops-alice", 1_004)
        .unwrap();
    assert_eq!(
        ledger.get_custody_transition(id2).unwrap().unwrap().state,
        CustodyTransitionState::Cancelled
    );
}

#[test]
fn custody_transition_fail_then_rollback_records_evidence_without_touching_pause_state() {
    let mut ledger = setup();
    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("sweep"))
        .unwrap();
    let id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-1".to_string()],
            &["new-1".to_string()],
            Some(1),
            "sweep",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger
        .verify_new_identity(id, "ops-verifier", 1_001)
        .unwrap();
    ledger
        .approve_custody_transition(id, "ops-alice", 1_002)
        .unwrap();
    ledger
        .record_custody_transition_executed(id, "glc-sweep-fail-1", "ops-alice", 1_003)
        .unwrap();

    ledger
        .fail_custody_transition(id, "new vault never observed active", "ops-alice", 1_004)
        .unwrap();
    let ct = ledger.get_custody_transition(id).unwrap().unwrap();
    assert_eq!(ct.state, CustodyTransitionState::Failed);
    assert_eq!(
        ct.failure_reason.as_deref(),
        Some("new vault never observed active")
    );

    // A rollback is only ever an audit marker of a real, out-of-band
    // revert — it never touches reserve pause state itself.
    let paused_before = ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap();
    ledger
        .rollback_custody_transition(id, "reverted to old vault out of band", "ops-alice", 1_005)
        .unwrap();
    let ct = ledger.get_custody_transition(id).unwrap().unwrap();
    assert_eq!(ct.state, CustodyTransitionState::RolledBack);
    assert_eq!(
        ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        paused_before,
        "rollback must never itself change pause state"
    );

    // Cannot roll back a second time.
    let result = ledger.rollback_custody_transition(id, "again", "ops-alice", 1_006);
    assert!(matches!(
        result,
        Err(LedgerError::CustodyTransitionWrongState { .. })
    ));
}

#[test]
fn custody_transition_list_filters_by_kind_and_open_state() {
    let mut ledger = setup();
    let sweep_id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-1".to_string()],
            &["new-1".to_string()],
            Some(1),
            "sweep",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    let rotation_id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::AttestationKeyRotation,
            &["old-signer".to_string()],
            &["new-signer".to_string()],
            None,
            "rotation",
            "ops-alice",
            1,
            1_001,
        )
        .unwrap();
    ledger
        .reject_custody_transition(sweep_id, "closed out", "ops-alice", 1_002)
        .unwrap();

    let sweeps = ledger
        .list_custody_transitions(Some(CustodyTransitionKind::GoldcoinVaultSweep), false)
        .unwrap();
    assert_eq!(sweeps.len(), 1);
    assert_eq!(sweeps[0].id, sweep_id);

    let open = ledger.list_custody_transitions(None, true).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, rotation_id);

    let all = ledger.list_custody_transitions(None, false).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn custody_transition_state_log_records_every_transition_in_order() {
    let mut ledger = setup();
    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("sweep"))
        .unwrap();
    let id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-1".to_string()],
            &["new-1".to_string()],
            Some(1),
            "sweep",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger
        .verify_new_identity(id, "ops-verifier", 1_001)
        .unwrap();
    ledger
        .approve_custody_transition(id, "ops-alice", 1_002)
        .unwrap();
    ledger
        .record_custody_transition_executed(id, "glc-sweep-log-1", "ops-alice", 1_003)
        .unwrap();
    ledger
        .confirm_custody_transition(id, "ops-alice", 1_004)
        .unwrap();

    let log = ledger.custody_transition_state_log(id).unwrap();
    let states: Vec<CustodyTransitionState> = log.iter().map(|e| e.1).collect();
    assert_eq!(
        states,
        vec![
            CustodyTransitionState::Proposed,
            CustodyTransitionState::IdentityVerified,
            CustodyTransitionState::Approved,
            CustodyTransitionState::Executed,
            CustodyTransitionState::Confirmed,
        ]
    );
}

// ------------------------------------------------- unique deposit addresses --

/// A fresh Solana recipient per call: the rolling-24h destination window
/// (`ledger::wallet_window`) now applies to `GlcToSol`, so two requests
/// to one pubkey inside a day would be a refusal, not a fixture.
fn create_glc_to_sol_request(ledger: &mut Ledger) -> i64 {
    let count: i64 = ledger
        .conn_for_tests()
        .query_row("SELECT COUNT(*) FROM bridge_requests", [], |r| r.get(0))
        .unwrap();
    let mut recipient = [1u8; 32];
    recipient[31] = count as u8;
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &recipient,
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected Reserved")
    };
    request_id
}

#[test]
fn set_goldcoin_deposit_address_round_trips() {
    let mut ledger = setup();
    let request_id = create_glc_to_sol_request(&mut ledger);

    ledger
        .set_goldcoin_deposit_address(
            request_id,
            "Qsomeaddress",
            "76a914somehash88ac",
            "5221...53ae",
        )
        .unwrap();

    assert_eq!(
        ledger
            .find_goldcoin_deposit_request_by_script("76a914somehash88ac")
            .unwrap(),
        Some((request_id, Direction::GlcToSol))
    );
    assert_eq!(
        ledger.all_goldcoin_deposit_script_pubkeys().unwrap(),
        vec!["76a914somehash88ac".to_string()]
    );
}

#[test]
fn set_goldcoin_deposit_address_is_idempotent_on_an_exact_repeat() {
    let mut ledger = setup();
    let request_id = create_glc_to_sol_request(&mut ledger);
    ledger
        .set_goldcoin_deposit_address(request_id, "Qaddr", "scripthex", "redeemhex")
        .unwrap();
    // Calling again with the SAME values must succeed, not error.
    ledger
        .set_goldcoin_deposit_address(request_id, "Qaddr", "scripthex", "redeemhex")
        .unwrap();
}

#[test]
fn set_goldcoin_deposit_address_never_silently_overwrites_a_different_value() {
    let mut ledger = setup();
    let request_id = create_glc_to_sol_request(&mut ledger);
    ledger
        .set_goldcoin_deposit_address(request_id, "Qfirst", "scripthex1", "redeemhex1")
        .unwrap();

    let err = ledger
        .set_goldcoin_deposit_address(request_id, "Qsecond", "scripthex2", "redeemhex2")
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::DepositAddressAlreadySet { id, .. } if id == request_id
    ));
    // The original assignment must still be the one in effect.
    assert_eq!(
        ledger
            .find_goldcoin_deposit_request_by_script("scripthex1")
            .unwrap(),
        Some((request_id, Direction::GlcToSol))
    );
    assert_eq!(
        ledger
            .find_goldcoin_deposit_request_by_script("scripthex2")
            .unwrap(),
        None
    );
}

#[test]
fn set_goldcoin_deposit_address_rejects_a_sol_to_glc_request() {
    let mut ledger = setup();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!("expected FoldedFinalized")
    };

    let err = ledger
        .set_goldcoin_deposit_address(request_id, "Qaddr", "scripthex", "redeemhex")
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::NotAGoldcoinSourcedRequest { id, actual_direction: Direction::SolToGlc } if id == request_id
    ));
}

#[test]
fn set_goldcoin_deposit_address_rejects_an_unknown_request_id() {
    let mut ledger = setup();
    let err = ledger
        .set_goldcoin_deposit_address(999_999, "Qaddr", "scripthex", "redeemhex")
        .unwrap_err();
    assert!(matches!(err, LedgerError::RequestNotFound(999_999)));
}

#[test]
fn find_goldcoin_deposit_request_by_script_returns_none_for_unknown_script() {
    let ledger = setup();
    assert_eq!(
        ledger
            .find_goldcoin_deposit_request_by_script("never-assigned")
            .unwrap(),
        None
    );
}

#[test]
fn find_goldcoin_deposit_request_by_script_does_not_match_a_sol_to_glc_row() {
    // Defense in depth: even if a SolToGlc row somehow had a non-NULL
    // deposit_script_pubkey_hex (it never legitimately can, since
    // `set_goldcoin_deposit_address` refuses that direction outright),
    // the lookup itself is also direction-scoped.
    let mut ledger = setup();
    let request_id = create_glc_to_sol_request(&mut ledger);
    ledger
        .set_goldcoin_deposit_address(request_id, "Qaddr", "shared-script", "redeemhex")
        .unwrap();
    assert_eq!(
        ledger
            .find_goldcoin_deposit_request_by_script("shared-script")
            .unwrap(),
        Some((request_id, Direction::GlcToSol))
    );
}

#[test]
fn all_goldcoin_deposit_script_pubkeys_includes_settled_requests() {
    // A settled request's derived address can still hold an unswept UTXO
    // -- the enumeration must include it, not just currently-open
    // AwaitingDeposit requests.
    let mut ledger = setup();
    let request_id = create_glc_to_sol_request(&mut ledger);
    ledger
        .set_goldcoin_deposit_address(request_id, "Qaddr", "settled-script", "redeemhex")
        .unwrap();
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();

    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );
    assert!(ledger
        .all_goldcoin_deposit_script_pubkeys()
        .unwrap()
        .contains(&"settled-script".to_string()));
}

#[test]
fn all_goldcoin_deposit_script_pubkeys_excludes_requests_with_no_address_assigned() {
    let mut ledger = setup();
    let _request_id = create_glc_to_sol_request(&mut ledger); // never assigned an address
    assert!(ledger
        .all_goldcoin_deposit_script_pubkeys()
        .unwrap()
        .is_empty());
}

#[test]
fn two_requests_can_never_share_the_same_deposit_script_pubkey() {
    let mut ledger = setup();
    let a = create_glc_to_sol_request(&mut ledger);
    let b = create_glc_to_sol_request(&mut ledger);
    ledger
        .set_goldcoin_deposit_address(a, "Qaddr-a", "same-script", "redeem-a")
        .unwrap();
    // The database-level partial unique index (ux_bridge_requests_deposit_script)
    // is the actual, race-safe guarantee here -- not application logic.
    let err = ledger
        .set_goldcoin_deposit_address(b, "Qaddr-b", "same-script", "redeem-b")
        .unwrap_err();
    assert!(matches!(err, LedgerError::Sqlite(_)));
}

// -------------------------------------- unmatched deposit / vault split reconciliation --

fn broadcast_vault_split(
    ledger: &mut Ledger,
    split_txid: [u8; 32],
    source_amount_atomic: u64,
    fee_atomic: u64,
    output_amounts: Vec<u64>,
) -> i64 {
    let plan = crate::goldcoin::split::SplitPlan {
        source: crate::goldcoin::coin::VaultUtxo {
            txid: [0xEEu8; 32],
            vout: 0,
            amount_atomic: source_amount_atomic,
            script_pubkey_hex: "deadbeef".to_string(),
        },
        vault_script_pubkey: vec![0xAA],
        output_amounts,
        fee_atomic,
    };
    ledger
        .raw()
        .execute(
            "INSERT INTO vault_utxos (txid, vout, amount_atomic, script_pubkey_hex, confirmations, first_seen_at, state)
             VALUES (?1, ?2, ?3, ?4, 20, 0, 'Available')
             ON CONFLICT(txid, vout) DO NOTHING",
            rusqlite::params![
                plan.source.txid.as_slice(),
                plan.source.vout,
                plan.source.amount_atomic as i64,
                plan.source.script_pubkey_hex,
            ],
        )
        .unwrap();
    let id = ledger
        .record_vault_utxo_split_built(&plan, 1, "unsigned-hex", "test split", 0)
        .unwrap();
    ledger
        .record_vault_utxo_split_signed(id, "signed-hex", 0)
        .unwrap();
    let output_amounts = plan.output_amounts.clone();
    ledger
        .record_vault_utxo_split_broadcast(id, split_txid, &output_amounts, "deadbeef", 0)
        .unwrap();
    id
}

#[test]
fn get_broadcast_vault_utxo_split_returns_the_persisted_figures() {
    let mut ledger = setup();
    let split_txid = [0xCCu8; 32];
    broadcast_vault_split(
        &mut ledger,
        split_txid,
        1_000_000,
        100,
        vec![333_300, 333_300, 333_300],
    );
    let split = ledger
        .get_broadcast_vault_utxo_split(split_txid)
        .unwrap()
        .unwrap();
    assert_eq!(split.source_amount_atomic, 1_000_000);
    assert_eq!(split.fee_atomic, 100);
    assert_eq!(split.chunk_count, 3);
}

#[test]
fn get_broadcast_vault_utxo_split_is_none_for_an_unknown_txid() {
    let ledger = setup();
    assert!(ledger
        .get_broadcast_vault_utxo_split([0x11u8; 32])
        .unwrap()
        .is_none());
}

#[test]
fn reconciles_an_unmatched_deposit_that_exactly_matches_a_split_output() {
    let mut ledger = setup();
    let split_txid = [0xCCu8; 32];
    broadcast_vault_split(
        &mut ledger,
        split_txid,
        1_000_000,
        100,
        vec![333_300, 333_300, 333_300],
    );
    ledger
        .record_unmatched_goldcoin_deposit(split_txid, 0, 333_300, 50, "no_request_binding", 1_000)
        .unwrap();

    let outcome = ledger
        .reconcile_unmatched_goldcoin_deposit(split_txid, 0, "reconciling", 2_000)
        .unwrap();
    assert_eq!(outcome, ReconcileUnmatchedDepositOutcome::Reconciled);

    let reconciled_at: Option<i64> = ledger
        .raw()
        .query_row(
            "SELECT reconciled_at FROM unmatched_goldcoin_deposits WHERE txid = ?1 AND vout = 0",
            [split_txid.as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        reconciled_at,
        Some(2_000),
        "the row must be marked reconciled, never deleted"
    );
    let count: i64 = ledger
        .raw()
        .query_row(
            "SELECT count(*) FROM unmatched_goldcoin_deposits",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "reconciling must never delete the audit row");
}

#[test]
fn reconcile_is_idempotent_on_an_already_reconciled_row() {
    let mut ledger = setup();
    let split_txid = [0xCCu8; 32];
    broadcast_vault_split(
        &mut ledger,
        split_txid,
        1_000_000,
        100,
        vec![333_300, 333_300, 333_300],
    );
    ledger
        .record_unmatched_goldcoin_deposit(split_txid, 0, 333_300, 50, "no_request_binding", 1_000)
        .unwrap();
    ledger
        .reconcile_unmatched_goldcoin_deposit(split_txid, 0, "first reconcile", 2_000)
        .unwrap();

    let outcome = ledger
        .reconcile_unmatched_goldcoin_deposit(split_txid, 0, "second reconcile attempt", 3_000)
        .unwrap();
    assert_eq!(outcome, ReconcileUnmatchedDepositOutcome::AlreadyReconciled);
    let reconciled_at: Option<i64> = ledger
        .raw()
        .query_row(
            "SELECT reconciled_at FROM unmatched_goldcoin_deposits WHERE txid = ?1 AND vout = 0",
            [split_txid.as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        reconciled_at,
        Some(2_000),
        "a repeat call must never overwrite the original reconciliation timestamp"
    );
}

#[test]
fn refuses_to_reconcile_a_row_that_does_not_match_any_split() {
    let mut ledger = setup();
    ledger
        .record_unmatched_goldcoin_deposit([0x99u8; 32], 0, 500, 50, "no_request_binding", 1_000)
        .unwrap();
    let err = ledger
        .reconcile_unmatched_goldcoin_deposit([0x99u8; 32], 0, "reconciling", 2_000)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::UnmatchedDepositNotAKnownSplitOutput { .. }
    ));
}

#[test]
fn refuses_to_reconcile_a_row_with_a_wrong_amount_even_if_the_split_exists() {
    let mut ledger = setup();
    let split_txid = [0xCCu8; 32];
    broadcast_vault_split(
        &mut ledger,
        split_txid,
        1_000_000,
        100,
        vec![333_300, 333_300, 333_300],
    );
    // Recorded amount does not match the split's expected output at vout 0.
    ledger
        .record_unmatched_goldcoin_deposit(split_txid, 0, 999_999, 50, "no_request_binding", 1_000)
        .unwrap();
    let err = ledger
        .reconcile_unmatched_goldcoin_deposit(split_txid, 0, "reconciling", 2_000)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::UnmatchedDepositNotAKnownSplitOutput { .. }
    ));
}

#[test]
fn refuses_to_reconcile_an_unknown_row() {
    let mut ledger = setup();
    let err = ledger
        .reconcile_unmatched_goldcoin_deposit([0x77u8; 32], 0, "reconciling", 2_000)
        .unwrap_err();
    assert!(matches!(err, LedgerError::UnmatchedDepositNotFound { .. }));
}

// ------------------------------------------------------- admin audit log --

fn audit_entry(at: i64, actor: &str, action: &str, outcome: AdminAuditOutcome) -> AdminAuditEntry {
    AdminAuditEntry {
        at,
        actor: actor.to_string(),
        action: action.to_string(),
        target: Some("goldcoin".to_string()),
        old_value: Some("false".to_string()),
        new_value: Some("true".to_string()),
        note: "test note".to_string(),
        outcome,
    }
}

#[test]
fn admin_audit_append_and_list_round_trips_success_and_error_rows() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let ok_id = ledger
        .append_admin_audit(&audit_entry(
            100,
            "alice",
            "pause",
            AdminAuditOutcome::Success,
        ))
        .unwrap();
    let err_id = ledger
        .append_admin_audit(&audit_entry(
            101,
            "bob",
            "admission_open",
            AdminAuditOutcome::Error("invariant violated".to_string()),
        ))
        .unwrap();
    assert!(err_id > ok_id);

    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(rows.len(), 2);
    // Newest first.
    assert_eq!(rows[0].id, err_id);
    assert_eq!(rows[0].actor, "bob");
    assert_eq!(rows[0].action, "admission_open");
    assert_eq!(
        rows[0].outcome,
        AdminAuditOutcome::Error("invariant violated".to_string())
    );
    assert_eq!(rows[1].id, ok_id);
    assert_eq!(rows[1].outcome, AdminAuditOutcome::Success);
    assert_eq!(rows[1].target.as_deref(), Some("goldcoin"));
    assert_eq!(rows[1].old_value.as_deref(), Some("false"));
    assert_eq!(rows[1].new_value.as_deref(), Some("true"));
    assert_eq!(rows[1].note, "test note");
}

#[test]
fn admin_audit_rejects_an_empty_note_at_the_schema_level() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut entry = audit_entry(100, "alice", "pause", AdminAuditOutcome::Success);
    entry.note = String::new();
    let err = ledger.append_admin_audit(&entry).unwrap_err();
    assert!(matches!(err, LedgerError::Sqlite(_)), "{err:?}");
}

#[test]
fn admin_audit_filters_by_action_and_actor_and_paginates_by_keyset() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    for i in 0..10i64 {
        let actor = if i % 2 == 0 { "alice" } else { "bob" };
        let action = if i < 5 { "pause" } else { "unpause" };
        ledger
            .append_admin_audit(&audit_entry(
                100 + i,
                actor,
                action,
                AdminAuditOutcome::Success,
            ))
            .unwrap();
    }

    let pauses = ledger
        .list_admin_audit(&AdminAuditFilter {
            action: Some("pause".to_string()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(pauses.len(), 5);
    assert!(pauses.iter().all(|r| r.action == "pause"));

    let bobs = ledger
        .list_admin_audit(&AdminAuditFilter {
            actor: Some("bob".to_string()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(bobs.len(), 5);
    assert!(bobs.iter().all(|r| r.actor == "bob"));

    // Keyset pagination: two pages of 4, then the rest, no overlap.
    let page1 = ledger
        .list_admin_audit(&AdminAuditFilter {
            limit: Some(4),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page1.len(), 4);
    let page2 = ledger
        .list_admin_audit(&AdminAuditFilter {
            limit: Some(4),
            before_id: Some(page1.last().unwrap().id),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page2.len(), 4);
    let seen: std::collections::HashSet<i64> =
        page1.iter().chain(page2.iter()).map(|r| r.id).collect();
    assert_eq!(seen.len(), 8, "pages must not overlap");
    let page1_min = page1.iter().map(|r| r.id).min().unwrap();
    let page2_max = page2.iter().map(|r| r.id).max().unwrap();
    assert!(
        page1_min > page2_max,
        "page 2 must be strictly older than page 1"
    );
}

#[test]
fn admin_audit_limit_is_clamped_to_200() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    for i in 0..210i64 {
        ledger
            .append_admin_audit(&audit_entry(
                i,
                "alice",
                "pause",
                AdminAuditOutcome::Success,
            ))
            .unwrap();
    }
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter {
            limit: Some(10_000),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 200);

    // And clamped UP from zero: a zero limit must never produce a
    // permanently empty page that reads as "no audit rows exist".
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter {
            limit: Some(0),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
}

// ------------------------------------------------------ ManualReview refunds --

/// Parks one SolToGlc fold in ManualReview via closed admission. Distinct
/// `requester`/`recipient` per call keep the rate limiters out of tests
/// that aren't about them.
fn park_sol_request(
    ledger: &mut Ledger,
    obligation_index: u64,
    gross: u64,
    requester: [u8; 32],
    recipient: &[u8],
) -> i64 {
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(
            obligation_index,
            amounts(gross),
            requester,
            recipient,
            None,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected admission-closed to route to ManualReview")
    };
    request_id
}

/// Chain-verified inputs matching what the ledger itself stored — what
/// `solana::refund::build_refund_plan` would produce after all its own
/// cross-checks succeeded. Ledger tests use the gross value as the native
/// amount (the decimals relationship is `solana::refund`'s concern; the
/// ledger only cross-checks the canonical gross byte-for-byte).
fn verified_for(ledger: &Ledger, request_id: i64) -> VerifiedRefundInputs {
    let request = ledger.get_request(request_id).unwrap().unwrap();
    VerifiedRefundInputs {
        obligation_index: request.source_obligation_index.unwrap(),
        amount_solana_atomic: request.gross_amount_atomic,
        gross_canonical_atomic: request.gross_amount_atomic,
        requester: request.requester.unwrap(),
        destination_token_account: [0xDD; 32],
        reserve_mint: [0xEE; 32],
        token_program: [0xFF; 32],
    }
}

/// Item 7 of the production-safety review: two different request ids can
/// never produce the same refund nonce, and no refund nonce can ever
/// collide with an ordinary operator rebalance nonce.
#[test]
fn refund_nonce_is_injective_and_never_collides_with_the_rebalance_domain() {
    // Injectivity: nonce = DOMAIN | id, and for every valid id
    // (1..=i64::MAX) `id as u64` occupies only the low 63 bits, so the
    // OR is a bijection onto the high half of u64 — distinct ids give
    // distinct nonces, and the id is exactly recoverable.
    let ids: Vec<i64> = vec![1, 2, 3, 42, 1_000, 1_000_000, i64::MAX - 1, i64::MAX];
    let mut seen = std::collections::HashSet::new();
    for &id in &ids {
        let nonce = Ledger::solana_refund_nonce(id).unwrap();
        assert!(seen.insert(nonce), "nonce collision at id {id}");
        // The domain bit is always set, and the id is recoverable —
        // which is what makes the mapping injective by construction.
        assert_ne!(nonce & Ledger::SOLANA_REFUND_NONCE_DOMAIN, 0);
        assert_eq!(nonce & !Ledger::SOLANA_REFUND_NONCE_DOMAIN, id as u64);
    }
    // Exhaustive over a dense low range, where real request ids live.
    let dense: std::collections::HashSet<u64> = (1..=20_000i64)
        .map(|id| Ledger::solana_refund_nonce(id).unwrap())
        .collect();
    assert_eq!(
        dense.len(),
        20_000,
        "every id in 1..=20000 must map uniquely"
    );

    // Disjointness from the ordinary rebalance nonce space: those are
    // operator-chosen counters/Unix timestamps, all far below 2^63, so
    // their top bit is clear and no refund nonce can ever equal one.
    for rebalance_nonce in [0u64, 1, 7, 1_000_000, 1_756_000_000, (1u64 << 63) - 1] {
        assert_eq!(rebalance_nonce & Ledger::SOLANA_REFUND_NONCE_DOMAIN, 0);
        assert!(
            !dense.contains(&rebalance_nonce),
            "rebalance nonce {rebalance_nonce} must not be reachable as a refund nonce"
        );
    }

    // Non-positive ids are refused rather than wrapping into the domain.
    assert!(Ledger::solana_refund_nonce(0).is_err());
    assert!(Ledger::solana_refund_nonce(-1).is_err());
    assert!(Ledger::solana_refund_nonce(i64::MIN).is_err());
}

#[test]
fn refund_nonce_is_the_refund_domain_bit_or_the_request_id() {
    assert_eq!(
        Ledger::solana_refund_nonce(1).unwrap(),
        (1u64 << 63) | 1,
        "nonce must live in the dedicated refund domain"
    );
    assert_eq!(Ledger::solana_refund_nonce(42).unwrap(), (1u64 << 63) | 42);
    assert!(Ledger::solana_refund_nonce(0).is_err());
    assert!(Ledger::solana_refund_nonce(-5).is_err());
}

#[test]
fn begin_refund_happy_path_records_row_and_transitions_without_touching_goldcoin_counters() {
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    let (gc_reserved_before, gc_pending_before) = {
        let (_, _, reserved, pending) = ledger
            .reserve_snapshot(ReserveDirection::GoldcoinReserve)
            .unwrap();
        (reserved, pending)
    };

    ledger
        .begin_solana_refund(
            request_id,
            &verified_for(&ledger, request_id),
            "refund: parked by closed admission",
            "cli:test",
            2_000,
        )
        .unwrap();

    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::RefundPending);
    // Original evidence preserved — the park reason is never cleared or
    // overwritten by the refund lifecycle.
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some("admission_closed_at_fold")
    );
    assert_eq!(request.source_obligation_index, Some(0));

    let refund = ledger.get_solana_refund(request_id).unwrap().unwrap();
    assert_eq!(refund.state, SolanaRefundState::Pending);
    assert_eq!(refund.nonce, (1u64 << 63) | request_id as u64);
    assert_eq!(refund.obligation_index, 0);
    assert_eq!(refund.amount_solana_atomic, 100_000);
    assert_eq!(refund.requester, [1u8; 32]);
    assert_eq!(refund.manual_review_reason, "admission_closed_at_fold");
    assert_eq!(refund.created_by, "cli:test");
    assert!(refund.refund_signature.is_none());

    // A fold-time park never reserved Goldcoin liquidity, and beginning a
    // refund must not release/alter anything there.
    let (_, _, gc_reserved_after, gc_pending_after) = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(gc_reserved_after, gc_reserved_before);
    assert_eq!(gc_pending_after, gc_pending_before);
    // The SolanaReserve book is untouched at begin (debited only at
    // confirm).
    let (sol_balance, _, _, _) = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .unwrap();
    assert_eq!(sol_balance, 1_000_000);
}

#[test]
fn begin_refund_rejects_wrong_direction() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(50_000),
            &[3u8; 32],
            None,
            600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected creation")
    };
    let verified = VerifiedRefundInputs {
        obligation_index: 0,
        amount_solana_atomic: 50_000,
        gross_canonical_atomic: 50_000,
        requester: [1u8; 32],
        destination_token_account: [0xDD; 32],
        reserve_mint: [0xEE; 32],
        token_program: [0xFF; 32],
    };
    let err = ledger
        .begin_solana_refund(request_id, &verified, "n", "a", 2_000)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::RefundNotEligible { .. }),
        "got: {err}"
    );
    assert!(err.to_string().contains("SolToGlc"), "got: {err}");
}

#[test]
fn begin_refund_rejects_every_non_whitelisted_reason() {
    for bad_reason in [
        "late_deposit_no_capacity",
        "deposit_amount_mismatch: expected 5 observed 4",
        "deposit_spent_before_finalized",
        "totally_new_future_reason",
    ] {
        let mut ledger = setup();
        let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
        ledger
            .conn
            .execute(
                "UPDATE bridge_requests SET manual_review_note = ?1 WHERE id = ?2",
                rusqlite::params![bad_reason, request_id],
            )
            .unwrap();
        let verified = verified_for(&ledger, request_id);
        let err = ledger
            .begin_solana_refund(request_id, &verified, "n", "a", 2_000)
            .unwrap_err();
        assert!(
            err.to_string().contains("whitelisted"),
            "reason {bad_reason:?} must be refused via the whitelist, got: {err}"
        );
    }
    // NULL reason too.
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    ledger
        .conn
        .execute(
            "UPDATE bridge_requests SET manual_review_note = NULL WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    let verified = verified_for(&ledger, request_id);
    assert!(ledger
        .begin_solana_refund(request_id, &verified, "n", "a", 2_000)
        .is_err());
}

#[test]
fn begin_refund_rejects_any_settlement_evidence() {
    // A Goldcoin payout row.
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    ledger
        .conn
        .execute(
            "INSERT INTO goldcoin_payouts (request_id, commitment_hash, payout_atomic,
                change_atomic, fee_atomic, dest_p2pkh_hash, state, built_at)
             VALUES (?1, X'00', 1, 0, 0, X'00', 'Built', 1)",
            [request_id],
        )
        .unwrap();
    let verified = verified_for(&ledger, request_id);
    let err = ledger
        .begin_solana_refund(request_id, &verified, "n", "a", 2_000)
        .unwrap_err();
    assert!(err.to_string().contains("payout"), "got: {err}");

    // A destination transaction.
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    ledger
        .conn
        .execute(
            "UPDATE bridge_requests SET destination_txid = X'AB' WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    let verified = verified_for(&ledger, request_id);
    assert!(ledger
        .begin_solana_refund(request_id, &verified, "n", "a", 2_000)
        .is_err());

    // A settled/completed request (state no longer ManualReview).
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    ledger
        .conn
        .execute(
            "UPDATE bridge_requests SET state = 'Settled', settled_at = 99 WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    let verified = verified_for(&ledger, request_id);
    let err = ledger
        .begin_solana_refund(request_id, &verified, "n", "a", 2_000)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::RefundNotEligible { .. }),
        "got: {err}"
    );
}

#[test]
fn begin_refund_rejects_a_request_that_ever_advanced_past_manual_review() {
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    ledger
        .resume_manual_review_sol_to_glc(request_id, "resume", "operator", 2_000)
        .unwrap();
    // Out-of-band edit shoving it back to ManualReview must NOT make it
    // refundable: the state log proves it held (holds) a reservation.
    ledger
        .conn
        .execute(
            "UPDATE bridge_requests SET state = 'ManualReview',
                manual_review_note = 'admission_closed_at_fold' WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    let verified = verified_for(&ledger, request_id);
    let err = ledger
        .begin_solana_refund(request_id, &verified, "n", "a", 3_000)
        .unwrap_err();
    assert!(
        err.to_string().contains("advanced"),
        "must be refused via the never-advanced proof, got: {err}"
    );
}

#[test]
fn begin_refund_rejects_cross_check_mismatches() {
    // Wrong requester.
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    let mut verified = verified_for(&ledger, request_id);
    verified.requester = [9u8; 32];
    let err = ledger
        .begin_solana_refund(request_id, &verified, "n", "a", 2_000)
        .unwrap_err();
    assert!(err.to_string().contains("requester"), "got: {err}");

    // Wrong obligation index.
    let mut verified = verified_for(&ledger, request_id);
    verified.obligation_index = 77;
    let err = ledger
        .begin_solana_refund(request_id, &verified, "n", "a", 2_000)
        .unwrap_err();
    assert!(err.to_string().contains("obligation"), "got: {err}");

    // Wrong gross.
    let mut verified = verified_for(&ledger, request_id);
    verified.gross_canonical_atomic += 1;
    let err = ledger
        .begin_solana_refund(request_id, &verified, "n", "a", 2_000)
        .unwrap_err();
    assert!(err.to_string().contains("gross"), "got: {err}");
}

#[test]
fn begin_refund_rejects_a_reserve_capacity_breach() {
    let mut ledger = setup();
    // 950_000 > 1_000_000 - 100_000 protected minimum.
    let request_id = park_sol_request(&mut ledger, 0, 950_000, [1u8; 32], &[2u8; 32]);
    let verified = verified_for(&ledger, request_id);
    let err = ledger
        .begin_solana_refund(request_id, &verified, "n", "a", 2_000)
        .unwrap_err();
    assert!(
        matches!(
            err,
            LedgerError::InvariantViolated {
                direction: ReserveDirection::SolanaReserve,
                ..
            }
        ),
        "got: {err}"
    );
    assert!(
        ledger.get_solana_refund(request_id).unwrap().is_none(),
        "a refused begin must leave no refund row"
    );
}

#[test]
fn refund_capacity_counts_other_open_refunds() {
    let mut ledger = setup();
    let first = park_sol_request(&mut ledger, 0, 500_000, [1u8; 32], &[2u8; 32]);
    let second = park_sol_request(&mut ledger, 1, 500_000, [3u8; 32], &[4u8; 32]);
    let verified = verified_for(&ledger, first);
    ledger
        .begin_solana_refund(first, &verified, "n", "a", 2_000)
        .unwrap();
    // 1_000_000 - 100_000 protected - 500_000 already-committed refund
    // leaves 400_000 < 500_000.
    let verified = verified_for(&ledger, second);
    let err = ledger
        .begin_solana_refund(second, &verified, "n", "a", 2_000)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::InvariantViolated { .. }),
        "got: {err}"
    );
}

#[test]
fn refund_broadcast_and_confirm_lifecycle_debits_the_book_exactly_once() {
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    let verified = verified_for(&ledger, request_id);
    ledger
        .begin_solana_refund(request_id, &verified, "refund", "cli:test", 2_000)
        .unwrap();

    ledger
        .record_solana_refund_broadcast(request_id, "sig-1", "hash-1", 0, 3_000)
        .unwrap();
    let refund = ledger.get_solana_refund(request_id).unwrap().unwrap();
    assert_eq!(refund.state, SolanaRefundState::Broadcast);
    assert_eq!(refund.refund_signature.as_deref(), Some("sig-1"));
    assert_eq!(refund.recent_blockhash.as_deref(), Some("hash-1"));
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::RefundBroadcast);

    // A recovery re-sign under the same nonce is latest-wins.
    ledger
        .record_solana_refund_broadcast(request_id, "sig-2", "hash-2", 0, 3_500)
        .unwrap();
    let refund = ledger.get_solana_refund(request_id).unwrap().unwrap();
    assert_eq!(refund.refund_signature.as_deref(), Some("sig-2"));

    ledger
        .mark_solana_refund_confirmed(request_id, 4_000)
        .unwrap();
    let refund = ledger.get_solana_refund(request_id).unwrap().unwrap();
    assert_eq!(refund.state, SolanaRefundState::Confirmed);
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::Refunded);
    let (sol_balance, _, _, _) = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .unwrap();
    assert_eq!(sol_balance, 900_000, "the book is debited at confirm");

    // Idempotent re-confirm: no second debit.
    ledger
        .mark_solana_refund_confirmed(request_id, 5_000)
        .unwrap();
    let (sol_balance, _, _, _) = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .unwrap();
    assert_eq!(sol_balance, 900_000);

    // No further broadcast can ever be recorded.
    let err = ledger
        .record_solana_refund_broadcast(request_id, "sig-3", "hash-3", 0, 6_000)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::RefundWrongState { .. }),
        "got: {err}"
    );
}

#[test]
fn refund_lifecycle_blocks_resume_at_every_stage() {
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    let verified = verified_for(&ledger, request_id);
    ledger
        .begin_solana_refund(request_id, &verified, "refund", "cli:test", 2_000)
        .unwrap();
    for stage in ["Pending", "Broadcast", "Confirmed"] {
        match stage {
            "Broadcast" => ledger
                .record_solana_refund_broadcast(request_id, "sig", "hash", 0, 3_000)
                .unwrap(),
            "Confirmed" => ledger
                .mark_solana_refund_confirmed(request_id, 4_000)
                .unwrap(),
            _ => {}
        }
        let err = ledger
            .resume_manual_review_sol_to_glc(request_id, "try resume", "operator", 5_000)
            .unwrap_err();
        assert!(
            matches!(err, LedgerError::RefundLifecycleExists { .. }),
            "stage {stage}: got {err}"
        );
    }
    // Defense in depth: even with the STATE shoved back to ManualReview
    // out-of-band, the refund ROW alone still blocks resume.
    ledger
        .conn
        .execute(
            "UPDATE bridge_requests SET state = 'ManualReview' WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "try resume", "operator", 6_000)
        .unwrap_err();
    assert!(matches!(err, LedgerError::RefundLifecycleExists { .. }));
}

#[test]
fn refund_lifecycle_blocks_goldcoin_payout_creation_at_the_boundary() {
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    let verified = verified_for(&ledger, request_id);
    ledger
        .begin_solana_refund(request_id, &verified, "refund", "cli:test", 2_000)
        .unwrap();
    let plan = crate::goldcoin::payout::PayoutPlan {
        inputs: vec![],
        input_contexts: vec![],
        dest_p2pkh_hash: [0u8; 20],
        payout_atomic: 1,
        change_outputs: vec![],
        vault_script_pubkey: vec![],
        fee_atomic: 0,
    };
    let err = ledger
        .record_goldcoin_payout_built(request_id, &plan, [0u8; 32], "00", 3_000)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::RefundLifecycleExists { .. }),
        "got: {err}"
    );
}

#[test]
fn pending_destination_settlement_amount_explains_broadcast_refunds_until_confirmed() {
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    let verified = verified_for(&ledger, request_id);
    ledger
        .begin_solana_refund(request_id, &verified, "refund", "cli:test", 2_000)
        .unwrap();
    // Pending: intent only, nothing can have left the chain yet.
    assert_eq!(
        ledger
            .pending_destination_settlement_amount(ReserveDirection::SolanaReserve, 2_500)
            .unwrap(),
        0
    );
    ledger
        .record_solana_refund_broadcast(request_id, "sig", "hash", 0, 3_000)
        .unwrap();
    assert_eq!(
        ledger
            .pending_destination_settlement_amount(ReserveDirection::SolanaReserve, 3_500)
            .unwrap(),
        100_000,
        "a broadcast refund must explain its own on-chain drop"
    );
    ledger
        .mark_solana_refund_confirmed(request_id, 4_000)
        .unwrap();
    assert_eq!(
        ledger
            .pending_destination_settlement_amount(ReserveDirection::SolanaReserve, 4_500)
            .unwrap(),
        0,
        "once the book itself is debited the explanation term must retire"
    );
}

#[test]
fn rate_limited_park_holds_no_reservation_and_is_refundable() {
    // Pins the whitelist's premise for the two rate-limit reasons
    // (decision 2026-09-01): a rate-limited park is a FINALIZED deposit
    // parked BEFORE any Goldcoin capacity was reserved.
    let mut ledger = setup();
    // First deposit from wallet [1;32] is admitted normally and reserves.
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!("expected a normal fold")
    };
    let (_, _, reserved_after_first, pending_after_first) = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    // Second deposit from the SAME wallet inside the window parks.
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(1, amounts(50_000), [1u8; 32], &[3u8; 32], None, 2_000)
        .unwrap()
    else {
        panic!("expected the second same-wallet fold to park")
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some("wallet_source_24h_limit")
    );
    assert!(request.source_finalized_at.is_some());
    let (_, _, reserved_after_park, pending_after_park) = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(reserved_after_park, reserved_after_first);
    assert_eq!(pending_after_park, pending_after_first);

    let verified = verified_for(&ledger, request_id);
    ledger
        .begin_solana_refund(
            request_id,
            &verified,
            "refund rate-limited park",
            "cli:test",
            3_000,
        )
        .unwrap();
    let (_, _, reserved_after_begin, pending_after_begin) = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(reserved_after_begin, reserved_after_first);
    assert_eq!(pending_after_begin, pending_after_first);
}

#[test]
fn double_begin_is_refused_and_leaves_one_row() {
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    let verified = verified_for(&ledger, request_id);
    ledger
        .begin_solana_refund(request_id, &verified, "refund", "cli:test", 2_000)
        .unwrap();
    let err = ledger
        .begin_solana_refund(request_id, &verified, "refund again", "cli:test", 3_000)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::RefundNotEligible { .. }),
        "got: {err}"
    );
    assert_eq!(ledger.list_solana_refunds(false).unwrap().len(), 1);
}

#[test]
fn concurrent_begin_from_two_connections_creates_exactly_one_refund() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refund-race.sqlite");
    {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                1_000_000,
                100_000,
                500_000,
                200_000,
                150_000,
                1_000,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                1_000_000,
                100_000,
                500_000,
                200_000,
                150_000,
                1_000,
            )
            .unwrap();
        park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    }
    let request_id = 1i64;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut ledger = Ledger::open(&path).unwrap();
                let verified = verified_for(&ledger, request_id);
                barrier.wait();
                ledger.begin_solana_refund(
                    request_id,
                    &verified,
                    &format!("refund attempt {i}"),
                    "cli:test",
                    2_000,
                )
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let ok_count = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok_count, 1, "exactly one racer may begin: {results:?}");

    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(ledger.list_solana_refunds(false).unwrap().len(), 1);
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::RefundPending
    );
}

#[test]
fn refund_lifecycle_survives_restart_at_every_stage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refund-restart.sqlite");
    let request_id;
    {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                1_000_000,
                100_000,
                500_000,
                200_000,
                150_000,
                1_000,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                1_000_000,
                100_000,
                500_000,
                200_000,
                150_000,
                1_000,
            )
            .unwrap();
        request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
        let verified = verified_for(&ledger, request_id);
        ledger
            .begin_solana_refund(request_id, &verified, "refund", "cli:test", 2_000)
            .unwrap();
        // Crash after begin.
    }
    {
        let mut ledger = Ledger::open(&path).unwrap();
        let refund = ledger.get_solana_refund(request_id).unwrap().unwrap();
        assert_eq!(refund.state, SolanaRefundState::Pending);
        ledger
            .record_solana_refund_broadcast(request_id, "sig", "hash", 0, 3_000)
            .unwrap();
        // Crash after broadcast record (possibly before the actual send).
    }
    {
        let mut ledger = Ledger::open(&path).unwrap();
        let refund = ledger.get_solana_refund(request_id).unwrap().unwrap();
        assert_eq!(refund.state, SolanaRefundState::Broadcast);
        assert_eq!(refund.refund_signature.as_deref(), Some("sig"));
        assert_eq!(refund.recent_blockhash.as_deref(), Some("hash"));
        ledger
            .mark_solana_refund_confirmed(request_id, 4_000)
            .unwrap();
    }
    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Refunded
    );
    let (sol_balance, _, _, _) = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .unwrap();
    assert_eq!(sol_balance, 900_000);
}

// ------------------------------ confirmed-liquidity admission safety buffer --
//
// docs/09-runbook.md's "Confirmed-liquidity admission safety buffer".
// These tests use REAL production magnitudes (8-decimal Goldcoin atomic
// units at the shipped 250 000 / 350 000 GLC thresholds) rather than the
// small round numbers the rest of this file uses, because the policy
// numbers themselves are part of what is being pinned: a future edit that
// changes them should fail here loudly, not silently ship a different
// production posture.

/// One GLC in Goldcoin-native atomic units (8 decimals, Bitcoin-fork
/// convention — `amount_conversion`'s module docs).
const GLC: u64 = 100_000_000;
/// The shipped production close threshold — `config::
/// default_admission_safety_buffer_atomic`.
const BUFFER: u64 = 250_000 * GLC;
/// The shipped production reopen threshold — `config::
/// default_admission_reopen_headroom_atomic`.
const REOPEN: u64 = 350_000 * GLC;
/// Protected minimum for these fixtures. Deliberately large and entirely
/// separate from the buffer: the buffer sits ON TOP of it, and no test
/// here may accidentally pass because the two happen to coincide.
const PROTECTED_MIN: u64 = 1_000_000 * GLC;

/// A GoldcoinReserve configured with the production buffer thresholds and
/// a starting balance of `PROTECTED_MIN + headroom`, i.e. exactly
/// `headroom` of confirmed unreserved headroom.
fn setup_buffered(headroom: u64) -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    for direction in [
        ReserveDirection::SolanaReserve,
        ReserveDirection::GoldcoinReserve,
    ] {
        ledger
            .configure_reserve(
                direction,
                PROTECTED_MIN + headroom,
                PROTECTED_MIN,
                PROTECTED_MIN * 2,
                PROTECTED_MIN + PROTECTED_MIN / 2,
                PROTECTED_MIN + 1,
                1_000,
            )
            .unwrap();
    }
    ledger
        .set_admission_liquidity_thresholds(ReserveDirection::GoldcoinReserve, BUFFER, REOPEN)
        .unwrap();
    ledger
}

/// Moves confirmed headroom to exactly `headroom` by refreshing the
/// observed balance, the same way a reconciliation tick would — never by
/// editing the buffer columns, so every test exercises the real path.
fn set_headroom(ledger: &mut Ledger, headroom: u64, now: i64) {
    let (_, protected_minimum, reserved, _) = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    ledger
        .refresh_reserve_balance(
            ReserveDirection::GoldcoinReserve,
            protected_minimum + reserved + headroom,
            now,
        )
        .unwrap();
    assert_eq!(
        ledger
            .confirmed_admission_headroom(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        headroom as i64
    );
}

#[test]
fn next_liquidity_admission_closed_is_the_one_pure_hysteresis_rule() {
    let (b, r) = (BUFFER as i64, REOPEN as i64);

    // Disabled: never closes, at any headroom, however negative.
    assert!(!Ledger::next_liquidity_admission_closed(false, -1, 0, 0));
    assert!(!Ledger::next_liquidity_admission_closed(true, -1, 0, r));

    // Open -> closed strictly below the close threshold; exactly AT it
    // stays open (the policy is "drops below 250 000").
    assert!(!Ledger::next_liquidity_admission_closed(false, b, b, r));
    assert!(Ledger::next_liquidity_admission_closed(false, b - 1, b, r));

    // Closed -> open only at or above the REOPEN threshold. Everything in
    // the band between the two holds the closed state.
    assert!(Ledger::next_liquidity_admission_closed(true, b, b, r));
    assert!(Ledger::next_liquidity_admission_closed(true, r - 1, b, r));
    assert!(!Ledger::next_liquidity_admission_closed(true, r, b, r));

    // The band, from the other side: the same headroom yields opposite
    // answers depending on which state it is evaluated from. That
    // asymmetry IS the hysteresis.
    let mid = (b + r) / 2;
    assert!(!Ledger::next_liquidity_admission_closed(false, mid, b, r));
    assert!(Ledger::next_liquidity_admission_closed(true, mid, b, r));
}

#[test]
fn admission_is_accepted_while_headroom_stays_above_the_safety_buffer() {
    // 400 000 GLC headroom, admitting a 100 000 GLC obligation leaves
    // 300 000 — comfortably above the 250 000 buffer.
    let mut ledger = setup_buffered(400_000 * GLC);
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(
            0,
            amounts(100_000 * GLC),
            [1u8; 32],
            &[2u8; 32],
            None,
            1_000,
        )
        .unwrap()
    else {
        panic!("headroom above the buffer must admit normally")
    };

    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::SourceFinalized);
    assert!(request.manual_review_note.is_none());
    assert!(!ledger
        .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
    // Capacity really was committed — an "accepted" fold that reserved
    // nothing would pass every other assertion here.
    assert_eq!(
        ledger
            .confirmed_admission_headroom(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        (300_000 * GLC) as i64
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

#[test]
fn admission_closes_and_parks_once_headroom_drops_below_the_safety_buffer() {
    // 240 000 GLC — below the 250 000 buffer, but the reserve is still
    // entirely solvent and nowhere near the protected minimum.
    let mut ledger = setup_buffered(240_000 * GLC);
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(1_000 * GLC), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!("headroom below the buffer must park, not admit")
    };

    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::ManualReview);
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some("liquidity_buffer_low_at_fold"),
        "the buffer park must be its own reason, distinguishable from \
         insufficient_capacity_at_fold and utxo_liquidity_low_at_fold"
    );
    assert!(ledger
        .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
    // Parked, therefore uncommitted: headroom is untouched.
    assert_eq!(
        ledger
            .confirmed_admission_headroom(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        (240_000 * GLC) as i64
    );
    // And the deposit is never dropped — the Solana-side tokens are
    // already locked, so the row must exist and stay refundable/resumable.
    assert!(Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS
        .contains(&request.manual_review_note.as_deref().unwrap()));
}

#[test]
fn a_request_that_would_eat_into_the_buffer_is_held_back_while_smaller_ones_flow() {
    // The per-request half of the policy: `protected_minimum +
    // reserved_liquidity + incoming amount + buffer`. Headroom is 300 000
    // — above the close threshold, so the direction-wide gate stays OPEN
    // — but a 60 000 obligation would push it to 240 000.
    let mut ledger = setup_buffered(300_000 * GLC);
    let SolFoldOutcome::FoldedManualReview { request_id: big } = ledger
        .fold_sol_deposit(0, amounts(60_000 * GLC), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!("an obligation that would breach the buffer must park")
    };
    assert_eq!(
        ledger
            .get_request(big)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("liquidity_buffer_low_at_fold")
    );
    assert!(
        !ledger
            .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        "one oversized request must not close the direction-wide gate — headroom itself \
         never fell below the close threshold"
    );

    // A smaller one, against unchanged headroom, still goes through:
    // 300 000 - 20 000 = 280 000, still above the buffer.
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(1, amounts(20_000 * GLC), [3u8; 32], &[4u8; 32], None, 1_100)
        .unwrap()
    else {
        panic!("a request that leaves the buffer intact must still be admitted")
    };
}

#[test]
fn immature_own_payout_change_is_never_counted_as_admission_headroom() {
    // Headroom is 240 000 confirmed — below the buffer. The vault ALSO
    // physically holds 200 000 GLC of this service's own broadcast payout
    // change, which is known, accounted for, and provably not missing...
    // and still must not buy any admission room, because it cannot be
    // spent yet.
    let mut ledger = setup_buffered(240_000 * GLC);
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(1_000 * GLC), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    ledger
        .raw()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, txid, state, built_at, broadcast_at, confirmations)
             VALUES (?1, X'00', 1000, ?2, 100, X'00', X'AABB', 'Broadcast', 1, 1, 0)",
            rusqlite::params![request_id, (200_000 * GLC) as i64],
        )
        .unwrap();
    ledger
        .raw()
        .execute(
            "INSERT INTO vault_utxos
                (txid, vout, amount_atomic, script_pubkey_hex, confirmations, first_seen_at, state)
             VALUES (X'AABB', 1, ?1, '51', 1, 1, 'Unconfirmed')",
            [(200_000 * GLC) as i64],
        )
        .unwrap();

    // The value is genuinely there and genuinely recognized as our own...
    assert_eq!(
        ledger.own_unconfirmed_change_atomic(2_000).unwrap(),
        200_000 * GLC
    );
    assert_eq!(
        ledger.immature_vault_utxo_total().unwrap(),
        200_000 * GLC,
        "precondition: the immature change really is visible to the ledger"
    );
    // ...and contributes exactly nothing to admission headroom. Counting
    // it would have put headroom at 440 000, above even the reopen mark.
    assert_eq!(
        ledger
            .confirmed_admission_headroom(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        (240_000 * GLC) as i64
    );
    let gate = ledger
        .evaluate_liquidity_admission_gate(ReserveDirection::GoldcoinReserve, 2_000)
        .unwrap();
    assert!(gate.closed);
    assert_eq!(gate.headroom, (240_000 * GLC) as i64);
    let SolFoldOutcome::FoldedManualReview { request_id: second } = ledger
        .fold_sol_deposit(1, amounts(1_000 * GLC), [3u8; 32], &[4u8; 32], None, 2_000)
        .unwrap()
    else {
        panic!("immature change must not reopen admission")
    };
    assert_eq!(
        ledger
            .get_request(second)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("liquidity_buffer_low_at_fold")
    );
}

#[test]
fn existing_obligations_keep_processing_after_admission_closes() {
    // Accept one obligation while headroom is healthy.
    let mut ledger = setup_buffered(400_000 * GLC);
    let SolFoldOutcome::FoldedFinalized {
        request_id: accepted,
    } = ledger
        .fold_sol_deposit(0, amounts(50_000 * GLC), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    let before = ledger.get_request(accepted).unwrap().unwrap();
    let (_, _, reserved_before, pending_before) = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();

    // Now drop confirmed headroom below the buffer and let a new deposit
    // arrive.
    set_headroom(&mut ledger, 100_000 * GLC, 2_000);
    let SolFoldOutcome::FoldedManualReview { request_id: parked } = ledger
        .fold_sol_deposit(1, amounts(1_000 * GLC), [3u8; 32], &[4u8; 32], None, 2_000)
        .unwrap()
    else {
        panic!("admission must be closed now")
    };
    assert!(ledger
        .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    // The already-accepted obligation is untouched in every respect: same
    // state, same amounts, same note, still holding its reservation and
    // its irreversible commitment. Closure parks NEW demand; it never
    // reaches back into what was already accepted.
    let after = ledger.get_request(accepted).unwrap().unwrap();
    assert_eq!(after.state, RequestState::SourceFinalized);
    assert_eq!(after.state, before.state);
    assert_eq!(after.net_destination_atomic, before.net_destination_atomic);
    assert!(after.manual_review_note.is_none());
    let (_, _, reserved_after, pending_after) = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(reserved_after, reserved_before);
    assert_eq!(pending_after, pending_before);

    // And nothing was cancelled: both rows still exist, in exactly the
    // two states they should be in.
    assert_eq!(
        ledger.get_request(parked).unwrap().unwrap().state,
        RequestState::ManualReview
    );
    assert_eq!(
        ledger
            .requests_by_state(Direction::SolToGlc, RequestState::Cancelled)
            .unwrap()
            .len(),
        0,
        "admission closure must never cancel a request"
    );
    assert_eq!(
        ledger
            .requests_by_state(Direction::SolToGlc, RequestState::SourceFinalized)
            .unwrap()
            .len(),
        1,
        "the accepted obligation must still be queued for normal processing"
    );

    // The whole point of the buffer: closing happened while the reserve
    // was, and remains, entirely solvent — so payout processing of the
    // accepted obligation has real, confirmed liquidity behind it.
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

#[test]
fn admission_reopens_only_once_headroom_reaches_the_reopen_threshold() {
    let mut ledger = setup_buffered(240_000 * GLC);
    assert!(
        ledger
            .evaluate_liquidity_admission_gate(ReserveDirection::GoldcoinReserve, 1_000)
            .unwrap()
            .closed
    );

    // Recovering back over the CLOSE threshold is not enough — that is
    // the entire difference between this and a single-threshold design.
    for headroom in [250_000 * GLC, 300_000 * GLC, 349_999 * GLC + 99_999_999] {
        set_headroom(&mut ledger, headroom, 2_000);
        let gate = ledger
            .evaluate_liquidity_admission_gate(ReserveDirection::GoldcoinReserve, 2_000)
            .unwrap();
        assert!(
            gate.closed,
            "must stay closed at headroom {headroom} — one atomic unit below the reopen \
             threshold is still below it"
        );
        assert!(!gate.transitioned);
    }

    // Exactly 350 000 GLC reopens it, and not a unit sooner.
    set_headroom(&mut ledger, REOPEN, 3_000);
    let gate = ledger
        .evaluate_liquidity_admission_gate(ReserveDirection::GoldcoinReserve, 3_000)
        .unwrap();
    assert!(!gate.closed);
    assert!(gate.transitioned);
    assert!(!ledger
        .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    // And admission really is live again, not merely flagged open.
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(0, amounts(1_000 * GLC), [1u8; 32], &[2u8; 32], None, 3_000)
        .unwrap()
    else {
        panic!("a reopened gate must admit")
    };
}

#[test]
fn the_hysteresis_band_never_flaps() {
    // Every headroom used here sits strictly inside the 250 000..350 000
    // band, where the gate must hold whatever state it is in. If either
    // threshold were ever compared against the wrong state, one of these
    // 40 evaluations would flip.
    let band = [
        250_000 * GLC,
        260_000 * GLC,
        299_999 * GLC,
        340_000 * GLC,
        REOPEN - 1,
    ];

    // Starting OPEN, nothing in the band closes it.
    let mut ledger = setup_buffered(400_000 * GLC);
    let mut now = 1_000;
    for round in 0..4 {
        for headroom in band {
            now += 1;
            set_headroom(&mut ledger, headroom, now);
            let gate = ledger
                .evaluate_liquidity_admission_gate(ReserveDirection::GoldcoinReserve, now)
                .unwrap();
            assert!(
                !gate.closed && !gate.transitioned,
                "round {round}: an open gate must hold open at headroom {headroom}"
            );
        }
    }

    // One genuine dip below the close threshold closes it, exactly once.
    set_headroom(&mut ledger, 249_999 * GLC, now);
    assert!(
        ledger
            .evaluate_liquidity_admission_gate(ReserveDirection::GoldcoinReserve, now)
            .unwrap()
            .transitioned
    );

    // Starting CLOSED, nothing in the band reopens it.
    for round in 0..4 {
        for headroom in band {
            now += 1;
            set_headroom(&mut ledger, headroom, now);
            let gate = ledger
                .evaluate_liquidity_admission_gate(ReserveDirection::GoldcoinReserve, now)
                .unwrap();
            assert!(
                gate.closed && !gate.transitioned,
                "round {round}: a closed gate must hold closed at headroom {headroom}"
            );
        }
    }

    // Exactly one transition in each direction over the whole run.
    set_headroom(&mut ledger, REOPEN, now);
    assert!(
        ledger
            .evaluate_liquidity_admission_gate(ReserveDirection::GoldcoinReserve, now)
            .unwrap()
            .transitioned
    );
    assert!(
        !ledger
            .evaluate_liquidity_admission_gate(ReserveDirection::GoldcoinReserve, now)
            .unwrap()
            .transitioned,
        "re-evaluating an unchanged state must be a no-op"
    );
}

#[test]
fn the_hard_invariant_and_available_capacity_are_unchanged_by_the_buffer() {
    // A reserve deep inside the buffer — admission firmly closed — still
    // satisfies the hard invariant, and reports the same capacity figure
    // it always did. The buffer is an admission gate layered on top of
    // `protected_minimum`, never a term inside it.
    let mut ledger = setup_buffered(GLC);
    let gate = ledger
        .evaluate_liquidity_admission_gate(ReserveDirection::GoldcoinReserve, 1_000)
        .unwrap();
    assert!(gate.closed);
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        GLC as i64
    );
    assert_eq!(
        ledger
            .confirmed_admission_headroom(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        "headroom and available_capacity must be the same figure, not two that can drift"
    );

    // The invariant still fails on a GENUINE breach, and fails for the
    // pre-existing reason — the buffer never widens or narrows it.
    ledger
        .refresh_reserve_balance(ReserveDirection::GoldcoinReserve, PROTECTED_MIN - 1, 2_000)
        .unwrap();
    assert!(matches!(
        ledger
            .check_invariant(ReserveDirection::GoldcoinReserve)
            .unwrap_err(),
        LedgerError::InvariantViolated { .. }
    ));

    // Same reserve state, buffer disabled: identical invariant answer.
    ledger
        .set_admission_liquidity_thresholds(ReserveDirection::GoldcoinReserve, 0, 0)
        .unwrap();
    assert!(matches!(
        ledger
            .check_invariant(ReserveDirection::GoldcoinReserve)
            .unwrap_err(),
        LedgerError::InvariantViolated { .. }
    ));
}

#[test]
fn an_unconfigured_buffer_reproduces_pre_buffer_admission_behavior_exactly() {
    // The ledger default is `(0, 0)` — every database that predates this
    // feature, and every test fixture that never configures it. Admission
    // must then be governed purely by the pre-existing capacity check: a
    // request fitting in headroom is admitted no matter how thin what is
    // left behind would be.
    let mut ledger = setup();
    assert_eq!(
        ledger
            .admission_liquidity_thresholds(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        (0, 0)
    );
    // Headroom is 900_000; take all but one unit of it.
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(0, amounts(899_999), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!("with the buffer disabled, anything that fits must still be admitted")
    };
    assert!(!ledger
        .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
    let gate = ledger
        .evaluate_liquidity_admission_gate(ReserveDirection::GoldcoinReserve, 2_000)
        .unwrap();
    assert!(!gate.closed && !gate.transitioned);
}

#[test]
fn the_buffer_never_governs_glc_to_sol_or_the_solana_reserve() {
    // The buffer is scoped to SolToGlc admission. `create_request`
    // (GlcToSol — destination SolanaReserve) must be completely
    // unaffected, and the SolanaReserve gate must never close.
    let mut ledger = setup_buffered(240_000 * GLC);
    // Takes SolanaReserve's headroom down to exactly zero — far past
    // anything the buffer would allow if it applied here.
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(240_000 * GLC),
            &[9u8; 32],
            None,
            600,
            1_000,
        )
        .unwrap();
    assert!(
        matches!(outcome, CreateRequestOutcome::Reserved { .. }),
        "GlcToSol admission must not consult the Goldcoin admission buffer"
    );
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        0
    );
    let gate = ledger
        .evaluate_liquidity_admission_gate(ReserveDirection::SolanaReserve, 1_000)
        .unwrap();
    assert!(!gate.closed);
}

#[test]
fn resume_is_held_back_by_the_same_buffer_and_succeeds_once_headroom_recovers() {
    // A resume re-admits real demand exactly as a fresh fold would, so it
    // is judged by the same arithmetic — the identical posture the
    // count-based UTXO floor already takes.
    let mut ledger = setup_buffered(240_000 * GLC);
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(1_000 * GLC), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "too early", "operator", 2_000)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::AdmissionLiquidityBufferLow { .. }),
        "got {err:?}"
    );
    // Refused with NO mutation — the request is exactly as it was.
    let parked = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(parked.state, RequestState::ManualReview);
    assert_eq!(
        parked.manual_review_note.as_deref(),
        Some("liquidity_buffer_low_at_fold")
    );

    // Transient, not terminal: the identical call succeeds once confirmed
    // headroom can carry the request AND leave the buffer intact.
    set_headroom(&mut ledger, 260_000 * GLC, 3_000);
    assert_eq!(
        ledger
            .resume_manual_review_sol_to_glc(request_id, "headroom recovered", "operator", 3_000)
            .unwrap(),
        ResumeManualReviewOutcome::Resumed
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );
}

#[test]
fn a_buffer_parked_request_stays_refundable() {
    // The buffer park must never become the one fold-time reason with no
    // exit: a deposit that will genuinely never be paid out has to remain
    // refundable to its original Solana depositor.
    assert!(Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS.contains(&"liquidity_buffer_low_at_fold"));
    // Same requirement for the route-scoped admission park (v25): a
    // route an operator closed may stay closed indefinitely, so a
    // deposit parked by it must keep its refund path.
    assert!(Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS.contains(&"route_admission_closed_at_fold"));
    assert_eq!(
        Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS.len(),
        10,
        "every fold-time park reason must be refundable — a new one added without a refund \
         path would strand real, irreversible deposits"
    );
}

#[test]
fn set_admission_liquidity_thresholds_refuses_a_reopen_below_the_close_threshold() {
    let mut ledger = setup();
    let err = ledger
        .set_admission_liquidity_thresholds(
            ReserveDirection::GoldcoinReserve,
            350_000 * GLC,
            250_000 * GLC,
        )
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::InvalidAdmissionThresholds { .. }),
        "an inverted pair cannot express hysteresis and must be refused, not stored"
    );
    // Nothing was written.
    assert_eq!(
        ledger
            .admission_liquidity_thresholds(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        (0, 0)
    );
    // Equal thresholds are degenerate but coherent, and allowed.
    ledger
        .set_admission_liquidity_thresholds(ReserveDirection::GoldcoinReserve, GLC, GLC)
        .unwrap();
}

#[test]
fn check_liquidity_buffer_for_admission_refuses_an_operator_reopen_while_the_gate_is_closed() {
    // `glc-admin open-admission` must not silently succeed into a state
    // where every new fold still parks.
    let mut ledger = setup_buffered(240_000 * GLC);
    let err = ledger
        .check_liquidity_buffer_for_admission(ReserveDirection::GoldcoinReserve, 1_000)
        .unwrap_err();
    assert!(
        matches!(
            err,
            LedgerError::LiquidityAdmissionClosedForAdmission { .. }
        ),
        "got {err:?}"
    );
    // Solana has no admission buffer and must always pass.
    ledger
        .check_liquidity_buffer_for_admission(ReserveDirection::SolanaReserve, 1_000)
        .unwrap();
    // Once genuinely recovered, the same check passes.
    set_headroom(&mut ledger, REOPEN, 2_000);
    ledger
        .check_liquidity_buffer_for_admission(ReserveDirection::GoldcoinReserve, 2_000)
        .unwrap();
}

// ------------------------------ ManualReview -> L1 settlement recovery --

/// The dry run is a TRIAL of the real function, rolled back. It must
/// leave absolutely nothing behind: no state change, no reserved
/// liquidity, no pending obligations, no state-log row, no audit row.
#[test]
fn settle_dry_run_is_strictly_read_only() {
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    let before = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    let state_log_before = ledger.state_log(request_id).unwrap().len();

    // Admission was closed by the park helper; reopen so the trial can
    // genuinely succeed — that is the interesting case for read-only-ness.
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopen"))
        .unwrap();

    let outcome = ledger
        .dry_run_resume_manual_review(request_id, 5_000)
        .unwrap();
    assert_eq!(outcome, ResumeDryRunOutcome::WouldResume);

    // Nothing moved.
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview,
        "a dry run must not change the request state"
    );
    assert_eq!(
        ledger
            .reserve_snapshot(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        before,
        "a dry run must not move any reserve counter"
    );
    assert_eq!(
        ledger.state_log(request_id).unwrap().len(),
        state_log_before,
        "a dry run must not write a state-log row"
    );
    assert!(
        ledger
            .list_admin_audit(&AdminAuditFilter::default())
            .unwrap()
            .is_empty(),
        "a dry run must not write an audit row"
    );

    // And the real execution still works afterwards — the rollback left
    // no lock or residue behind.
    assert_eq!(
        ledger
            .resume_manual_review_sol_to_glc(request_id, "real", "operator", 6_000)
            .unwrap(),
        ResumeManualReviewOutcome::Resumed
    );
    let after = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(after.2, before.2 + 100_000, "reserved_liquidity");
    assert_eq!(after.3, before.3 + 100_000, "pending_obligations");
}

/// The dry run reports the same refusal the real call would, verbatim —
/// because it IS the real call. Exercised across the distinct refusal
/// classes so the two can never diverge.
#[test]
fn settle_dry_run_reports_the_same_refusal_the_real_call_would() {
    // Non-whitelisted reason.
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    ledger
        .conn
        .execute(
            "UPDATE bridge_requests SET manual_review_note = 'deposit_spent_before_finalized'
             WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    let dry = ledger
        .dry_run_resume_manual_review(request_id, 5_000)
        .unwrap();
    let real = ledger
        .resume_manual_review_sol_to_glc(request_id, "n", "operator", 5_000)
        .unwrap_err();
    assert_eq!(
        dry,
        ResumeDryRunOutcome::WouldRefuse {
            reason: real.to_string()
        }
    );

    // A refund lifecycle blocks recovery, and the dry run says so.
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    let verified = verified_for(&ledger, request_id);
    ledger
        .begin_solana_refund(request_id, &verified, "refunding", "cli:test", 4_000)
        .unwrap();
    let dry = ledger
        .dry_run_resume_manual_review(request_id, 5_000)
        .unwrap();
    match dry {
        ResumeDryRunOutcome::WouldRefuse { reason } => {
            assert!(reason.contains("refund lifecycle"), "got: {reason}")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A request already recovered reports the idempotent no-op, not a
/// refusal — matching what an execute would do.
#[test]
fn settle_dry_run_reports_already_resumed_after_recovery() {
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    ledger
        .resume_manual_review_sol_to_glc(request_id, "first", "operator", 5_000)
        .unwrap();
    let dry = ledger
        .dry_run_resume_manual_review(request_id, 6_000)
        .unwrap();
    assert_eq!(
        dry,
        ResumeDryRunOutcome::AlreadyResumed {
            state: RequestState::SourceFinalized
        }
    );
}

/// The mutual exclusion in the other direction: a request recovered for
/// L1 settlement can never start a refund lifecycle.
#[test]
fn a_recovered_request_can_never_be_refunded() {
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 100_000, [1u8; 32], &[2u8; 32]);
    ledger
        .resume_manual_review_sol_to_glc(request_id, "recover for L1", "operator", 5_000)
        .unwrap();
    let verified = verified_for(&ledger, request_id);
    let err = ledger
        .begin_solana_refund(request_id, &verified, "try refund", "cli:test", 6_000)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::RefundNotEligible { .. }),
        "got: {err}"
    );
    assert!(ledger.get_solana_refund(request_id).unwrap().is_none());
}

/// The drift guard that matters: `RECOVERABLE_MANUAL_REVIEW_REASONS` and
/// the resume path must agree in BOTH directions, over the whole universe
/// of `manual_review_note` values this codebase can write.
///
/// The one-directional version of this test (every listed reason is
/// accepted — still asserted below) passed throughout the two days
/// `liquidity_buffer_low_at_fold` was accepted by
/// `resume_manual_review_sol_to_glc` while absent from the constant, so
/// `manual-review-settle-list` hid three recoverable production requests
/// that `manual-review-settle` reported as ELIGIBLE. Only the reverse
/// direction — every reason the resume path ACCEPTS must be on the list —
/// catches that, and it is what this test adds.
///
/// The reason universe is enumerated here on purpose. It is the one place
/// the check cannot be derived from the code under test without asking
/// the code under test what it accepts, which would make the test vacuous.
#[test]
fn resume_acceptance_matches_the_recoverable_reason_list() {
    // Every `manual_review_note` any path in this codebase writes, plus a
    // never-written string. A new one added to `fold_sol_deposit` (or
    // anywhere else) must be added here too — at which point this test
    // states, in one place, whether recovery accepts it.
    const ALL_KNOWN_REASONS: [&str; 13] = [
        "admission_closed_at_fold",
        "route_admission_closed_at_fold",
        "reserve_paused_at_fold",
        "insufficient_capacity_at_fold",
        "utxo_liquidity_low_at_fold",
        "liquidity_buffer_low_at_fold",
        "wallet_destination_24h_limit",
        "wallet_source_24h_limit",
        // The pre-generalization spellings of the two wallet-window
        // parks, as they stand on every row parked before the rule was
        // generalized — still recoverable, still refundable.
        "recipient_rate_limited",
        "source_wallet_rate_limited",
        "late_deposit_no_capacity",
        "deposit_spent_before_finalized",
        "some_future_reason_nobody_has_written_yet",
    ];

    // 1. The CONTENT, stated once, explicitly. The refactor that made
    //    `resume_manual_review_sol_to_glc` read this very constant means
    //    the trial loop below can no longer catch an entry being dropped
    //    (drop it and both sides refuse, in agreement). This is what
    //    catches that: these seven are every reason `fold_sol_deposit`
    //    can write for a SolToGlc park, and every one of them is a park
    //    that happened INSTEAD of reserving capacity, on an
    //    already-finalized deposit — so every one of them is recoverable.
    const FOLD_TIME_PARK_REASONS: [&str; 10] = [
        "admission_closed_at_fold",
        // The route-scoped twin of the reserve-wide reason above (v25).
        // Same premises: a park that happened INSTEAD of reserving
        // capacity, on an already-finalized deposit.
        "route_admission_closed_at_fold",
        "reserve_paused_at_fold",
        "insufficient_capacity_at_fold",
        "utxo_liquidity_low_at_fold",
        "liquidity_buffer_low_at_fold",
        "wallet_destination_24h_limit",
        "wallet_source_24h_limit",
        // Legacy spellings of the two above: never written any more,
        // but every row parked under them before the generalization
        // must keep both exits.
        "recipient_rate_limited",
        "source_wallet_rate_limited",
    ];
    for reason in FOLD_TIME_PARK_REASONS {
        assert!(
            Ledger::RECOVERABLE_MANUAL_REVIEW_REASONS.contains(&reason),
            "{reason:?} is a SolToGlc fold-time park and must be recoverable"
        );
    }
    assert_eq!(
        Ledger::RECOVERABLE_MANUAL_REVIEW_REASONS.len(),
        FOLD_TIME_PARK_REASONS.len(),
        "a reason was added to the recoverable list without being listed here"
    );

    // 2. Cross-check against the OTHER, independently maintained list of
    //    the same fold-time reasons. `REFUNDABLE_MANUAL_REVIEW_REASONS`
    //    received `liquidity_buffer_low_at_fold` when the admission
    //    safety buffer landed and this one did not, which is precisely
    //    the shape of the production defect. The two answer different
    //    questions (pay out vs. give back) but range over the same set:
    //    a fold-time park is either, and never only one.
    for reason in Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS {
        assert!(
            Ledger::RECOVERABLE_MANUAL_REVIEW_REASONS.contains(&reason),
            "{reason:?} is refundable but not recoverable — the two fold-time reason lists have \
             diverged, which is exactly how the settlement listing went stale"
        );
    }
    for reason in Ledger::RECOVERABLE_MANUAL_REVIEW_REASONS {
        assert!(
            Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS.contains(&reason),
            "{reason:?} is recoverable but not refundable — a park with only one exit"
        );
    }

    // 3. And the enforced path agrees with the constant in both
    //    directions, trialled through the real function.
    for (i, reason) in ALL_KNOWN_REASONS.iter().enumerate() {
        let mut ledger = setup();
        // Distinct wallet/recipient per case so neither 24h rate limiter
        // can confound the reason under test.
        let tag = (i + 1) as u8;
        let request_id = park_sol_request(&mut ledger, i as u64, 10_000, [tag; 32], &[tag; 32]);
        ledger
            .conn
            .execute(
                "UPDATE bridge_requests SET manual_review_note = ?1 WHERE id = ?2",
                rusqlite::params![reason, request_id],
            )
            .unwrap();
        ledger
            .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopen"))
            .unwrap();

        let listed = Ledger::RECOVERABLE_MANUAL_REVIEW_REASONS.contains(reason);
        let accepted = matches!(
            ledger
                .dry_run_resume_manual_review(request_id, 5_000)
                .unwrap(),
            ResumeDryRunOutcome::WouldResume
        );
        assert_eq!(
            listed, accepted,
            "reason {reason:?}: RECOVERABLE_MANUAL_REVIEW_REASONS says listed={listed} but the \
             real resume path says accepted={accepted}. The listing and the enforced policy have \
             drifted — every discovery surface filters on that constant."
        );
        // And the shared predicate the two now share agrees with both.
        assert_eq!(
            Ledger::is_recoverable_manual_review_reason(Some(reason)),
            listed,
            "the shared predicate must be the constant, for {reason:?}"
        );
    }
    // A NULL note is never recoverable.
    assert!(!Ledger::is_recoverable_manual_review_reason(None));
}

/// The original one-directional guard, kept: every listed entry is
/// genuinely accepted by the real resume path (trialled, not asserted
/// from a copy of the rules).
#[test]
fn recoverable_reason_list_matches_what_resume_accepts() {
    for (i, reason) in Ledger::RECOVERABLE_MANUAL_REVIEW_REASONS.iter().enumerate() {
        let mut ledger = setup();
        // Distinct wallet/recipient per case so the 24h rate limiters
        // never confound the reason under test.
        let tag = (i + 1) as u8;
        let request_id = park_sol_request(&mut ledger, i as u64, 10_000, [tag; 32], &[tag; 32]);
        ledger
            .conn
            .execute(
                "UPDATE bridge_requests SET manual_review_note = ?1 WHERE id = ?2",
                rusqlite::params![reason, request_id],
            )
            .unwrap();
        ledger
            .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopen"))
            .unwrap();

        assert_eq!(
            ledger
                .dry_run_resume_manual_review(request_id, 5_000)
                .unwrap(),
            ResumeDryRunOutcome::WouldResume,
            "reason {reason:?} is on RECOVERABLE_MANUAL_REVIEW_REASONS but the resume path \
             refuses it"
        );
    }

    // A reason deliberately NOT on the list must be refused, or the
    // constant would be under-restrictive rather than merely stale.
    let mut ledger = setup();
    let request_id = park_sol_request(&mut ledger, 0, 10_000, [9u8; 32], &[9u8; 32]);
    ledger
        .conn
        .execute(
            "UPDATE bridge_requests SET manual_review_note = 'deposit_spent_before_finalized'
             WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopen"))
        .unwrap();
    assert!(!Ledger::RECOVERABLE_MANUAL_REVIEW_REASONS.contains(&"deposit_spent_before_finalized"));
    match ledger
        .dry_run_resume_manual_review(request_id, 5_000)
        .unwrap()
    {
        ResumeDryRunOutcome::WouldRefuse { .. } => {}
        other => panic!("a non-listed reason must be refused, got {other:?}"),
    }
}

// ------------------------------------ blocker I: the route-aware deposit --
//
// The Goldcoin deposit pipeline is shared by BOTH Goldcoin-sourced
// directions. These tests pin the two halves of that: `GlcToSol` behaves
// exactly as it always did, and `GlcToRhn` gets the identical protections
// rather than a parallel, weaker copy of them.

/// A Robinhood reserve alongside the two `setup` configures, so a
/// `GlcToRhn` request has somewhere to reserve capacity. Canonical
/// 8-decimal units, like every other reserve row.
fn setup_with_robinhood_reserve() -> Ledger {
    let mut ledger = setup();
    ledger
        .configure_reserve(
            ReserveDirection::RobinhoodReserve,
            1_000_000,
            100_000,
            500_000,
            200_000,
            150_000,
            1_000,
        )
        .unwrap();
    ledger
}

/// A 20-byte EVM recipient, the shape a `GlcToRhn` payout requires.
const TEST_EVM_RECIPIENT: [u8; 20] = [0xE1; 20];

fn create_glc_to_rhn_request(ledger: &mut Ledger) -> i64 {
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToRhn,
            amounts(100_000),
            &TEST_EVM_RECIPIENT,
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected Reserved")
    };
    request_id
}

/// The SQL list and the Rust predicate must name the same set. They are
/// two spellings of one rule, used in different languages, and nothing
/// but this test holds them together — a fifth direction that is
/// Goldcoin-sourced but missing from the literal would be silently
/// excluded from the deposit lookup, the watch list, the reorg sweeps and
/// the coin-selection exclusion all at once.
#[test]
fn source_is_goldcoin_sql_in_matches_the_rust_predicate() {
    let from_predicate: Vec<&str> = Direction::ALL
        .into_iter()
        .filter(|d| d.source_is_goldcoin())
        .map(|d| d.as_str())
        .collect();
    let literal = Direction::SOURCE_IS_GOLDCOIN_SQL_IN;
    assert!(
        literal.starts_with('(') && literal.ends_with(')'),
        "{literal}"
    );
    let from_sql: Vec<&str> = literal
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split(',')
        .map(|s| s.trim().trim_matches('\''))
        .collect();
    assert_eq!(from_sql, from_predicate);
}

/// The destination-side twin of the test above, and the one that pins the
/// GLOBAL Goldcoin-destination rate limit's reach. A fifth
/// inbound-to-Goldcoin direction missing from the literal would silently
/// receive a rolling-24h payout window of its own instead of sharing the
/// one window per destination address.
#[test]
fn destination_is_goldcoin_sql_in_matches_the_rust_predicate() {
    let from_predicate: Vec<&str> = Direction::ALL
        .into_iter()
        .filter(|d| d.destination_is_goldcoin())
        .map(|d| d.as_str())
        .collect();
    let literal = Direction::DESTINATION_IS_GOLDCOIN_SQL_IN;
    assert!(
        literal.starts_with('(') && literal.ends_with(')'),
        "{literal}"
    );
    let from_sql: Vec<&str> = literal
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split(',')
        .map(|s| s.trim().trim_matches('\''))
        .collect();
    assert_eq!(from_sql, from_predicate);
    assert_eq!(from_sql, vec!["SolToGlc", "RhnToGlc"]);
}

/// The shared exclude-list is the single predicate deciding which states
/// consume a rate-limit window, and it is interpolated into six queries.
/// This pins its exact membership — in particular that the refund
/// lifecycle is NOT excluded, which is the long-standing `SolToGlc`
/// behaviour every inbound route now shares.
#[test]
fn the_rate_limit_exclude_list_names_exactly_the_terminal_no_payout_states() {
    let literal = Ledger::RATE_LIMIT_EXCLUDED_STATES_SQL_IN;
    let listed: Vec<String> = literal
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split(',')
        .map(|s| s.trim().trim_matches('\'').to_string())
        .collect();
    assert_eq!(
        listed,
        vec![
            "Failed",
            "DestinationSubmissionFailed",
            "InsufficientReserveAtSettlement",
            "Cancelled",
            "Expired",
            "Reorged",
        ]
    );
    // Every entry must be a real state, or the SQL silently matches
    // nothing.
    for name in &listed {
        assert!(
            name.parse::<RequestState>().is_ok(),
            "{name} is not a RequestState"
        );
    }
    // The states that must NOT be excluded, spelled out so removing one
    // from the list above is a test failure rather than a silent policy
    // change: a refund still consumes its windows, and a park still
    // blocks the next arrival.
    for must_count in [
        RequestState::RefundPending,
        RequestState::RefundBroadcast,
        RequestState::Refunded,
        RequestState::ManualReview,
        RequestState::SourceFinalized,
        RequestState::Settled,
    ] {
        assert!(
            !listed.iter().any(|n| n == must_count.as_str()),
            "{must_count:?} must consume a rate-limit window"
        );
    }
}

/// The deposit-address binding is the moment a route becomes durable on
/// the Goldcoin side, and it accepts `GlcToRhn` on exactly the same terms
/// as `GlcToSol` — including reporting the direction back, so a caller
/// resolving an address never has to guess what it funds.
#[test]
fn set_goldcoin_deposit_address_accepts_a_glc_to_rhn_request() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = create_glc_to_rhn_request(&mut ledger);

    ledger
        .set_goldcoin_deposit_address(request_id, "Qrhn", "76a914rhn88ac", "5221...53ae")
        .unwrap();

    assert_eq!(
        ledger
            .find_goldcoin_deposit_request_by_script("76a914rhn88ac")
            .unwrap(),
        Some((request_id, Direction::GlcToRhn)),
        "the script must resolve to the request AND to the route it was created with"
    );
}

/// Fail closed the other way: a direction whose source leg is NOT a
/// Goldcoin deposit still cannot be assigned a deposit address, and the
/// widening did not quietly admit `RhnToGlc` along with `GlcToRhn`.
#[test]
fn set_goldcoin_deposit_address_rejects_an_rhn_to_glc_request() {
    let mut ledger = setup_with_robinhood_reserve();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::RhnToGlc,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected Reserved")
    };

    let err = ledger
        .set_goldcoin_deposit_address(request_id, "Qaddr", "scripthex", "redeemhex")
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::NotAGoldcoinSourcedRequest { id, actual_direction: Direction::RhnToGlc }
            if id == request_id
    ));
    assert_eq!(
        ledger
            .find_goldcoin_deposit_request_by_script("scripthex")
            .unwrap(),
        None,
        "a refused assignment must leave nothing behind"
    );
}

/// Two requests of DIFFERENT routes cannot share one deposit script. The
/// guarantee is the partial unique index, not a Rust check, so it holds
/// against a concurrent writer too — and it is what makes "the address
/// witnesses the route" true rather than merely usual.
#[test]
fn a_deposit_script_cannot_be_shared_across_two_routes() {
    let mut ledger = setup_with_robinhood_reserve();
    let glc_to_sol = create_glc_to_sol_request(&mut ledger);
    let glc_to_rhn = create_glc_to_rhn_request(&mut ledger);

    ledger
        .set_goldcoin_deposit_address(glc_to_sol, "Qa", "contested-script", "redeem")
        .unwrap();
    ledger
        .set_goldcoin_deposit_address(glc_to_rhn, "Qb", "contested-script", "redeem")
        .unwrap_err();

    assert_eq!(
        ledger
            .find_goldcoin_deposit_request_by_script("contested-script")
            .unwrap(),
        Some((glc_to_sol, Direction::GlcToSol)),
        "the first binding stands; the second must not steal or shadow it"
    );
}

/// The watch list is what puts an address in front of `list_unspent` at
/// all. A `GlcToRhn` deposit address absent from it would mean a real
/// payment the node is never asked about.
#[test]
fn the_watch_list_covers_both_goldcoin_sourced_routes() {
    let mut ledger = setup_with_robinhood_reserve();
    let glc_to_sol = create_glc_to_sol_request(&mut ledger);
    let glc_to_rhn = create_glc_to_rhn_request(&mut ledger);
    ledger
        .set_goldcoin_deposit_address(glc_to_sol, "Qsol", "script-sol", "redeem")
        .unwrap();
    ledger
        .set_goldcoin_deposit_address(glc_to_rhn, "Qrhn", "script-rhn", "redeem")
        .unwrap();

    let mut addresses = ledger.all_goldcoin_deposit_addresses().unwrap();
    addresses.sort();
    assert_eq!(addresses, vec!["Qrhn".to_string(), "Qsol".to_string()]);

    let mut scripts = ledger.all_goldcoin_deposit_script_pubkeys().unwrap();
    scripts.sort();
    assert_eq!(
        scripts,
        vec!["script-rhn".to_string(), "script-sol".to_string()]
    );
}

/// A `GlcToRhn` deposit is observed, amount-checked and advanced to
/// `Confirming` by the same call and the same rules as a `GlcToSol` one.
#[test]
fn a_glc_to_rhn_deposit_is_observed_under_the_same_rules() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = create_glc_to_rhn_request(&mut ledger);

    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert!(matches!(outcome, GlcObservationOutcome::Recorded));

    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::Confirming);
    assert_eq!(request.direction, Direction::GlcToRhn);
    assert_eq!(
        request.recipient, TEST_EVM_RECIPIENT,
        "the intended Robinhood recipient must survive the deposit unchanged"
    );
}

/// The exact-amount rule is not relaxed for the new route: a short or
/// long payment parks the request in `ManualReview` with the observed
/// amount recorded as the witness, exactly as for `GlcToSol`.
#[test]
fn a_glc_to_rhn_deposit_of_the_wrong_amount_is_parked_not_accepted() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = create_glc_to_rhn_request(&mut ledger);

    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 99_999, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert!(matches!(
        outcome,
        GlcObservationOutcome::AmountMismatch {
            expected: 100_000,
            observed: 99_999
        }
    ));
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::ManualReview);
}

/// Restart safety: re-observing the SAME outpoint is `AlreadyRecorded`,
/// not a second binding — the property a rescan depends on.
#[test]
fn re_observing_a_glc_to_rhn_deposit_is_idempotent() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = create_glc_to_rhn_request(&mut ledger);
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();

    let again = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_200)
        .unwrap();
    assert!(matches!(again, GlcObservationOutcome::AlreadyRecorded));
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Confirming
    );
}

/// A deposit can never bind to a direction with no Goldcoin source leg,
/// however it was resolved. This is the backstop under the script lookup:
/// even a caller that passed the wrong request id gets `NoMatchingRequest`
/// rather than a funded `RhnToGlc` row.
#[test]
fn a_deposit_cannot_bind_to_a_contract_sourced_request() {
    let mut ledger = setup_with_robinhood_reserve();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::RhnToGlc,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected Reserved")
    };

    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert!(matches!(outcome, GlcObservationOutcome::NoMatchingRequest));
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::AwaitingDeposit,
        "the refused observation must not have moved the request"
    );
}

/// The same stranding protection, one direction over: an unfinalized
/// `GlcToRhn` deposit's UTXO must not be offered to coin selection for an
/// unrelated Goldcoin payout.
#[test]
fn available_vault_utxos_excludes_a_utxo_backing_an_unfinalized_glc_to_rhn_deposit() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = create_glc_to_rhn_request(&mut ledger);
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();

    let backing_deposit = crate::goldcoin::coin::VaultUtxo {
        txid: [0xAA; 32],
        vout: 0,
        amount_atomic: 100_000,
        script_pubkey_hex: "51".to_string(),
    };
    let unrelated = crate::goldcoin::coin::VaultUtxo {
        txid: [0xCC; 32],
        vout: 1,
        amount_atomic: 250_000,
        script_pubkey_hex: "51".to_string(),
    };
    ledger
        .sync_vault_utxos(
            &[
                (backing_deposit.clone(), 6, "51".to_string()),
                (unrelated.clone(), 6, "51".to_string()),
            ],
            1,
            1_150,
        )
        .unwrap();

    let available = ledger.available_vault_utxos().unwrap();
    assert!(
        !available
            .iter()
            .any(|u| u.txid == backing_deposit.txid && u.vout == backing_deposit.vout),
        "must exclude the UTXO backing a not-yet-SourceFinalized GlcToRhn deposit: {available:?}"
    );
    assert!(
        available
            .iter()
            .any(|u| u.txid == unrelated.txid && u.vout == unrelated.vout),
        "must still offer an unrelated, unencumbered UTXO: {available:?}"
    );

    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();
    assert!(
        ledger
            .available_vault_utxos()
            .unwrap()
            .iter()
            .any(|u| u.txid == backing_deposit.txid && u.vout == backing_deposit.vout),
        "must offer the UTXO once its backing deposit is SourceFinalized"
    );
}

/// A `GlcToRhn` deposit in an orphaned block is reverted by the same
/// rollback sweep, and a post-finality reorg over one is DETECTED rather
/// than silently rolled back.
#[test]
fn the_reorg_sweeps_cover_a_glc_to_rhn_deposit() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = create_glc_to_rhn_request(&mut ledger);
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();

    // Pre-finality: the orphaned deposit is reverted to AwaitingDeposit.
    assert_eq!(
        ledger.detect_post_finality_reorg(9).unwrap(),
        Vec::<i64>::new(),
        "nothing is final yet, so nothing is a post-finality reorg"
    );
    assert_eq!(
        ledger
            .goldcoin_rollback_reorg(9, [0x01; 32], 12, [0x02; 32], 1_200)
            .unwrap(),
        1
    );
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::AwaitingDeposit);
    assert_eq!(request.source_txid, None);

    // Post-finality: the SAME deposit, now final, is reported as an
    // incident instead — never auto-reverted.
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_300)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 1_400).unwrap();
    assert_eq!(
        ledger.detect_post_finality_reorg(9).unwrap(),
        vec![request_id]
    );
}

// ------------------- blocker J: the Robinhood payout-not-started proof --
//
// A `GlcToRhn` Goldcoin deposit is refundable only while it is certain no
// custody payout ever started for it. The `GlcToSol` proof is
// Solana-shaped and a Robinhood payout writes none of those columns, so
// this is its own proof: no durable Robinhood payout state names the
// request. These tests drive it through every state of the Phase F
// transaction lifecycle.

/// A `GlcToRhn` request parked in `ManualReview` by the amount-mismatch
/// path — the shape an operator would be asked to refund.
fn parked_glc_to_rhn(ledger: &mut Ledger) -> i64 {
    let request_id = create_glc_to_rhn_request(ledger);
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 99_999, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview,
        "the fixture must be a request that would otherwise look refundable"
    );
    request_id
}

/// Inserts a `robinhood_transactions` row for `request_id` directly, so a
/// test can pin one exact lifecycle state without driving the whole
/// settlement engine. `extra` is appended to the column/value lists.
#[allow(clippy::too_many_arguments)]
fn seed_robinhood_tx(
    ledger: &Ledger,
    request_id: i64,
    kind: &str,
    action: i64,
    route: &str,
    state: &str,
    extra_columns: &str,
    extra_values: &str,
) -> i64 {
    let obligation = if kind == "Payout" { "NULL" } else { "0" };
    let recipient = if kind == "Settlement" {
        "NULL"
    } else {
        "X'e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1'"
    };
    let amount = if kind == "Settlement" {
        "NULL"
    } else {
        "X'0000000000000000000000000000000000000000000000000000000000000001'"
    };
    ledger
        .conn_for_tests()
        .execute_batch(&format!(
            "INSERT INTO robinhood_transactions
                (kind, request_id, route, bridge_contract, chain_id, action,
                 contract_request_id, obligation_index, recipient, amount_robinhood,
                 signer_epoch, expiry, auth_digest, state, created_at, updated_at{extra_columns})
             VALUES ('{kind}', {request_id}, '{route}',
                 X'b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1', 4663, {action},
                 X'{cid}', {obligation}, {recipient}, {amount},
                 0, 9999999999, X'{cid}', '{state}', 100, 100{extra_values});",
            cid = format!("{:02x}", request_id as u8).repeat(32),
        ))
        .unwrap_or_else(|e| panic!("seeding a {kind}/{state} row must succeed: {e}"));
    ledger.conn_for_tests().last_insert_rowid()
}

/// The signed-transaction columns the schema requires from `Signed`
/// onward: raw bytes, their hash, the envelope, and a nonce.
const SIGNED_COLUMNS: &str = ", submitter, nonce, envelope, raw_tx, tx_hash";
const SIGNED_VALUES: &str =
    ", X'5115115115115115115115115115115115115115', 7, 'eip1559', X'02f8', \
                             X'aa00000000000000000000000000000000000000000000000000000000000000'";

/// The baseline: a parked `GlcToRhn` request with NO Robinhood state at
/// all is refundable. Every other test in this section is this fixture
/// plus one piece of payout evidence, so a refusal below is attributable
/// to that evidence and nothing else.
#[test]
fn a_parked_glc_to_rhn_request_with_no_robinhood_state_is_refund_eligible() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = parked_glc_to_rhn(&mut ledger);

    let checks = ledger.glc_refund_db_checks(request_id).unwrap();
    assert_eq!(checks.direction, Some(Direction::GlcToRhn));
    assert!(checks.direction_is_goldcoin_sourced);
    assert!(
        !checks.direction_is_glc_to_sol,
        "the Solana proof must NOT be the one claimed for this route"
    );
    assert!(checks.no_robinhood_payout_started);
    assert!(checks.robinhood_payout_evidence.is_empty());
    assert_eq!(
        checks.refusal, None,
        "every database check must pass for a request with no payout state"
    );
    assert!(checks.all_passed());
}

/// Every state of the Phase F payout lifecycle blocks the refund — from
/// the row's first existence in `Authorizing`, before any custody domain
/// has been contacted, through to `Finalized`. The row is written before
/// the first signer is asked and is never deleted, so its existence alone
/// is the earliest and most permanent witness there is.
#[test]
fn every_robinhood_payout_state_blocks_a_goldcoin_refund() {
    for (state, extra_columns, extra_values) in [
        ("Authorizing", "", ""),
        ("Authorized", "", ""),
        ("Signed", SIGNED_COLUMNS, SIGNED_VALUES),
        ("Broadcast", SIGNED_COLUMNS, SIGNED_VALUES),
        ("Included", SIGNED_COLUMNS, SIGNED_VALUES),
        ("Reverted", SIGNED_COLUMNS, SIGNED_VALUES),
        ("ManualReview", "", ""),
    ] {
        let mut ledger = setup_with_robinhood_reserve();
        let request_id = parked_glc_to_rhn(&mut ledger);
        seed_robinhood_tx(
            &ledger,
            request_id,
            "Payout",
            1,
            "GlcToRhn",
            state,
            extra_columns,
            extra_values,
        );

        let checks = ledger.glc_refund_db_checks(request_id).unwrap();
        assert!(
            !checks.no_robinhood_payout_started,
            "{state}: a payout row must block the refund"
        );
        let refusal = checks
            .refusal
            .unwrap_or_else(|| panic!("{state}: must be refused"));
        assert!(refusal.contains("Robinhood payout operation"), "{refusal}");
        assert!(refusal.contains(state), "{refusal}");
    }
}

/// The refusal names the specific durable step reached, so an operator
/// sees WHY rather than only THAT — and never sees the signed bytes.
#[test]
fn the_refusal_names_the_payout_step_reached_without_exposing_the_signed_bytes() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = parked_glc_to_rhn(&mut ledger);
    let tx_id = seed_robinhood_tx(
        &ledger,
        request_id,
        "Payout",
        1,
        "GlcToRhn",
        "Broadcast",
        &format!("{SIGNED_COLUMNS}, broadcast_attempts, first_broadcast_at, replacement_attempts"),
        &format!("{SIGNED_VALUES}, 3, 100, 2"),
    );
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_authorization_signatures
                (transaction_id, position, signer, signature, created_at)
             VALUES (?1, 0, X'a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1', ?2, 100),
                    (?1, 1, X'a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2', ?2, 100)",
            rusqlite::params![tx_id, vec![7u8; 65]],
        )
        .unwrap();

    let checks = ledger.glc_refund_db_checks(request_id).unwrap();
    let evidence = checks
        .robinhood_payout_evidence
        .first()
        .expect("one piece of evidence");
    assert_eq!(evidence.code(), "payout_broadcast");
    let reason = evidence.reason();
    for expected in [
        "2 authorization signature(s) persisted",
        "a submitter nonce is allocated",
        "signed transaction bytes are persisted",
        "a transaction hash is persisted",
        "3 broadcast attempt(s)",
        "2 replacement attempt(s)",
    ] {
        assert!(
            reason.contains(expected),
            "{expected:?} missing from: {reason}"
        );
    }
    // The SHAPE of the payout state is reported; its CONTENT is not.
    assert!(
        !reason.contains("02f8"),
        "raw signed bytes leaked: {reason}"
    );
    assert!(
        !reason.contains("5115115115"),
        "submitter address leaked: {reason}"
    );
    assert!(!reason.to_lowercase().contains("signature "), "{reason}");
}

/// A REVERTED payout is not an all-clear. The transaction consumed its
/// nonce and its gas; whether the contract moved value is a question for
/// a human with the chain in front of them, and this path never answers
/// it by assumption.
#[test]
fn a_reverted_payout_does_not_re_open_the_refund() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = parked_glc_to_rhn(&mut ledger);
    seed_robinhood_tx(
        &ledger,
        request_id,
        "Payout",
        1,
        "GlcToRhn",
        "Reverted",
        &format!("{SIGNED_COLUMNS}, receipt_status, broadcast_attempts, first_broadcast_at"),
        &format!("{SIGNED_VALUES}, 0, 1, 100"),
    );

    let checks = ledger.glc_refund_db_checks(request_id).unwrap();
    assert!(!checks.no_robinhood_payout_started);
    let reason = checks.robinhood_payout_evidence[0].reason();
    assert!(
        reason.contains("a REVERTED receipt was read back"),
        "{reason}"
    );
    assert_eq!(
        checks.robinhood_payout_evidence[0].code(),
        "payout_reverted"
    );
}

/// A SUCCESSFUL receipt blocks it just as firmly, and says so.
#[test]
fn a_successful_payout_receipt_blocks_the_refund() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = parked_glc_to_rhn(&mut ledger);
    seed_robinhood_tx(
        &ledger,
        request_id,
        "Payout",
        1,
        "GlcToRhn",
        "Included",
        &format!("{SIGNED_COLUMNS}, receipt_status, broadcast_attempts, first_broadcast_at"),
        &format!("{SIGNED_VALUES}, 1, 1, 100"),
    );

    let checks = ledger.glc_refund_db_checks(request_id).unwrap();
    assert!(!checks.no_robinhood_payout_started);
    let reason = checks.robinhood_payout_evidence[0].reason();
    assert!(
        reason.contains("a SUCCESSFUL receipt was read back"),
        "{reason}"
    );
}

/// Corrupted linkage — an operation kind that belongs to the OTHER route
/// naming this Goldcoin-sourced request — is refused rather than ignored.
/// The ledger disagreeing with itself is not a state to refund from.
#[test]
fn a_mismatched_robinhood_operation_kind_refuses_the_refund() {
    for (kind, action) in [("Settlement", 3), ("Refund", 2)] {
        let mut ledger = setup_with_robinhood_reserve();
        let request_id = parked_glc_to_rhn(&mut ledger);
        seed_robinhood_tx(
            &ledger,
            request_id,
            kind,
            action,
            "RhnToGlc",
            "Authorizing",
            "",
            "",
        );

        let checks = ledger.glc_refund_db_checks(request_id).unwrap();
        assert!(!checks.no_robinhood_payout_started, "{kind}");
        assert_eq!(
            checks.robinhood_payout_evidence[0].code(),
            "unexpected_robinhood_operation"
        );
        let refusal = checks.refusal.unwrap_or_else(|| panic!("{kind}"));
        assert!(refusal.contains("cannot be trusted"), "{refusal}");
    }
}

/// A Robinhood DEPOSIT observation naming a Goldcoin-sourced request is
/// the same class of contradiction, from the inbound side.
#[test]
fn a_robinhood_deposit_fold_into_a_goldcoin_sourced_request_refuses_the_refund() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = parked_glc_to_rhn(&mut ledger);
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_deposit_observations
                (source_chain, source_contract, source_obligation_index, contract_route_id, route,
                 depositor, destination, amount_robinhood_atomic, amount_canonical_atomic,
                 tx_hash, log_index, block_number, block_hash, finality, observed_at,
                 finalized_at, settled, folded_request_id)
             VALUES ('robinhood', X'b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1', 5, 2, 'RhnToGlc',
                 X'd1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1', X'aabb', ?1, 100,
                 ?2, 0, 10, ?2, 'Final', 100, 100, 0, ?3)",
            rusqlite::params![vec![1u8; 32], vec![2u8; 32], request_id],
        )
        .unwrap();

    let checks = ledger.glc_refund_db_checks(request_id).unwrap();
    assert!(!checks.no_robinhood_payout_started);
    assert_eq!(
        checks.robinhood_payout_evidence[0],
        RobinhoodPayoutEvidence::UnexpectedDepositFold {
            observation_index: 5
        }
    );
}

/// The proof is DATABASE state, so it survives a process restart by
/// construction — there is no in-memory daemon state to lose. Asserted
/// against a real file-backed ledger reopened from scratch, because
/// "durable" is a property of what was committed, not of what a handle
/// happens to remember.
#[test]
fn the_refusal_survives_reopening_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for reserve in ReserveDirection::ALL {
            ledger
                .configure_reserve(
                    reserve, 1_000_000, 100_000, 500_000, 200_000, 150_000, 1_000,
                )
                .unwrap();
        }
        let request_id = parked_glc_to_rhn(&mut ledger);
        seed_robinhood_tx(
            &ledger,
            request_id,
            "Payout",
            1,
            "GlcToRhn",
            "Broadcast",
            &format!("{SIGNED_COLUMNS}, broadcast_attempts, first_broadcast_at"),
            &format!("{SIGNED_VALUES}, 1, 100"),
        );
        assert!(
            !ledger
                .glc_refund_db_checks(request_id)
                .unwrap()
                .no_robinhood_payout_started
        );
        request_id
    };

    // A brand-new handle over the same file: the refusal is unchanged.
    let reopened = Ledger::open(&db_path).unwrap();
    let checks = reopened.glc_refund_db_checks(request_id).unwrap();
    assert!(
        !checks.no_robinhood_payout_started,
        "a restart must not make an in-flight payout look refundable"
    );
    assert!(checks.refusal.is_some());
}

/// The enforced gate refuses on the same evidence the printable view
/// reports — and refuses INSIDE the write transaction, so a payout that
/// starts between a dry run and an execute is caught rather than raced.
#[test]
fn begin_goldcoin_refund_enforces_the_robinhood_proof_itself() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = parked_glc_to_rhn(&mut ledger);
    seed_robinhood_tx(
        &ledger,
        request_id,
        "Payout",
        1,
        "GlcToRhn",
        "Authorizing",
        "",
        "",
    );

    let err = ledger
        .begin_goldcoin_refund(
            request_id,
            99_999,
            [0xAA; 32],
            0,
            [0xD1; 20],
            "Qdest",
            1_000,
            &[crate::goldcoin::coin::VaultUtxo {
                txid: [0xAA; 32],
                vout: 0,
                amount_atomic: 99_999,
                script_pubkey_hex: "51".to_string(),
            }],
            "00",
            "operator note",
            "tester",
            2_000,
        )
        .unwrap_err();
    match err {
        LedgerError::GlcRefundNotEligible { id, detail } => {
            assert_eq!(id, request_id);
            assert!(detail.contains("Robinhood payout operation"), "{detail}");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        ledger.get_goldcoin_refund(request_id).unwrap().is_none(),
        "a refused refund must leave no row"
    );
}

/// A direction with no Goldcoin deposit at all is still refused, and the
/// gate that refuses it is the Goldcoin-sourced one — not the
/// GlcToSol-only one it replaced.
#[test]
fn a_contract_sourced_request_is_still_refused_by_the_gate() {
    let mut ledger = setup_with_robinhood_reserve();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::RhnToGlc,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected Reserved")
    };

    let checks = ledger.glc_refund_db_checks(request_id).unwrap();
    assert!(!checks.direction_is_goldcoin_sourced);
    let refusal = checks.refusal.expect("RhnToGlc must be refused");
    assert!(refusal.contains("not a Goldcoin"), "{refusal}");
}

/// The tripwire in the other direction: a Robinhood payout row naming a
/// `GlcToSol` request is a contradiction, and the Solana proof passing
/// does not excuse it. This STRENGTHENS the legacy path — it can only
/// fire on state that should not exist.
#[test]
fn a_robinhood_payout_row_naming_a_glc_to_sol_request_refuses_the_refund() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = create_glc_to_sol_request(&mut ledger);
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 99_999, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert!(
        ledger
            .glc_refund_db_checks(request_id)
            .unwrap()
            .all_passed(),
        "the fixture must pass every check before the contradiction is introduced"
    );

    seed_robinhood_tx(
        &ledger,
        request_id,
        "Payout",
        1,
        "GlcToRhn",
        "Authorizing",
        "",
        "",
    );
    let checks = ledger.glc_refund_db_checks(request_id).unwrap();
    assert!(checks.direction_is_glc_to_sol, "still the Solana proof");
    assert!(checks.no_destination_txid && checks.no_settlement_claim);
    assert!(
        !checks.no_robinhood_payout_started,
        "the Solana proof passing must not excuse Robinhood state that cannot exist"
    );
    assert!(checks.refusal.is_some());
}

/// No duplicate refund, unchanged: an existing `goldcoin_refunds` row
/// refuses a second one for a `GlcToRhn` request exactly as it does for
/// `GlcToSol`.
#[test]
fn a_duplicate_glc_to_rhn_refund_is_refused() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = parked_glc_to_rhn(&mut ledger);
    // The refund spends a real, Available vault UTXO — the same
    // reservation the GlcToSol path performs, unchanged.
    let funding = crate::goldcoin::coin::VaultUtxo {
        txid: [0xCC; 32],
        vout: 1,
        amount_atomic: 500_000,
        script_pubkey_hex: "51".to_string(),
    };
    ledger
        .sync_vault_utxos(&[(funding.clone(), 6, "51".to_string())], 1, 1_150)
        .unwrap();
    let inputs = [funding];
    ledger
        .begin_goldcoin_refund(
            request_id,
            99_999,
            [0xAA; 32],
            0,
            [0xD1; 20],
            "Qdest",
            1_000,
            &inputs,
            "00",
            "operator note",
            "tester",
            2_000,
        )
        .expect("the first refund opens");
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::RefundPending
    );

    // Refused immediately — the request has already left `ManualReview`.
    let err = ledger
        .begin_goldcoin_refund(
            request_id,
            99_999,
            [0xAA; 32],
            0,
            [0xD1; 20],
            "Qdest",
            1_000,
            &inputs,
            "00",
            "operator note",
            "tester",
            2_100,
        )
        .unwrap_err();
    assert!(
        matches!(&err, LedgerError::GlcRefundNotEligible { id, detail }
                 if *id == request_id && detail.contains("RefundPending")),
        "{err:?}"
    );

    // And the duplicate guard stands on its OWN, not merely as a
    // side effect of the state guard: forcing the request back to
    // `ManualReview` still cannot produce a second refund row.
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'ManualReview' WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    let err = ledger
        .begin_goldcoin_refund(
            request_id,
            99_999,
            [0xAA; 32],
            0,
            [0xD1; 20],
            "Qdest",
            1_000,
            &inputs,
            "00",
            "operator note",
            "tester",
            2_200,
        )
        .unwrap_err();
    assert!(
        matches!(&err, LedgerError::GoldcoinRefundExists { id, .. } if *id == request_id),
        "{err:?}"
    );

    let checks = ledger.glc_refund_db_checks(request_id).unwrap();
    assert!(!checks.no_existing_refund);
    let count: i64 = ledger
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(*) FROM goldcoin_refunds WHERE request_id = ?1",
            [request_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "exactly one refund row, ever");
}

// =====================================================================
// `transfers_page`: the chain-tagged "my activity" filter.
// =====================================================================

/// Inserts an `RhnToGlc` request and the FINAL observation that funded
/// it, linked as the fold links them. Returns the request id.
fn create_rhn_to_glc_request_with_depositor(
    ledger: &mut Ledger,
    obligation_index: u64,
    depositor: [u8; 20],
) -> i64 {
    // A destination per obligation: the rolling-24h destination window
    // would refuse a second request to one address inside a day.
    let destination = format!("Qgoldcoindestinationaddress{obligation_index}");
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::RhnToGlc,
            amounts(100_000),
            destination.as_bytes(),
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected Reserved")
    };
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_deposit_observations
                (source_chain, source_contract, source_obligation_index, contract_route_id,
                 route, depositor, destination, amount_robinhood_atomic,
                 amount_canonical_atomic, tx_hash, log_index, block_number, block_hash,
                 finality, observed_at, finalized_at, folded_request_id)
             VALUES ('robinhood', ?1, ?2, 2, 'RhnToGlc', ?3, ?4, ?5, 100000, ?6, 0, 500, ?7,
                     'Final', 100, 200, ?8)",
            rusqlite::params![
                &[0x11u8; 20][..],
                obligation_index as i64,
                &depositor[..],
                destination.as_bytes().to_vec(),
                &[0u8; 32][..],
                {
                    let mut h = [0xaau8; 32];
                    h[0] = obligation_index as u8;
                    h.to_vec()
                },
                &[0xbbu8; 32][..],
                request_id,
            ],
        )
        .unwrap();
    request_id
}

/// The Solana half, restated after the widening: still `GlcToSol
/// .recipient` and `SolToGlc.requester`, still nothing else.
#[test]
fn transfers_page_solana_filter_matches_the_same_two_columns_it_always_did() {
    let mut ledger = setup();
    let mine = [0x01u8; 32];
    let theirs = [0x02u8; 32];

    let CreateRequestOutcome::Reserved { request_id: sent } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &mine,
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected Reserved")
    };
    ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &theirs,
            None,
            3600,
            1_000,
        )
        .unwrap();
    let CreateRequestOutcome::Reserved {
        request_id: received,
    } = ledger
        .create_request(
            Direction::SolToGlc,
            amounts(100_000),
            b"Qgoldcoinaddress",
            Some(mine),
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected Reserved")
    };

    let page = ledger
        .transfers_page(Some(TransferAddressFilter::Solana(mine)), None, None, 50)
        .unwrap();
    let mut ids: Vec<i64> = page.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![sent, received]);
}

/// The EVM half: `GlcToRhn.recipient` and the folded observation's own
/// `depositor` — a column that is not on `bridge_requests` at all.
#[test]
fn transfers_page_evm_filter_matches_the_recipient_and_the_folded_depositor() {
    let mut ledger = setup_with_robinhood_reserve();
    let outbound = create_glc_to_rhn_request(&mut ledger);
    let inbound = create_rhn_to_glc_request_with_depositor(&mut ledger, 0, TEST_EVM_RECIPIENT);
    // Somebody else's inbound deposit, same route.
    create_rhn_to_glc_request_with_depositor(&mut ledger, 1, [0x99; 20]);

    let page = ledger
        .transfers_page(
            Some(TransferAddressFilter::Evm(TEST_EVM_RECIPIENT)),
            None,
            None,
            50,
        )
        .unwrap();
    let mut ids: Vec<i64> = page.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![outbound, inbound]);
}

/// The cross-chain guarantee, at the level that actually enforces it.
#[test]
fn transfers_page_filters_never_reach_the_other_chains_rows() {
    let mut ledger = setup_with_robinhood_reserve();
    let solana_recipient = [0xE1u8; 32];
    ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &solana_recipient,
            None,
            3600,
            1_000,
        )
        .unwrap();
    create_glc_to_rhn_request(&mut ledger);
    create_rhn_to_glc_request_with_depositor(&mut ledger, 0, TEST_EVM_RECIPIENT);

    // A Solana pubkey whose first 20 bytes ARE the EVM address in use —
    // the adversarial case for any prefix or untagged-bytes comparison.
    let page = ledger
        .transfers_page(
            Some(TransferAddressFilter::Solana(solana_recipient)),
            None,
            None,
            50,
        )
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].direction, Direction::GlcToSol);

    // And the reverse: the EVM filter reaches neither Solana-addressed
    // direction, even though `GlcToSol.recipient` starts with the same
    // twenty bytes.
    let page = ledger
        .transfers_page(
            Some(TransferAddressFilter::Evm(TEST_EVM_RECIPIENT)),
            None,
            None,
            50,
        )
        .unwrap();
    assert!(page
        .iter()
        .all(|r| r.direction == Direction::GlcToRhn || r.direction == Direction::RhnToGlc));
}

/// A `SolToGlc` recipient is an ASCII Goldcoin address in the SAME column
/// a `GlcToRhn` payout address lives in. Neither filter may reach it.
#[test]
fn transfers_page_never_matches_a_goldcoin_address_in_the_recipient_column() {
    let mut ledger = setup_with_robinhood_reserve();
    // Exactly 20 ASCII bytes, so it is the same WIDTH as an EVM address.
    let goldcoin: &[u8] = b"Qaddress20byteslong!";
    assert_eq!(goldcoin.len(), 20);
    ledger
        .create_request(
            Direction::SolToGlc,
            amounts(100_000),
            goldcoin,
            Some([0x07u8; 32]),
            3600,
            1_000,
        )
        .unwrap();

    let mut as_evm = [0u8; 20];
    as_evm.copy_from_slice(goldcoin);
    assert!(ledger
        .transfers_page(Some(TransferAddressFilter::Evm(as_evm)), None, None, 50)
        .unwrap()
        .is_empty());
}

/// An orphaned sighting is not evidence of who funded a request, so it
/// stops attributing one.
#[test]
fn transfers_page_ignores_a_reorged_observations_depositor() {
    let mut ledger = setup_with_robinhood_reserve();
    let request_id = create_rhn_to_glc_request_with_depositor(&mut ledger, 0, TEST_EVM_RECIPIENT);
    assert_eq!(
        ledger
            .transfers_page(
                Some(TransferAddressFilter::Evm(TEST_EVM_RECIPIENT)),
                None,
                None,
                50
            )
            .unwrap()
            .len(),
        1
    );

    ledger
        .conn_for_tests()
        .execute(
            "UPDATE robinhood_deposit_observations
                SET finality = 'Reorged', reorged_at = 900, finalized_at = NULL
              WHERE folded_request_id = ?1",
            [request_id],
        )
        .unwrap();

    assert!(ledger
        .transfers_page(
            Some(TransferAddressFilter::Evm(TEST_EVM_RECIPIENT)),
            None,
            None,
            50
        )
        .unwrap()
        .is_empty());
}

/// An absent filter still lists everything, on every direction — the
/// unfiltered listing did not become chain-scoped by accident.
#[test]
fn transfers_page_with_no_address_still_lists_every_direction() {
    let mut ledger = setup_with_robinhood_reserve();
    ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[0x01; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    create_glc_to_rhn_request(&mut ledger);
    create_rhn_to_glc_request_with_depositor(&mut ledger, 0, TEST_EVM_RECIPIENT);

    let page = ledger.transfers_page(None, None, None, 50).unwrap();
    assert_eq!(page.len(), 3);
    // Newest first, as before.
    assert!(page.windows(2).all(|w| w[0].id > w[1].id));
}

// ------------------------------------------------ auto-resume hold (v29) --

/// The hold is per-row, explicit, and absolute for resumes: a held park
/// refuses every resume entry point until released, a row folded after
/// the hold is unheld, and release restores exactly the pre-hold
/// behaviour. Nothing else about the row changes.
#[test]
fn a_hold_blocks_resume_until_released_and_never_reaches_other_rows() {
    let mut ledger = setup();
    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("incident"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    let before = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(before.auto_resume_hold_note, None);

    ledger
        .set_manual_review_hold(request_id, 1_000 + 72 * 3600, "72h freeze", "cli:op", 1_000)
        .unwrap();
    let held = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(held.state, RequestState::ManualReview);
    assert_eq!(
        held.manual_review_note, before.manual_review_note,
        "the fold note is untouched"
    );
    assert_eq!(held.auto_resume_hold_note.as_deref(), Some("72h freeze"));
    assert_eq!(held.auto_resume_hold_until, Some(1_000 + 72 * 3600));

    // Every resume entry point refuses — the operator one included — and
    // an elapsed `hold_until` changes nothing: the hold never expires on
    // its own.
    for now in [2_000, 1_000 + 72 * 3600 + 1] {
        let err = ledger
            .resume_manual_review_sol_to_glc(request_id, "try", "operator", now)
            .unwrap_err();
        assert!(
            matches!(err, LedgerError::ManualReviewNotRecoverable { .. }),
            "held row must be refused: {err}"
        );
        assert!(err.to_string().contains("held by operator"), "{err}");
    }
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview
    );

    // A row folded after the hold is unheld and behaves as it always did.
    let SolFoldOutcome::FoldedManualReview { request_id: later } = ledger
        .fold_sol_deposit(1, amounts(100_000), [3u8; 32], &[4u8; 32], None, 3_000)
        .unwrap()
    else {
        panic!()
    };
    let later_row = ledger.get_request(later).unwrap().unwrap();
    assert_eq!(later_row.auto_resume_hold_note, None);
    assert_eq!(later_row.auto_resume_hold_until, None);
    assert_eq!(
        ledger
            .resume_manual_review_sol_to_glc(later, "unheld resumes", "operator", 4_000)
            .unwrap(),
        ResumeManualReviewOutcome::Resumed
    );

    // Listing shows exactly the held row.
    let listed: Vec<i64> = ledger
        .held_manual_review_requests()
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(listed, vec![request_id]);

    // Release restores the pre-hold behaviour; a second release is a no-op.
    assert!(ledger
        .clear_manual_review_hold(request_id, "cli:op", 5_000)
        .unwrap());
    assert!(!ledger
        .clear_manual_review_hold(request_id, "cli:op", 5_001)
        .unwrap());
    assert!(ledger.held_manual_review_requests().unwrap().is_empty());
    assert_eq!(
        ledger
            .resume_manual_review_sol_to_glc(request_id, "released", "operator", 6_000)
            .unwrap(),
        ResumeManualReviewOutcome::Resumed
    );
}

/// A hold applies only to a parked request that nothing has paid: any
/// other state, a recorded destination txid, or an existing payout row
/// is refused without a write — so the "already processing" set can never
/// be marked, however the id list was assembled.
#[test]
fn a_hold_is_refused_for_anything_that_is_not_an_unpaid_park() {
    let mut ledger = setup();
    // Admitted straight through: SourceFinalized, not ManualReview.
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], None, 1_000)
        .unwrap()
    else {
        panic!()
    };
    let err = ledger
        .set_manual_review_hold(request_id, 9_999, "x", "cli:op", 1_000)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::ManualReviewNotRecoverable { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("not ManualReview"), "{err}");
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .auto_resume_hold_note,
        None
    );

    // Unknown id.
    assert!(matches!(
        ledger.set_manual_review_hold(424_242, 9_999, "x", "cli:op", 1_000),
        Err(LedgerError::RequestNotFound(424_242))
    ));

    // Empty note.
    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("incident"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id: parked } = ledger
        .fold_sol_deposit(1, amounts(100_000), [3u8; 32], &[4u8; 32], None, 2_000)
        .unwrap()
    else {
        panic!()
    };
    assert!(ledger
        .set_manual_review_hold(parked, 9_999, "   ", "cli:op", 2_000)
        .is_err());
    // Re-holding an already-held row is idempotent (note/time replaced).
    ledger
        .set_manual_review_hold(parked, 9_999, "first", "cli:op", 2_000)
        .unwrap();
    ledger
        .set_manual_review_hold(parked, 10_000, "second", "cli:op", 2_001)
        .unwrap();
    let row = ledger.get_request(parked).unwrap().unwrap();
    assert_eq!(row.auto_resume_hold_note.as_deref(), Some("second"));
    assert_eq!(row.auto_resume_hold_until, Some(10_000));
}

/// The v29 migration is idempotent and leaves every pre-existing row
/// unheld — reopening a ledger never manufactures a hold.
#[test]
fn v29_reopen_keeps_rows_unheld_and_the_hold_durable() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (held, unheld) = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                1_000_000,
                100_000,
                500_000,
                200_000,
                150_000,
                1_000,
            )
            .unwrap();
        ledger
            .set_paused(ReserveDirection::GoldcoinReserve, true, Some("incident"))
            .unwrap();
        let mut ids = Vec::new();
        for i in 0..2u64 {
            let SolFoldOutcome::FoldedManualReview { request_id } = ledger
                .fold_sol_deposit(
                    i,
                    amounts(100_000),
                    [i as u8 + 1; 32],
                    &[i as u8 + 10; 32],
                    None,
                    1_000,
                )
                .unwrap()
            else {
                panic!()
            };
            ids.push(request_id);
        }
        ledger
            .set_manual_review_hold(ids[0], 9_999, "freeze", "cli:op", 1_000)
            .unwrap();
        (ids[0], ids[1])
    };
    let ledger = Ledger::open(&db_path).unwrap();
    assert_eq!(
        ledger
            .get_request(held)
            .unwrap()
            .unwrap()
            .auto_resume_hold_note
            .as_deref(),
        Some("freeze")
    );
    assert_eq!(
        ledger
            .get_request(unheld)
            .unwrap()
            .unwrap()
            .auto_resume_hold_note,
        None
    );
}
