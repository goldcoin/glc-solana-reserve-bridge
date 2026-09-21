//! Preflight tests.
//!
//! Every check is a REFUSAL, not a warning, and each of these sets
//! exactly one thing wrong and changes nothing else — so a passing test
//! proves that check is load-bearing rather than that the whole fixture
//! happens to be consistent.

use super::*;
use crate::robinhood::testkit::{signer_addresses, MockNode, BRIDGE, TOKEN};

fn node() -> MockNode {
    MockNode::new(BRIDGE)
}

async fn run(node: &MockNode) -> Result<VerifiedDeployment, PreflightError> {
    verify(node, &node.indexer_config(), &node.settlement_config()).await
}

#[tokio::test]
async fn a_healthy_deployment_verifies_and_reports_what_it_established() {
    let node = node();
    let verified = run(&node).await.expect("a healthy deployment verifies");

    assert_eq!(verified.chain_id.get(), 4663);
    assert_eq!(verified.bridge_contract, BRIDGE);
    assert_eq!(verified.token, TOKEN);
    assert_eq!(verified.token_decimals, 18);
    assert_eq!(verified.signers, signer_addresses());
    assert_eq!(verified.tx_envelope, TxEnvelope::Eip1559);
    assert!(verified.chain_has_base_fee);

    // The chain pairs are read from the CONTRACT, not configured.
    assert_eq!(
        verified.chains_for(Route::GlcToRhn).unwrap().source,
        crate::robinhood::testkit::PROTOCOL_GOLDCOIN
    );
    assert_eq!(
        verified.chains_for(Route::RhnToGlc).unwrap().source,
        crate::robinhood::testkit::PROTOCOL_ROBINHOOD
    );
    // The two Solana<->Robinhood pairs are read off the contract too —
    // every leg is an immutable the deployment carries — and no pair is
    // offered for a route the contract does not model.
    assert_eq!(
        verified.chains_for(Route::SolToRhn).unwrap(),
        crate::robinhood::auth::ProtocolChainPair {
            source: crate::robinhood::testkit::PROTOCOL_SOLANA,
            dest: crate::robinhood::testkit::PROTOCOL_ROBINHOOD,
        }
    );
    assert_eq!(
        verified.chains_for(Route::RhnToSol).unwrap(),
        crate::robinhood::auth::ProtocolChainPair {
            source: crate::robinhood::testkit::PROTOCOL_ROBINHOOD,
            dest: crate::robinhood::testkit::PROTOCOL_SOLANA,
        }
    );
    assert!(verified.chains_for(Route::GlcToSol).is_none());
    assert!(verified.chains_for(Route::SolToGlc).is_none());
}

#[tokio::test]
async fn a_wrong_chain_is_refused() {
    let node = node();
    let mut cfg = node.settlement_config();
    // A settlement config for the testnet, pointed at a mainnet endpoint.
    // Built directly rather than through `new`, which would refuse the
    // mismatch against the indexer first — this isolates the CHAIN check.
    cfg.chain_id = crate::evm::EvmChainId::new(46630).unwrap();
    let err = verify(&node, &node.indexer_config(), &cfg)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            PreflightError::WrongChain {
                expected: 46630,
                actual: 4663
            }
        ),
        "{err}"
    );
}

#[tokio::test]
async fn an_address_with_no_contract_code_is_refused_with_its_own_message() {
    // An `eth_call` to a codeless address returns EMPTY DATA rather than
    // failing, so without this check the operator would see a confusing
    // ABI decode error instead of "there is nothing deployed there".
    let node = node();
    node.with(|s| s.contract.bridge_code = Vec::new());
    let err = run(&node).await.unwrap_err();
    assert!(
        matches!(err, PreflightError::NoContractCode { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn a_token_address_with_no_code_is_refused_separately() {
    let node = node();
    node.with(|s| s.contract.token_code = Vec::new());
    let err = run(&node).await.unwrap_err();
    assert!(matches!(err, PreflightError::NoTokenCode { .. }), "{err}");
}

#[tokio::test]
async fn a_contract_of_a_different_protocol_family_is_refused() {
    let node = node();
    node.with(|s| s.contract.protocol_id = [0x99; 32]);
    let err = run(&node).await.unwrap_err();
    assert!(matches!(err, PreflightError::WrongProtocol { .. }), "{err}");
}

#[tokio::test]
async fn the_wrong_token_is_refused() {
    // Paying out against this contract would move a DIFFERENT asset.
    let node = node();
    node.with(|s| s.contract.token = crate::evm::EvmAddress::from_bytes([0x99; 20]));
    let err = run(&node).await.unwrap_err();
    assert!(matches!(err, PreflightError::WrongToken { .. }), "{err}");
}

#[tokio::test]
async fn a_token_with_the_wrong_decimals_is_refused() {
    // Not "scale differently" — this is not the asset the amount model is
    // built for, and every amount would be off by orders of magnitude.
    for decimals in [6u8, 8, 17, 19] {
        let node = node();
        node.with(|s| s.contract.token_decimals = decimals);
        let err = run(&node).await.unwrap_err();
        assert!(
            matches!(err, PreflightError::WrongDecimals { actual, .. } if actual == decimals),
            "{decimals} decimals must be refused, got {err}"
        );
    }
}

#[tokio::test]
async fn a_signer_set_that_disagrees_with_the_contract_is_refused() {
    // Every quorum this service assembled would be rejected on-chain
    // after gas was spent.
    let node = node();
    node.with(|s| s.contract.signers[1] = crate::evm::EvmAddress::from_bytes([0x99; 20]));
    let err = run(&node).await.unwrap_err();
    assert!(
        matches!(err, PreflightError::WrongSignerSet { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn the_signer_set_is_compared_as_a_set_not_as_an_ordered_list() {
    // The contract stores an array but membership is a mapping, and a
    // rotation may legitimately reorder. Requiring an order would make a
    // correct configuration fail.
    let node = node();
    node.with(|s| {
        let signers = s.contract.signers;
        s.contract.signers = [signers[2], signers[0], signers[1]];
    });
    run(&node)
        .await
        .expect("a reordered signer set is the same set");
}

#[tokio::test]
async fn a_domain_separator_that_disagrees_with_the_deployment_is_refused() {
    // The golden fixture proves the FORMULA matches the contract's. This
    // proves the formula, applied to THIS deployment, produces the
    // separator the deployed contract actually uses.
    let node = node();
    node.with(|s| s.contract.domain_separator = [0x5a; 32]);
    let err = run(&node).await.unwrap_err();
    assert!(
        matches!(err, PreflightError::DomainSeparatorMismatch { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn a_migrated_contract_is_refused_as_terminal() {
    let node = node();
    node.with(|s| s.contract.migrated = true);
    let err = run(&node).await.unwrap_err();
    assert!(matches!(err, PreflightError::AlreadyMigrated), "{err}");
}

#[tokio::test]
async fn the_envelope_is_verified_against_the_chains_own_fee_market_in_both_directions() {
    // THE check the whole configurable-envelope decision rests on. A
    // configured value nothing verifies is still a guess — just one an
    // operator made instead of one this code made.
    let node = node();

    // eip1559 configured, chain has NO base fee.
    node.with(|s| s.contract.base_fee = None);
    let err = run(&node).await.unwrap_err();
    assert!(
        matches!(
            err,
            PreflightError::EnvelopeMismatch {
                envelope: "eip1559",
                ..
            }
        ),
        "{err}"
    );

    // legacy configured, chain HAS a base fee.
    node.with(|s| s.contract.base_fee = Some(1_000_000_000));
    let mut cfg = node.settlement_config();
    cfg.tx_envelope = TxEnvelope::Legacy;
    let err = verify(&node, &node.indexer_config(), &cfg)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            PreflightError::EnvelopeMismatch {
                envelope: "legacy",
                ..
            }
        ),
        "{err}"
    );

    // legacy configured, no base fee: agrees.
    node.with(|s| s.contract.base_fee = None);
    let verified = verify(&node, &node.indexer_config(), &cfg).await.unwrap();
    assert_eq!(verified.tx_envelope, TxEnvelope::Legacy);
    assert!(!verified.chain_has_base_fee);
}

#[tokio::test]
async fn a_disabled_route_does_not_fail_preflight() {
    // Route enablement is a GOVERNANCE state, checked live before every
    // broadcast — not a startup condition. A deployment must be able to
    // start against a contract whose routes are all still closed, which
    // is exactly the state a fresh deployment is in.
    let node = node();
    node.with(|s| {
        s.contract.route_enabled.insert(0x01, false);
        s.contract.route_enabled.insert(0x02, false);
    });
    run(&node)
        .await
        .expect("a closed route is a governance state, not a preflight failure");
}

#[tokio::test]
async fn a_paused_contract_does_not_fail_preflight() {
    // Same reasoning: a pause is what a guardian just did, and a service
    // that refused to START during an incident could not be brought up to
    // observe or refund.
    let node = node();
    node.with(|s| {
        s.contract.deposits_paused = true;
        s.contract.payouts_paused = true;
    });
    run(&node)
        .await
        .expect("a pause is not a preflight failure");
}

#[tokio::test]
async fn preflight_reads_the_contract_and_does_not_broadcast_anything() {
    let node = node();
    run(&node).await.unwrap();
    let calls = node.with(|s| s.calls.clone());
    assert!(calls.iter().any(|c| c == "eth_call"));
    assert!(calls.iter().any(|c| c == "eth_getCode"));
    assert!(
        !calls.iter().any(|c| c == "eth_sendRawTransaction"),
        "preflight must never broadcast: {calls:?}"
    );
}

// ==================================================================
// The operator preflight
// ==================================================================
//
// `verify` answers yes or no; this answers the whole picture, and the
// property that matters most is the third verdict. A report with only
// PASS and FAIL forces every check into a claim, and the most harmful
// thing this module could do is answer "pass" to a question it never
// asked.

async fn operator_run(node: &MockNode, signers_available: usize) -> PreflightReport {
    operator_preflight(
        node,
        &OperatorPreflightInputs {
            indexer: &node.indexer_config(),
            settlement: &node.settlement_config(),
            expected_routes: ExpectedRoutes::default(),
            signers_available,
            signers_required: crate::robinhood::SIGNER_THRESHOLD,
            policy: None,
            route_fees: None,
        },
    )
    .await
}

fn verdict(report: &PreflightReport, name: &str) -> Verdict {
    report
        .checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no check named {name} in {:#?}", report.checks))
        .verdict
}

/// Every check the mock's healthy deployment can satisfy does — including
/// the contract's four route flags, which the fixture ships with the two
/// executable routes OPEN.
#[tokio::test]
async fn a_healthy_deployment_passes_the_chain_and_token_identity_checks() {
    let node = node();
    // The fixture's contract has GlcToRhn/RhnToGlc enabled, so expecting
    // them open is what makes this a clean run.
    let report = operator_preflight(
        &node,
        &OperatorPreflightInputs {
            indexer: &node.indexer_config(),
            settlement: &node.settlement_config(),
            expected_routes: ExpectedRoutes {
                expect_enabled: vec![Route::GlcToRhn, Route::RhnToGlc],
            },
            signers_available: 3,
            signers_required: crate::robinhood::SIGNER_THRESHOLD,
            policy: None,
            route_fees: None,
        },
    )
    .await;

    assert!(!report.any_failed(), "{:#?}", report.checks);
    assert_eq!(verdict(&report, "chain_id"), Verdict::Pass);
    assert_eq!(verdict(&report, "bridge_token"), Verdict::Pass);
    assert_eq!(verdict(&report, "token_decimals"), Verdict::Pass);
    assert_eq!(verdict(&report, "bridge_signer_set"), Verdict::Pass);
    assert_eq!(verdict(&report, "eip712_domain_separator"), Verdict::Pass);
    assert_eq!(verdict(&report, "signer_quorum_available"), Verdict::Pass);
    assert!(report.deployment.is_some());
}

/// The permanent gap. A successful `decimals()` read establishes nothing
/// about a mint authority, a blocklist, a transfer hook, a pause or a
/// proxy — and the report must never suggest otherwise, even on a
/// completely healthy deployment.
#[tokio::test]
async fn token_security_properties_are_always_unverified_never_pass() {
    let node = node();
    let report = operator_run(&node, 3).await;
    let unverified = report.unverified();
    assert!(
        unverified.len() >= 5,
        "every token security property must be reported: {unverified:#?}"
    );
    for expected in ["mint authority", "blocklist", "upgradeable"] {
        assert!(
            unverified.iter().any(|c| c.detail.contains(expected)),
            "{expected} must be named UNVERIFIED"
        );
    }
    // And none of them is ever a PASS, however healthy the deployment.
    for check in &report.checks {
        if check.name == "token_security_property" {
            assert_eq!(check.verdict, Verdict::Unverified);
        }
    }
    // UNVERIFIED is not a failure either — it is a gap.
    let (_, fail, unverified_count) = report.counts();
    assert!(unverified_count > 0);
    assert_eq!(fail > 0, report.any_failed());
}

#[tokio::test]
async fn the_wrong_token_fails_and_everything_after_it_is_unverified() {
    let node = node();
    node.with(|s| s.contract.token = EvmAddress::from_bytes([0xbe; 20]));
    let report = operator_run(&node, 3).await;

    assert_eq!(verdict(&report, "bridge_token"), Verdict::Fail);
    // Checks before the failure genuinely ran.
    assert_eq!(verdict(&report, "chain_id"), Verdict::Pass);
    assert_eq!(verdict(&report, "bridge_protocol_id"), Verdict::Pass);
    // Checks after it did NOT — reported as unverified, not as passing
    // and not as failing.
    assert_eq!(verdict(&report, "token_decimals"), Verdict::Unverified);
    assert_eq!(verdict(&report, "bridge_signer_set"), Verdict::Unverified);
    assert_eq!(verdict(&report, "tx_envelope"), Verdict::Unverified);
    assert!(report.deployment.is_none());
    assert!(report.any_failed());
}

#[tokio::test]
async fn the_wrong_decimals_fails() {
    let node = node();
    node.with(|s| s.contract.token_decimals = 6);
    let report = operator_run(&node, 3).await;
    assert_eq!(verdict(&report, "token_decimals"), Verdict::Fail);
    assert_eq!(verdict(&report, "bridge_signer_set"), Verdict::Unverified);
}

#[tokio::test]
async fn no_contract_code_fails_at_the_first_check_that_looks_for_it() {
    let node = node();
    node.with(|s| s.contract.bridge_code.clear());
    let report = operator_run(&node, 3).await;
    assert_eq!(verdict(&report, "bridge_contract_code"), Verdict::Fail);
    // Nothing downstream could have run: an eth_call to an address with
    // no code returns empty data rather than failing.
    assert_eq!(verdict(&report, "bridge_token"), Verdict::Unverified);
}

#[tokio::test]
async fn the_wrong_chain_id_fails() {
    let node = node();
    // A settlement config for the testnet, pointed at a mainnet
    // endpoint — the same isolation the `verify` test above uses.
    let mut settlement = node.settlement_config();
    settlement.chain_id = EvmChainId::new(46630).unwrap();
    let report = operator_preflight(
        &node,
        &OperatorPreflightInputs {
            indexer: &node.indexer_config(),
            settlement: &settlement,
            expected_routes: ExpectedRoutes {
                expect_enabled: vec![Route::GlcToRhn, Route::RhnToGlc],
            },
            signers_available: 3,
            signers_required: crate::robinhood::SIGNER_THRESHOLD,
            policy: None,
            route_fees: None,
        },
    )
    .await;
    assert_eq!(verdict(&report, "chain_id"), Verdict::Fail);
    // The endpoint answered, so the reachability check passed; it is the
    // IDENTITY that is wrong.
    assert_eq!(verdict(&report, "rpc_reachable"), Verdict::Pass);
    assert_eq!(
        verdict(&report, "bridge_contract_code"),
        Verdict::Unverified
    );
}

/// The launch blocker, surfaced as its own check rather than buried.
#[tokio::test]
async fn an_unformable_signer_quorum_fails() {
    let node = node();
    for available in [0, 1] {
        let report = operator_run(&node, available).await;
        assert_eq!(
            verdict(&report, "signer_quorum_available"),
            Verdict::Fail,
            "{available} signer(s) must not be reported as a quorum"
        );
        assert!(report.any_failed());
    }
    // Exactly the threshold is enough.
    let report = operator_run(&node, crate::robinhood::SIGNER_THRESHOLD).await;
    assert_eq!(verdict(&report, "signer_quorum_available"), Verdict::Pass);
}

/// The check that matters before launch: a route that is OPEN on-chain
/// when the operator expected it closed is a FAIL, not something nobody
/// looked at.
#[tokio::test]
async fn an_unexpectedly_enabled_route_fails() {
    let node = node();
    // The fixture ships GlcToRhn/RhnToGlc enabled on the contract, and
    // the default expectation is that all four are CLOSED.
    let report = operator_run(&node, 3).await;
    assert_eq!(verdict(&report, "contract_route_glc_to_rhn"), Verdict::Fail);
    assert_eq!(verdict(&report, "contract_route_rhn_to_glc"), Verdict::Fail);
    // The two Solana<->Robinhood routes ship closed and are expected
    // closed.
    assert_eq!(verdict(&report, "contract_route_sol_to_rhn"), Verdict::Pass);
    assert_eq!(verdict(&report, "contract_route_rhn_to_sol"), Verdict::Pass);
    assert!(report.any_failed());

    let failing = report
        .checks
        .iter()
        .find(|c| c.name == "contract_route_glc_to_rhn")
        .unwrap();
    assert!(
        failing.detail.contains("OPEN on-chain"),
        "the message must say what is wrong: {}",
        failing.detail
    );
}

/// All four closed and expected closed — the state this ships in.
#[tokio::test]
async fn all_four_routes_closed_is_the_expected_shipping_state() {
    let node = node();
    node.with(|s| {
        for route in [0x01u8, 0x02, 0x03, 0x04] {
            s.contract.route_enabled.insert(route, false);
        }
    });
    let report = operator_run(&node, 3).await;
    for name in [
        "contract_route_glc_to_rhn",
        "contract_route_rhn_to_glc",
        "contract_route_sol_to_rhn",
        "contract_route_rhn_to_sol",
    ] {
        assert_eq!(verdict(&report, name), Verdict::Pass, "{name}");
    }
    assert!(!report.any_failed(), "{:#?}", report.checks);
}

#[tokio::test]
async fn an_underfunded_submitter_fails_and_a_funded_one_passes() {
    let node = node();
    assert_eq!(
        verdict(&operator_run(&node, 3).await, "submitter_funded"),
        Verdict::Pass
    );
    node.with(|s| s.contract.submitter_balance = 0);
    let report = operator_run(&node, 3).await;
    assert_eq!(verdict(&report, "submitter_reachable"), Verdict::Pass);
    assert_eq!(verdict(&report, "submitter_funded"), Verdict::Fail);
}

// ==================================================================
// The launch policy against the contract's own limits
// ==================================================================
//
// The contract is the enforcement layer for both the per-transfer
// maximum and the rolling window, so a configured backend limit is only
// ever a STATEMENT about what the contract holds. These tests are about
// telling a true statement from a false one.

/// 1 GLC in canonical 8-decimal units.
const ONE_GLC_CANONICAL: u64 = 100_000_000;

fn glc_18dp(glc: u128) -> crate::evm::EvmU256 {
    crate::evm::EvmU256::from_u128(glc * 1_000_000_000_000_000_000)
}

/// The approved Robinhood launch policy: 6.00%, 20,000 GLC per transfer,
/// 10,000,000 GLC strict per 24h.
fn approved_policy() -> ChainPolicy {
    ChainPolicy::new_symmetric(
        crate::routes::Chain::Robinhood,
        600,
        crate::amount_conversion::CanonicalAtomic(20_000 * ONE_GLC_CANONICAL),
        crate::amount_conversion::CanonicalAtomic(10_000_000 * ONE_GLC_CANONICAL),
    )
    .expect("the approved policy")
}

/// Installs the limits the approved policy requires: the max equal to the
/// per-transfer ceiling, and the rolling limit at HALF the strict policy.
fn install_matching_limits(node: &MockNode) {
    node.with(|s| {
        s.contract.limits.inbound_max = glc_18dp(20_000);
        s.contract.limits.outbound_max = glc_18dp(20_000);
        s.contract.limits.inbound_rolling_limit = glc_18dp(5_000_000);
        s.contract.limits.outbound_rolling_limit = glc_18dp(5_000_000);
    });
}

async fn operator_run_with_policy(
    node: &MockNode,
    policy: Option<&ChainPolicy>,
) -> PreflightReport {
    operator_preflight(
        node,
        &OperatorPreflightInputs {
            indexer: &node.indexer_config(),
            settlement: &node.settlement_config(),
            expected_routes: ExpectedRoutes {
                expect_enabled: vec![Route::GlcToRhn, Route::RhnToGlc],
            },
            signers_available: 3,
            signers_required: crate::robinhood::SIGNER_THRESHOLD,
            policy,
            route_fees: None,
        },
    )
    .await
}

#[tokio::test]
async fn a_contract_holding_the_approved_policy_passes_both_policy_checks() {
    let node = node();
    install_matching_limits(&node);
    let policy = approved_policy();
    let report = operator_run_with_policy(&node, Some(&policy)).await;

    assert_eq!(verdict(&report, "policy_per_transfer_limit"), Verdict::Pass);
    assert_eq!(
        verdict(&report, "policy_rolling_daily_limit"),
        Verdict::Pass
    );
    assert!(!report.any_failed(), "{:#?}", report.checks);
}

/// The fixture's stock deployment holds 10,000 GLC per transfer and
/// 100,000 GLC per bucket — the pilot-sized limits. Pointing the approved
/// launch policy at it must FAIL, in both checks, rather than pass
/// because nothing looked.
#[tokio::test]
async fn the_launch_policy_against_an_unmigrated_pilot_deployment_fails_both_checks() {
    let node = node();
    let policy = approved_policy();
    let report = operator_run_with_policy(&node, Some(&policy)).await;

    assert_eq!(verdict(&report, "policy_per_transfer_limit"), Verdict::Fail);
    assert_eq!(
        verdict(&report, "policy_rolling_daily_limit"),
        Verdict::Fail
    );
    assert!(report.any_failed());

    let detail = check_detail(&report, "policy_per_transfer_limit");
    assert!(
        detail.contains("AmountAboveMaximum"),
        "the message must name what the contract would actually do: {detail}"
    );
}

/// Putting the POLICY figure on chain rather than half of it is the one
/// mistake the contract's own `_consumeWindow` docs warn about, and it
/// silently doubles the real ceiling. Preflight has to catch it.
#[tokio::test]
async fn a_rolling_limit_configured_at_the_policy_figure_fails() {
    let node = node();
    install_matching_limits(&node);
    node.with(|s| {
        s.contract.limits.inbound_rolling_limit = glc_18dp(10_000_000);
        s.contract.limits.outbound_rolling_limit = glc_18dp(10_000_000);
    });
    let policy = approved_policy();
    let report = operator_run_with_policy(&node, Some(&policy)).await;

    assert_eq!(verdict(&report, "policy_per_transfer_limit"), Verdict::Pass);
    assert_eq!(
        verdict(&report, "policy_rolling_daily_limit"),
        Verdict::Fail
    );
    let detail = check_detail(&report, "policy_rolling_daily_limit");
    assert!(
        detail.contains("2000000000000000"),
        "the doubled worst case in canonical units must be stated: {detail}"
    );
}

/// No `[robinhood.policy]` section means no backend limit was stated, so
/// there is nothing to check — and "nothing to check" is UNVERIFIED, not
/// PASS. Answering PASS to a question it never asked is the single most
/// harmful thing this report could do.
#[tokio::test]
async fn an_unconfigured_policy_is_unverified_rather_than_passing() {
    let node = node();
    let report = operator_run_with_policy(&node, None).await;

    assert_eq!(
        verdict(&report, "policy_per_transfer_limit"),
        Verdict::Unverified
    );
    assert_eq!(
        verdict(&report, "policy_rolling_daily_limit"),
        Verdict::Unverified
    );
    assert!(!report.any_failed(), "{:#?}", report.checks);
    assert!(check_detail(&report, "policy_rolling_daily_limit").contains("[robinhood.policy]"));
}

/// A limits() read that fails is UNVERIFIED too — never a silent pass and
/// never an invented value.
#[tokio::test]
async fn a_failed_limits_read_is_unverified() {
    let node = node();
    node.fail_calls("connection reset");
    let policy = approved_policy();
    let report = operator_run_with_policy(&node, Some(&policy)).await;

    assert_eq!(
        verdict(&report, "policy_per_transfer_limit"),
        Verdict::Unverified
    );
    assert_eq!(
        verdict(&report, "policy_rolling_daily_limit"),
        Verdict::Unverified
    );
}

fn check_detail(report: &PreflightReport, name: &str) -> String {
    report
        .checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no check named {name}"))
        .detail
        .clone()
}

// ------------------------- the source-minimum deliverability check --------

/// Builds a fee table naming only the outbound Robinhood routes, so the
/// check under test reads exactly the rates this test states.
fn fees_for(glc_to_rhn: u64, sol_to_rhn: Option<u64>) -> crate::fees::RouteFees {
    let mut fees = crate::fees::RouteFees::new();
    fees.insert(Route::GlcToRhn, glc_to_rhn).unwrap();
    fees.insert(Route::RhnToGlc, glc_to_rhn).unwrap();
    fees.insert(Route::GlcToSol, 300).unwrap();
    fees.insert(Route::SolToGlc, 300).unwrap();
    if let Some(bps) = sol_to_rhn {
        fees.insert(Route::SolToRhn, bps).unwrap();
        fees.insert(Route::RhnToSol, bps).unwrap();
    }
    fees
}

async fn operator_run_with_fees(
    node: &MockNode,
    policy: Option<&ChainPolicy>,
    fees: Option<&crate::fees::RouteFees>,
) -> PreflightReport {
    operator_preflight(
        node,
        &OperatorPreflightInputs {
            indexer: &node.indexer_config(),
            settlement: &node.settlement_config(),
            expected_routes: ExpectedRoutes {
                expect_enabled: vec![Route::GlcToRhn, Route::RhnToGlc],
            },
            signers_available: 3,
            signers_required: crate::robinhood::SIGNER_THRESHOLD,
            policy,
            route_fees: fees,
        },
    )
    .await
}

/// The production situation this check exists for: `outboundMin` at the
/// policy figure itself. A 100 GLC transfer nets 97 at 300 bps, so the
/// contract would refuse to deliver it — and on `GlcToRhn` that happens
/// after the depositor's Goldcoin has already moved.
#[tokio::test]
async fn an_outbound_min_at_the_policy_figure_fails_because_the_fee_comes_off_after_it() {
    let node = node();
    install_matching_limits(&node);
    node.with(|s| s.contract.limits.outbound_min = glc_18dp(100));
    let policy = approved_policy();

    let report = operator_run_with_fees(&node, Some(&policy), Some(&fees_for(300, None))).await;

    assert_eq!(
        verdict(&report, "policy_source_minimum_deliverable"),
        Verdict::Fail
    );
    let detail = check_detail(&report, "policy_source_minimum_deliverable");
    assert!(detail.contains("GlcToRhn"), "{detail}");
    // The value an operator must set, spelled out — 97 GLC in canonical
    // 8dp — rather than left as an exercise.
    assert!(detail.contains("9700000000"), "{detail}");
}

/// Lowered to 97 GLC, the same deployment passes.
#[tokio::test]
async fn an_outbound_min_at_the_nets_of_a_minimum_transfer_passes() {
    let node = node();
    install_matching_limits(&node);
    node.with(|s| s.contract.limits.outbound_min = glc_18dp(97));
    let policy = approved_policy();

    let report = operator_run_with_fees(&node, Some(&policy), Some(&fees_for(300, None))).await;

    assert_eq!(
        verdict(&report, "policy_source_minimum_deliverable"),
        Verdict::Pass
    );
}

/// The requirement tracks the ROUTE's rate, not a constant. At 600 bps a
/// minimum transfer nets 94, so a 97 GLC floor that passes at 300 bps
/// fails here — which is exactly why 97 is never written into the check.
#[tokio::test]
async fn the_required_floor_follows_the_routes_fee_rather_than_a_constant() {
    let node = node();
    install_matching_limits(&node);
    node.with(|s| s.contract.limits.outbound_min = glc_18dp(97));
    let policy = approved_policy();

    let report = operator_run_with_fees(&node, Some(&policy), Some(&fees_for(600, None))).await;

    assert_eq!(
        verdict(&report, "policy_source_minimum_deliverable"),
        Verdict::Fail
    );
    let detail = check_detail(&report, "policy_source_minimum_deliverable");
    assert!(
        detail.contains("9400000000"),
        "the 600 bps requirement: {detail}"
    );
}

/// `outboundMin` is ONE field serving both outbound routes, so a rate
/// that differs between them is checked against both. `SolToRhn` priced
/// at 600 bps fails on a floor `GlcToRhn` at 300 bps is fine with.
#[tokio::test]
async fn both_outbound_routes_are_checked_against_the_one_shared_floor() {
    let node = node();
    install_matching_limits(&node);
    node.with(|s| s.contract.limits.outbound_min = glc_18dp(97));
    let policy = approved_policy();

    let report =
        operator_run_with_fees(&node, Some(&policy), Some(&fees_for(300, Some(600)))).await;

    assert_eq!(
        verdict(&report, "policy_source_minimum_deliverable"),
        Verdict::Fail
    );
    let detail = check_detail(&report, "policy_source_minimum_deliverable");
    assert!(detail.contains("SolToRhn"), "{detail}");
    assert!(
        !detail.contains("GlcToRhn"),
        "only the failing route: {detail}"
    );
}

/// An unpriced cross route is reported as unchecked rather than passed
/// silently — it cannot deliver anything, but "not checked" is not the
/// same claim as "fine".
#[tokio::test]
async fn an_unpriced_cross_route_is_named_rather_than_silently_skipped() {
    let node = node();
    install_matching_limits(&node);
    node.with(|s| s.contract.limits.outbound_min = glc_18dp(97));
    let policy = approved_policy();

    let report = operator_run_with_fees(&node, Some(&policy), Some(&fees_for(300, None))).await;

    let detail = check_detail(&report, "policy_source_minimum_deliverable");
    assert!(detail.contains("SolToRhn is unpriced"), "{detail}");
}

/// No fee table at all is UNVERIFIED, never PASS: the net of a minimum
/// transfer is unknown, so nothing was actually established.
#[tokio::test]
async fn no_fee_table_reports_unverified_never_pass() {
    let node = node();
    install_matching_limits(&node);
    node.with(|s| s.contract.limits.outbound_min = glc_18dp(100));
    let policy = approved_policy();

    let report = operator_run_with_fees(&node, Some(&policy), None).await;

    assert_eq!(
        verdict(&report, "policy_source_minimum_deliverable"),
        Verdict::Unverified
    );
}

// =====================================================================
// Treasury capability and migration state
// =====================================================================

/// The healthy fixture has a treasury and no pending migration: both
/// checks PASS and the treasury check names the address.
#[tokio::test]
async fn a_deployment_with_a_treasury_and_no_pending_migration_passes_both_checks() {
    let node = node();
    let report = operator_run(&node, 3).await;
    assert_eq!(
        verdict(&report, "treasury_withdraw_capability"),
        Verdict::Pass
    );
    assert_eq!(verdict(&report, "no_pending_migration"), Verdict::Pass);
    let check = report
        .checks
        .iter()
        .find(|c| c.name == "treasury_withdraw_capability")
        .unwrap();
    assert!(
        check
            .detail
            .contains(&crate::robinhood::testkit::TREASURY.to_checksum_string()),
        "{}",
        check.detail
    );
}

/// A deployment constructed with `address(0)` as its treasury declined
/// the capability by construction; the check says so.
#[tokio::test]
async fn a_zero_treasury_is_reported_as_no_capability() {
    let node = node();
    node.with(|s| s.contract.treasury = crate::evm::EvmAddress::ZERO);
    let report = operator_run(&node, 3).await;
    assert_eq!(
        verdict(&report, "treasury_withdraw_capability"),
        Verdict::Fail
    );
    let check = report
        .checks
        .iter()
        .find(|c| c.name == "treasury_withdraw_capability")
        .unwrap();
    assert!(
        check.detail.contains("TreasuryNotConfigured"),
        "{}",
        check.detail
    );
}

/// A committed migration is reported with its successor and the time
/// from which it can be finalized — never silently passed over.
#[tokio::test]
async fn a_committed_migration_is_reported_with_its_successor() {
    let node = node();
    let successor = crate::evm::EvmAddress::from_bytes([0x5c; 20]);
    node.with(|s| {
        s.contract.migration_committed = true;
        s.contract.migration_successor = successor;
        s.contract.migration_finalizable_at = 1_800_172_800;
    });
    let report = operator_run(&node, 3).await;
    assert_eq!(verdict(&report, "no_pending_migration"), Verdict::Fail);
    let check = report
        .checks
        .iter()
        .find(|c| c.name == "no_pending_migration")
        .unwrap();
    assert!(
        check.detail.contains(&successor.to_checksum_string()),
        "{}",
        check.detail
    );
    assert!(check.detail.contains("1800172800"), "{}", check.detail);
}
