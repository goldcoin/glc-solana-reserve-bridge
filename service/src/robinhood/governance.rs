//! The `GlcRobinhoodBridge` EIP-712 GOVERNANCE authorization: the five
//! actions an operator may propose against a deployed bridge, and the
//! config-driven derivation of the limit set one of them installs.
//!
//! # This is a transcription, not a design
//!
//! Exactly as [`super::auth`] says of the payout/refund/settlement
//! payloads: every constant, every field and every field ORDER below is
//! copied from `contracts/src/GlcRobinhoodBridge.sol` and must match the
//! deployed bytecode. A single character's difference in the type string
//! yields a different `typeHash`, a different digest, and a signature the
//! contract refuses — or, in the case that matters, a signature over a
//! payload that says something other than what the operator read.
//!
//! Because a Rust transcription of a Solidity constant is exactly the
//! kind of thing that drifts silently, this module is not trusted on its
//! own. It is checked TWICE, independently:
//!
//! - [`tests`]'s `golden_*` cases assert this module reproduces the five
//!   governance vectors in `contracts/test/fixtures/eip712-golden.json`,
//!   the same file `contracts/test/GoldenDigests.t.sol` asserts the
//!   DEPLOYED CONTRACT produces. Neither side generates it.
//! - [`tests`] additionally re-extracts every constant, and the `Limits`
//!   field ORDER, from `GlcRobinhoodBridge.sol` at test time, so a rename
//!   or a reorder fails here even before a digest is computed.
//!
//! The fixture's vectors are chosen so that a mismatch is detectable
//! rather than merely possible: the seven `Limits` figures are all
//! distinct and the pause pair is asymmetric, so transposing any two
//! fields changes the hash. `transposing_any_two_limit_fields_changes_
//! the_payload_hash` and `every_bound_field_changes_the_golden_struct_
//! hash` prove the vectors would catch the mistakes they exist for.
//!
//! # Five actions; rotation and abandonment are still absent
//!
//! The contract's governance surface is larger than this: it also carries
//! `rotateSigners`, `rotateGuardians` and `ACTION_ABANDON`. None of them
//! is representable here, for the same reason [`super::auth`] cannot
//! build an `AbandonmentAuth`: a rotation replaces the very signer set
//! that authorizes it, so it does not belong behind an operator CLI that
//! a single person runs. A future need for either is a deliberate,
//! separately reviewed change.
//!
//! `commitMigration` and `finalizeMigration` ARE representable, since the
//! first migration — out of the deployment that predates the treasury
//! withdrawal — has to be driven by this tooling or by hand. They are
//! terminal for the routes, and everything about them is arranged so a
//! single person cannot reach the end alone: two separate authorizations
//! at two consecutive governance nonces, each a 2-of-3 quorum of custody
//! domains that independently opt in to the action by name
//! (`GLC_RHN_SIGNER_ALLOWED_GOVERNANCE_ACTIONS`), a guardian veto open
//! from commit until finalize, and the contract's own zero-liability and
//! (on a delayed deployment) time gates. The SUCCESSOR is bound into the
//! signed payload of both, so the finalize quorum re-approves the address
//! the commit quorum approved, and the session refuses to build a
//! finalize for any address but the one the chain says is committed.
//!
//! [`GovernancePayload::SetRouteEnabled`] governs every route the
//! contract models, including `SolToRhn` and `RhnToSol` since Phase H
//! gave them settlement machinery. Until then this tool refused them
//! ([`GovernanceError::RouteNotGovernable`]) because a switch wired to
//! nothing is worse than no switch; the variant remains for the two
//! Solana<->Goldcoin routes, which the contract does not model at all.
//!
//! # Nothing here signs, sends, or holds a key
//!
//! This module builds payload hashes, struct hashes, digests and
//! calldata. Producing a signature is [`crate::signing`]'s job and
//! requires a 2-of-3 quorum of custody domains; broadcasting is
//! [`super::submitter`]'s. A digest built here authorizes nothing until
//! two independent domains have each rebuilt it from structured fields
//! and agreed.

use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::evm::abi::{word_address, word_bool, word_u128, word_u256, Calldata};
use crate::evm::eip712::{encode_bool, encode_uint128, encode_uint256};
use crate::evm::keccak::{keccak256, keccak256_concat};
use crate::evm::{EvmAddress, EvmU256};
use crate::robinhood::auth::BridgeDomain;
use crate::robinhood::calls::BridgeLimits;
use crate::robinhood::policy::RobinhoodPolicyBinding;
use crate::routes::Route;

/// Action discriminator: set both pause flags. `ACTION_SET_PAUSE`.
pub const ACTION_SET_PAUSE: u8 = 0x04;
/// Action discriminator: replace the whole limit set. `ACTION_SET_LIMITS`.
pub const ACTION_SET_LIMITS: u8 = 0x07;
/// Action discriminator: commit to a migration successor.
/// `ACTION_COMMIT_MIGRATION`.
pub const ACTION_COMMIT_MIGRATION: u8 = 0x08;
/// Action discriminator: move the whole reserve to the committed
/// successor. `ACTION_FINALIZE_MIGRATION`.
pub const ACTION_FINALIZE_MIGRATION: u8 = 0x09;
/// Action discriminator: enable or disable one route.
/// `ACTION_SET_ROUTE_ENABLED`.
pub const ACTION_SET_ROUTE_ENABLED: u8 = 0x0B;

/// `GovernanceAuth`'s type string, character for character.
///
/// `payloadHash` is the keccak256 of the exact proposed change, so
/// approving one payload can never authorize a different one under the
/// same action.
pub const GOVERNANCE_TYPE: &str =
    "GovernanceAuth(uint8 action,bytes32 payloadHash,uint64 signerEpoch,uint256 nonce,\
uint64 expiry)";

/// The contract's route discriminators. A SEPARATE space from the action
/// bytes, and it must stay separate.
pub const ROUTE_GLC_TO_RHN: u8 = 0x01;
pub const ROUTE_RHN_TO_GLC: u8 = 0x02;
pub const ROUTE_SOL_TO_RHN: u8 = 0x03;
pub const ROUTE_RHN_TO_SOL: u8 = 0x04;

/// `CANONICAL_SCALE` — every limit must be a whole multiple of this, or
/// `_validateLimits` reverts. 10^(18-8).
pub const CANONICAL_SCALE: u128 = 10_000_000_000;

/// Function signatures, for calldata selectors. A struct argument is
/// spelled as its tuple expansion, which is what the selector hashes.
pub const SIG_SET_LIMITS: &str =
    "setLimits((uint256,uint256,uint256,uint256,uint256,uint256,uint256),uint256,uint64,bytes[])";
pub const SIG_SET_PAUSED: &str = "setPaused(bool,bool,uint256,uint64,bytes[])";
pub const SIG_SET_ROUTE_ENABLED: &str = "setRouteEnabled(uint8,bool,uint256,uint64,bytes[])";
pub const SIG_COMMIT_MIGRATION: &str = "commitMigration(address,uint256,uint64,bytes[])";
pub const SIG_FINALIZE_MIGRATION: &str = "finalizeMigration(uint256,uint64,bytes[])";
/// `governanceNonce()` — the next nonce a governance action must carry.
pub const SIG_GOVERNANCE_NONCE: &str = "governanceNonce()";

/// Why a governance authorization could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GovernanceError {
    #[error(
        "{route} cannot be enabled or disabled by this tool — the custody contract does not \
         model it (it has no route discriminator), so there is no on-chain flag to set"
    )]
    RouteNotGovernable { route: &'static str },
    #[error(
        "a limit is not a whole multiple of CANONICAL_SCALE ({CANONICAL_SCALE}): {field} = \
         {value}. `_validateLimits` reverts on this, so the proposal could never be installed"
    )]
    NotCanonical { field: &'static str, value: u128 },
    #[error(
        "{field} is zero. `_validateLimits` refuses a zero minimum, and a zero maximum closes \
         the direction silently rather than obviously"
    )]
    ZeroLimit { field: &'static str },
    #[error(
        "{direction}Min {min} exceeds {direction}Max {max} — `_validateLimits` reverts, and the \
         direction would be closed to every amount"
    )]
    MinAboveMax {
        direction: &'static str,
        min: u128,
        max: u128,
    },
    #[error(
        "{direction}RollingLimit {rolling} is below {direction}Max {max} — `_validateLimits` \
         reverts: a single legal transfer could not fit in the bucket"
    )]
    RollingBelowMax {
        direction: &'static str,
        rolling: u128,
        max: u128,
    },
    #[error("a limit does not fit a u128: {field}")]
    LimitTooLarge { field: &'static str },
}

/// One proposed governance change, in the exact shape the contract hashes.
///
/// Constructing one of these is the ONLY way to reach
/// [`GovernanceAuth`], and each variant carries typed, already-validated
/// values — there is no path that takes a loose `u8` route or a raw byte
/// array standing in for a limit set.
// The `SetLimits` variant is 224 bytes and the other two are two bytes.
// Boxing it would spare a stack copy of one value that exists once per
// operator command, in exchange for an indirection in every match on a
// payload whose whole job is to be read by a human before it is signed.
// The clarity is worth more than the bytes here.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GovernancePayload {
    /// `setLimits(Limits)` — whole-struct replacement, so signers approve
    /// a complete policy rather than a delta whose meaning depends on
    /// state they cannot see.
    SetLimits(BridgeLimits),
    /// `setPaused(bool,bool)` — both flags, always. The contract takes
    /// both, so a proposal that named one would be hiding the other.
    SetPaused {
        deposits_paused: bool,
        payouts_paused: bool,
    },
    /// `setRouteEnabled(uint8,bool)` — one route, absolutely stated.
    SetRouteEnabled { route: Route, enabled: bool },
    /// `commitMigration(address)` — name the successor the whole reserve
    /// will move to. Terminal for every route the moment it lands.
    CommitMigration { successor: EvmAddress },
    /// `finalizeMigration()` — move the whole reserve to the successor
    /// the chain holds as committed. The contract hashes
    /// `abi.encode(migrationSuccessor)`; the address is carried here so
    /// the SIGNED payload names it and the finalize quorum approves the
    /// same address the commit quorum did, rather than "whatever is
    /// committed by the time this lands".
    FinalizeMigration { successor: EvmAddress },
}

impl GovernancePayload {
    /// The contract's action discriminator for this payload.
    pub fn action(&self) -> u8 {
        match self {
            GovernancePayload::SetLimits(_) => ACTION_SET_LIMITS,
            GovernancePayload::SetPaused { .. } => ACTION_SET_PAUSE,
            GovernancePayload::SetRouteEnabled { .. } => ACTION_SET_ROUTE_ENABLED,
            GovernancePayload::CommitMigration { .. } => ACTION_COMMIT_MIGRATION,
            GovernancePayload::FinalizeMigration { .. } => ACTION_FINALIZE_MIGRATION,
        }
    }

    /// The wire spelling, which must agree with [`Self::action`]. Carried
    /// alongside the action byte on the signer protocol for the same
    /// reason [`crate::signing::evm_policy`] carries `kind`: two
    /// spellings that must agree catch a malformed or deliberately
    /// confusing request.
    pub fn kind_str(&self) -> &'static str {
        match self {
            GovernancePayload::SetLimits(_) => "set_limits",
            GovernancePayload::SetPaused { .. } => "set_pause",
            GovernancePayload::SetRouteEnabled { .. } => "set_route_enabled",
            GovernancePayload::CommitMigration { .. } => "commit_migration",
            GovernancePayload::FinalizeMigration { .. } => "finalize_migration",
        }
    }

    /// `keccak256(abi.encode(...))` of the exact proposed change — the
    /// contract's `payloadHash`.
    ///
    /// The encodings are transcribed one for one:
    /// - `setLimits`:  `abi.encode(newLimits)`, a static struct, so its
    ///   seven `uint256` members are inlined in declaration order with no
    ///   offset word.
    /// - `setPaused`:  `abi.encode(depositsPaused_, payoutsPaused_)`.
    /// - `setRouteEnabled`: `abi.encode(route, enabled)`, the `uint8`
    ///   left-padded into a full word.
    /// - `commitMigration` / `finalizeMigration`: `abi.encode(successor)`,
    ///   the address left-padded into a full word. The SAME payload under
    ///   two action bytes; the action inside the struct hash is what keeps
    ///   a commit signature from verifying as a finalize.
    pub fn payload_hash(&self) -> Result<[u8; 32], GovernanceError> {
        match self {
            GovernancePayload::SetLimits(limits) => Ok(keccak256_concat(&[
                &encode_uint256(limits.inbound_min),
                &encode_uint256(limits.inbound_max),
                &encode_uint256(limits.inbound_rolling_limit),
                &encode_uint256(limits.outbound_min),
                &encode_uint256(limits.outbound_max),
                &encode_uint256(limits.outbound_rolling_limit),
                &encode_uint256(limits.protected_min_reserve),
            ])),
            GovernancePayload::SetPaused {
                deposits_paused,
                payouts_paused,
            } => Ok(keccak256_concat(&[
                &encode_bool(*deposits_paused),
                &encode_bool(*payouts_paused),
            ])),
            GovernancePayload::SetRouteEnabled { route, enabled } => {
                let byte = governance_route_byte(*route)?;
                Ok(keccak256_concat(&[
                    &encode_uint128(u128::from(byte)),
                    &encode_bool(*enabled),
                ]))
            }
            GovernancePayload::CommitMigration { successor }
            | GovernancePayload::FinalizeMigration { successor } => {
                Ok(keccak256(&word_address(*successor)))
            }
        }
    }
}

/// The contract's route byte for a route this tool may govern: every
/// route the contract models. The two Solana<->Goldcoin routes have no
/// byte and are refused.
///
/// Governing a route here says nothing about whether it opens: the
/// contract flag is one of four independent gates, and this service's
/// own three (config, `bridge_routes`, adapter capability) still stand.
pub fn governance_route_byte(route: Route) -> Result<u8, GovernanceError> {
    match route {
        Route::GlcToRhn => Ok(ROUTE_GLC_TO_RHN),
        Route::RhnToGlc => Ok(ROUTE_RHN_TO_GLC),
        Route::SolToRhn => Ok(ROUTE_SOL_TO_RHN),
        Route::RhnToSol => Ok(ROUTE_RHN_TO_SOL),
        other => Err(GovernanceError::RouteNotGovernable {
            route: other.as_str(),
        }),
    }
}

/// A complete governance authorization: the payload, plus the three
/// fields that bind it to one contract state and one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernanceAuth {
    pub payload: GovernancePayload,
    /// The contract's CURRENT `signerEpoch`. Read from the chain, never
    /// assumed: a rotation invalidates every outstanding signature and an
    /// authorization built against a stale epoch simply will not verify.
    pub signer_epoch: u64,
    /// The contract's CURRENT `governanceNonce`. Strict equality on
    /// chain, consumed on use, so governance actions are totally ordered
    /// and no signature can be replayed.
    pub nonce: EvmU256,
    /// Unix seconds after which the contract refuses this authorization.
    pub expiry: u64,
}

impl GovernanceAuth {
    /// `hashStruct(GovernanceAuth)`.
    pub fn struct_hash(&self) -> Result<[u8; 32], GovernanceError> {
        Ok(keccak256_concat(&[
            &keccak256(GOVERNANCE_TYPE.as_bytes()),
            &encode_uint128(u128::from(self.payload.action())),
            &self.payload.payload_hash()?,
            &encode_uint128(u128::from(self.signer_epoch)),
            &encode_uint256(self.nonce),
            &encode_uint128(u128::from(self.expiry)),
        ]))
    }

    /// The 32 bytes each of the two signers must sign, under `domain`.
    pub fn digest(&self, domain: BridgeDomain) -> Result<[u8; 32], GovernanceError> {
        Ok(domain.digest(&self.struct_hash()?))
    }

    /// The exact calldata for this action, given a quorum's signatures.
    ///
    /// Built here rather than at the call site so the calldata and the
    /// digest that authorizes it come from one place: a mismatch between
    /// what was signed and what is broadcast is the failure this
    /// arrangement makes unrepresentable.
    pub fn calldata(&self, signatures: &[Vec<u8>]) -> Result<Vec<u8>, GovernanceError> {
        let nonce = word_u256(self.nonce);
        let expiry = word_u128(u128::from(self.expiry));
        let sigs = signatures.to_vec();
        Ok(match &self.payload {
            GovernancePayload::SetLimits(limits) => Calldata::new(SIG_SET_LIMITS)
                .word(word_u256(limits.inbound_min))
                .word(word_u256(limits.inbound_max))
                .word(word_u256(limits.inbound_rolling_limit))
                .word(word_u256(limits.outbound_min))
                .word(word_u256(limits.outbound_max))
                .word(word_u256(limits.outbound_rolling_limit))
                .word(word_u256(limits.protected_min_reserve))
                .word(nonce)
                .word(expiry)
                .bytes_array(sigs)
                .finish(),
            GovernancePayload::SetPaused {
                deposits_paused,
                payouts_paused,
            } => Calldata::new(SIG_SET_PAUSED)
                .word(word_bool(*deposits_paused))
                .word(word_bool(*payouts_paused))
                .word(nonce)
                .word(expiry)
                .bytes_array(sigs)
                .finish(),
            GovernancePayload::SetRouteEnabled { route, enabled } => {
                let byte = governance_route_byte(*route)?;
                Calldata::new(SIG_SET_ROUTE_ENABLED)
                    .word(word_u128(u128::from(byte)))
                    .word(word_bool(*enabled))
                    .word(nonce)
                    .word(expiry)
                    .bytes_array(sigs)
                    .finish()
            }
            GovernancePayload::CommitMigration { successor } => Calldata::new(SIG_COMMIT_MIGRATION)
                .word(word_address(*successor))
                .word(nonce)
                .word(expiry)
                .bytes_array(sigs)
                .finish(),
            // The contract takes no successor argument: it hashes the one
            // it holds. The address in the payload was bound into the
            // digest and checked against the chain by the session.
            GovernancePayload::FinalizeMigration { .. } => Calldata::new(SIG_FINALIZE_MIGRATION)
                .word(nonce)
                .word(expiry)
                .bytes_array(sigs)
                .finish(),
        })
    }
}

/// Which minimums an operator chose to change, if any.
///
/// Absent means PRESERVE what the chain currently holds. That default is
/// the important one: `setLimits` replaces the whole struct, so a tool
/// that defaulted a minimum to zero — or to any figure of its own — would
/// silently rewrite a value nobody asked about, under a signature that
/// covered it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MinimumOverrides {
    pub inbound_min: Option<RobinhoodAtomic>,
    pub outbound_min: Option<RobinhoodAtomic>,
    pub protected_min_reserve: Option<RobinhoodAtomic>,
}

impl MinimumOverrides {
    pub fn is_empty(&self) -> bool {
        self.inbound_min.is_none()
            && self.outbound_min.is_none()
            && self.protected_min_reserve.is_none()
    }
}

/// The limit set that `[robinhood.policy]` implies, given what the chain
/// currently holds.
///
/// # Where every field comes from
///
/// | field | source |
/// |---|---|
/// | `inboundMax`, `outboundMax` | [`RobinhoodPolicyBinding::per_transfer_limit`] — the configured `per_transfer_limit`, widened |
/// | `inboundRollingLimit`, `outboundRollingLimit` | [`RobinhoodPolicyBinding::expected_onchain_rolling_limit`] — HALF the configured `rolling_daily_limit`, because the contract's window is a fixed bucket whose reachable worst case is 2x |
/// | `inboundMin`, `outboundMin`, `protectedMinReserve` | the CURRENT on-chain values, unless explicitly overridden |
///
/// There is deliberately no default, fallback or compiled-in figure for
/// any of them. A policy this service has not been configured with is not
/// a policy it can propose.
///
/// The halving and its exact-divisibility requirement live in
/// [`RobinhoodPolicyBinding::new`], which refuses an odd
/// `rolling_daily_limit` outright — so an unhalvable policy never reaches
/// this function.
pub fn limits_from_policy(
    binding: &RobinhoodPolicyBinding,
    current: &BridgeLimits,
    overrides: MinimumOverrides,
) -> Result<BridgeLimits, GovernanceError> {
    // Two SEPARATE maxima (docs/40, "SOURCE TRANSFER LIMIT vs DESTINATION
    // PAYOUT CAP"): `inboundMax` is the user-facing deposit ceiling,
    // `outboundMax` the destination settlement capacity. One bucket for
    // both rolling windows, already proven to cover the larger maximum.
    let rolling = binding.expected_onchain_rolling_limit().to_u256();
    let proposed = BridgeLimits {
        inbound_min: overrides
            .inbound_min
            .map(RobinhoodAtomic::to_u256)
            .unwrap_or(current.inbound_min),
        inbound_max: binding.inbound_max().to_u256(),
        inbound_rolling_limit: rolling,
        outbound_min: overrides
            .outbound_min
            .map(RobinhoodAtomic::to_u256)
            .unwrap_or(current.outbound_min),
        outbound_max: binding.outbound_max().to_u256(),
        outbound_rolling_limit: rolling,
        protected_min_reserve: overrides
            .protected_min_reserve
            .map(RobinhoodAtomic::to_u256)
            .unwrap_or(current.protected_min_reserve),
    };
    validate_limits(&proposed)?;
    Ok(proposed)
}

/// `_validateLimits`, re-stated so a proposal that would revert is
/// refused before a single signer is asked to look at it.
///
/// This is a SECOND implementation of a contract rule, which this
/// codebase generally refuses. It is justified here on the same ground
/// the preflight checks are: the contract remains the enforcer, and this
/// never authorizes anything the contract would reject — it only declines
/// earlier, with a message naming the field. A drift can therefore only
/// make this stricter than the chain, never looser.
pub fn validate_limits(limits: &BridgeLimits) -> Result<(), GovernanceError> {
    let fields: [(&'static str, EvmU256); 7] = [
        ("inboundMin", limits.inbound_min),
        ("inboundMax", limits.inbound_max),
        ("inboundRollingLimit", limits.inbound_rolling_limit),
        ("outboundMin", limits.outbound_min),
        ("outboundMax", limits.outbound_max),
        ("outboundRollingLimit", limits.outbound_rolling_limit),
        ("protectedMinReserve", limits.protected_min_reserve),
    ];
    let mut values = [0u128; 7];
    for (i, (field, value)) in fields.iter().enumerate() {
        let raw = value
            .try_to_u128()
            .map_err(|_| GovernanceError::LimitTooLarge { field })?;
        if raw % CANONICAL_SCALE != 0 {
            return Err(GovernanceError::NotCanonical { field, value: raw });
        }
        values[i] = raw;
    }
    let [inbound_min, inbound_max, inbound_rolling, outbound_min, outbound_max, outbound_rolling, _protected] =
        values;

    if inbound_min == 0 {
        return Err(GovernanceError::ZeroLimit {
            field: "inboundMin",
        });
    }
    if outbound_min == 0 {
        return Err(GovernanceError::ZeroLimit {
            field: "outboundMin",
        });
    }
    // Not a contract rule, an OPERATOR rule: the contract permits a zero
    // max only in the sense that `min > max` catches it first. Naming it
    // separately makes the refusal readable.
    if inbound_max == 0 {
        return Err(GovernanceError::ZeroLimit {
            field: "inboundMax",
        });
    }
    if outbound_max == 0 {
        return Err(GovernanceError::ZeroLimit {
            field: "outboundMax",
        });
    }
    if inbound_min > inbound_max {
        return Err(GovernanceError::MinAboveMax {
            direction: "inbound",
            min: inbound_min,
            max: inbound_max,
        });
    }
    if outbound_min > outbound_max {
        return Err(GovernanceError::MinAboveMax {
            direction: "outbound",
            min: outbound_min,
            max: outbound_max,
        });
    }
    if inbound_rolling < inbound_max {
        return Err(GovernanceError::RollingBelowMax {
            direction: "inbound",
            rolling: inbound_rolling,
            max: inbound_max,
        });
    }
    if outbound_rolling < outbound_max {
        return Err(GovernanceError::RollingBelowMax {
            direction: "outbound",
            rolling: outbound_rolling,
            max: outbound_max,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
