//! The USER/SOURCE-SIDE transfer limits: the smallest and the largest
//! GROSS amount a user may hand this bridge, on any route.
//!
//! # SOURCE TRANSFER LIMIT vs DESTINATION PAYOUT CAP (2026-09-20)
//!
//! Two different things, stated in two different places, deliberately:
//!
//! - **The source transfer limit** is THIS module: 100 GLC minimum,
//!   [`SOURCE_MAXIMUM_CANONICAL`] (50 000 GLC) maximum, on what the user
//!   SENDS. It is the product's economic rule, it is the figure a UI caps
//!   its entry at (`RouteView::max_transfer_atomic`), and it does not move
//!   with any rate.
//! - **A destination payout cap** is a chain's own per-transfer ceiling
//!   on what a settlement may pay in one release (the Solana program's
//!   `per_transfer_limit`, the Robinhood contract's `outboundMax`). Under
//!   the elastic bridge rate the payout for a 50 000 GLC transfer is
//!   whatever the rate makes it — ~830 000 GLC on Solana at 16.6× is
//!   correct, intended bridge behaviour — so the cap is a SETTLEMENT
//!   CAPACITY figure the operators size to permit every legitimate payout
//!   a ≤ 50 000 GLC transfer can produce. It is never turned into a user
//!   limit: docs/40-destination-bound-admission.md refuses, fail-closed,
//!   a transfer the destination genuinely cannot settle in one release,
//!   and reports the destination's capacity separately
//!   (`RouteView::destination_admissible_atomic`), but the published user
//!   maximum is this module's figure and nothing smaller.
//!
//! # The rule, stated once
//!
//! **A source-side gross of exactly 100 GLC is valid on every route, and
//! anything below it is not.** The bridge fee is deducted AFTER this
//! check, so a 100 GLC transfer at 300 bps delivers 97 GLC and is
//! correct — a destination figure below 100 GLC is the expected outcome
//! of the policy, never evidence of a violation of it.
//!
//! # Why this module has to exist
//!
//! Before it, this service enforced no minimum at all. Every floor lived
//! on a chain, and the two kinds of floor are not the same rule:
//!
//! | Leg | Enforced by | Applied to |
//! |---|---|---|
//! | Solana deposit | `limits.rs::enforce_transfer_amount` in `deposit_to_reserve` | the GROSS deposited |
//! | Solana release | the same helper, in `release_from_reserve` | the NET released |
//! | Robinhood deposit | `GlcRobinhoodBridge.deposit`'s `inboundMin` | the GROSS deposited |
//! | Robinhood payout | `executePayout`'s `outboundMin` | the NET paid out |
//!
//! A NET floor is not a statement about what a user may type — it moves
//! with the fee. Deriving a user-facing minimum from one is what produced
//! the "102.061856 GLC" entry floor: the Solana program's 99 GLC NET
//! floor grossed up at 300 bps. The number was correct and the rule was
//! wrong, and it silently drifted every time a fee changed.
//!
//! So the source-side minimum is stated HERE, as a policy constant, and
//! the chain floors are demoted to what they actually are: enforcement
//! backstops that must not bind the policy. Whether they do is a checkable
//! property, not an assumption — see [`required_destination_floor`] and
//! `crate::min_transfer::preflight`.
//!
//! # This is an ADDITIONAL refusal, never a permission
//!
//! Nothing here relaxes a chain check. Every on-chain floor, ceiling,
//! rolling window and reserve invariant stands exactly as before, and a
//! transfer must satisfy this module AND all of them. The only thing that
//! changes is that this service now refuses a sub-minimum transfer itself,
//! at the earliest point it can, instead of letting it reach a chain that
//! reverts it.
//!
//! # Where it is enforced, and why "refuse" means two different things
//!
//! On a route this service originates (`GlcToSol`, `GlcToRhn`), the
//! request is created here, so a sub-minimum amount is refused outright
//! and nothing moves.
//!
//! On a route whose source leg is a deposit the USER already made on a
//! chain (`SolToGlc`, `RhnToGlc`, `SolToRhn`, `RhnToSol`), the value has
//! already moved irreversibly by the time this service sees it. Refusing
//! such a deposit cannot mean discarding it: it is parked for manual
//! review with an explicit reason, reserves nothing, and stays refundable
//! — the same discipline `crate::robinhood::fold` already applies to an
//! undeliverable destination or an unrepresentable amount.

use crate::amount_conversion::{compute_fee_at_bps, CanonicalAtomic, GOLDCOIN_DECIMALS};
use crate::routes::Route;

#[cfg(test)]
mod tests;

/// Whole GLC in the ledger's canonical 8-decimal unit.
const CANONICAL_SCALE: u64 = 10u64.pow(GOLDCOIN_DECIMALS);

/// **The policy.** The smallest gross a user may bridge, canonical 8dp.
///
/// One number for every route, deliberately. A per-route minimum would be
/// a commercial lever nobody asked for and a second thing to keep in sync
/// with four chains' worth of floors; a single figure is the whole rule
/// and can be stated to a user as one sentence.
///
/// A compiled-in constant rather than config: it is the product's own
/// definition of "a transfer", not a per-deployment tuning knob, and a
/// deployment that could lower it could admit transfers whose destination
/// figure rounds to nothing on a 6-decimal mint. Raising or lowering it is
/// a reviewable edit here, exactly like [`crate::fees::MAX_FEE_BPS`].
pub const SOURCE_MINIMUM_CANONICAL: CanonicalAtomic = CanonicalAtomic(100 * CANONICAL_SCALE);

/// **The policy, upper end.** The largest gross a user may bridge,
/// canonical 8dp: 50 000 GLC on every route — the user-facing transfer
/// maximum (founder decision, 2026-09-20), stated here as a policy
/// constant for exactly the reasons [`SOURCE_MINIMUM_CANONICAL`] is.
///
/// This is a SOURCE figure. It is not derived from, and is never reduced
/// by, any destination chain's per-transfer payout cap — see the module
/// docs. A chain's own deposit ceiling (the Solana program's
/// `per_transfer_limit` on `deposit_to_reserve`, the Robinhood contract's
/// `inboundMax`) can sit BELOW this figure, in which case the chain
/// refuses the deposit itself; the published maximum for such a route is
/// the smaller of the two (`api::published_max_transfer`).
pub const SOURCE_MAXIMUM_CANONICAL: CanonicalAtomic = CanonicalAtomic(50_000 * CANONICAL_SCALE);

/// The source-side gross maximum for `route`, canonical 8dp. One figure
/// for every route today, through one function, exactly as
/// [`source_minimum`].
pub fn source_maximum(route: Route) -> CanonicalAtomic {
    let _ = route;
    SOURCE_MAXIMUM_CANONICAL
}

/// Why a gross amount exceeds the source transfer limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MaxTransferError {
    #[error(
        "{route}: {gross} is above the {maximum} maximum transfer (canonical 8dp). This is the \
         bridge's limit on the amount SENT; the amount that arrives is the bridge rate's business"
    )]
    AboveSourceMaximum {
        route: &'static str,
        gross: u64,
        maximum: u64,
    },
}

/// The source-maximum admission check. `Ok(())` means `gross` is within
/// the policy ceiling for `route` — it says nothing about any
/// destination cap, window, reserve or pause.
pub fn enforce_source_maximum(
    route: Route,
    gross: CanonicalAtomic,
) -> Result<(), MaxTransferError> {
    enforce_source_maximum_at(route, gross, source_maximum(route))
}

/// The same check against an explicitly supplied ceiling — the twin of
/// [`enforce_source_minimum_at`], for the same test-only reason.
pub fn enforce_source_maximum_at(
    route: Route,
    gross: CanonicalAtomic,
    maximum: CanonicalAtomic,
) -> Result<(), MaxTransferError> {
    if gross.0 > maximum.0 {
        return Err(MaxTransferError::AboveSourceMaximum {
            route: route.as_str(),
            gross: gross.0,
            maximum: maximum.0,
        });
    }
    Ok(())
}

/// The source-side gross minimum for `route`, canonical 8dp.
///
/// Takes a route and ignores it today. That is not an oversight: every
/// caller asks a per-route question, and answering it through one function
/// means the day a route needs a different floor there is exactly one
/// place to say so — instead of six call sites that each inlined the
/// constant and now disagree.
pub fn source_minimum(route: Route) -> CanonicalAtomic {
    let _ = route;
    SOURCE_MINIMUM_CANONICAL
}

/// Why a gross amount is not an acceptable transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MinTransferError {
    #[error(
        "{route}: {gross} is below the {minimum} minimum transfer (canonical 8dp). The bridge fee \
         is deducted after this check, so the amount that arrives is smaller than this and that \
         is expected — but the amount SENT may not be"
    )]
    BelowSourceMinimum {
        route: &'static str,
        gross: u64,
        minimum: u64,
    },
}

/// The one admission check. `Ok(())` means `gross` clears the policy
/// floor for `route` — it says nothing about any ceiling, window, reserve
/// or pause, each of which is checked by the layer that owns it.
pub fn enforce_source_minimum(
    route: Route,
    gross: CanonicalAtomic,
) -> Result<(), MinTransferError> {
    enforce_source_minimum_at(route, gross, source_minimum(route))
}

/// The same check against an explicitly supplied floor.
///
/// Exists for one reason: `BridgeApi` carries its floor as a field so its
/// own tests can opt down to the tiny fixture amounts they were written
/// with (see `BridgeApi::with_source_minimum_for_tests`), and routing
/// that through the same comparison keeps the error shape, the wording
/// and the boundary identical on both paths. Production never calls this
/// with anything but [`SOURCE_MINIMUM_CANONICAL`], because nothing
/// production-reachable can put another value in that field.
///
/// Prefer [`enforce_source_minimum`] anywhere a floor is not already
/// being carried — it cannot be handed the wrong one.
pub fn enforce_source_minimum_at(
    route: Route,
    gross: CanonicalAtomic,
    minimum: CanonicalAtomic,
) -> Result<(), MinTransferError> {
    if gross.0 < minimum.0 {
        return Err(MinTransferError::BelowSourceMinimum {
            route: route.as_str(),
            gross: gross.0,
            minimum: minimum.0,
        });
    }
    Ok(())
}

/// The largest a route's DESTINATION-side chain floor may be for the
/// policy to be deliverable: the net of a minimum transfer at `fee_bps`.
///
/// # What this is for
///
/// A destination floor above this value silently overrides the policy. A
/// user hands over exactly 100 GLC, this service accepts it, the fee comes
/// off, and the chain refuses to deliver the remainder — on the two
/// Goldcoin-sourced routes after the deposit is already made. The figure
/// is therefore a launch gate, not a diagnostic.
///
/// Returns the floor in canonical units; each chain's binding converts it
/// into that chain's own precision (6-decimal mint, 18-decimal token)
/// before comparing.
///
/// Note that it moves with the fee, which is exactly why the POLICY does
/// not: at 300 bps a 100 GLC minimum needs a destination floor of 97 GLC
/// or lower, at 600 bps it needs 94 or lower. A deployment that raises a
/// route's fee without lowering that route's destination floor breaks the
/// policy, and this function is what a preflight uses to catch it.
pub fn required_destination_floor(fee_bps: u64) -> Result<CanonicalAtomic, MinTransferPolicyError> {
    compute_fee_at_bps(SOURCE_MINIMUM_CANONICAL, fee_bps)
        .map(|breakdown| breakdown.net)
        .map_err(|source| MinTransferPolicyError::Fee { fee_bps, source })
}

/// A policy that cannot be expressed at all, as opposed to one a chain
/// currently disagrees with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MinTransferPolicyError {
    #[error("computing the net of a minimum transfer at {fee_bps} bps: {source}")]
    Fee {
        fee_bps: u64,
        #[source]
        source: crate::amount_conversion::ConversionError,
    },
}

/// One route's destination-side floor, as some chain actually holds it,
/// measured against what the policy needs it to be.
///
/// Constructed by each chain's own binding (which knows that chain's
/// precision and which of its fields governs the route) and reported by
/// preflight. Deliberately carries the numbers rather than a verdict
/// string: an operator fixing this needs the value to set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DestinationFloorCheck {
    pub route: Route,
    /// The rate this route prices at — the reason `required` is what it is.
    pub fee_bps: u64,
    /// The net of a minimum transfer at `fee_bps`; the largest the chain's
    /// floor may be.
    pub required_at_most: CanonicalAtomic,
    /// What the chain holds, converted to canonical units.
    pub chain_floor: CanonicalAtomic,
}

impl DestinationFloorCheck {
    /// Whether the chain's floor lets a minimum transfer be delivered.
    pub fn is_satisfied(&self) -> bool {
        self.chain_floor.0 <= self.required_at_most.0
    }

    /// The operator-facing sentence, naming the value to set. Returns
    /// `None` when the floor is fine, so a caller can collect only the
    /// failures without formatting the rest.
    pub fn violation(&self) -> Option<String> {
        if self.is_satisfied() {
            return None;
        }
        Some(format!(
            "{}: the destination chain's minimum is {} (canonical 8dp) but this route prices at \
             {} bps, so a {} minimum transfer delivers only {}. The chain would refuse to deliver \
             a policy-minimum transfer. Lower that chain's floor to {} or below",
            self.route.as_str(),
            self.chain_floor.0,
            self.fee_bps,
            SOURCE_MINIMUM_CANONICAL.0,
            self.required_at_most.0,
            self.required_at_most.0,
        ))
    }
}
