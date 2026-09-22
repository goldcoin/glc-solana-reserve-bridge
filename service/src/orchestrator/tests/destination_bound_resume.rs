//! `glc-admin resume-destination-bound` end to end (docs/40): a
//! `GlcToSol` release parked `destination_payout_out_of_bounds` because
//! its LOCKED payout exceeded the program's `per_transfer_limit` is
//! re-admitted once the live limit admits it, and the EXISTING pipeline
//! then attests and pays exactly the locked figure — plus every refusal
//! the command must make.

use super::*;
use crate::bridge_rate::{compute_bridge_quote, RailPrices, StruckQuote};
use crate::ledger::{LiveSolanaBounds, ResumeDryRunOutcome, ResumeManualReviewOutcome};
use crate::solana::resume_destination_bound;

const GLC: u64 = 100_000_000;
/// Requests 4438 and 4483 as the ledger holds them (2026-09-20 read):
/// 50 000 GLC gross, 300 bps, locked rail prices, locked nets.
const REQ_4438: (u64, u64, u64) = (983_906_422, 50_389_216, 947_017_343_294);
const REQ_4483: (u64, u64, u64) = (772_204_750, 45_187_267, 828_816_010_824);
const MINT: [u8; 32] = [7u8; 32];
const RECIPIENT: [u8; 32] = [9u8; 32];

/// The fake `bridge_config` with an explicit `per_transfer_limit` (mint
/// units). Layout: `fake_bridge_config_bytes_with_token_program`'s, the
/// limit at bytes 135..143.
fn config_with_limit(per_transfer_limit: u64) -> Vec<u8> {
    let mut v = fake_bridge_config_bytes(MINT, 0);
    v[135..143].copy_from_slice(&per_transfer_limit.to_le_bytes());
    assert_eq!(
        accounts::decode_bridge_config(&v)
            .unwrap()
            .per_transfer_limit,
        per_transfer_limit
    );
    v
}

/// A 50 000 GLC `GlcToSol` request whose deposit was observed and locked
/// at the given rail prices, then finalized — the shape of 4438/4483
/// before the release path looked at it. Reserves are deep enough for
/// any payout in these tests.
fn locked_request(db_path: &std::path::Path, src_e12: u64, dst_e12: u64) -> i64 {
    let mut ledger = Ledger::open(db_path).unwrap();
    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        ledger
            .configure_reserve(
                direction,
                5_000_000_000_000,
                20_000_000_000,
                2_000_000_000_000,
                1_000_000_000_000,
                500_000_000_000,
                0,
            )
            .unwrap();
    }
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            glc_to_sol_amounts(50_000 * GLC, TEST_SOLANA_DECIMALS),
            &RECIPIENT,
            None,
            3600,
            0,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed_from(
            request_id,
            [0xABu8; 32],
            2,
            50_000 * GLC,
            10,
            [0u8; 32],
            &[],
            |direction, gross, fee_bps, now, scale| {
                assert_eq!(direction, Direction::GlcToSol);
                let quote = compute_bridge_quote(
                    gross,
                    RailPrices {
                        source_price_e12: src_e12,
                        destination_price_e12: dst_e12,
                        source_feed_at: now,
                        destination_feed_at: now,
                    },
                    fee_bps,
                    now,
                    60,
                    scale,
                )
                .unwrap();
                Ok(StruckQuote {
                    quote,
                    band: None,
                    route_rate: None,
                })
            },
            5,
        )
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 6).unwrap();
    request_id
}

fn solana_node(
    per_transfer_limit: u64,
    signers: &[Box<dyn AttestationSigner>],
) -> Arc<MockSolanaRpc> {
    let rpc = Arc::new(MockSolanaRpc::new());
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            5,
            2,
            &signers.iter().map(|s| s.pubkey()).collect::<Vec<_>>(),
        ),
    );
    rpc.set_account(
        accounts::bridge_config_pda(),
        config_with_limit(per_transfer_limit),
    );
    rpc.set_account(
        Pubkey::new_from_array(MINT),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    rpc
}

fn live(per_transfer_limit: u64) -> LiveSolanaBounds {
    LiveSolanaBounds {
        min_transfer_amount: 100,
        per_transfer_limit,
        solana_decimals: TEST_SOLANA_DECIMALS,
    }
}

/// Parks the request exactly as production did: one orchestrator tick
/// against a 50 000 GLC (Solana) limit.
async fn park_at_settlement(
    db_path: &std::path::Path,
    request_id: i64,
) -> (
    Orchestrator<Arc<MockGoldcoinRpc>, Arc<MockSolanaRpc>>,
    Arc<MockSolanaRpc>,
) {
    let attestation_signers = attestation_signers();
    let solana_rpc = solana_node(50_000_000_000, &attestation_signers);
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        db_path,
        Arc::new(MockGoldcoinRpc::new()),
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );
    let report = orchestrator.tick(10).await;
    assert_eq!(
        report.releases_parked_out_of_bounds, 1,
        "{:?}",
        report.errors
    );
    let request = orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(request.state, RequestState::ManualReview);
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some(Ledger::MANUAL_REVIEW_REASON_DESTINATION_PAYOUT_OUT_OF_BOUNDS)
    );
    assert!(orchestrator
        .ledger()
        .all_attestation_records()
        .unwrap()
        .is_empty());
    (orchestrator, solana_rpc)
}

/// The happy path, for both incident shapes: park at 50 000, raise the
/// live limit to 2 000 000, dry-run (zero writes), execute, and the
/// EXISTING pipeline attests and submits exactly the locked payout to the
/// original recipient, then settles. Idempotent on a rerun.
#[tokio::test]
async fn a_parked_glc_to_sol_release_is_resumed_at_its_locked_quote_and_settles() {
    for (label, (src, dst, locked_net)) in [("4438", REQ_4438), ("4483", REQ_4483)] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.sqlite3");
        let request_id = locked_request(&db_path, src, dst);
        let before = Ledger::open(&db_path)
            .unwrap()
            .get_request(request_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            before.net_destination_atomic, locked_net,
            "{label}: the locked payout"
        );
        let (mut orchestrator, solana_rpc) = park_at_settlement(&db_path, request_id).await;
        let (_, _, reserved_before, pending_before) = orchestrator
            .ledger()
            .reserve_snapshot(ReserveDirection::SolanaReserve)
            .unwrap();
        assert!(
            reserved_before >= locked_net,
            "{label}: the park kept its reservation"
        );

        // Still refused while the live limit is 50 000.
        {
            let mut ledger = Ledger::open(&db_path).unwrap();
            let dry = resume_destination_bound::dry_run(&*solana_rpc, &mut ledger, request_id, 20)
                .await
                .unwrap();
            assert_eq!(dry.locked_payout_mint_units, Ok(locked_net), "{label}");
            assert!(
                matches!(&dry.ledger, ResumeDryRunOutcome::WouldRefuse { reason } if reason.contains("exceeds the program's live per_transfer_limit 50000000000")),
                "{label}: {:?}",
                dry.ledger
            );
        }

        // The operator raises the limit on chain (set_limit); the daemon
        // and the command both read it live.
        solana_rpc.set_account(
            accounts::bridge_config_pda(),
            config_with_limit(2_000_000_000_000),
        );

        // Dry run: would resume, and wrote nothing.
        {
            let mut ledger = Ledger::open(&db_path).unwrap();
            let dry = resume_destination_bound::dry_run(&*solana_rpc, &mut ledger, request_id, 21)
                .await
                .unwrap();
            assert!(dry.would_resume(), "{label}: {:?}", dry.ledger);
            assert_eq!(dry.live.per_transfer_limit, 2_000_000_000_000);
            assert!(!dry.destination_txid_present);
            let after_dry = ledger.get_request(request_id).unwrap().unwrap();
            assert_eq!(
                after_dry.state,
                RequestState::ManualReview,
                "{label}: dry run wrote nothing"
            );
            assert_eq!(
                after_dry.manual_review_note,
                before.manual_review_note.clone().or(Some(
                    Ledger::MANUAL_REVIEW_REASON_DESTINATION_PAYOUT_OUT_OF_BOUNDS.to_string()
                ))
            );
            assert!(
                ledger
                    .list_admin_audit(&crate::ledger::AdminAuditFilter::default())
                    .unwrap()
                    .is_empty(),
                "{label}: no audit row from a dry run"
            );
            assert_eq!(
                ledger.state_log(request_id).unwrap().len(),
                after_dry_state_log_len(&ledger, request_id)
            );
        }

        // Execute.
        {
            let mut ledger = Ledger::open(&db_path).unwrap();
            let outcome = resume_destination_bound::execute(
                &*solana_rpc,
                &mut ledger,
                request_id,
                "operator: limit raised to 2,000,000",
                "cli:test",
                22,
            )
            .await
            .unwrap();
            assert_eq!(outcome, ResumeManualReviewOutcome::Resumed, "{label}");
            let resumed = ledger.get_request(request_id).unwrap().unwrap();
            assert_eq!(resumed.state, RequestState::SourceFinalized);
            assert_eq!(resumed.manual_review_note, None);
            // Exact original quote, amounts, destination: untouched.
            assert_eq!(resumed.quote, before.quote, "{label}: the locked quote");
            assert_eq!(resumed.gross_amount_atomic, before.gross_amount_atomic);
            assert_eq!(resumed.net_amount_atomic, before.net_amount_atomic);
            assert_eq!(resumed.net_destination_atomic, locked_net);
            assert_eq!(
                resumed.recipient,
                RECIPIENT.to_vec(),
                "{label}: the destination"
            );
            // Nothing reserved anew.
            let (_, _, reserved, pending) = ledger
                .reserve_snapshot(ReserveDirection::SolanaReserve)
                .unwrap();
            assert_eq!(
                (reserved, pending),
                (reserved_before, pending_before),
                "{label}"
            );
            // Audited, with the live bounds it was judged against.
            let audit = ledger
                .list_admin_audit(&crate::ledger::AdminAuditFilter::default())
                .unwrap();
            assert_eq!(audit.len(), 1, "{label}");
            assert_eq!(audit[0].action, resume_destination_bound::AUDIT_ACTION);
            assert!(audit[0]
                .new_value
                .as_deref()
                .unwrap()
                .contains("max=2000000000000"));
            // Idempotent: a rerun is a no-op.
            let again = resume_destination_bound::execute(
                &*solana_rpc,
                &mut ledger,
                request_id,
                "again",
                "cli:test",
                23,
            )
            .await
            .unwrap();
            assert_eq!(
                again,
                ResumeManualReviewOutcome::AlreadyResumed {
                    state: RequestState::SourceFinalized
                }
            );
            assert_eq!(
                ledger
                    .list_admin_audit(&crate::ledger::AdminAuditFilter::default())
                    .unwrap()
                    .len(),
                2
            );
        }

        // The EXISTING pipeline: bounds re-check passes, 2-of-3 attest
        // the LOCKED payout to the ORIGINAL recipient, release submitted.
        let report = orchestrator.tick(30).await;
        assert_eq!(report.releases_submitted, 1, "{label}: {:?}", report.errors);
        assert_eq!(report.releases_parked_out_of_bounds, 0);
        let records = orchestrator.ledger().all_attestation_records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].action_type, "release");
        let msg = &records[0].canonical_message;
        // release_claim_message layout: … txid (32) | vout (4) | amount u64 LE | recipient (32) | mint (32)
        let n = msg.len();
        let mint_at = n - 32;
        let recipient_at = mint_at - 32;
        let amount_at = recipient_at - 8;
        assert_eq!(&msg[mint_at..], &MINT, "{label}: mint");
        assert_eq!(
            &msg[recipient_at..mint_at],
            &RECIPIENT,
            "{label}: recipient"
        );
        assert_eq!(
            u64::from_le_bytes(msg[amount_at..recipient_at].try_into().unwrap()),
            locked_net,
            "{label}: the attested amount is the LOCKED payout"
        );
        let submitted = orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap();
        assert_eq!(submitted.state, RequestState::DestinationSubmitted);
        // Confirmation → Settled, at the locked figure.
        let destination_txid = orchestrator
            .ledger()
            .get_destination_txid(request_id)
            .unwrap()
            .unwrap();
        let signature = Signature::from(<[u8; 64]>::try_from(destination_txid).unwrap());
        solana_rpc.set_status(signature, Ok(()));
        let report = orchestrator.tick(40).await;
        assert_eq!(report.releases_confirmed, 1, "{label}: {:?}", report.errors);
        let settled = orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap();
        assert_eq!(settled.state, RequestState::Settled, "{label}");
        assert_eq!(
            orchestrator
                .ledger()
                .settled_liquidity(ReserveDirection::SolanaReserve)
                .unwrap(),
            locked_net,
            "{label}"
        );
        let (_, _, reserved_after, _) = orchestrator
            .ledger()
            .reserve_snapshot(ReserveDirection::SolanaReserve)
            .unwrap();
        assert_eq!(
            reserved_after,
            reserved_before - locked_net,
            "{label}: reservation released on settlement"
        );
    }
}

fn after_dry_state_log_len(ledger: &Ledger, request_id: i64) -> usize {
    ledger.state_log(request_id).unwrap().len()
}

/// Every refusal, each on a fresh 4438-shaped park at a raised limit,
/// through the SAME ledger function the command calls.
#[tokio::test]
async fn every_refusal_is_named_and_writes_nothing() {
    let (src, dst, locked_net) = REQ_4438;

    // payout > live limit (and payout < live minimum)
    {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.sqlite3");
        let request_id = locked_request(&db_path, src, dst);
        park_at_settlement(&db_path, request_id).await;
        let mut ledger = Ledger::open(&db_path).unwrap();
        for (bounds, needle) in [
            (
                live(locked_net - 1),
                "exceeds the program's live per_transfer_limit",
            ),
            (
                LiveSolanaBounds {
                    min_transfer_amount: locked_net + 1,
                    per_transfer_limit: 2_000_000_000_000,
                    solana_decimals: TEST_SOLANA_DECIMALS,
                },
                "below the program's live min_transfer_amount",
            ),
        ] {
            let err = ledger
                .resume_glc_to_sol_destination_bound(request_id, bounds, "n", "cli:test", 50)
                .unwrap_err();
            assert!(err.to_string().contains(needle), "{err}");
        }
        // Exactly at the limit is admitted.
        assert_eq!(
            ledger
                .resume_glc_to_sol_destination_bound(
                    request_id,
                    live(locked_net),
                    "n",
                    "cli:test",
                    51
                )
                .unwrap(),
            ResumeManualReviewOutcome::Resumed
        );
    }

    // wrong route: a SolToGlc row (whatever its note) is refused by direction
    {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.sqlite3");
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(
                Direction::SolToGlc,
                glc_to_sol_amounts(500_000, TEST_SOLANA_DECIMALS),
                &[1u8; 20],
                None,
                3600,
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        let err = ledger
            .resume_glc_to_sol_destination_bound(
                request_id,
                live(2_000_000_000_000),
                "n",
                "cli:test",
                50,
            )
            .unwrap_err();
        assert!(err.to_string().contains("not GlcToSol"), "{err}");
    }

    // wrong ManualReview reason, missing locked quote, holds, conflicts,
    // missing reservation, invariant — on parked rows perturbed through
    // the ledger's own writers (or, for the two columns no public writer
    // sets on a parked row, the test connection).
    let park = || async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.sqlite3");
        let request_id = locked_request(&db_path, src, dst);
        park_at_settlement(&db_path, request_id).await;
        (dir, db_path, request_id)
    };

    // wrong reason
    {
        let (_dir, db_path, request_id) = park().await;
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .conn_for_tests()
            .execute(
                "UPDATE bridge_requests SET manual_review_note = 'insufficient_capacity' WHERE id = ?1",
                [request_id],
            )
            .unwrap();
        let err = ledger
            .resume_glc_to_sol_destination_bound(
                request_id,
                live(2_000_000_000_000),
                "n",
                "cli:test",
                50,
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("not \"destination_payout_out_of_bounds\""),
            "{err}"
        );
    }
    // missing locked quote
    {
        let (_dir, db_path, request_id) = park().await;
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .conn_for_tests()
            .execute(
                "UPDATE bridge_requests SET quote_locked_at = NULL WHERE id = ?1",
                [request_id],
            )
            .unwrap();
        let err = ledger
            .resume_glc_to_sol_destination_bound(
                request_id,
                live(2_000_000_000_000),
                "n",
                "cli:test",
                50,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("not locked") || err.to_string().contains("no locked"),
            "{err}"
        );
    }
    // operator hold and rapid-burst hold
    {
        let (_dir, db_path, request_id) = park().await;
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .conn_for_tests()
            .execute(
                "UPDATE bridge_requests SET auto_resume_hold_note = 'incident review' WHERE id = ?1",
                [request_id],
            )
            .unwrap();
        let err = ledger
            .resume_glc_to_sol_destination_bound(
                request_id,
                live(2_000_000_000_000),
                "n",
                "cli:test",
                50,
            )
            .unwrap_err();
        assert!(err.to_string().contains("held by operator"), "{err}");
        ledger
            .conn_for_tests()
            .execute(
                "UPDATE bridge_requests SET auto_resume_hold_note = NULL, manual_review_disposition = 'rapid_burst_hold', review_after = 999999 WHERE id = ?1",
                [request_id],
            )
            .unwrap();
        let err = ledger
            .resume_glc_to_sol_destination_bound(
                request_id,
                live(2_000_000_000_000),
                "n",
                "cli:test",
                50,
            )
            .unwrap_err();
        assert!(err.to_string().contains("rapid-burst hold"), "{err}");
    }
    // payout / refund / closure conflicts
    {
        let (_dir, db_path, request_id) = park().await;
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .conn_for_tests()
            .execute(
                "UPDATE bridge_requests SET destination_txid = X'01' WHERE id = ?1",
                [request_id],
            )
            .unwrap();
        let err = ledger
            .resume_glc_to_sol_destination_bound(
                request_id,
                live(2_000_000_000_000),
                "n",
                "cli:test",
                50,
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("destination transaction already exists"),
            "{err}"
        );
        ledger
            .conn_for_tests()
            .execute(
                "UPDATE bridge_requests SET destination_txid = NULL WHERE id = ?1",
                [request_id],
            )
            .unwrap();
        ledger
            .record_attestation(request_id, "release", &[1u8; 8], 50)
            .unwrap();
        let err = ledger
            .resume_glc_to_sol_destination_bound(
                request_id,
                live(2_000_000_000_000),
                "n",
                "cli:test",
                50,
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("release attestation was already recorded"),
            "{err}"
        );
    }
    {
        let (_dir, db_path, request_id) = park().await;
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .conn_for_tests()
            .execute(
                "INSERT INTO goldcoin_refunds (request_id, source_txid, source_vout,
                        observed_amount_atomic, source_input_txid, source_input_vout,
                        refund_dest_p2pkh_hash, refund_dest_address, refund_amount_atomic,
                        fee_atomic, state, manual_review_reason, note, created_by, built_at)
                 VALUES (?1, X'AB', 2, 1, X'AB', 0, X'01', 'addr', 1, 0, 'Built',
                         'destination_payout_out_of_bounds', 'n', 'cli:test', 1)",
                [request_id],
            )
            .unwrap_or_else(|e| panic!("seed a refund row: {e}"));
        let err = ledger
            .resume_glc_to_sol_destination_bound(
                request_id,
                live(2_000_000_000_000),
                "n",
                "cli:test",
                50,
            )
            .unwrap_err();
        assert!(err.to_string().contains("refund lifecycle exists"), "{err}");
    }
    {
        let (_dir, db_path, request_id) = park().await;
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .conn_for_tests()
            .execute(
                "INSERT INTO request_closures (request_id, disposition, reference, note, actor,
                        closed_at, from_state, manual_review_disposition)
                 VALUES (?1, 'refunded_out_of_band', 'txid', 'n', 'cli:test', 1, 'ManualReview', 'none')",
                [request_id],
            )
            .unwrap_or_else(|e| panic!("seed a closure row: {e}"));
        let err = ledger
            .resume_glc_to_sol_destination_bound(
                request_id,
                live(2_000_000_000_000),
                "n",
                "cli:test",
                50,
            )
            .unwrap_err();
        assert!(err.to_string().contains("closure is recorded"), "{err}");
    }
    // missing reservation, then a broken invariant
    {
        let (_dir, db_path, request_id) = park().await;
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .conn_for_tests()
            .execute(
                "UPDATE reserve_ledger SET reserved_liquidity = 0 WHERE direction = 'SolanaReserve'",
                [],
            )
            .unwrap();
        let err = ledger
            .resume_glc_to_sol_destination_bound(
                request_id,
                live(2_000_000_000_000),
                "n",
                "cli:test",
                50,
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("no longer holds this request's reservation"),
            "{err}"
        );
        ledger
            .conn_for_tests()
            .execute(
                "UPDATE reserve_ledger SET reserved_liquidity = ?1, total_reserve_balance = 1 WHERE direction = 'SolanaReserve'",
                [locked_net as i64],
            )
            .unwrap();
        let err = ledger
            .resume_glc_to_sol_destination_bound(
                request_id,
                live(2_000_000_000_000),
                "n",
                "cli:test",
                50,
            )
            .unwrap_err();
        assert!(
            matches!(err, LedgerError::InvariantViolated { .. }),
            "{err}"
        );
        // Every refusal above left the row parked.
        let r = ledger.get_request(request_id).unwrap().unwrap();
        assert_eq!(r.state, RequestState::ManualReview);
        assert_eq!(
            r.manual_review_note.as_deref(),
            Some(Ledger::MANUAL_REVIEW_REASON_DESTINATION_PAYOUT_OUT_OF_BOUNDS)
        );
    }
}
