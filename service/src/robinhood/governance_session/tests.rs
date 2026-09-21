//! The whole session against a mock node: plan, quorum, simulate,
//! broadcast, verify.
//!
//! The signers here hold real secp256k1 keys and produce real
//! signatures over the real digest, so "a distinct quorum" and "a
//! duplicate signer" are facts about recovered addresses rather than
//! about a mock's expectations.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::amount_conversion::CanonicalAtomic;
use crate::chain_policy::ChainPolicy;
use crate::evm::secp;
use crate::robinhood::governance::{limits_from_policy, MinimumOverrides, CANONICAL_SCALE};
use crate::robinhood::policy::RobinhoodPolicyBinding;
use crate::robinhood::testkit::MockNode;
use crate::routes::Chain;

const ONE_GLC: u64 = 100_000_000;
const EXPIRY: u64 = 1_800_000_000;
/// "Now" for the plan's time gate; well before `EXPIRY`.
const NOW: u64 = 1_790_000_000;

fn bridge() -> EvmAddress {
    EvmAddress::from_bytes([0xb1; 20])
}

fn domain() -> BridgeDomain {
    BridgeDomain::new(EvmChainId::new(4663).unwrap(), bridge())
}

/// A local key standing in for one custody domain.
struct LocalSigner {
    secret: secp::EvmSecretKey,
    label: String,
    calls: AtomicUsize,
    refuse: Option<String>,
}

impl LocalSigner {
    fn new(seed: u8, label: &str) -> LocalSigner {
        let mut bytes = [seed; 32];
        // Keep every seed a valid, distinct scalar.
        bytes[31] = seed.wrapping_add(1);
        LocalSigner {
            secret: secp::EvmSecretKey::from_bytes(&bytes).expect("a valid key"),
            label: label.to_string(),
            calls: AtomicUsize::new(0),
            refuse: None,
        }
    }

    fn refusing(seed: u8, label: &str, detail: &str) -> LocalSigner {
        LocalSigner {
            refuse: Some(detail.to_string()),
            ..LocalSigner::new(seed, label)
        }
    }

    fn address(&self) -> EvmAddress {
        self.secret.address()
    }
}

impl GovernanceQuorumSigner for LocalSigner {
    fn identity(&self) -> String {
        self.label.clone()
    }

    fn sign_governance<'a>(
        &'a self,
        auth: &'a GovernanceAuth,
        domain: BridgeDomain,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<EvmSignature, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(detail) = &self.refuse {
                return Err(detail.clone());
            }
            // The digest is derived from the auth, exactly as a real
            // custody domain derives it — never handed in.
            let digest = auth.digest(domain).map_err(|e| e.to_string())?;
            Ok(secp::sign_digest(&self.secret, &digest))
        })
    }
}

fn binding(per_transfer_glc: u64, rolling_glc: u64) -> RobinhoodPolicyBinding {
    RobinhoodPolicyBinding::new(
        ChainPolicy::new_symmetric(
            Chain::Robinhood,
            600,
            CanonicalAtomic(per_transfer_glc * ONE_GLC),
            CanonicalAtomic(rolling_glc * ONE_GLC),
        )
        .expect("a valid policy"),
    )
    .expect("a bindable policy")
}

/// A node whose signer set is the three local signers.
fn node(signers: [&LocalSigner; 3]) -> MockNode {
    let node = MockNode::new(bridge());
    node.with(|s| {
        s.contract.signers = [
            signers[0].address(),
            signers[1].address(),
            signers[2].address(),
        ];
        s.contract.limits = BridgeLimits {
            inbound_min: EvmU256::from_u128(CANONICAL_SCALE),
            inbound_max: EvmU256::from_u128(5 * CANONICAL_SCALE),
            inbound_rolling_limit: EvmU256::from_u128(10 * CANONICAL_SCALE),
            outbound_min: EvmU256::from_u128(2 * CANONICAL_SCALE),
            outbound_max: EvmU256::from_u128(5 * CANONICAL_SCALE),
            outbound_rolling_limit: EvmU256::from_u128(10 * CANONICAL_SCALE),
            protected_min_reserve: EvmU256::from_u128(9 * CANONICAL_SCALE),
        };
    });
    node
}

async fn snapshot(node: &MockNode) -> GovernanceStateSnapshot {
    read_state(&BridgeReader::new(bridge()), node, EvmBlockTag::Latest)
        .await
        .expect("the mock answers every read")
}

// =====================================================================
// Planning writes nothing and contacts nobody
// =====================================================================

#[tokio::test]
async fn a_plan_is_derived_from_configured_policy_and_preserves_the_minimums() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "domain-a"),
        LocalSigner::new(0x22, "domain-b"),
        LocalSigner::new(0x33, "domain-c"),
    );
    let node = node([&a, &b, &c]);
    let before = snapshot(&node).await;

    let binding = binding(20_000, 10_000_000);
    let proposed = limits_from_policy(&binding, &before.limits, MinimumOverrides::default())
        .expect("a valid proposal");
    let plan = plan(
        before.clone(),
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetLimits(proposed),
        EXPIRY,
        NOW,
    )
    .expect("a plannable proposal");

    // Derived from the configured policy...
    assert_eq!(
        plan.after.limits.inbound_max,
        binding.inbound_max().to_u256()
    );
    assert_eq!(
        plan.after.limits.inbound_rolling_limit,
        binding.expected_onchain_rolling_limit().to_u256()
    );
    // ...and the minimums are the chain's own, untouched.
    assert_eq!(plan.after.limits.inbound_min, before.limits.inbound_min);
    assert_eq!(plan.after.limits.outbound_min, before.limits.outbound_min);
    assert_eq!(
        plan.after.limits.protected_min_reserve,
        before.limits.protected_min_reserve
    );

    // Planning contacts no signer and sends nothing.
    assert_eq!(a.calls.load(Ordering::SeqCst), 0);
    assert_eq!(b.calls.load(Ordering::SeqCst), 0);
    assert!(node.with(|s| s.broadcasts.is_empty()));
    assert_eq!(plan.auth.nonce, before.governance_nonce);
    assert_eq!(plan.auth.signer_epoch, before.signer_epoch);
}

/// A limit or pause proposal changes exactly its own fields. This is the
/// "never enable a route as a side effect" requirement, as a test.
#[tokio::test]
async fn no_proposal_changes_a_field_its_action_does_not_own() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "a"),
        LocalSigner::new(0x22, "b"),
        LocalSigner::new(0x33, "c"),
    );
    let node = node([&a, &b, &c]);
    node.with(|s| {
        s.contract.route_enabled.insert(0x01, false);
        s.contract.route_enabled.insert(0x02, false);
    });
    let before = snapshot(&node).await;
    assert!(!before.glc_to_rhn_enabled && !before.rhn_to_glc_enabled);

    let binding = binding(20_000, 10_000_000);
    let limits = limits_from_policy(&binding, &before.limits, MinimumOverrides::default()).unwrap();

    for payload in [
        GovernancePayload::SetLimits(limits),
        GovernancePayload::SetPaused {
            deposits_paused: false,
            payouts_paused: false,
        },
    ] {
        let after = before.apply_to(&payload).unwrap();
        assert!(
            !after.glc_to_rhn_enabled && !after.rhn_to_glc_enabled,
            "{payload:?} must not enable a route"
        );
    }

    // And enabling a route changes nothing else.
    let after = before
        .apply_to(&GovernancePayload::SetRouteEnabled {
            route: Route::RhnToGlc,
            enabled: true,
        })
        .unwrap();
    assert!(after.rhn_to_glc_enabled);
    assert!(!after.glc_to_rhn_enabled, "only the named route");
    assert_eq!(after.limits, before.limits, "limits untouched");
    assert_eq!(after.deposits_paused, before.deposits_paused);
    assert_eq!(after.payouts_paused, before.payouts_paused);
}

#[tokio::test]
async fn a_migrated_contract_refuses_every_proposal() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "a"),
        LocalSigner::new(0x22, "b"),
        LocalSigner::new(0x33, "c"),
    );
    let node = node([&a, &b, &c]);
    node.with(|s| s.contract.migrated = true);
    let before = snapshot(&node).await;

    let err = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: true,
        },
        EXPIRY,
        NOW,
    )
    .expect_err("a migrated contract accepts nothing");
    assert!(
        matches!(err, GovernanceSessionError::AlreadyMigrated { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn a_chain_id_that_is_not_the_configured_one_is_refused() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "a"),
        LocalSigner::new(0x22, "b"),
        LocalSigner::new(0x33, "c"),
    );
    let node = node([&a, &b, &c]);
    let before = snapshot(&node).await;

    let elsewhere = BridgeDomain::new(EvmChainId::new(1).unwrap(), bridge());
    let err = plan(
        before,
        elsewhere,
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: true,
        },
        EXPIRY,
        NOW,
    )
    .expect_err("chain 1 is not this deployment");
    assert!(
        matches!(
            err,
            GovernanceSessionError::WrongChainId {
                configured: 4663,
                actual: 1
            }
        ),
        "{err}"
    );
}

/// The two Solana<->Robinhood routes are governable exactly like the
/// Goldcoin pair (Phase H), each moving only its own flag; the two
/// Solana<->Goldcoin routes, which the contract does not model, have no
/// plan at all.
#[tokio::test]
async fn a_cross_route_can_be_planned_and_a_solana_goldcoin_route_cannot() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "a"),
        LocalSigner::new(0x22, "b"),
        LocalSigner::new(0x33, "c"),
    );
    let node = node([&a, &b, &c]);
    let before = snapshot(&node).await;
    assert!(!before.sol_to_rhn_enabled && !before.rhn_to_sol_enabled);

    let after = before
        .apply_to(&GovernancePayload::SetRouteEnabled {
            route: Route::SolToRhn,
            enabled: true,
        })
        .unwrap();
    assert!(after.sol_to_rhn_enabled);
    assert!(!after.rhn_to_sol_enabled, "only the named route");
    assert_eq!(after.glc_to_rhn_enabled, before.glc_to_rhn_enabled);
    assert_eq!(after.rhn_to_glc_enabled, before.rhn_to_glc_enabled);
    plan(
        before.clone(),
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetRouteEnabled {
            route: Route::RhnToSol,
            enabled: true,
        },
        EXPIRY,
        NOW,
    )
    .expect("a cross route is governable");

    for route in [Route::GlcToSol, Route::SolToGlc] {
        let err = plan(
            before.clone(),
            domain(),
            EvmChainId::new(4663).unwrap(),
            GovernancePayload::SetRouteEnabled {
                route,
                enabled: true,
            },
            EXPIRY,
            NOW,
        )
        .expect_err("not a contract route");
        assert!(
            matches!(
                err,
                GovernanceSessionError::Encoding(GovernanceError::RouteNotGovernable { .. })
            ),
            "{route:?}: {err}"
        );
    }
}

// =====================================================================
// Execute: quorum, simulation, broadcast, verification
// =====================================================================

use crate::robinhood::testkit::submitter_key;
use crate::robinhood::EvmReceipt;

fn submitter(node: &MockNode) -> Submitter {
    Submitter::from_key(submitter_key(), &node.settlement_config())
        .expect("the configured submitter key")
}

/// A `sleep` that MINES instead of waiting: on the first tick it takes
/// the last broadcast, writes a successful receipt for it, and applies
/// the governance effect to the mock contract — which is what a real
/// node does between the poll that returns `None` and the one that
/// returns a receipt.
fn mining_sleep(
    node: &MockNode,
    effect: impl Fn(&mut crate::robinhood::testkit::MockNodeState) + Send + Sync + 'static,
    succeed: bool,
) -> impl Fn(u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    let state = std::sync::Arc::clone(&node.state);
    let effect = std::sync::Arc::new(effect);
    move |_secs| {
        let state = std::sync::Arc::clone(&state);
        let effect = std::sync::Arc::clone(&effect);
        Box::pin(async move {
            let mut guard = state.lock().expect("mock node lock");
            let Some(last) = guard.broadcasts.last() else {
                return;
            };
            let tx_hash = last.tx_hash;
            guard.receipts.insert(
                tx_hash,
                EvmReceipt {
                    tx_hash: crate::evm::EvmTxHash::from_bytes(tx_hash),
                    from: None,
                    success: succeed,
                    block_number: 101,
                    block_hash: crate::evm::EvmBlockHash::from_bytes([0x99; 32]),
                    gas_used: 120_000,
                    logs: Vec::new(),
                },
            );
            if succeed {
                effect(&mut guard);
                // Every accepted governance action consumes its nonce.
                let next = guard
                    .contract
                    .governance_nonce
                    .try_to_u128()
                    .expect("small")
                    + 1;
                guard.contract.governance_nonce = EvmU256::from_u128(next);
            }
        })
    }
}

fn fast() -> ReceiptWait {
    ReceiptWait {
        timeout_secs: 30,
        poll_interval_secs: 1,
    }
}

/// The happy path, end to end: two distinct domains sign, the call
/// simulates, it is broadcast, the receipt succeeds, and the contract is
/// re-read and agrees with the plan.
#[tokio::test]
async fn a_quorum_of_two_distinct_signers_installs_the_proposal_and_it_is_verified() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "domain-a"),
        LocalSigner::new(0x22, "domain-b"),
        LocalSigner::new(0x33, "domain-c"),
    );
    let node = node([&a, &b, &c]);
    let before = snapshot(&node).await;
    let binding = binding(20_000, 10_000_000);
    let proposed =
        limits_from_policy(&binding, &before.limits, MinimumOverrides::default()).unwrap();
    let plan = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetLimits(proposed),
        EXPIRY,
        NOW,
    )
    .unwrap();
    assert!(!plan.is_noop());

    let installed = proposed;
    let outcome = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a, &b, &c],
        2,
        fast(),
        mining_sleep(&node, move |s| s.contract.limits = installed, true),
    )
    .await
    .expect("the proposal installs");

    assert_eq!(outcome.signers.len(), 2, "exactly two, not three");
    assert!(outcome.signers[0].contains("domain-a"));
    assert!(outcome.signers[1].contains("domain-b"));
    assert_eq!(
        c.calls.load(Ordering::SeqCst),
        0,
        "the third domain is not asked once a quorum is complete"
    );
    assert_eq!(outcome.verified.limits, proposed);
    assert_eq!(node.with(|s| s.broadcasts.len()), 1);
}

/// The contract requires exactly two signatures from DISTINCT addresses.
/// A configuration that lists one domain twice is refused here rather
/// than reverting on chain.
#[tokio::test]
async fn a_duplicate_signer_is_refused_before_anything_is_broadcast() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "domain-a"),
        LocalSigner::new(0x22, "domain-b"),
        LocalSigner::new(0x33, "domain-c"),
    );
    let node = node([&a, &b, &c]);
    let before = snapshot(&node).await;
    let plan = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: true,
        },
        EXPIRY,
        NOW,
    )
    .unwrap();

    // The same domain listed twice.
    let err = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a, &a],
        2,
        fast(),
        mining_sleep(&node, |_| {}, true),
    )
    .await
    .expect_err("two signatures from one address are not a quorum");
    assert!(
        matches!(err, GovernanceSessionError::DuplicateSigner { .. }),
        "{err}"
    );
    assert!(node.with(|s| s.broadcasts.is_empty()), "nothing was sent");
}

/// Fewer distinct domains than the threshold is a refusal, not a
/// short quorum.
#[tokio::test]
async fn a_short_quorum_is_refused() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "domain-a"),
        LocalSigner::new(0x22, "domain-b"),
        LocalSigner::new(0x33, "domain-c"),
    );
    let node = node([&a, &b, &c]);
    let before = snapshot(&node).await;
    let plan = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: true,
        },
        EXPIRY,
        NOW,
    )
    .unwrap();

    let err = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a],
        2,
        fast(),
        mining_sleep(&node, |_| {}, true),
    )
    .await
    .expect_err("one domain is not a quorum");
    assert!(
        matches!(
            err,
            GovernanceSessionError::QuorumNotMet {
                required: 2,
                gathered: 1
            }
        ),
        "{err}"
    );
    assert!(node.with(|s| s.broadcasts.is_empty()));
}

/// A domain that refuses is named, and nothing is sent on a short quorum.
#[tokio::test]
async fn a_refusing_domain_is_named_and_nothing_is_broadcast() {
    let a = LocalSigner::refusing(0x11, "domain-a", "governance is disabled on this signer");
    let b = LocalSigner::refusing(0x22, "domain-b", "endpoint answered 404");
    let c = LocalSigner::new(0x33, "domain-c");
    let node = node([&a, &b, &c]);
    let before = snapshot(&node).await;
    let plan = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: true,
        },
        EXPIRY,
        NOW,
    )
    .unwrap();

    let err = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a, &b, &c],
        2,
        fast(),
        mining_sleep(&node, |_| {}, true),
    )
    .await
    .expect_err("only one domain could sign");
    let detail = err.to_string();
    assert!(detail.contains("domain-a"), "{detail}");
    assert!(detail.contains("governance is disabled"), "{detail}");
    assert!(node.with(|s| s.broadcasts.is_empty()));
}

/// The nonce moving between planning and signing invalidates the plan,
/// and it is caught BEFORE a quorum is asked to look at anything.
#[tokio::test]
async fn a_stale_governance_nonce_is_refused_before_a_signer_is_contacted() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "domain-a"),
        LocalSigner::new(0x22, "domain-b"),
        LocalSigner::new(0x33, "domain-c"),
    );
    let node = node([&a, &b, &c]);
    let before = snapshot(&node).await;
    let plan = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: true,
        },
        EXPIRY,
        NOW,
    )
    .unwrap();

    // Somebody else's governance action lands in between.
    node.with(|s| s.contract.governance_nonce = EvmU256::from_u64(9));

    let err = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a, &b, &c],
        2,
        fast(),
        mining_sleep(&node, |_| {}, true),
    )
    .await
    .expect_err("the plan is stale");
    assert!(
        matches!(err, GovernanceSessionError::StaleNonce { .. }),
        "{err}"
    );
    assert_eq!(a.calls.load(Ordering::SeqCst), 0, "no domain was contacted");
    assert!(node.with(|s| s.broadcasts.is_empty()));
}

/// A rotation between planning and signing voids every authorization
/// built under the old epoch.
#[tokio::test]
async fn a_rotated_signer_epoch_is_refused_before_a_signer_is_contacted() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "domain-a"),
        LocalSigner::new(0x22, "domain-b"),
        LocalSigner::new(0x33, "domain-c"),
    );
    let node = node([&a, &b, &c]);
    let before = snapshot(&node).await;
    let plan = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: true,
        },
        EXPIRY,
        NOW,
    )
    .unwrap();

    node.with(|s| s.contract.signer_epoch = 8);

    let err = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a, &b, &c],
        2,
        fast(),
        mining_sleep(&node, |_| {}, true),
    )
    .await
    .expect_err("the signer set rotated");
    assert!(
        matches!(
            err,
            GovernanceSessionError::StaleSignerEpoch {
                planned: 7,
                actual: 8
            }
        ),
        "{err}"
    );
    assert_eq!(a.calls.load(Ordering::SeqCst), 0);
}

/// A failing simulation stops the session with the chain untouched — the
/// cheapest possible discovery that a call would revert.
#[tokio::test]
async fn a_failing_simulation_stops_before_broadcast_and_changes_nothing() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "domain-a"),
        LocalSigner::new(0x22, "domain-b"),
        LocalSigner::new(0x33, "domain-c"),
    );
    let node = node([&a, &b, &c]);
    node.with(|s| s.contract.gas_estimate = None); // eth_estimateGas reverts
    let before = snapshot(&node).await;
    let plan = plan(
        before.clone(),
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: true,
        },
        EXPIRY,
        NOW,
    )
    .unwrap();

    let err = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a, &b, &c],
        2,
        fast(),
        mining_sleep(&node, |_| {}, true),
    )
    .await
    .expect_err("the simulation reverts");
    assert!(
        matches!(err, GovernanceSessionError::SimulationReverted { .. }),
        "{err}"
    );
    assert!(node.with(|s| s.broadcasts.is_empty()), "nothing was sent");
    assert_eq!(snapshot(&node).await, before, "the chain is untouched");
}

/// A receipt with status 0 is a failure, and it is reported as one.
#[tokio::test]
async fn a_reverted_transaction_is_reported_and_no_state_is_claimed() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "domain-a"),
        LocalSigner::new(0x22, "domain-b"),
        LocalSigner::new(0x33, "domain-c"),
    );
    let node = node([&a, &b, &c]);
    let before = snapshot(&node).await;
    let plan = plan(
        before.clone(),
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: true,
        },
        EXPIRY,
        NOW,
    )
    .unwrap();

    let err = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a, &b, &c],
        2,
        fast(),
        // Mined, but reverted: the effect is NOT applied.
        mining_sleep(&node, |_| {}, false),
    )
    .await
    .expect_err("status 0");
    assert!(
        matches!(err, GovernanceSessionError::TransactionReverted { .. }),
        "{err}"
    );
    assert_eq!(snapshot(&node).await, before, "the chain is unchanged");
}

/// THE check that exists because a successful receipt proves a
/// transaction executed, not that it meant what was intended: a chain
/// that ends up holding something else is a failure, loudly.
#[tokio::test]
async fn a_successful_receipt_whose_state_disagrees_is_a_failure() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "domain-a"),
        LocalSigner::new(0x22, "domain-b"),
        LocalSigner::new(0x33, "domain-c"),
    );
    let node = node([&a, &b, &c]);
    let before = snapshot(&node).await;
    let plan = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: true,
        },
        EXPIRY,
        NOW,
    )
    .unwrap();

    let err = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a, &b, &c],
        2,
        fast(),
        // Mines successfully but installs only HALF the change.
        mining_sleep(
            &node,
            |s| {
                s.contract.deposits_paused = true;
                s.contract.payouts_paused = false;
            },
            true,
        ),
    )
    .await
    .expect_err("the chain does not hold what was proposed");
    let detail = err.to_string();
    assert!(
        matches!(err, GovernanceSessionError::PostStateDisagrees { .. }),
        "{detail}"
    );
    assert!(detail.contains("payoutsPaused"), "{detail}");
}

/// Enabling one route leaves everything else exactly as it was, verified
/// against the chain after the fact.
#[tokio::test]
async fn enabling_one_route_changes_only_that_route() {
    let (a, b, c) = (
        LocalSigner::new(0x11, "domain-a"),
        LocalSigner::new(0x22, "domain-b"),
        LocalSigner::new(0x33, "domain-c"),
    );
    let node = node([&a, &b, &c]);
    node.with(|s| {
        s.contract.route_enabled.insert(0x01, false);
        s.contract.route_enabled.insert(0x02, false);
    });
    let before = snapshot(&node).await;
    let plan = plan(
        before.clone(),
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::SetRouteEnabled {
            route: Route::RhnToGlc,
            enabled: true,
        },
        EXPIRY,
        NOW,
    )
    .unwrap();

    let outcome = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a, &b, &c],
        2,
        fast(),
        mining_sleep(
            &node,
            |s| {
                s.contract.route_enabled.insert(0x02, true);
            },
            true,
        ),
    )
    .await
    .expect("the route enables");

    assert!(outcome.verified.rhn_to_glc_enabled);
    assert!(!outcome.verified.glc_to_rhn_enabled, "the other route");
    assert_eq!(outcome.verified.limits, before.limits);
    assert_eq!(outcome.verified.deposits_paused, before.deposits_paused);
    assert_eq!(outcome.verified.payouts_paused, before.payouts_paused);
}

// =====================================================================
// Migration: commit, finalize, and every gate between them
// =====================================================================

fn successor() -> EvmAddress {
    EvmAddress::from_bytes([0x5c; 20])
}

/// A node with a conforming successor deployed beside the bridge, and
/// both directions paused — the state `commitMigration` requires.
fn migration_ready_node(signers: [&LocalSigner; 3]) -> MockNode {
    let node = node(signers);
    node.with(|s| {
        s.contract.deposits_paused = true;
        s.contract.payouts_paused = true;
        s.contract.successor = Some(crate::robinhood::testkit::MockSuccessor::conforming(
            successor(),
        ));
    });
    node
}

fn three() -> (LocalSigner, LocalSigner, LocalSigner) {
    (
        LocalSigner::new(0x11, "domain-a"),
        LocalSigner::new(0x22, "domain-b"),
        LocalSigner::new(0x33, "domain-c"),
    )
}

/// `commitMigration` needs both directions paused on chain. The plan says
/// so, with the flags, before any domain is asked.
#[tokio::test]
async fn a_commit_is_refused_unless_both_directions_are_paused() {
    let (a, b, c) = three();
    for (deposits, payouts) in [(false, false), (true, false), (false, true)] {
        let node = migration_ready_node([&a, &b, &c]);
        node.with(|s| {
            s.contract.deposits_paused = deposits;
            s.contract.payouts_paused = payouts;
        });
        let before = snapshot(&node).await;
        let err = plan(
            before,
            domain(),
            EvmChainId::new(4663).unwrap(),
            GovernancePayload::CommitMigration {
                successor: successor(),
            },
            EXPIRY,
            NOW,
        )
        .expect_err("not paused");
        assert!(
            matches!(
                err,
                GovernanceSessionError::MigrationRequiresPause {
                    deposits_paused,
                    payouts_paused
                } if deposits_paused == deposits && payouts_paused == payouts
            ),
            "{err}"
        );
    }
    assert_eq!(a.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_second_commit_is_refused_while_one_is_pending() {
    let (a, b, c) = three();
    let node = migration_ready_node([&a, &b, &c]);
    node.with(|s| {
        s.contract.migration_committed = true;
        s.contract.migration_successor = successor();
    });
    let before = snapshot(&node).await;
    let err = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::CommitMigration {
            successor: EvmAddress::from_bytes([0x5d; 20]),
        },
        EXPIRY,
        NOW,
    )
    .expect_err("already committed");
    assert!(
        matches!(
            err,
            GovernanceSessionError::MigrationAlreadyCommitted { .. }
        ),
        "{err}"
    );
}

/// The contract's structural successor checks, re-stated off chain with
/// the reason named: no code, the bridge itself, the wrong token, the
/// wrong protocol family.
#[tokio::test]
async fn a_successor_that_would_revert_invalid_successor_is_refused_with_the_reason() {
    let (a, b, c) = three();

    // No code at the address.
    let node = migration_ready_node([&a, &b, &c]);
    let err = check_successor(&node, bridge(), EvmAddress::from_bytes([0x77; 20]))
        .await
        .expect_err("an EOA");
    assert!(err.to_string().contains("no contract code"), "{err}");

    // The bridge itself.
    let err = check_successor(&node, bridge(), bridge())
        .await
        .expect_err("self");
    assert!(err.to_string().contains("the bridge itself"), "{err}");

    // Wrong token.
    node.with(|s| {
        s.contract.successor.as_mut().unwrap().token = EvmAddress::from_bytes([0xee; 20]);
    });
    let err = check_successor(&node, bridge(), successor())
        .await
        .expect_err("wrong token");
    assert!(err.to_string().contains("custodies"), "{err}");

    // Wrong protocol family.
    node.with(|s| {
        let succ = s.contract.successor.as_mut().unwrap();
        succ.token = crate::robinhood::testkit::TOKEN;
        succ.protocol_id = [0xab; 32];
    });
    let err = check_successor(&node, bridge(), successor())
        .await
        .expect_err("wrong protocol");
    assert!(err.to_string().contains("protocol family"), "{err}");

    // And the conforming one passes.
    node.with(|s| {
        s.contract.successor.as_mut().unwrap().protocol_id =
            crate::robinhood::calls::bridge_protocol_id();
    });
    check_successor(&node, bridge(), successor())
        .await
        .expect("a conforming successor");
}

/// The happy path for a commit: quorum, simulate, broadcast, and the
/// chain re-read holds the successor as committed.
#[tokio::test]
async fn a_commit_installs_the_successor_and_is_verified() {
    let (a, b, c) = three();
    let node = migration_ready_node([&a, &b, &c]);
    let before = snapshot(&node).await;
    let plan = plan(
        before.clone(),
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::CommitMigration {
            successor: successor(),
        },
        EXPIRY,
        NOW,
    )
    .unwrap();
    assert!(!plan.is_noop());
    assert!(plan.after.migration_committed);
    assert_eq!(plan.after.migration_successor, successor());
    assert!(!plan.after.migrated, "a commit does not migrate");

    let outcome = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a, &b, &c],
        2,
        fast(),
        mining_sleep(
            &node,
            |s| {
                s.contract.migration_committed = true;
                s.contract.migration_successor = successor();
                s.contract.migration_finalizable_at = NOW;
            },
            true,
        ),
    )
    .await
    .expect("the commit lands");
    assert!(outcome.verified.migration_committed);
    assert_eq!(outcome.verified.migration_successor, successor());
    assert_eq!(outcome.signers.len(), 2);

    // The calldata that went out names the successor.
    let raw = node.with(|s| s.broadcasts[0].raw.clone());
    assert!(
        raw.windows(20).any(|w| w == successor().as_bytes()),
        "the broadcast carries the successor"
    );
}

/// A chain that reports a DIFFERENT committed successor than the plan
/// said is a verification failure, never a success.
#[tokio::test]
async fn a_commit_whose_chain_state_names_another_successor_fails_verification() {
    let (a, b, c) = three();
    let node = migration_ready_node([&a, &b, &c]);
    let before = snapshot(&node).await;
    let plan = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::CommitMigration {
            successor: successor(),
        },
        EXPIRY,
        NOW,
    )
    .unwrap();
    let err = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a, &b, &c],
        2,
        fast(),
        mining_sleep(
            &node,
            |s| {
                s.contract.migration_committed = true;
                s.contract.migration_successor = EvmAddress::from_bytes([0x5d; 20]);
            },
            true,
        ),
    )
    .await
    .expect_err("the chain disagrees");
    let detail = err.to_string();
    assert!(
        matches!(err, GovernanceSessionError::PostStateDisagrees { .. }),
        "{detail}"
    );
    assert!(detail.contains("migrationSuccessor"), "{detail}");
}

#[tokio::test]
async fn a_finalize_is_refused_when_nothing_is_committed() {
    let (a, b, c) = three();
    let node = migration_ready_node([&a, &b, &c]);
    let before = snapshot(&node).await;
    let err = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::FinalizeMigration {
            successor: successor(),
        },
        EXPIRY,
        NOW,
    )
    .expect_err("nothing committed");
    assert!(
        matches!(err, GovernanceSessionError::MigrationNotCommitted),
        "{err}"
    );
}

/// A finalize is a proposal about the committed address. Naming any
/// other is refused — the quorum must approve what the chain holds.
#[tokio::test]
async fn a_finalize_naming_a_different_successor_than_the_chain_holds_is_refused() {
    let (a, b, c) = three();
    let node = migration_ready_node([&a, &b, &c]);
    node.with(|s| {
        s.contract.migration_committed = true;
        s.contract.migration_successor = successor();
        s.contract.migration_finalizable_at = NOW;
    });
    let before = snapshot(&node).await;
    let err = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::FinalizeMigration {
            successor: EvmAddress::from_bytes([0x5d; 20]),
        },
        EXPIRY,
        NOW,
    )
    .expect_err("wrong successor");
    assert!(
        matches!(err, GovernanceSessionError::SuccessorMismatch { .. }),
        "{err}"
    );
}

/// A deployment that carries a MIGRATION_DELAY answers
/// `migrationFinalizableAt` in the future; the plan refuses with the
/// remaining time rather than letting a quorum sign a call that reverts.
/// A deployment with no delay answers the commit time and is planned.
#[tokio::test]
async fn a_finalize_before_the_deployed_contracts_own_delay_is_refused_with_the_remaining_time() {
    let (a, b, c) = three();
    let node = migration_ready_node([&a, &b, &c]);
    node.with(|s| {
        s.contract.migration_committed = true;
        s.contract.migration_successor = successor();
        // A 48-hour predecessor, committed one hour ago.
        s.contract.migration_finalizable_at = NOW - 3_600 + 48 * 3_600;
    });
    let before = snapshot(&node).await;
    let err = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::FinalizeMigration {
            successor: successor(),
        },
        EXPIRY,
        NOW,
    )
    .expect_err("too early");
    assert!(
        matches!(
            err,
            GovernanceSessionError::MigrationNotReady { remaining_secs, .. }
                if remaining_secs == 47 * 3_600
        ),
        "{err}"
    );
    assert!(
        err.to_string().contains("nothing off chain shortens it"),
        "{err}"
    );

    // The same node with no delay: finalizable at the commit time.
    node.with(|s| s.contract.migration_finalizable_at = NOW - 3_600);
    let before = snapshot(&node).await;
    plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::FinalizeMigration {
            successor: successor(),
        },
        EXPIRY,
        NOW,
    )
    .expect("no delay, plannable");
}

#[tokio::test]
async fn a_finalize_with_pending_obligations_is_refused_with_the_liability_named() {
    let (a, b, c) = three();
    let node = migration_ready_node([&a, &b, &c]);
    node.with(|s| {
        s.contract.migration_committed = true;
        s.contract.migration_successor = successor();
        s.contract.migration_finalizable_at = NOW;
        s.contract.outstanding_refundable_count = EvmU256::from_u64(4);
        s.contract.outstanding_refundable_principal = EvmU256::from_u128(80_000 * 10u128.pow(18));
    });
    let before = snapshot(&node).await;
    let err = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::FinalizeMigration {
            successor: successor(),
        },
        EXPIRY,
        NOW,
    )
    .expect_err("liability");
    let detail = err.to_string();
    assert!(
        matches!(err, GovernanceSessionError::OutstandingRefundsRemain { .. }),
        "{detail}"
    );
    assert!(detail.contains("4 pending"), "{detail}");
    assert!(detail.contains("80000000000000000000000"), "{detail}");
}

/// The happy path for a finalize: the contract reads back `migrated`.
#[tokio::test]
async fn a_finalize_lands_and_the_contract_reads_back_migrated() {
    let (a, b, c) = three();
    let node = migration_ready_node([&a, &b, &c]);
    node.with(|s| {
        s.contract.migration_committed = true;
        s.contract.migration_successor = successor();
        s.contract.migration_finalizable_at = NOW;
    });
    let before = snapshot(&node).await;
    let plan = plan(
        before,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::FinalizeMigration {
            successor: successor(),
        },
        EXPIRY,
        NOW,
    )
    .unwrap();
    assert!(plan.after.migrated);

    let outcome = execute(
        &plan,
        &BridgeReader::new(bridge()),
        &node,
        &submitter(&node),
        &[&a, &b, &c],
        2,
        fast(),
        mining_sleep(&node, |s| s.contract.migrated = true, true),
    )
    .await
    .expect("the finalize lands");
    assert!(outcome.verified.migrated);

    // And now every further proposal is refused, finalize included.
    let after = snapshot(&node).await;
    let err = super::plan(
        after,
        domain(),
        EvmChainId::new(4663).unwrap(),
        GovernancePayload::FinalizeMigration {
            successor: successor(),
        },
        EXPIRY,
        NOW,
    )
    .expect_err("terminal");
    assert!(
        matches!(err, GovernanceSessionError::AlreadyMigrated { .. }),
        "{err}"
    );
}

/// Neither migration action touches a field it does not own.
#[tokio::test]
async fn migration_proposals_change_only_migration_fields() {
    let (a, b, c) = three();
    let node = migration_ready_node([&a, &b, &c]);
    let before = snapshot(&node).await;
    let after = before
        .apply_to(&GovernancePayload::CommitMigration {
            successor: successor(),
        })
        .unwrap();
    assert_eq!(after.limits, before.limits);
    assert_eq!(after.deposits_paused, before.deposits_paused);
    assert_eq!(after.payouts_paused, before.payouts_paused);
    assert_eq!(after.glc_to_rhn_enabled, before.glc_to_rhn_enabled);
    assert_eq!(after.rhn_to_glc_enabled, before.rhn_to_glc_enabled);
    assert!(!after.migrated);
    let done = after
        .apply_to(&GovernancePayload::FinalizeMigration {
            successor: successor(),
        })
        .unwrap();
    assert!(done.migrated);
    assert_eq!(done.migration_successor, successor());
    assert_eq!(done.limits, before.limits);
}
