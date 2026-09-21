use super::*;

use crate::amount_conversion::CanonicalAtomic;
use crate::chain_policy::ChainPolicy;
use crate::evm::{EvmAddress, EvmChainId};
use crate::routes::Chain;

// =====================================================================
// Cross-checks against the CONTRACT SOURCE
// =====================================================================
//
// `fixtures/eip712-golden.json` has no governance vectors and foundry is
// not available in every environment this suite runs in, so the golden
// round-trip `auth::tests` enjoys does not exist here yet (see the module
// docs). These tests are the substitute: every constant transcribed above
// is re-extracted FROM THE SOLIDITY SOURCE at test time and compared.
//
// That catches the drift that actually matters — a renamed field, a
// reordered struct, a changed action byte — because those all change the
// source text. It does NOT prove the deployed bytecode agrees, which only
// a forge-generated golden vector can. Adding one is on the launch
// checklist.

fn contract_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the service crate has a parent directory")
        .join("contracts/src/GlcRobinhoodBridge.sol");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Joins the adjacent string literals of a `keccak256("..." "...")`
/// constant exactly as the Solidity compiler does.
fn solidity_type_string(source: &str, constant: &str) -> String {
    let start = source
        .find(&format!("{constant} = keccak256("))
        .unwrap_or_else(|| panic!("{constant} not found in the contract source"));
    let rest = &source[start..];
    let end = rest.find(");").expect("the constant is terminated");
    let body = &rest[..end];

    let mut out = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        for c in chars.by_ref() {
            if c == '"' {
                break;
            }
            out.push(c);
        }
    }
    out
}

/// Reads a `uint8 public constant NAME = 0xNN;` style declaration.
fn solidity_u8_constant(source: &str, name: &str) -> u8 {
    let needle = format!("constant {name} = ");
    let start = source
        .find(&needle)
        .unwrap_or_else(|| panic!("{name} not found in the contract source"));
    let rest = &source[start + needle.len()..];
    let end = rest.find(';').expect("the constant is terminated");
    let raw = rest[..end].trim();
    let hex = raw.strip_prefix("0x").expect("an 0x-prefixed byte");
    u8::from_str_radix(hex, 16).expect("a hex byte")
}

/// The single highest-risk constant in this module: one character's
/// difference produces a different typehash, therefore a different
/// digest, therefore a signature over something other than what the
/// operator read.
#[test]
fn the_governance_type_string_matches_the_contract_source() {
    let source = contract_source();
    assert_eq!(
        GOVERNANCE_TYPE,
        solidity_type_string(&source, "GOVERNANCE_TYPEHASH"),
        "the Rust transcription of GovernanceAuth drifted from the contract"
    );
}

#[test]
fn every_action_byte_matches_the_contract_source() {
    let source = contract_source();
    for (name, ours) in [
        ("ACTION_SET_PAUSE", ACTION_SET_PAUSE),
        ("ACTION_SET_LIMITS", ACTION_SET_LIMITS),
        ("ACTION_SET_ROUTE_ENABLED", ACTION_SET_ROUTE_ENABLED),
        ("ACTION_COMMIT_MIGRATION", ACTION_COMMIT_MIGRATION),
        ("ACTION_FINALIZE_MIGRATION", ACTION_FINALIZE_MIGRATION),
    ] {
        assert_eq!(solidity_u8_constant(&source, name), ours, "{name} drifted");
    }
}

#[test]
fn every_route_byte_matches_the_contract_source() {
    let source = contract_source();
    for (name, ours) in [
        ("ROUTE_GLC_TO_RHN", ROUTE_GLC_TO_RHN),
        ("ROUTE_RHN_TO_GLC", ROUTE_RHN_TO_GLC),
        ("ROUTE_SOL_TO_RHN", ROUTE_SOL_TO_RHN),
        ("ROUTE_RHN_TO_SOL", ROUTE_RHN_TO_SOL),
    ] {
        assert_eq!(solidity_u8_constant(&source, name), ours, "{name} drifted");
    }
}

/// `abi.encode(newLimits)` inlines the struct's members IN DECLARATION
/// ORDER. A reordered struct would keep every field name valid and change
/// every payload hash, so the order is asserted against the source.
#[test]
fn the_limits_struct_field_order_matches_the_contract_source() {
    let source = contract_source();
    let start = source
        .find("struct Limits {")
        .expect("the Limits struct is declared");
    let rest = &source[start..];
    let end = rest.find('}').expect("the struct is closed");
    let members: Vec<&str> = rest[..end]
        .lines()
        .filter_map(|line| line.trim().strip_prefix("uint256 "))
        .filter_map(|line| line.strip_suffix(';'))
        .collect();

    assert_eq!(
        members,
        vec![
            "inboundMin",
            "inboundMax",
            "inboundRollingLimit",
            "outboundMin",
            "outboundMax",
            "outboundRollingLimit",
            "protectedMinReserve",
        ],
        "the Limits struct was reordered or renamed; every payload hash changes"
    );
}

#[test]
fn the_canonical_scale_matches_the_contract_source() {
    let source = contract_source();
    assert!(
        source.contains("uint256 public constant CANONICAL_SCALE = 1e10;"),
        "CANONICAL_SCALE changed in the contract; the divisibility rule changes with it"
    );
    assert_eq!(CANONICAL_SCALE, 10_000_000_000);
}

// =====================================================================
// Payload hashing
// =====================================================================

fn domain() -> BridgeDomain {
    BridgeDomain::new(
        EvmChainId::new(4663).expect("a valid chain id"),
        EvmAddress::from_bytes([0x0b; 20]),
    )
}

const ONE_GLC: u64 = 100_000_000;

/// The launch policy's shape, but built from VALUES THE TEST SUPPLIES —
/// this module must contain no figure of its own, so a test that used a
/// compiled-in number would be testing the wrong thing.
fn policy(fee_bps: u64, per_transfer_glc: u64, rolling_glc: u64) -> RobinhoodPolicyBinding {
    let policy = ChainPolicy::new_symmetric(
        Chain::Robinhood,
        fee_bps,
        CanonicalAtomic(per_transfer_glc * ONE_GLC),
        CanonicalAtomic(rolling_glc * ONE_GLC),
    )
    .expect("a valid policy");
    RobinhoodPolicyBinding::new(policy).expect("a bindable policy")
}

fn limits(max: u128, rolling: u128, min: u128, protected: u128) -> BridgeLimits {
    BridgeLimits {
        inbound_min: EvmU256::from_u128(min),
        inbound_max: EvmU256::from_u128(max),
        inbound_rolling_limit: EvmU256::from_u128(rolling),
        outbound_min: EvmU256::from_u128(min),
        outbound_max: EvmU256::from_u128(max),
        outbound_rolling_limit: EvmU256::from_u128(rolling),
        protected_min_reserve: EvmU256::from_u128(protected),
    }
}

/// `abi.encode(bool,bool)` is two full words holding 0 or 1. Asserted on
/// the bytes rather than on a hash, so a failure says which word is wrong.
#[test]
fn the_set_paused_payload_encodes_two_boolean_words() {
    for (deposits, payouts) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut expected = [0u8; 64];
        expected[31] = u8::from(deposits);
        expected[63] = u8::from(payouts);
        let payload = GovernancePayload::SetPaused {
            deposits_paused: deposits,
            payouts_paused: payouts,
        };
        assert_eq!(
            payload.payload_hash().unwrap(),
            crate::evm::keccak::keccak256(&expected),
            "setPaused({deposits}, {payouts})"
        );
    }
}

/// `abi.encode(uint8,bool)` — the route byte left-padded into a full
/// word, not packed.
#[test]
fn the_set_route_enabled_payload_encodes_a_padded_route_byte() {
    for (route, byte) in [
        (Route::GlcToRhn, ROUTE_GLC_TO_RHN),
        (Route::RhnToGlc, ROUTE_RHN_TO_GLC),
    ] {
        for enabled in [true, false] {
            let mut expected = [0u8; 64];
            expected[31] = byte;
            expected[63] = u8::from(enabled);
            let payload = GovernancePayload::SetRouteEnabled { route, enabled };
            assert_eq!(
                payload.payload_hash().unwrap(),
                crate::evm::keccak::keccak256(&expected),
                "{route:?} -> {enabled}"
            );
        }
    }
}

/// Seven inlined words, in declaration order.
#[test]
fn the_set_limits_payload_inlines_seven_words_in_declaration_order() {
    let l = limits(20, 40, 1, 5);
    let mut expected = Vec::new();
    for value in [
        l.inbound_min,
        l.inbound_max,
        l.inbound_rolling_limit,
        l.outbound_min,
        l.outbound_max,
        l.outbound_rolling_limit,
        l.protected_min_reserve,
    ] {
        expected.extend_from_slice(&value.to_be_bytes());
    }
    assert_eq!(expected.len(), 7 * 32);
    assert_eq!(
        GovernancePayload::SetLimits(l).payload_hash().unwrap(),
        crate::evm::keccak::keccak256(&expected)
    );
}

/// The action byte is bound INSIDE the struct hash, so two payloads that
/// happened to hash alike still produce different digests.
#[test]
fn each_action_produces_a_distinct_digest() {
    let base = |payload| GovernanceAuth {
        payload,
        signer_epoch: 7,
        nonce: EvmU256::from_u64(3),
        expiry: 1_800_000_000,
    };
    let a = base(GovernancePayload::SetPaused {
        deposits_paused: false,
        payouts_paused: false,
    })
    .digest(domain())
    .unwrap();
    let b = base(GovernancePayload::SetRouteEnabled {
        route: Route::RhnToGlc,
        enabled: true,
    })
    .digest(domain())
    .unwrap();
    let c = base(GovernancePayload::SetLimits(limits(20, 40, 1, 5)))
        .digest(domain())
        .unwrap();
    let successor = EvmAddress::from_bytes([0x5c; 20]);
    let d = base(GovernancePayload::CommitMigration { successor })
        .digest(domain())
        .unwrap();
    let e = base(GovernancePayload::FinalizeMigration { successor })
        .digest(domain())
        .unwrap();
    let all = [a, b, c, d, e];
    for i in 0..all.len() {
        for j in i + 1..all.len() {
            assert_ne!(all[i], all[j], "digests {i} and {j} collide");
        }
    }
}

/// Commit and finalize hash the SAME payload — `abi.encode(successor)` —
/// and differ only by the action byte inside the struct hash. That is the
/// whole guard against a commit signature being replayed as a finalize,
/// so it is stated as a test rather than assumed.
#[test]
fn commit_and_finalize_share_a_payload_hash_and_differ_only_by_action() {
    let successor = EvmAddress::from_bytes([0x5c; 20]);
    let commit = GovernancePayload::CommitMigration { successor };
    let finalize = GovernancePayload::FinalizeMigration { successor };
    assert_eq!(
        commit.payload_hash().unwrap(),
        finalize.payload_hash().unwrap()
    );
    // The payload is the address as one left-padded word, exactly
    // `keccak256(abi.encode(address))`.
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(successor.as_bytes());
    assert_eq!(commit.payload_hash().unwrap(), keccak256(&word));
    assert_ne!(commit.action(), finalize.action());
    // And a different successor is a different payload.
    let other = GovernancePayload::CommitMigration {
        successor: EvmAddress::from_bytes([0x5d; 20]),
    };
    assert_ne!(
        commit.payload_hash().unwrap(),
        other.payload_hash().unwrap()
    );
}

/// Nonce, epoch and expiry are all bound. Changing any one must change
/// the digest, or a signature could be replayed against different state.
#[test]
fn the_nonce_epoch_and_expiry_are_all_bound_into_the_digest() {
    let payload = GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    };
    let base = GovernanceAuth {
        payload: payload.clone(),
        signer_epoch: 7,
        nonce: EvmU256::from_u64(3),
        expiry: 1_800_000_000,
    };
    let reference = base.digest(domain()).unwrap();

    let mut other_nonce = base.clone();
    other_nonce.nonce = EvmU256::from_u64(4);
    assert_ne!(reference, other_nonce.digest(domain()).unwrap(), "nonce");

    let mut other_epoch = base.clone();
    other_epoch.signer_epoch = 8;
    assert_ne!(reference, other_epoch.digest(domain()).unwrap(), "epoch");

    let mut other_expiry = base.clone();
    other_expiry.expiry = 1_800_000_001;
    assert_ne!(reference, other_expiry.digest(domain()).unwrap(), "expiry");

    // And the domain: a digest for one deployment must not verify on
    // another.
    let elsewhere = BridgeDomain::new(
        EvmChainId::new(4663).unwrap(),
        EvmAddress::from_bytes([0x0c; 20]),
    );
    assert_ne!(reference, base.digest(elsewhere).unwrap(), "contract");
}

// =====================================================================
// Route governance covers exactly the contract's routes
// =====================================================================

#[test]
fn the_solana_goldcoin_routes_cannot_be_enabled_or_disabled() {
    for route in [Route::GlcToSol, Route::SolToGlc] {
        for enabled in [true, false] {
            let payload = GovernancePayload::SetRouteEnabled { route, enabled };
            let err = payload
                .payload_hash()
                .expect_err("a route the contract does not model must be refused");
            assert!(
                matches!(err, GovernanceError::RouteNotGovernable { .. }),
                "{route:?}: {err}"
            );
            // And no calldata can be produced for one either, so the
            // refusal cannot be walked around by skipping the hash.
            let auth = GovernanceAuth {
                payload,
                signer_epoch: 1,
                nonce: EvmU256::ZERO,
                expiry: 1,
            };
            assert!(auth.digest(domain()).is_err(), "{route:?} digest");
            assert!(auth.calldata(&[]).is_err(), "{route:?} calldata");
        }
    }
}

/// The two Solana<->Robinhood routes are governable (Phase H), and each
/// binds its OWN contract byte — a SolToRhn payload can never hash to a
/// GlcToRhn one.
#[test]
fn the_cross_routes_are_governable_under_their_own_bytes() {
    let mut hashes = Vec::new();
    for route in [
        Route::GlcToRhn,
        Route::RhnToGlc,
        Route::SolToRhn,
        Route::RhnToSol,
    ] {
        let payload = GovernancePayload::SetRouteEnabled {
            route,
            enabled: true,
        };
        hashes.push(payload.payload_hash().unwrap());
    }
    hashes.sort_unstable();
    hashes.dedup();
    assert_eq!(hashes.len(), 4, "every route's payload hash is distinct");
    assert_eq!(
        governance_route_byte(Route::SolToRhn).unwrap(),
        ROUTE_SOL_TO_RHN
    );
    assert_eq!(
        governance_route_byte(Route::RhnToSol).unwrap(),
        ROUTE_RHN_TO_SOL
    );
}

#[test]
fn the_governable_routes_map_to_the_contract_bytes() {
    assert_eq!(
        governance_route_byte(Route::GlcToRhn).unwrap(),
        ROUTE_GLC_TO_RHN
    );
    assert_eq!(
        governance_route_byte(Route::RhnToGlc).unwrap(),
        ROUTE_RHN_TO_GLC
    );
}

// =====================================================================
// Deriving the limit set from configured policy
// =====================================================================

/// The whole point: max comes from the configured per-transfer limit,
/// the rolling bucket is HALF the configured strict policy, and the three
/// minimums are whatever the chain already holds.
#[test]
fn the_proposal_comes_from_configured_policy_and_preserves_the_minimums() {
    let binding = policy(600, 20_000, 10_000_000);
    // Deliberately unrelated current values, so a field that came from
    // anywhere but the two intended sources is visible.
    let current = limits(1, 2, 7 * u128::from(ONE_GLC) * 10_000_000_000, 3);
    let current = BridgeLimits {
        protected_min_reserve: EvmU256::from_u128(999 * CANONICAL_SCALE),
        ..current
    };

    let proposed = limits_from_policy(&binding, &current, MinimumOverrides::default()).unwrap();

    assert_eq!(proposed.inbound_max, binding.inbound_max().to_u256());
    assert_eq!(proposed.outbound_max, binding.outbound_max().to_u256());
    assert_eq!(
        proposed.inbound_rolling_limit,
        binding.expected_onchain_rolling_limit().to_u256()
    );
    assert_eq!(
        proposed.outbound_rolling_limit,
        binding.expected_onchain_rolling_limit().to_u256()
    );

    // Preserved, exactly.
    assert_eq!(proposed.inbound_min, current.inbound_min);
    assert_eq!(proposed.outbound_min, current.outbound_min);
    assert_eq!(
        proposed.protected_min_reserve,
        current.protected_min_reserve
    );
}

/// The bucket is half the strict policy, whatever the policy is — stated
/// for two different configurations so the relationship is the assertion,
/// not a memorised number.
#[test]
fn the_on_chain_bucket_is_always_half_the_configured_strict_policy() {
    for (per_transfer, rolling) in [(20_000u64, 10_000_000u64), (500, 4_000), (1, 2)] {
        let binding = policy(600, per_transfer, rolling);
        let current = limits(1, 2, CANONICAL_SCALE, 0);
        let proposed = limits_from_policy(&binding, &current, MinimumOverrides::default()).unwrap();

        let strict = binding.rolling_daily_policy().get();
        let bucket = proposed
            .inbound_rolling_limit
            .try_to_u128()
            .expect("fits u128");
        assert_eq!(
            bucket * 2,
            strict,
            "the bucket must be exactly half the strict policy for {rolling} GLC"
        );
    }
}

/// An odd strict policy cannot be halved exactly, and the refusal happens
/// where the policy is bound rather than surfacing as an 18-decimal
/// remainder later.
#[test]
fn an_unhalvable_strict_policy_is_refused_before_any_proposal_exists() {
    let policy = ChainPolicy::new_symmetric(
        Chain::Robinhood,
        600,
        CanonicalAtomic(2),
        CanonicalAtomic(5), // odd
    )
    .expect("a valid chain policy");
    let err = RobinhoodPolicyBinding::new(policy).expect_err("5 is not halvable");
    let detail = err.to_string();
    assert!(
        detail.contains("is odd") && detail.contains("exactly half"),
        "the refusal must name the odd policy and the half it implies: {detail}"
    );
}

/// A minimum is only ever changed when an operator says so — and then it
/// is that value, not a default.
#[test]
fn a_minimum_changes_only_when_explicitly_overridden() {
    let binding = policy(600, 20_000, 10_000_000);
    let current = limits(1, 2, CANONICAL_SCALE, 5 * CANONICAL_SCALE);

    let overrides = MinimumOverrides {
        inbound_min: Some(RobinhoodAtomic::new(3 * CANONICAL_SCALE)),
        ..MinimumOverrides::default()
    };
    assert!(!overrides.is_empty());
    let proposed = limits_from_policy(&binding, &current, overrides).unwrap();

    assert_eq!(
        proposed.inbound_min,
        EvmU256::from_u128(3 * CANONICAL_SCALE)
    );
    // The two nobody mentioned are untouched.
    assert_eq!(proposed.outbound_min, current.outbound_min);
    assert_eq!(
        proposed.protected_min_reserve,
        current.protected_min_reserve
    );
    assert!(MinimumOverrides::default().is_empty());
}

/// A chain whose current minimums are zero cannot produce a valid
/// proposal without an override — because `_validateLimits` would revert,
/// and refusing here is better than spending a nonce to find out.
#[test]
fn a_zero_minimum_on_chain_is_refused_rather_than_proposed() {
    let binding = policy(600, 20_000, 10_000_000);
    let current = limits(1, 2, 0, 0);
    let err = limits_from_policy(&binding, &current, MinimumOverrides::default())
        .expect_err("a zero inboundMin cannot be installed");
    assert!(
        matches!(
            err,
            GovernanceError::ZeroLimit {
                field: "inboundMin"
            }
        ),
        "{err}"
    );
}

// =====================================================================
// `_validateLimits`, mirrored
// =====================================================================

#[test]
fn a_non_canonical_limit_is_refused_with_the_field_named() {
    let mut l = limits(
        20 * CANONICAL_SCALE,
        40 * CANONICAL_SCALE,
        CANONICAL_SCALE,
        0,
    );
    l.outbound_max = EvmU256::from_u128(20 * CANONICAL_SCALE + 1);
    let err = validate_limits(&l).expect_err("not a multiple of CANONICAL_SCALE");
    assert!(
        matches!(
            err,
            GovernanceError::NotCanonical {
                field: "outboundMax",
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn a_rolling_limit_below_the_max_is_refused() {
    let l = limits(
        40 * CANONICAL_SCALE,
        20 * CANONICAL_SCALE,
        CANONICAL_SCALE,
        0,
    );
    let err = validate_limits(&l).expect_err("rolling below max reverts on chain");
    assert!(
        matches!(err, GovernanceError::RollingBelowMax { .. }),
        "{err}"
    );
}

#[test]
fn a_minimum_above_the_maximum_is_refused() {
    let l = limits(
        2 * CANONICAL_SCALE,
        40 * CANONICAL_SCALE,
        5 * CANONICAL_SCALE,
        0,
    );
    let err = validate_limits(&l).expect_err("min above max reverts on chain");
    assert!(matches!(err, GovernanceError::MinAboveMax { .. }), "{err}");
}

#[test]
fn a_valid_limit_set_passes() {
    let l = limits(
        20 * CANONICAL_SCALE,
        40 * CANONICAL_SCALE,
        CANONICAL_SCALE,
        7 * CANONICAL_SCALE,
    );
    assert!(validate_limits(&l).is_ok());
}

// =====================================================================
// Calldata
// =====================================================================

/// The calldata carries the same values the digest bound, and the three
/// actions have three distinct selectors.
#[test]
fn each_action_encodes_its_own_selector_and_arguments() {
    let sigs = vec![vec![0x11u8; 65], vec![0x22u8; 65]];
    let nonce = EvmU256::from_u64(9);

    let set_limits = GovernanceAuth {
        payload: GovernancePayload::SetLimits(limits(
            20 * CANONICAL_SCALE,
            40 * CANONICAL_SCALE,
            CANONICAL_SCALE,
            0,
        )),
        signer_epoch: 7,
        nonce,
        expiry: 1_800_000_000,
    }
    .calldata(&sigs)
    .unwrap();

    let set_paused = GovernanceAuth {
        payload: GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: false,
        },
        signer_epoch: 7,
        nonce,
        expiry: 1_800_000_000,
    }
    .calldata(&sigs)
    .unwrap();

    let set_route = GovernanceAuth {
        payload: GovernancePayload::SetRouteEnabled {
            route: Route::RhnToGlc,
            enabled: true,
        },
        signer_epoch: 7,
        nonce,
        expiry: 1_800_000_000,
    }
    .calldata(&sigs)
    .unwrap();

    let successor = EvmAddress::from_bytes([0x5c; 20]);
    let commit = GovernanceAuth {
        payload: GovernancePayload::CommitMigration { successor },
        signer_epoch: 7,
        nonce,
        expiry: 1_800_000_000,
    }
    .calldata(&sigs)
    .unwrap();
    let finalize = GovernanceAuth {
        payload: GovernancePayload::FinalizeMigration { successor },
        signer_epoch: 7,
        nonce,
        expiry: 1_800_000_000,
    }
    .calldata(&sigs)
    .unwrap();

    assert_eq!(&set_limits[..4], &crate::evm::abi::selector(SIG_SET_LIMITS));
    assert_eq!(&set_paused[..4], &crate::evm::abi::selector(SIG_SET_PAUSED));
    assert_eq!(
        &set_route[..4],
        &crate::evm::abi::selector(SIG_SET_ROUTE_ENABLED)
    );
    assert_eq!(
        &commit[..4],
        &crate::evm::abi::selector(SIG_COMMIT_MIGRATION)
    );
    assert_eq!(
        &finalize[..4],
        &crate::evm::abi::selector(SIG_FINALIZE_MIGRATION)
    );
    assert_ne!(set_limits[..4], set_paused[..4]);
    assert_ne!(set_paused[..4], set_route[..4]);
    assert_ne!(commit[..4], finalize[..4]);

    // commitMigration's head: the successor word, the nonce, the expiry.
    assert_eq!(&commit[4 + 12..4 + 32], successor.as_bytes(), "successor");
    assert_eq!(&commit[4 + 32..4 + 64], &nonce.to_be_bytes(), "nonce");
    // finalizeMigration takes NO successor: its head starts at the nonce.
    assert_eq!(&finalize[4..4 + 32], &nonce.to_be_bytes(), "nonce first");

    // setPaused's head: two bools, the nonce, the expiry, then the
    // offset to the signatures array.
    assert_eq!(set_paused[4 + 31], 1, "depositsPaused");
    assert_eq!(set_paused[4 + 63], 0, "payoutsPaused");
    assert_eq!(
        &set_paused[4 + 64..4 + 96],
        &nonce.to_be_bytes(),
        "the nonce word"
    );

    // setRouteEnabled's head begins with the padded route byte.
    assert_eq!(set_route[4 + 31], ROUTE_RHN_TO_GLC);
    assert_eq!(set_route[4 + 63], 1, "enabled");
}

#[test]
fn the_kind_strings_agree_with_the_action_bytes() {
    for (payload, kind, action) in [
        (
            GovernancePayload::SetLimits(limits(1, 2, 1, 0)),
            "set_limits",
            ACTION_SET_LIMITS,
        ),
        (
            GovernancePayload::SetPaused {
                deposits_paused: false,
                payouts_paused: false,
            },
            "set_pause",
            ACTION_SET_PAUSE,
        ),
        (
            GovernancePayload::SetRouteEnabled {
                route: Route::GlcToRhn,
                enabled: true,
            },
            "set_route_enabled",
            ACTION_SET_ROUTE_ENABLED,
        ),
        (
            GovernancePayload::CommitMigration {
                successor: EvmAddress::from_bytes([0x5c; 20]),
            },
            "commit_migration",
            ACTION_COMMIT_MIGRATION,
        ),
        (
            GovernancePayload::FinalizeMigration {
                successor: EvmAddress::from_bytes([0x5c; 20]),
            },
            "finalize_migration",
            ACTION_FINALIZE_MIGRATION,
        ),
    ] {
        assert_eq!(payload.kind_str(), kind);
        assert_eq!(payload.action(), action);
    }
}

// =====================================================================
// The CROSS-LANGUAGE golden vectors
// =====================================================================
//
// The checks above compare this module against the contract's SOURCE
// TEXT. These compare it against the same fixture
// `contracts/test/GoldenDigests.t.sol` asserts the DEPLOYED CONTRACT
// produces — which is the stronger claim, and the one the payout, refund
// and settlement payloads have always had.
//
// Neither side generates the file. If this module's encoding drifts by a
// single byte, or two fields of the same width are transposed, the digest
// changes and these fail; if the contract drifts, the Foundry suite fails
// against the same file. Agreement is therefore evidence rather than
// coincidence.

use crate::robinhood::golden::{self, hex32};

/// The fixture's `governanceLimits` — seven distinct figures, a TEST
/// VECTOR and never a policy. Read FROM the fixture rather than restated,
/// so a change there cannot be silently ignored here.
fn golden_limits() -> BridgeLimits {
    let field = |name: &str| {
        let raw = golden::get(&format!("governanceLimits.{name}"));
        EvmU256::from_u128(
            raw.parse::<u128>()
                .unwrap_or_else(|e| panic!("governanceLimits.{name} = {raw:?}: {e}")),
        )
    };
    BridgeLimits {
        inbound_min: field("inboundMin"),
        inbound_max: field("inboundMax"),
        inbound_rolling_limit: field("inboundRollingLimit"),
        outbound_min: field("outboundMin"),
        outbound_max: field("outboundMax"),
        outbound_rolling_limit: field("outboundRollingLimit"),
        protected_min_reserve: field("protectedMinReserve"),
    }
}

fn golden_u64(path: &str) -> u64 {
    let raw = golden::get(path);
    raw.parse()
        .unwrap_or_else(|e| panic!("{path} = {raw:?}: {e}"))
}

fn golden_domain() -> BridgeDomain {
    BridgeDomain::new(
        EvmChainId::new(golden_u64("evmChainId")).expect("a valid chain id"),
        golden::VERIFYING_CONTRACT
            .parse()
            .expect("the fixture's verifying contract"),
    )
}

fn golden_auth(payload: GovernancePayload) -> GovernanceAuth {
    GovernanceAuth {
        payload,
        signer_epoch: golden_u64("signerEpoch"),
        nonce: EvmU256::from_u128(
            golden::get("governanceNonce")
                .parse()
                .expect("the fixture's governance nonce"),
        ),
        expiry: golden_u64("expiry"),
    }
}

/// The single highest-risk value: one character's difference in the type
/// string is a different typehash, a different digest, and a signature
/// over something other than what the operator read.
#[test]
fn golden_governance_typehash() {
    assert_eq!(
        hex32(&crate::evm::keccak::keccak256(GOVERNANCE_TYPE.as_bytes())),
        golden::get("governanceTypehash"),
        "the Rust GovernanceAuth type string drifted from the deployed contract's"
    );
}

/// Every action byte, as the contract reports it to the fixture.
#[test]
fn golden_governance_action_bytes() {
    for (path, ours) in [
        ("governance.setLimits.action", ACTION_SET_LIMITS),
        ("governance.setPause.action", ACTION_SET_PAUSE),
        (
            "governance.setRouteEnabled.action",
            ACTION_SET_ROUTE_ENABLED,
        ),
    ] {
        assert_eq!(golden::get(path), ours.to_string(), "{path}");
    }
}

/// `setLimits`: the payload hash over seven inlined words, the struct
/// hash that binds it with the action, epoch, nonce and expiry, and the
/// final digest.
#[test]
fn golden_set_limits_payload_struct_hash_and_digest() {
    let payload = GovernancePayload::SetLimits(golden_limits());
    assert_eq!(
        hex32(&payload.payload_hash().unwrap()),
        golden::get("governance.setLimits.payloadHash"),
        "setLimits payload hash"
    );
    let auth = golden_auth(payload);
    assert_eq!(
        hex32(&auth.struct_hash().unwrap()),
        golden::get("governance.setLimits.structHash"),
        "setLimits struct hash"
    );
    assert_eq!(
        hex32(&auth.digest(golden_domain()).unwrap()),
        golden::get("governance.setLimits.digest"),
        "setLimits digest"
    );
}

/// `setPaused`. The fixture's pair is `(true, false)` deliberately —
/// asymmetric, so transposing the two booleans changes the hash.
#[test]
fn golden_set_pause_payload_struct_hash_and_digest() {
    let payload = GovernancePayload::SetPaused {
        deposits_paused: golden::get("governanceDepositsPaused") == "true",
        payouts_paused: golden::get("governancePayoutsPaused") == "true",
    };
    assert_eq!(
        hex32(&payload.payload_hash().unwrap()),
        golden::get("governance.setPause.payloadHash"),
        "setPause payload hash"
    );
    let auth = golden_auth(payload);
    assert_eq!(
        hex32(&auth.struct_hash().unwrap()),
        golden::get("governance.setPause.structHash"),
        "setPause struct hash"
    );
    assert_eq!(
        hex32(&auth.digest(golden_domain()).unwrap()),
        golden::get("governance.setPause.digest"),
        "setPause digest"
    );
}

#[test]
fn golden_set_route_enabled_payload_struct_hash_and_digest() {
    // The fixture names the contract's route BYTE; this side is typed, so
    // the mapping itself is part of what agrees.
    let route = match golden::get("governanceRoute").as_str() {
        "1" => Route::GlcToRhn,
        "2" => Route::RhnToGlc,
        other => panic!("the fixture's governanceRoute {other} is not a governable route"),
    };
    let payload = GovernancePayload::SetRouteEnabled {
        route,
        enabled: golden::get("governanceRouteEnabled") == "true",
    };
    assert_eq!(
        hex32(&payload.payload_hash().unwrap()),
        golden::get("governance.setRouteEnabled.payloadHash"),
        "setRouteEnabled payload hash"
    );
    let auth = golden_auth(payload);
    assert_eq!(
        hex32(&auth.struct_hash().unwrap()),
        golden::get("governance.setRouteEnabled.structHash"),
        "setRouteEnabled struct hash"
    );
    assert_eq!(
        hex32(&auth.digest(golden_domain()).unwrap()),
        golden::get("governance.setRouteEnabled.digest"),
        "setRouteEnabled digest"
    );
}

/// The two migration vectors: the SAME payload hash under `0x08` and
/// `0x09`, and two digests that differ only by that byte.
#[test]
fn golden_migration_payload_struct_hashes_and_digests() {
    let successor: EvmAddress = golden::get("inputs.governanceMigrationSuccessor")
        .parse()
        .expect("the fixture's successor is an address");
    for (payload, key) in [
        (
            GovernancePayload::CommitMigration { successor },
            "commitMigration",
        ),
        (
            GovernancePayload::FinalizeMigration { successor },
            "finalizeMigration",
        ),
    ] {
        assert_eq!(
            u32::from(payload.action()),
            golden::get(&format!("governance.{key}.action"))
                .parse::<u32>()
                .unwrap(),
            "{key} action byte"
        );
        assert_eq!(
            hex32(&payload.payload_hash().unwrap()),
            golden::get(&format!("governance.{key}.payloadHash")),
            "{key} payload hash"
        );
        let auth = golden_auth(payload);
        assert_eq!(
            hex32(&auth.struct_hash().unwrap()),
            golden::get(&format!("governance.{key}.structHash")),
            "{key} struct hash"
        );
        assert_eq!(
            hex32(&auth.digest(golden_domain()).unwrap()),
            golden::get(&format!("governance.{key}.digest")),
            "{key} digest"
        );
    }
}

// ---------------------------------------------------------------------
// Byte-and-order mismatch detection
// ---------------------------------------------------------------------
//
// A golden vector proves agreement on one input. These prove the vectors
// would actually CATCH the mistakes they exist to catch: every one below
// is a change that still compiles, still produces 32 bytes, and must
// produce DIFFERENT 32 bytes.

/// Transposing any two `Limits` members changes the payload hash. The
/// pairs chosen are the ones a careless edit would actually swap: the two
/// minimums, the two maximums, the two rolling limits, and a min with its
/// own max.
#[test]
fn transposing_any_two_limit_fields_changes_the_payload_hash() {
    let good = golden_limits();
    let reference = golden::get("governance.setLimits.payloadHash");

    /// A named transposition of two `Limits` members.
    type Swap = (&'static str, fn(&mut BridgeLimits));

    let swaps: [Swap; 5] = [
        ("inboundMin <-> outboundMin", |l| {
            std::mem::swap(&mut l.inbound_min, &mut l.outbound_min)
        }),
        ("inboundMax <-> outboundMax", |l| {
            std::mem::swap(&mut l.inbound_max, &mut l.outbound_max)
        }),
        ("inboundRolling <-> outboundRolling", |l| {
            std::mem::swap(&mut l.inbound_rolling_limit, &mut l.outbound_rolling_limit)
        }),
        ("inboundMin <-> inboundMax", |l| {
            std::mem::swap(&mut l.inbound_min, &mut l.inbound_max)
        }),
        ("outboundRolling <-> protectedMin", |l| {
            std::mem::swap(&mut l.outbound_rolling_limit, &mut l.protected_min_reserve)
        }),
    ];

    for (what, swap) in swaps {
        let mut limits = good;
        swap(&mut limits);
        assert_ne!(limits, good, "{what} must actually change the struct");
        assert_ne!(
            hex32(&GovernancePayload::SetLimits(limits).payload_hash().unwrap()),
            reference,
            "{what} must change the payload hash — the golden vector would not catch a reorder"
        );
    }
}

/// Transposing the two pause booleans changes the payload hash. This is
/// why the fixture pins an asymmetric pair.
#[test]
fn transposing_the_pause_booleans_changes_the_payload_hash() {
    let straight = GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: false,
    };
    let transposed = GovernancePayload::SetPaused {
        deposits_paused: false,
        payouts_paused: true,
    };
    assert_eq!(
        hex32(&straight.payload_hash().unwrap()),
        golden::get("governance.setPause.payloadHash")
    );
    assert_ne!(
        hex32(&transposed.payload_hash().unwrap()),
        golden::get("governance.setPause.payloadHash"),
        "swapping the two directions must not reproduce the golden payload hash"
    );
}

/// Every field the struct hash binds must change it. `signer_epoch` and
/// `nonce` are the dangerous pair: both are small integers sitting next
/// to each other, so only the FIELD ORDER tells them apart.
#[test]
fn every_bound_field_changes_the_golden_struct_hash() {
    let payload = GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: false,
    };
    let reference = golden::get("governance.setPause.structHash");
    let base = golden_auth(payload.clone());
    assert_eq!(hex32(&base.struct_hash().unwrap()), reference);

    let mut other_nonce = base.clone();
    other_nonce.nonce =
        EvmU256::from_u128(golden::get("governanceNonce").parse::<u128>().unwrap() + 1);
    assert_ne!(
        hex32(&other_nonce.struct_hash().unwrap()),
        reference,
        "nonce"
    );

    let mut other_epoch = base.clone();
    other_epoch.signer_epoch = base.signer_epoch + 1;
    assert_ne!(
        hex32(&other_epoch.struct_hash().unwrap()),
        reference,
        "epoch"
    );

    let mut other_expiry = base.clone();
    other_expiry.expiry = base.expiry + 1;
    assert_ne!(
        hex32(&other_expiry.struct_hash().unwrap()),
        reference,
        "expiry"
    );

    // The epoch and the nonce carry each other's values: only the order
    // in which they are encoded distinguishes this from `base`.
    let transposed = GovernanceAuth {
        payload,
        signer_epoch: base.nonce.try_to_u128().unwrap() as u64,
        nonce: EvmU256::from_u128(u128::from(base.signer_epoch)),
        expiry: base.expiry,
    };
    assert_ne!(
        hex32(&transposed.struct_hash().unwrap()),
        reference,
        "signerEpoch and nonce must not be interchangeable"
    );
}

/// A digest is bound to ONE deployment. The same proposal against another
/// chain id or another contract must not reproduce the golden digest.
#[test]
fn the_golden_digest_is_bound_to_one_deployment() {
    let auth = golden_auth(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: false,
    });
    let reference = golden::get("governance.setPause.digest");
    assert_eq!(hex32(&auth.digest(golden_domain()).unwrap()), reference);

    let other_chain = BridgeDomain::new(
        EvmChainId::new(1).unwrap(),
        golden::VERIFYING_CONTRACT.parse().unwrap(),
    );
    assert_ne!(
        hex32(&auth.digest(other_chain).unwrap()),
        reference,
        "chain id"
    );

    let other_contract = BridgeDomain::new(
        EvmChainId::new(golden_u64("evmChainId")).unwrap(),
        EvmAddress::from_bytes([0x0c; 20]),
    );
    assert_ne!(
        hex32(&auth.digest(other_contract).unwrap()),
        reference,
        "verifying contract"
    );
}

/// The three golden digests differ from one another even though every
/// field but the action and the payload is identical — the action byte is
/// bound INSIDE the struct hash exactly so a signature for one can never
/// verify as another.
#[test]
fn the_three_golden_governance_digests_are_distinct() {
    let digests: Vec<String> = ["setLimits", "setPause", "setRouteEnabled"]
        .iter()
        .map(|k| golden::get(&format!("governance.{k}.digest")))
        .collect();
    assert_ne!(digests[0], digests[1]);
    assert_ne!(digests[1], digests[2]);
    assert_ne!(digests[0], digests[2]);
}

/// `setLimits` reconciles the two maxima SEPARATELY: `inboundMax` from
/// the inbound limit (the user's deposit ceiling, unchanged at 20_000),
/// `outboundMax` from the outbound one (destination settlement capacity),
/// with the minimums and the protected reserve preserved and one rolling
/// bucket covering the larger maximum — and the proposal passes the
/// contract's own `_validateLimits` rules.
#[test]
fn the_proposal_reconciles_inbound_and_outbound_maxima_separately() {
    let policy = ChainPolicy::new(
        Chain::Robinhood,
        600,
        CanonicalAtomic(20_000 * ONE_GLC),
        CanonicalAtomic(2_000_000 * ONE_GLC),
        CanonicalAtomic(10_000_000 * ONE_GLC),
    )
    .unwrap();
    let binding = RobinhoodPolicyBinding::new(policy).unwrap();
    let current = limits(
        20_000 * u128::from(ONE_GLC) * 10_000_000_000,
        5_000_000 * u128::from(ONE_GLC) * 10_000_000_000,
        u128::from(ONE_GLC) * 10_000_000_000,
        1_000 * u128::from(ONE_GLC) * 10_000_000_000,
    );
    let proposed = limits_from_policy(&binding, &current, MinimumOverrides::default()).unwrap();
    assert_eq!(
        proposed.inbound_max,
        EvmU256::from_u128(20_000 * 10u128.pow(18))
    );
    assert_eq!(
        proposed.outbound_max,
        EvmU256::from_u128(2_000_000 * 10u128.pow(18))
    );
    assert_eq!(
        proposed.inbound_rolling_limit,
        EvmU256::from_u128(5_000_000 * 10u128.pow(18))
    );
    assert_eq!(
        proposed.outbound_rolling_limit,
        EvmU256::from_u128(5_000_000 * 10u128.pow(18))
    );
    assert_eq!(proposed.inbound_min, current.inbound_min);
    assert_eq!(proposed.outbound_min, current.outbound_min);
    assert_eq!(
        proposed.protected_min_reserve,
        current.protected_min_reserve
    );
    assert!(validate_limits(&proposed).is_ok());
}
