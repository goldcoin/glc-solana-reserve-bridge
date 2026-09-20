//! The Solana halves of the two cross routes, driven through the REAL
//! orchestrator tick against the mock Solana node: `RhnToSol`'s fold and
//! reserve release, `SolToRhn`'s deposit classification and its Solana
//! close-out — plus the guarantee that neither happens unless the route
//! is priced, and that an unpriced deployment behaves exactly as before.

use super::*;
use crate::amount_conversion::{compute_fee_at_bps, CanonicalAtomic};
use crate::ledger::{RequestAmounts, RobinhoodDepositObservation};
use crate::routes::{Route, RouteGate, RoutesConfig};

const SOL_RECIPIENT: [u8; 32] = [0x51; 32];
const EVM_RECIPIENT_TEXT: &str = "0x00000000000000000000000000000000000000ec";
const MINT: [u8; 32] = [7u8; 32];

/// A gate with both cross routes OPEN on config and ledger; the adapter
/// leg is the permissive Solana adapter plus a verified Robinhood one.
fn open_cross_route_gate(db_path: &std::path::Path) -> Arc<RouteGate> {
    let mut ledger = Ledger::open(db_path).unwrap();
    for route in [Route::SolToRhn, Route::RhnToSol] {
        ledger.set_route_enabled(route, true, None).unwrap();
    }
    Arc::new(RouteGate::new(
        RoutesConfig::default().with_robinhood(true, true, true, true),
        crate::chains::ChainRegistry::with_verified_robinhood(
            crate::robinhood::testkit::MockNode::new(crate::robinhood::testkit::BRIDGE)
                .verified_deployment(),
        ),
    ))
}

fn closed_gate() -> Arc<RouteGate> {
    Arc::new(RouteGate::legacy_only())
}

fn configure_every_reserve(ledger: &mut Ledger) {
    // Goldcoin and Robinhood in canonical units, Solana in mint units.
    for (direction, balance) in [
        (ReserveDirection::GoldcoinReserve, 10_000_000_000u64),
        (ReserveDirection::SolanaReserve, 10_000_000),
        (ReserveDirection::RobinhoodReserve, 10_000_000_000),
    ] {
        ledger
            .configure_reserve(
                direction,
                balance,
                0,
                balance / 2,
                balance / 5,
                balance / 10,
                0,
            )
            .unwrap();
    }
}

fn solana_node(attestation_signers: &[Box<dyn AttestationSigner>]) -> Arc<MockSolanaRpc> {
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            5,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(MINT, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(MINT),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    solana_rpc
}

/// Records a FINAL `RhnToSol` observation the way the Robinhood indexer
/// would, for `gross_canonical` GLC bound for `SOL_RECIPIENT`.
fn record_final_rhn_to_sol_observation(ledger: &mut Ledger, index: u64, gross_canonical: u64) {
    let observation = RobinhoodDepositObservation {
        source_contract: crate::robinhood::testkit::BRIDGE.to_bytes(),
        obligation_index: index,
        route: Route::RhnToSol,
        depositor: [0x33; 20],
        destination: SOL_RECIPIENT.to_vec(),
        amount_robinhood_atomic: crate::evm::EvmU256::from_u128(
            u128::from(gross_canonical) * 10_000_000_000,
        )
        .to_be_bytes(),
        amount_canonical_atomic: gross_canonical,
        tx_hash: {
            let mut h = [0xaa; 32];
            h[0] = index as u8;
            h
        },
        log_index: 2,
        block_number: 100 + index,
        block_hash: [(100 + index) as u8; 32],
    };
    ledger
        .robinhood_apply_scan_range(
            &[observation],
            &[],
            100 + index,
            [(100 + index) as u8; 32],
            12,
            10,
        )
        .unwrap();
    ledger.robinhood_promote_final(200, 12, 11).unwrap();
}

// =====================================================================
// RhnToSol: fold + release, through the tick
// =====================================================================

#[tokio::test]
async fn rhn_to_sol_folds_and_releases_across_two_ticks_and_stops_at_destination_confirmed() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
        record_final_rhn_to_sol_observation(&mut ledger, 4, 500_000_000);
    }
    let gate = open_cross_route_gate(&db_path);
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let attestation_signers = attestation_signers();
    let solana_rpc = solana_node(&attestation_signers);
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    )
    .with_rhn_to_sol(CrossRouteFold {
        fee_bps: 300,
        route_gate: gate,
    })
    .with_robinhood_contract(crate::robinhood::testkit::BRIDGE);

    // Tick 1: the observation folds (RhnToSol, mint-unit destination
    // amount) and the release is submitted in the same tick.
    let report = orchestrator.tick(10).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.rhn_to_sol_folded, 1);
    assert_eq!(report.releases_submitted, 1);
    let request = orchestrator
        .ledger()
        .transfers_page(None, None, None, 10)
        .unwrap()
        .pop()
        .expect("one request");
    assert_eq!(request.direction, Direction::RhnToSol);
    assert_eq!(request.state, RequestState::DestinationSubmitted);
    assert_eq!(request.net_amount_atomic, 485_000_000);
    assert_eq!(request.net_destination_atomic, 4_850_000);
    assert_eq!(request.recipient, SOL_RECIPIENT.to_vec());
    let request_id = request.id;

    // The release claim bound the Robinhood deposit's own tx hash and
    // log index where a Goldcoin outpoint would go, and the mint-unit
    // net as the amount — the exact bytes every signer signed.
    let records = orchestrator.ledger().all_attestation_records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].action_type, "release");
    let message = &records[0].canonical_message;
    assert_eq!(&message[58..90], &request.source_txid.unwrap()[..]);
    assert_eq!(&message[90..94], &2u32.to_le_bytes()[..]);
    assert_eq!(&message[94..102], &4_850_000u64.to_le_bytes()[..]);
    assert_eq!(&message[102..134], &SOL_RECIPIENT[..]);
    // And the submitted transaction is the same release instruction
    // GlcToSol submits (ATA creation, proof, release), to this recipient.
    {
        let sent = solana_rpc.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].message.instructions.len(), 3);
    }

    // Tick 2: finalized on Solana — DestinationConfirmed, reserve moved,
    // NOT settled: the Robinhood obligation is closed by the settlement
    // loop afterwards.
    let signature = Signature::from(
        <[u8; 64]>::try_from(
            orchestrator
                .ledger()
                .get_destination_txid(request_id)
                .unwrap()
                .unwrap(),
        )
        .unwrap(),
    );
    solana_rpc.set_status(signature, Ok(()));
    let report = orchestrator.tick(20).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.releases_confirmed, 1);
    let request = orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(request.state, RequestState::DestinationConfirmed);
    assert_eq!(
        orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::SolanaReserve)
            .unwrap(),
        4_850_000
    );
    // A third tick re-folds nothing and re-releases nothing.
    let report = orchestrator.tick(30).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.rhn_to_sol_folded, 0);
    assert_eq!(report.releases_submitted, 0);
    assert_eq!(solana_rpc.sent.lock().unwrap().len(), 1);
}

/// The exact 4105 pattern: the release finalized, but its signature
/// aged out of the node's status cache before the poll ever saw it
/// (`get_signature_status` = None forever). After the grace period the
/// daemon proves the release from its claim PDA — amount and recipient
/// exactly the request's — and records DestinationConfirmed through the
/// normal body, re-sending nothing. Without the claim it keeps waiting.
#[tokio::test]
async fn an_aged_out_rhn_to_sol_release_is_confirmed_from_its_claim_pda_never_resent() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
        record_final_rhn_to_sol_observation(&mut ledger, 4, 500_000_000);
    }
    let gate = open_cross_route_gate(&db_path);
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let attestation_signers = attestation_signers();
    let solana_rpc = solana_node(&attestation_signers);
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    )
    .with_rhn_to_sol(CrossRouteFold {
        fee_bps: 300,
        route_gate: gate,
    })
    .with_robinhood_contract(crate::robinhood::testkit::BRIDGE);
    let report = orchestrator.tick(10).await;
    assert_eq!(report.releases_submitted, 1);
    let request = orchestrator
        .ledger()
        .transfers_page(None, None, None, 10)
        .unwrap()
        .pop()
        .unwrap();
    let request_id = request.id;
    let txid = request.source_txid.unwrap();
    let vout = request.source_vout.unwrap();

    // No status, no claim yet: still in flight — nothing happens, even
    // long after the grace period.
    let report = orchestrator.tick(10 + 700).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.releases_confirmed, 0);
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::DestinationSubmitted
    );

    // The claim appears (the release DID execute), status still None.
    let mut claim = vec![0u8; 8];
    claim.extend_from_slice(&txid);
    claim.extend_from_slice(&vout.to_le_bytes());
    claim.extend_from_slice(&4_850_000u64.to_le_bytes());
    claim.extend_from_slice(&SOL_RECIPIENT);
    claim.extend_from_slice(&0u64.to_le_bytes());
    claim.push(1);
    claim.extend_from_slice(&123u64.to_le_bytes());
    claim.push(1);
    claim.extend_from_slice(&[0u8; 16]);
    solana_rpc.set_account(accounts::deposit_claim_pda(&txid, vout), claim);

    // Inside the grace window measured from the submission: still waits.
    let report = orchestrator.tick(10 + 100).await;
    assert_eq!(report.releases_confirmed, 0);
    // Past it: proven from the claim, confirmed, nothing re-sent.
    let report = orchestrator.tick(10 + 700).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.releases_confirmed, 1);
    let request = orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(request.state, RequestState::DestinationConfirmed);
    assert_eq!(
        orchestrator
            .ledger()
            .state_log(request_id)
            .unwrap()
            .last()
            .unwrap()
            .3
            .as_deref(),
        Some(Ledger::CHAIN_TERMINAL_RECONCILIATION_REASON)
    );
    assert_eq!(
        orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::SolanaReserve)
            .unwrap(),
        4_850_000
    );
    assert_eq!(
        solana_rpc.sent.lock().unwrap().len(),
        1,
        "the release was never re-sent"
    );
    let report = orchestrator.tick(10 + 1_400).await;
    assert_eq!(
        (report.releases_confirmed, report.releases_submitted),
        (0, 0)
    );
}

/// A claim that names a different recipient is never accepted: the
/// request stays DestinationSubmitted with the refusal in the report.
#[tokio::test]
async fn an_aged_out_release_whose_claim_disagrees_is_not_confirmed() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
        record_final_rhn_to_sol_observation(&mut ledger, 4, 500_000_000);
    }
    let gate = open_cross_route_gate(&db_path);
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let attestation_signers = attestation_signers();
    let solana_rpc = solana_node(&attestation_signers);
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    )
    .with_rhn_to_sol(CrossRouteFold {
        fee_bps: 300,
        route_gate: gate,
    })
    .with_robinhood_contract(crate::robinhood::testkit::BRIDGE);
    orchestrator.tick(10).await;
    let request = orchestrator
        .ledger()
        .transfers_page(None, None, None, 10)
        .unwrap()
        .pop()
        .unwrap();
    let (txid, vout) = (request.source_txid.unwrap(), request.source_vout.unwrap());
    let mut claim = vec![0u8; 8];
    claim.extend_from_slice(&txid);
    claim.extend_from_slice(&vout.to_le_bytes());
    claim.extend_from_slice(&4_850_000u64.to_le_bytes());
    claim.extend_from_slice(&[0x77u8; 32]);
    claim.extend_from_slice(&0u64.to_le_bytes());
    claim.push(1);
    claim.extend_from_slice(&123u64.to_le_bytes());
    claim.push(1);
    claim.extend_from_slice(&[0u8; 16]);
    solana_rpc.set_account(accounts::deposit_claim_pda(&txid, vout), claim);
    let report = orchestrator.tick(10 + 700).await;
    assert_eq!(report.releases_confirmed, 0);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("chain-terminal proof refused") && e.contains("recipient")),
        "{:?}",
        report.errors
    );
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request.id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::DestinationSubmitted
    );
}

#[tokio::test]
async fn rhn_to_sol_folds_parked_while_the_route_is_closed_and_releases_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
        record_final_rhn_to_sol_observation(&mut ledger, 0, 500_000_000);
    }
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let attestation_signers = attestation_signers();
    let solana_rpc = solana_node(&attestation_signers);
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    )
    .with_rhn_to_sol(CrossRouteFold {
        fee_bps: 300,
        route_gate: closed_gate(),
    });
    let report = orchestrator.tick(10).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.rhn_to_sol_folded, 1);
    assert_eq!(report.releases_submitted, 0);
    let request = orchestrator
        .ledger()
        .transfers_page(None, None, None, 10)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(request.state, RequestState::ManualReview);
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some("route_disabled_at_fold")
    );
    assert!(solana_rpc.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unpriced_rhn_to_sol_leaves_the_observation_recorded_and_unfolded() {
    // No `with_rhn_to_sol`: exactly the pre-Phase-H behaviour.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
        record_final_rhn_to_sol_observation(&mut ledger, 0, 500_000_000);
    }
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let attestation_signers = attestation_signers();
    let solana_rpc = solana_node(&attestation_signers);
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );
    let report = orchestrator.tick(10).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.rhn_to_sol_folded, 0);
    assert!(orchestrator
        .ledger()
        .transfers_page(None, None, None, 10)
        .unwrap()
        .is_empty());
    assert_eq!(
        orchestrator
            .ledger()
            .unfolded_final_robinhood_observations()
            .unwrap()
            .len(),
        1
    );
}

// =====================================================================
// SolToRhn: classification at the Solana indexer, close-out on Solana
// =====================================================================

fn sol_to_rhn_node_with_deposit(
    attestation_signers: &[Box<dyn AttestationSigner>],
    destination: &[u8],
    amount_mint_units: u64,
) -> Arc<MockSolanaRpc> {
    let solana_rpc = solana_node(attestation_signers);
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(MINT, 1),
    );
    solana_rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_withdrawal_obligation_bytes(0, amount_mint_units, &[5u8; 32], destination),
    );
    solana_rpc
}

#[tokio::test]
async fn a_robinhood_bound_solana_deposit_folds_as_sol_to_rhn_when_the_route_is_priced() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
    }
    let gate = open_cross_route_gate(&db_path);
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let attestation_signers = attestation_signers();
    let solana_rpc =
        sol_to_rhn_node_with_deposit(&attestation_signers, EVM_RECIPIENT_TEXT.as_bytes(), 500_000);
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    )
    .with_sol_to_rhn(CrossRouteFold {
        fee_bps: 450,
        route_gate: gate,
    });
    let report = orchestrator.tick(10).await;
    assert_eq!(report.errors, Vec::<String>::new());
    let request = orchestrator
        .ledger()
        .transfers_page(None, None, None, 10)
        .unwrap()
        .pop()
        .expect("one request");
    assert_eq!(request.direction, Direction::SolToRhn);
    assert_eq!(request.state, RequestState::SourceFinalized);
    assert_eq!(request.fee_bps, 450, "THIS route's rate, not SolToGlc's");
    assert_eq!(
        request.gross_amount_atomic, 50_000_000,
        "500_000 mint units x100"
    );
    assert_eq!(
        request.recipient,
        vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xec]
    );
    assert_eq!(request.requester, Some([5u8; 32]));
    // Reserved on Robinhood, never on Goldcoin.
    let (_, _, reserved, _) = orchestrator
        .ledger()
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(reserved, request.net_destination_atomic);
    let (_, _, goldcoin_reserved, _) = orchestrator
        .ledger()
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(goldcoin_reserved, 0);
    // No Goldcoin payout was built for it, and no Solana release either.
    assert_eq!(report.payouts_built, 0);
    assert_eq!(report.releases_submitted, 0);
}

#[tokio::test]
async fn a_malformed_robinhood_destination_parks_as_sol_to_rhn_never_as_sol_to_glc() {
    // A mixed-case spelling whose EIP-55 checksum does not verify: the
    // real checksum spelling of an address with one letter's case flipped.
    let checksummed = crate::evm::address::EvmAddress::from_bytes([0xab; 20]).to_checksum_string();
    let flipped: String = {
        let mut done = false;
        checksummed
            .chars()
            .map(|c| {
                if !done && c.is_ascii_alphabetic() && c != 'x' {
                    done = true;
                    if c.is_ascii_uppercase() {
                        c.to_ascii_lowercase()
                    } else {
                        c.to_ascii_uppercase()
                    }
                } else {
                    c
                }
            })
            .collect()
    };
    assert!(flipped.parse::<crate::evm::address::EvmAddress>().is_err());
    for (bad, why) in [
        ("0xnothex".to_string(), "bad hex"),
        (
            "0x0000000000000000000000000000000000000000".to_string(),
            "the zero address",
        ),
        (flipped, "a checksum that does not verify"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.sqlite3");
        {
            let mut ledger = Ledger::open(&db_path).unwrap();
            configure_every_reserve(&mut ledger);
        }
        let gate = open_cross_route_gate(&db_path);
        let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
        let attestation_signers = attestation_signers();
        let solana_rpc =
            sol_to_rhn_node_with_deposit(&attestation_signers, bad.as_bytes(), 500_000);
        let (vault, vault_signers) = vault_and_signers();
        let mut orchestrator = build_orchestrator(
            &db_path,
            goldcoin_rpc,
            Arc::clone(&solana_rpc),
            vault,
            vault_signers,
            attestation_signers,
        )
        .with_sol_to_rhn(CrossRouteFold {
            fee_bps: 450,
            route_gate: gate,
        });
        let report = orchestrator.tick(10).await;
        assert_eq!(report.errors, Vec::<String>::new(), "{why}");
        let request = orchestrator
            .ledger()
            .transfers_page(None, None, None, 10)
            .unwrap()
            .pop()
            .expect("one request");
        assert_eq!(request.direction, Direction::SolToRhn, "{why}");
        assert_eq!(request.state, RequestState::ManualReview, "{why}");
        assert!(
            request
                .manual_review_note
                .as_deref()
                .unwrap()
                .starts_with("undeliverable destination"),
            "{why}: {:?}",
            request.manual_review_note
        );
        assert_eq!(request.recipient, bad.as_bytes().to_vec(), "{why}");
        // Refundable on Solana.
        assert_eq!(
            orchestrator
                .ledger()
                .solana_refund_db_checks(request.id)
                .unwrap()
                .first_failure_for_begin(),
            None,
            "{why}"
        );
    }
}

/// docs/40-destination-bound-admission.md at the fold: a Robinhood-bound
/// Solana deposit whose quoted payout exceeds the contract's `outboundMax`
/// (less the buffer) is parked `destination_payout_out_of_bounds` AT THE
/// FOLD — before any Robinhood capacity is held for it — while one at
/// the maximum folds payable; and a contract that cannot be read makes
/// no decision (the settler's own check still refuses).
#[tokio::test]
async fn a_sol_to_rhn_deposit_above_the_outbound_max_derived_maximum_parks_at_the_fold() {
    use crate::bridge_rate::{max_source_for_destination_limit, RailPrices};
    use crate::robinhood::testkit::{MockNode, BRIDGE};
    use crate::solana::indexer::RobinhoodDestinationLimits;

    // outboundMax 0.06 GLC (18 dp); 450 bps; unit rate; 25 % buffer →
    // the largest gross whose net fits 0.045 GLC.
    let node = MockNode::new(BRIDGE);
    node.with(|s| {
        s.contract.limits.outbound_max = crate::evm::EvmU256::from_u128(60_000_000_000_000_000)
    });
    let max = max_source_for_destination_limit(
        CanonicalAtomic(6_000_000),
        RailPrices::unit(0),
        450,
        1,
        2_500,
    )
    .unwrap()
    .0;
    assert_eq!(max, 4_712_041);
    let limits = || {
        Some(RobinhoodDestinationLimits {
            source: Arc::new(crate::robinhood::public::LiveRobinhoodContractSource::new(
                node.clone(),
                BRIDGE,
            )),
            buffer_bps: 2_500,
        })
    };
    // Mint units (6 dp) are canonical / 100.
    for (mint_units, expect_parked, why) in [
        (max / 100, false, "at the maximum: payable"),
        (
            max / 100 + 1,
            true,
            "one mint unit above: parked at the fold",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.sqlite3");
        {
            let mut ledger = Ledger::open(&db_path).unwrap();
            configure_every_reserve(&mut ledger);
        }
        let gate = open_cross_route_gate(&db_path);
        let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
        let attestation_signers = attestation_signers();
        let solana_rpc = sol_to_rhn_node_with_deposit(
            &attestation_signers,
            EVM_RECIPIENT_TEXT.as_bytes(),
            mint_units,
        );
        let (vault, vault_signers) = vault_and_signers();
        let mut orchestrator = build_orchestrator(
            &db_path,
            goldcoin_rpc,
            Arc::clone(&solana_rpc),
            vault,
            vault_signers,
            attestation_signers,
        )
        .with_sol_to_rhn(CrossRouteFold {
            fee_bps: 450,
            route_gate: gate,
        })
        .with_robinhood_destination_limits(limits());
        let report = orchestrator.tick(10).await;
        assert_eq!(report.errors, Vec::<String>::new(), "{why}");
        let request = orchestrator
            .ledger()
            .transfers_page(None, None, None, 10)
            .unwrap()
            .pop()
            .expect("one request");
        assert_eq!(request.direction, Direction::SolToRhn, "{why}");
        assert_eq!(request.gross_amount_atomic, mint_units * 100, "{why}");
        let (_, _, reserved, _) = orchestrator
            .ledger()
            .reserve_snapshot(ReserveDirection::RobinhoodReserve)
            .unwrap();
        if expect_parked {
            assert_eq!(request.state, RequestState::ManualReview, "{why}");
            assert_eq!(
                request.manual_review_note.as_deref(),
                Some(Ledger::MANUAL_REVIEW_REASON_DESTINATION_PAYOUT_OUT_OF_BOUNDS),
                "{why}"
            );
            assert_eq!(reserved, 0, "{why}: no Robinhood capacity held");
            // Refundable on Solana, like every other fold-time park.
            assert_eq!(
                orchestrator
                    .ledger()
                    .solana_refund_db_checks(request.id)
                    .unwrap()
                    .first_failure_for_begin(),
                None,
                "{why}"
            );
        } else {
            assert_eq!(request.state, RequestState::SourceFinalized, "{why}");
            assert_eq!(reserved, request.net_destination_atomic, "{why}");
        }
    }

    // An unreadable contract: the deposit above the maximum folds as it
    // did before this check existed (payable at the fold; the settler
    // refuses it later), never parked on a read failure.
    node.fail_calls("node down");
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
    }
    let gate = open_cross_route_gate(&db_path);
    let attestation_signers = attestation_signers();
    let solana_rpc = sol_to_rhn_node_with_deposit(
        &attestation_signers,
        EVM_RECIPIENT_TEXT.as_bytes(),
        max / 100 + 1,
    );
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::new(MockGoldcoinRpc::new()),
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    )
    .with_sol_to_rhn(CrossRouteFold {
        fee_bps: 450,
        route_gate: gate,
    })
    .with_robinhood_destination_limits(limits());
    let report = orchestrator.tick(10).await;
    assert_eq!(report.errors, Vec::<String>::new());
    let request = orchestrator
        .ledger()
        .transfers_page(None, None, None, 10)
        .unwrap()
        .pop()
        .expect("one request");
    assert_eq!(request.state, RequestState::SourceFinalized);
}

#[tokio::test]
async fn a_goldcoin_bound_solana_deposit_still_folds_as_sol_to_glc_with_classification_on() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
    }
    let gate = open_cross_route_gate(&db_path);
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let attestation_signers = attestation_signers();
    let solana_rpc =
        sol_to_rhn_node_with_deposit(&attestation_signers, dest_addr.as_bytes(), 500_000);
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    )
    .with_sol_to_rhn(CrossRouteFold {
        fee_bps: 450,
        route_gate: gate,
    });
    let _ = orchestrator.tick(10).await;
    let request = orchestrator
        .ledger()
        .transfers_page(None, None, None, 10)
        .unwrap()
        .pop()
        .expect("one request");
    assert_eq!(request.direction, Direction::SolToGlc);
    assert_eq!(request.fee_bps, crate::amount_conversion::BRIDGE_FEE_BPS);
    assert_eq!(request.recipient, dest_addr.as_bytes().to_vec());
}

#[tokio::test]
async fn without_a_sol_to_rhn_rate_every_solana_deposit_folds_as_sol_to_glc_as_before() {
    // The pre-Phase-H behaviour, bit for bit: a `0x` destination is not
    // classified at all and folds as SolToGlc at SolToGlc's rate.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
    }
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let attestation_signers = attestation_signers();
    let solana_rpc =
        sol_to_rhn_node_with_deposit(&attestation_signers, EVM_RECIPIENT_TEXT.as_bytes(), 500_000);
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );
    let _ = orchestrator.tick(10).await;
    let request = orchestrator
        .ledger()
        .transfers_page(None, None, None, 10)
        .unwrap()
        .pop()
        .expect("one request");
    assert_eq!(request.direction, Direction::SolToGlc);
    assert_eq!(request.fee_bps, crate::amount_conversion::BRIDGE_FEE_BPS);
}

/// A `SolToRhn` request whose Robinhood payout is FINAL, as the settlement
/// loop leaves it: `DestinationConfirmed`, Robinhood reserve moved, and a
/// `Finalized` payout operation row carrying the EVM tx hash and block.
fn seed_sol_to_rhn_paid_out(ledger: &mut Ledger, request_id_hint: u64) -> (i64, [u8; 32]) {
    let fb = compute_fee_at_bps(CanonicalAtomic(50_000_000), 450).unwrap();
    let request_id = match ledger
        .fold_sol_deposit_to_robinhood(
            request_id_hint,
            RequestAmounts {
                gross_atomic: fb.gross.0,
                fee_bps: fb.fee_bps,
                fee_atomic: fb.fee.0,
                net_atomic: fb.net.0,
                net_destination_atomic: fb.net.0,
                quote: None,
            },
            [5u8; 32],
            Some([0xec; 20]),
            EVM_RECIPIENT_TEXT.as_bytes(),
            true,
            None,
            5,
        )
        .unwrap()
    {
        SolFoldOutcome::FoldedFinalized { request_id } => request_id,
        other => panic!("{other:?}"),
    };
    let evm_tx_hash = [0x9a; 32];
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_transactions
                (kind, request_id, route, bridge_contract, chain_id, action,
                 contract_request_id, recipient, amount_robinhood, signer_epoch, expiry,
                 auth_digest, submitter, nonce, envelope, raw_tx, state, tx_hash,
                 first_broadcast_at, broadcast_attempts, receipt_status,
                 receipt_block_number, confirmations, finalized_at, created_at, updated_at)
             VALUES ('Payout', ?1, 'SolToRhn', ?2, 4663, 1, ?3, ?4, ?5, 1, 9999, ?3,
                     ?4, 0, 'eip1559', X'02', 'Finalized', ?6, 6, 1, 1, 777, 12, 8, 5, 8)",
            rusqlite::params![
                request_id,
                &crate::robinhood::testkit::BRIDGE.to_bytes()[..],
                &[0x01u8; 32][..],
                &[0xecu8; 20][..],
                &[0u8; 32][..],
                &evm_tx_hash[..],
            ],
        )
        .unwrap();
    ledger.mark_robinhood_payout_settled(request_id, 8).unwrap();
    (request_id, evm_tx_hash)
}

#[tokio::test]
async fn sol_to_rhn_closes_its_solana_obligation_after_the_robinhood_payout_is_final() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (request_id, evm_tx_hash) = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
        seed_sol_to_rhn_paid_out(&mut ledger, 0)
    };
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let attestation_signers = attestation_signers();
    // The obligation on Solana, still Pending, for the SolToRhn deposit:
    // 500_000 mint units, bound for the EVM address.
    let solana_rpc =
        sol_to_rhn_node_with_deposit(&attestation_signers, EVM_RECIPIENT_TEXT.as_bytes(), 500_000);
    // The indexer must not re-fold it: the cursor is already past it.
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger.set_last_synced_obligation_count(1, 1, 5).unwrap();
    }
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    // Tick 1: the completion is submitted, binding the EVM tx hash as
    // the payout id, its block as the height, and the canonical net.
    let report = orchestrator.tick(10).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.completions_submitted, 1);
    let (signature, submitted_at) = orchestrator
        .ledger()
        .robinhood_payout_completion_submission(request_id)
        .unwrap()
        .expect("the submission is recorded on the payout row");
    assert_eq!(submitted_at, 10);
    let records = orchestrator.ledger().all_attestation_records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].action_type, "completion");
    let message = &records[0].canonical_message;
    assert_eq!(
        &message[58..66],
        &0u64.to_le_bytes()[..],
        "obligation index"
    );
    assert_eq!(&message[66..98], &evm_tx_hash[..], "the EVM payout tx hash");
    assert_eq!(
        &message[98..106],
        &777u64.to_le_bytes()[..],
        "its block number"
    );
    let net = compute_fee_at_bps(CanonicalAtomic(50_000_000), 450)
        .unwrap()
        .net
        .0;
    assert_eq!(
        &message[106..114],
        &net.to_le_bytes()[..],
        "the canonical net"
    );
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::DestinationConfirmed
    );

    // Tick 2: confirmed on Solana — Settled, with no reserve movement
    // (the Robinhood reserve moved at payout finality).
    let robinhood_before = orchestrator
        .ledger()
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    solana_rpc.set_status(Signature::from(signature), Ok(()));
    let report = orchestrator.tick(20).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.completions_confirmed, 1);
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::Settled
    );
    assert_eq!(
        orchestrator
            .ledger()
            .reserve_snapshot(ReserveDirection::RobinhoodReserve)
            .unwrap(),
        robinhood_before
    );
    // And a third tick does nothing.
    let report = orchestrator.tick(30).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.completions_submitted, 0);
    assert_eq!(report.completions_confirmed, 0);
    assert_eq!(solana_rpc.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_dropped_sol_to_rhn_completion_is_resubmitted_and_settles_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (request_id, _) = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
        seed_sol_to_rhn_paid_out(&mut ledger, 0)
    };
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let attestation_signers = attestation_signers();
    let solana_rpc =
        sol_to_rhn_node_with_deposit(&attestation_signers, EVM_RECIPIENT_TEXT.as_bytes(), 500_000);
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger.set_last_synced_obligation_count(1, 1, 5).unwrap();
    }
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );
    let report = orchestrator.tick(10).await;
    assert_eq!(report.completions_submitted, 1, "{:?}", report.errors);
    let (first, _) = orchestrator
        .ledger()
        .robinhood_payout_completion_submission(request_id)
        .unwrap()
        .unwrap();
    // Inside the grace window: in flight, nothing re-sent.
    let report = orchestrator.tick(100).await;
    assert_eq!(report.completions_submitted, 0, "{:?}", report.errors);
    // Past it, obligation still Pending: re-submitted under a new
    // signature.
    let report = orchestrator.tick(400).await;
    assert_eq!(report.completions_submitted, 1, "{:?}", report.errors);
    let (second, _) = orchestrator
        .ledger()
        .robinhood_payout_completion_submission(request_id)
        .unwrap()
        .unwrap();
    assert_ne!(first, second);
    // The obligation is now Completed on-chain even though the status
    // cache never answers: settled from the obligation's own status.
    solana_rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_withdrawal_obligation_bytes_with_status(
            0,
            500_000,
            &[5u8; 32],
            EVM_RECIPIENT_TEXT.as_bytes(),
            accounts::WITHDRAWAL_STATUS_COMPLETED,
        ),
    );
    let report = orchestrator.tick(800).await;
    assert_eq!(report.completions_confirmed, 1, "{:?}", report.errors);
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::Settled
    );
    assert_eq!(solana_rpc.sent.lock().unwrap().len(), 2);
}

// =====================================================================
// RhnToSol: the contract binding in front of the release
// =====================================================================

/// The V1/V2 shape: a `RhnToSol` deposit recorded under a contract other
/// than the one the process is bound to. It folds (the fold records the
/// TRUE contract), but the release is never built — the request is
/// parked as `foreign_contract`, nothing is signed, nothing is sent.
#[tokio::test]
async fn an_rhn_to_sol_deposit_on_a_foreign_contract_is_parked_not_released() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
        record_final_rhn_to_sol_observation(&mut ledger, 29, 500_000_000);
    }
    let gate = open_cross_route_gate(&db_path);
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let attestation_signers = attestation_signers();
    let solana_rpc = solana_node(&attestation_signers);
    let (vault, vault_signers) = vault_and_signers();
    let successor = crate::evm::EvmAddress::try_from_slice(&[0xba; 20]).unwrap();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    )
    .with_rhn_to_sol(CrossRouteFold {
        fee_bps: 300,
        route_gate: gate,
    })
    // Bound to the SUCCESSOR; the observation above is on `BRIDGE`.
    .with_robinhood_contract(successor);

    let report = orchestrator.tick(10).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.rhn_to_sol_folded, 1);
    assert_eq!(report.releases_submitted, 0);
    assert_eq!(report.foreign_contract_parked, 1);
    let request = orchestrator
        .ledger()
        .transfers_page(None, None, None, 10)
        .unwrap()
        .pop()
        .expect("one request");
    assert_eq!(request.direction, Direction::RhnToSol);
    assert_eq!(request.state, RequestState::ManualReview);
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some(Ledger::MANUAL_REVIEW_REASON_FOREIGN_CONTRACT)
    );
    assert!(orchestrator
        .ledger()
        .all_attestation_records()
        .unwrap()
        .is_empty());
    assert!(solana_rpc.sent.lock().unwrap().is_empty());
    // The park is not auto-resumable and a second tick does nothing.
    assert!(!Ledger::is_recoverable_manual_review_reason(
        request.manual_review_note.as_deref()
    ));
    let report = orchestrator.tick(20).await;
    assert_eq!(report.foreign_contract_parked, 0);
    assert_eq!(report.releases_submitted, 0);
    let events = orchestrator.ledger().state_log(request.id).unwrap();
    let (_, to_state, _, reason) = events.last().unwrap();
    assert_eq!(*to_state, RequestState::ManualReview);
    assert!(
        reason
            .as_deref()
            .unwrap_or("")
            .starts_with("foreign_contract: "),
        "{reason:?}"
    );
}

/// No binding at all (no `[robinhood.indexer]`) is the same refusal: a
/// Robinhood-sourced request has nowhere to be closed out.
#[tokio::test]
async fn an_rhn_to_sol_release_with_no_bound_contract_is_parked() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_every_reserve(&mut ledger);
        record_final_rhn_to_sol_observation(&mut ledger, 5, 500_000_000);
    }
    let gate = open_cross_route_gate(&db_path);
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let attestation_signers = attestation_signers();
    let solana_rpc = solana_node(&attestation_signers);
    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    )
    .with_rhn_to_sol(CrossRouteFold {
        fee_bps: 300,
        route_gate: gate,
    });
    let report = orchestrator.tick(10).await;
    assert_eq!(report.releases_submitted, 0);
    assert_eq!(report.foreign_contract_parked, 1);
    assert!(solana_rpc.sent.lock().unwrap().is_empty());
}
