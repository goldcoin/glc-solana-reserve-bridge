//! Folding a FINALIZED Robinhood deposit observation into exactly one
//! `bridge_requests` row.
//!
//! # The one job, stated precisely
//!
//! A `DepositCreated` event that this service has observed, decoded,
//! recorded, and watched reach the configured confirmation depth is an
//! irreversible transfer of a user's GLC into the custody contract. From
//! that moment the bridge owes the user a payout on the route's
//! destination network (Goldcoin for `RhnToGlc`, Solana for `RhnToSol`)
//! OR a Robinhood refund — never nothing, and never both. This module is
//! where that obligation enters the ledger: [`fold_observation`] for
//! `RhnToGlc`, [`fold_observation_to_solana`] for `RhnToSol`.
//!
//! # Why folding happens even when the route is disabled
//!
//! This is the most important design decision in the module and it is
//! deliberately the opposite of what "the route is off" suggests.
//!
//! A deposit that has already landed on-chain cannot be un-landed by a
//! flag on this side. If a disabled route meant "do not fold", then
//! turning a route off — or a deployment simply not being configured for
//! it yet — would leave real, irreversible deposits with no ledger row,
//! no reserve accounting, no operator visibility and no refund path. The
//! money would be in the contract and nothing in this service would know.
//!
//! So a finalized deposit is ALWAYS folded, and the route gate governs
//! what happens NEXT: a request folded while the route is closed lands in
//! `ManualReview`, holds no reserve, pays out nothing, and is visible to
//! an operator with an explicit reason. When the route later opens it can
//! be resumed through the existing manual-review resume path, or refunded
//! through the Robinhood refund path — both of which are deliberate acts.
//!
//! That is the same posture `Ledger::fold_sol_deposit` already takes for
//! a Solana deposit that arrives while the Goldcoin reserve is paused: the
//! deposit is recorded, parked, and made visible, rather than dropped.
//!
//! # Amount handling
//!
//! The event carries BOTH the 18-decimal `amount` and the contract's own
//! `canonicalAmount`, and the Phase E decoder already cross-checked that
//! the second is exactly the first divided by `CANONICAL_SCALE`. This
//! module re-derives the canonical amount from the 18-decimal word a
//! second time, through
//! [`crate::amount_conversion::robinhood::RobinhoodAtomic::to_canonical`],
//! and refuses on any disagreement.
//!
//! Deriving it again is not redundant caution about the decoder: it is
//! what makes the exactness rule apply at the moment the amount becomes
//! MONEY IN A LEDGER rather than at the moment it was read off a wire. An
//! amount that is not an exact multiple of 10^10 has no canonical
//! representation, so it cannot be admitted — and because the contract
//! refuses such a deposit on-chain (`_requireCanonicalAmount`), observing
//! one means the contract and this service disagree about what was
//! deposited, which is not a rounding decision to make.
//!
//! # Fee
//!
//! The normal bridge fee policy, in canonical units, through the one fee
//! engine every route uses
//! ([`crate::amount_conversion::compute_fee_at_bps`]). There is still no
//! second Robinhood fee PATH and there must not be one — a second way of
//! computing what a user is owed is a second thing that can be wrong.
//!
//! What is route-specific is the RATE, and only the rate: it is a
//! parameter of each fold rather than a constant read inside it, so a
//! route launching under different commercial terms changes one number
//! rather than adding an arithmetic path. The caller supplies it from
//! the per-route table ([`crate::fees::RouteFees`]), resolved once at
//! config load.
//!
//! The rate is snapshotted onto the request, and every later step settles
//! at THAT snapshot, so changing the configured rate cannot re-price
//! anything already in flight.

use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::amount_conversion::CanonicalAtomic;
use crate::evm::EvmU256;
use crate::ledger::{Ledger, LedgerError, RobinhoodObservationRow};
use crate::routes::Route;

/// Why one observation could not be folded.
///
/// Every variant is a refusal to create a request, never a silent skip:
/// a finalized deposit that cannot be folded is a condition an operator
/// has to see.
#[derive(Debug, thiserror::Error)]
pub enum FoldError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(
        "observation {obligation_index}'s 18-decimal amount cannot be represented exactly in \
         canonical units: {detail}. The contract refuses a non-canonical deposit on-chain, so \
         observing one means this service and the contract disagree about what was deposited"
    )]
    NotCanonical {
        obligation_index: u64,
        detail: String,
    },
    #[error(
        "observation {obligation_index} records canonicalAmount {recorded} but its 18-decimal \
         amount scales down to {derived} — two independently recorded facts about one deposit \
         disagree"
    )]
    AmountDisagreement {
        obligation_index: u64,
        recorded: u64,
        derived: u64,
    },
    #[error("observation {obligation_index}: computing the bridge fee failed: {detail}")]
    Fee {
        obligation_index: u64,
        detail: String,
    },
    #[error(
        "observation {obligation_index} is on route {route}, which this fold does not serve: \
         RhnToGlc folds through `fold_observation`, RhnToSol through `fold_observation_to_solana`"
    )]
    UnsupportedRoute {
        obligation_index: u64,
        route: &'static str,
    },
    #[error(
        "observation {obligation_index} has finality {finality}, but only a FINAL observation may \
         be folded — a provisional deposit can still be reorged away"
    )]
    NotFinal {
        obligation_index: u64,
        finality: &'static str,
    },
    #[error(
        "observation {obligation_index}'s destination payload is not a usable Goldcoin address: \
         {detail}. The deposit is real and irreversible; it must be refunded on Robinhood rather \
         than paid out to a guess"
    )]
    UndeliverableDestination {
        obligation_index: u64,
        detail: String,
    },
    #[error(
        "observation {obligation_index}'s net entitlement of {net_canonical} canonical unit(s) \
         cannot be represented exactly in the Solana reserve mint's {solana_decimals}-decimal \
         unit: {detail}. Nothing is rounded; the deposit is real and must be refunded on \
         Robinhood rather than paid out at a different amount"
    )]
    UndeliverableAmount {
        obligation_index: u64,
        net_canonical: u64,
        solana_decimals: u8,
        detail: String,
    },
}

/// What [`fold_observation`] did with one observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldOutcome {
    /// A new request was created and is eligible to pay out: the route is
    /// open and the destination reserve had capacity.
    FoldedFinalized { request_id: i64 },
    /// A new request was created and PARKED in `ManualReview`. The
    /// deposit is recorded and visible; nothing is reserved and nothing
    /// will pay out until a human resumes or refunds it.
    FoldedManualReview { request_id: i64 },
    /// This observation already has a request. The normal, expected
    /// result of a re-tick or a restart.
    AlreadyFolded { request_id: i64 },
}

impl FoldOutcome {
    pub fn request_id(self) -> i64 {
        match self {
            FoldOutcome::FoldedFinalized { request_id }
            | FoldOutcome::FoldedManualReview { request_id }
            | FoldOutcome::AlreadyFolded { request_id } => request_id,
        }
    }
}

/// The canonical amounts one observation resolves to, with every
/// exactness rule applied.
///
/// Split out from the fold itself so the arithmetic can be tested against
/// adversarial values without a database, and so the fold reads as the
/// admission decision it is rather than as arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldAmounts {
    pub gross_canonical: u64,
    pub fee_bps: u64,
    pub fee_canonical: u64,
    pub net_canonical: u64,
    /// The bridge quote the three figures above were derived from —
    /// struck at the fold, which is the lock for a Robinhood-sourced
    /// deposit (docs/38-elastic-bridge-rate.md). `None` when the book
    /// refused to quote (feed unavailable / stale / warming up): the
    /// figures above are then the unit-rate RECORD of the deposit and
    /// `rate_park` names the reason the row is parked.
    pub quote: Option<crate::bridge_rate::BridgeQuote>,
    /// The ManualReview reason the bridge rate imposes on this fold, if
    /// any: a band breach (with `quote` locked) or a refused quote.
    pub rate_park: Option<&'static str>,
}

impl FoldAmounts {
    /// The ledger's amounts for a fold, at `net_destination_atomic` in
    /// the destination's own unit.
    fn request_amounts(&self, net_destination_atomic: u64) -> crate::ledger::RequestAmounts {
        crate::ledger::RequestAmounts {
            gross_atomic: self.gross_canonical,
            fee_bps: self.fee_bps,
            fee_atomic: self.fee_canonical,
            net_atomic: self.net_canonical,
            net_destination_atomic,
            quote: self.quote,
        }
    }
}

/// The Phase 2A book every fold without an explicit one strikes from: the
/// fixed unit rate at the default quote lifetime — exactly what the
/// daemon's own book answers. [`fold_observation`] and
/// [`fold_observation_to_solana`] use it; production paths pass their
/// configured book to the `_with_rate_book` forms.
fn default_rate_book() -> crate::bridge_rate::RateBook {
    crate::bridge_rate::RateBook::fixed_unit(crate::bridge_rate::DEFAULT_QUOTE_LIFETIME_SECS)
}

/// Resolves and cross-checks one observation's amounts at `fee_bps`.
///
/// `fee_bps` is the rate this chain's approved policy prices at. It is
/// passed in rather than read from a constant so that one chain's
/// commercial terms cannot become another's; it is still validated
/// against the configurable range ([`crate::fees::MIN_FEE_BPS`]..=
/// [`crate::fees::MAX_FEE_BPS`]) inside
/// `compute_fee_at_bps`, so a rate the protocol does not know fails
/// closed here rather than producing a request that could never settle.
pub fn resolve_amounts(
    observation: &RobinhoodObservationRow,
    fee_bps: u64,
    rate_book: &crate::bridge_rate::RateBook,
    now: i64,
    destination_scale: u64,
) -> Result<FoldAmounts, FoldError> {
    let obligation_index = observation.observation.obligation_index;

    // The 18-decimal word, narrowed through the one conversion that
    // enforces exactness. `RobinhoodAtomic::try_from_u256` additionally
    // refuses a word above `u128::MAX` rather than truncating it.
    let robinhood = RobinhoodAtomic::try_from_u256(EvmU256::from_be_bytes(
        observation.observation.amount_robinhood_atomic,
    ))
    .map_err(|e| FoldError::NotCanonical {
        obligation_index,
        detail: e.to_string(),
    })?;
    let derived = robinhood
        .to_canonical()
        .map_err(|e| FoldError::NotCanonical {
            obligation_index,
            detail: e.to_string(),
        })?;

    // The contract emitted `canonicalAmount` as its own field and the
    // decoder already checked it against the 18-decimal word. Checking it
    // AGAIN here is what makes a single stored number trustworthy at the
    // moment it becomes an entitlement: two independent derivations of
    // one value, agreeing.
    if derived.0 != observation.observation.amount_canonical_atomic {
        return Err(FoldError::AmountDisagreement {
            obligation_index,
            recorded: observation.observation.amount_canonical_atomic,
            derived: derived.0,
        });
    }

    // The normal fee policy at this chain's approved rate — the same fee
    // engine every other route uses. The snapshot is stored on the
    // request and every later step settles at THAT rate, so a rate change
    // mid-flight cannot alter what this user is owed.
    //
    // Struck as a bridge quote at THIS route's rate
    // (docs/38-elastic-bridge-rate.md); the fee rule inside it is the
    // same one every other route uses.
    let pricing = crate::bridge_rate::price_final_deposit(
        rate_book,
        observation.observation.route,
        CanonicalAtomic(derived.0),
        fee_bps,
        now,
        destination_scale,
    )
    .map_err(|e| FoldError::Fee {
        obligation_index,
        detail: e.to_string(),
    })?;
    let breakdown = pricing.breakdown;

    Ok(FoldAmounts {
        gross_canonical: derived.0,
        fee_bps: breakdown.fee_bps,
        fee_canonical: breakdown.fee.0,
        net_canonical: breakdown.net.0,
        quote: pricing.quote,
        rate_park: pricing.park,
    })
}

/// Validates that an observation's opaque destination payload is a
/// Goldcoin address this service can actually pay out to.
///
/// # Why this is checked at fold time and not at payout time
///
/// A destination that cannot be paid out is a deposit that must be
/// REFUNDED, and the sooner that is known the better: parking it in
/// `ManualReview` with an explicit reason at fold time puts it in front
/// of an operator immediately, rather than surfacing as a payout failure
/// after the route opens.
///
/// The contract deliberately does not parse addresses — it stores opaque
/// bytes and lets the route say what they mean — so this is the first
/// point at which anything checks that the bytes are an address at all.
pub fn validate_goldcoin_destination(
    observation: &RobinhoodObservationRow,
    network: crate::goldcoin::address::Network,
) -> Result<String, FoldError> {
    let obligation_index = observation.observation.obligation_index;
    let text = std::str::from_utf8(&observation.observation.destination).map_err(|_| {
        FoldError::UndeliverableDestination {
            obligation_index,
            detail: "the destination payload is not valid UTF-8, so it is not a Base58Check \
                     Goldcoin address"
                .to_string(),
        }
    })?;
    // P2PKH specifically, and on THIS network: the payout builder
    // (`signing::goldcoin_vault`) decodes the recipient the same way and
    // would refuse anything else, so accepting a broader form here would
    // only defer the failure to a point where a reserve reservation had
    // already been taken.
    crate::goldcoin::address::decode_p2pkh(text, network).map_err(|e| {
        FoldError::UndeliverableDestination {
            obligation_index,
            detail: e.to_string(),
        }
    })?;
    Ok(text.to_string())
}

/// Validates that an observation's opaque destination payload is a Solana
/// wallet this service can release to, returning the 32 raw pubkey bytes
/// `bridge_requests.recipient` stores for every Solana-bound request.
///
/// Two encodings are accepted, and they are structurally unconfusable:
/// the 32 raw bytes of the pubkey, or its base58 text (43–44 ASCII
/// characters — a 32-byte payload can never be a valid base58 spelling
/// of a 32-byte key, and a 43-byte one can never be a raw key). Anything
/// else is undeliverable. No address is ever guessed: the same
/// fold-time-not-payout-time reasoning as
/// [`validate_goldcoin_destination`] applies.
pub fn validate_solana_destination(
    observation: &RobinhoodObservationRow,
) -> Result<[u8; 32], FoldError> {
    let obligation_index = observation.observation.obligation_index;
    let payload = &observation.observation.destination;
    if let Ok(raw) = <[u8; 32]>::try_from(payload.as_slice()) {
        return Ok(raw);
    }
    let text = std::str::from_utf8(payload).map_err(|_| FoldError::UndeliverableDestination {
        obligation_index,
        detail: "the destination payload is neither 32 raw bytes nor valid UTF-8, so it is not \
                 a Solana pubkey in either accepted spelling"
            .to_string(),
    })?;
    text.parse::<solana_sdk::pubkey::Pubkey>()
        .map(|p| p.to_bytes())
        .map_err(|e| FoldError::UndeliverableDestination {
            obligation_index,
            detail: format!("the destination payload is not a base58 Solana pubkey: {e}"),
        })
}

/// Folds one FINAL `RhnToGlc` observation into a bridge request.
///
/// Idempotent by the ledger's own unique indexes — the durable
/// `(source_chain, source_contract, source_obligation_index)` identity
/// and the `folded_request_id` link — not by a prior read: two ticks
/// racing each other both attempt the insert and exactly one wins.
pub fn fold_observation(
    ledger: &mut Ledger,
    observation: &RobinhoodObservationRow,
    network: crate::goldcoin::address::Network,
    fee_bps: u64,
    // The source-side floor to admit against. Production passes
    // `crate::min_transfer::SOURCE_MINIMUM_CANONICAL`; it is a parameter
    // rather than a direct read of that constant for the same reason
    // `BridgeApi` carries one as a field — most of this module's tests
    // are about finality, idempotency, destination validation or rate
    // limiting, and were written against deposits far below the policy
    // floor.
    source_minimum: CanonicalAtomic,
    route_open: bool,
    now: i64,
) -> Result<FoldOutcome, FoldError> {
    fold_observation_with_rate_book(
        ledger,
        observation,
        network,
        fee_bps,
        &default_rate_book(),
        source_minimum,
        route_open,
        now,
    )
}

/// The source-side transfer limits, both ends, as one park note: below
/// the floor or above the ceiling (`crate::min_transfer`). The floor is
/// the caller's (test-tunable) figure; the ceiling is the policy
/// constant — a deposit the contract accepted above 50 000 GLC is parked,
/// recoverable, exactly as a sub-minimum one, because the policy is on
/// what was SENT and the deposit is already final.
fn source_limit_refusal(
    route: Route,
    gross: CanonicalAtomic,
    source_minimum: CanonicalAtomic,
) -> Result<(), String> {
    crate::min_transfer::enforce_source_minimum_at(route, gross, source_minimum)
        .map_err(|e| format!("below source minimum: {e}"))?;
    crate::min_transfer::enforce_source_maximum(route, gross)
        .map_err(|e| format!("above source maximum: {e}"))?;
    Ok(())
}

/// [`fold_observation`] at an explicit bridge-rate book — the form the
/// settlement loop and deposit recovery call with the daemon's configured
/// book.
#[allow(clippy::too_many_arguments)]
pub fn fold_observation_with_rate_book(
    ledger: &mut Ledger,
    observation: &RobinhoodObservationRow,
    network: crate::goldcoin::address::Network,
    fee_bps: u64,
    rate_book: &crate::bridge_rate::RateBook,
    source_minimum: CanonicalAtomic,
    route_open: bool,
    now: i64,
) -> Result<FoldOutcome, FoldError> {
    let obligation_index = observation.observation.obligation_index;

    if observation.finality != crate::ledger::RobinhoodFinality::Final {
        return Err(FoldError::NotFinal {
            obligation_index,
            finality: observation.finality.as_str(),
        });
    }
    if observation.observation.route != Route::RhnToGlc {
        return Err(FoldError::UnsupportedRoute {
            obligation_index,
            route: observation.observation.route.as_str(),
        });
    }

    // Goldcoin's native atomic unit IS the canonical accounting unit
    // (both 8 decimals), so the destination amount needs no conversion —
    // exactly as for `SolToGlc` — and the destination scale is 1.
    let amounts = resolve_amounts(observation, fee_bps, rate_book, now, 1)?;
    let request_amounts = amounts.request_amounts(amounts.net_canonical);

    // A destination this service cannot pay out to is folded anyway — the
    // deposit is real — but never as payable. It is parked with an
    // explicit reason so the refund path is the obvious next step.
    let destination = match validate_goldcoin_destination(observation, network) {
        Ok(address) => Some(address),
        Err(FoldError::UndeliverableDestination { detail, .. }) => {
            return ledger
                .fold_robinhood_deposit(
                    observation,
                    request_amounts,
                    None,
                    false,
                    Some(&format!("undeliverable destination: {detail}")),
                    now,
                )
                .map_err(FoldError::from);
        }
        Err(other) => return Err(other),
    };

    // Below the source-side minimum. The contract's own `inboundMin` is
    // the first line of defence and currently holds exactly the policy
    // figure, so this should be unreachable — which is precisely why it
    // is checked: a governance action that lowered `inboundMin` would
    // otherwise admit sub-minimum deposits silently, and this service
    // would pay them out.
    //
    // Parked, never dropped. The deposit has already happened and the
    // tokens are in the custody contract; the only honest outcome is a
    // recorded request that reserves nothing and is refundable through
    // the normal path.
    if let Err(refusal) = source_limit_refusal(
        Route::RhnToGlc,
        CanonicalAtomic(amounts.gross_canonical),
        source_minimum,
    ) {
        return ledger
            .fold_robinhood_deposit(
                observation,
                // The amounts are passed through unchanged, exactly as
                // the undeliverable-destination park above passes them:
                // an `RhnToGlc` request's destination figure IS its
                // canonical net (the ledger asserts it), and `payable =
                // false` is what stops any capacity being held — not a
                // zeroed amount, which would be a lie about the deposit.
                request_amounts,
                destination.as_deref().map(str::as_bytes),
                false,
                Some(&refusal),
                now,
            )
            .map_err(FoldError::from);
    }

    // The bridge-rate park (band breach with the quote locked, or a
    // refused quote) ranks after the destination and the floor, exactly
    // as it does on the Solana folds.
    ledger
        .fold_robinhood_deposit(
            observation,
            request_amounts,
            destination.as_deref().map(str::as_bytes),
            route_open,
            amounts.rate_park,
            now,
        )
        .map_err(FoldError::from)
}

/// Folds one FINAL `RhnToSol` observation into a bridge request — the
/// Solana-bound twin of [`fold_observation`].
///
/// # The one extra exactness rule
///
/// The net entitlement is narrowed from canonical (8 decimals) to the
/// Solana reserve mint's live `solana_decimals` through the one
/// conversion that refuses inexactness (`CanonicalAtomic::to_solana`).
/// A net that is not a whole multiple of that scale — which CAN happen
/// even for a canonical-exact deposit, because the fee is computed in
/// canonical units — is never rounded in either direction: the deposit
/// folds parked, with the reason spelled out, and is refunded on
/// Robinhood through the normal refund path. `GET /quote` applies the
/// identical check so a UI can warn before the deposit is made.
///
/// `solana_decimals` is the mint's LIVE value, read by the caller from
/// the mint account on every tick exactly as `solana::indexer` reads it
/// for a `SolToGlc` fold — never a compile-time constant.
pub fn fold_observation_to_solana(
    ledger: &mut Ledger,
    observation: &RobinhoodObservationRow,
    fee_bps: u64,
    // The source-side floor — see `fold_observation`'s parameter of the
    // same name.
    source_minimum: CanonicalAtomic,
    solana_decimals: u8,
    route_open: bool,
    now: i64,
) -> Result<FoldOutcome, FoldError> {
    fold_observation_to_solana_with_rate_book(
        ledger,
        observation,
        fee_bps,
        &default_rate_book(),
        source_minimum,
        solana_decimals,
        route_open,
        now,
    )
}

/// [`fold_observation_to_solana`] at an explicit bridge-rate book — the
/// form the orchestrator and deposit recovery call with the daemon's
/// configured book.
#[allow(clippy::too_many_arguments)]
pub fn fold_observation_to_solana_with_rate_book(
    ledger: &mut Ledger,
    observation: &RobinhoodObservationRow,
    fee_bps: u64,
    rate_book: &crate::bridge_rate::RateBook,
    source_minimum: CanonicalAtomic,
    solana_decimals: u8,
    route_open: bool,
    now: i64,
) -> Result<FoldOutcome, FoldError> {
    let obligation_index = observation.observation.obligation_index;

    if observation.finality != crate::ledger::RobinhoodFinality::Final {
        return Err(FoldError::NotFinal {
            obligation_index,
            finality: observation.finality.as_str(),
        });
    }
    if observation.observation.route != Route::RhnToSol {
        return Err(FoldError::UnsupportedRoute {
            obligation_index,
            route: observation.observation.route.as_str(),
        });
    }

    let amounts = resolve_amounts(
        observation,
        fee_bps,
        rate_book,
        now,
        crate::bridge_rate::destination_scale_for_decimals(solana_decimals),
    )?;

    // The destination first, then the amount: both are parked with their
    // own explicit reason, and a deposit that fails both is reported for
    // the destination — the fact a refund decision rests on.
    let destination = match validate_solana_destination(observation) {
        Ok(pubkey) => pubkey,
        Err(FoldError::UndeliverableDestination { detail, .. }) => {
            return ledger
                .fold_robinhood_deposit(
                    observation,
                    // Nothing is reserved for a park, and no destination
                    // figure exists for a destination that cannot be paid.
                    amounts.request_amounts(0),
                    None,
                    false,
                    Some(&format!("undeliverable destination: {detail}")),
                    now,
                )
                .map_err(FoldError::from);
        }
        Err(other) => return Err(other),
    };

    // The same source-side floor as every other route, before the
    // representability question below: "this deposit is under the
    // minimum" is a more actionable reason for an operator than "its net
    // does not fit six decimals", and a sub-minimum deposit is refused
    // whether or not its net happens to be spellable.
    if let Err(refusal) = source_limit_refusal(
        Route::RhnToSol,
        CanonicalAtomic(amounts.gross_canonical),
        source_minimum,
    ) {
        return ledger
            .fold_robinhood_deposit(
                observation,
                amounts.request_amounts(0),
                Some(&destination),
                false,
                Some(&refusal),
                now,
            )
            .map_err(FoldError::from);
    }

    let net_destination = match CanonicalAtomic(amounts.net_canonical).to_solana(solana_decimals) {
        Ok(solana) => solana.0,
        Err(e) => {
            let refusal = FoldError::UndeliverableAmount {
                obligation_index,
                net_canonical: amounts.net_canonical,
                solana_decimals,
                detail: e.to_string(),
            };
            return ledger
                .fold_robinhood_deposit(
                    observation,
                    amounts.request_amounts(0),
                    Some(&destination),
                    false,
                    Some(&format!("undeliverable amount: {refusal}")),
                    now,
                )
                .map_err(FoldError::from);
        }
    };

    ledger
        .fold_robinhood_deposit(
            observation,
            amounts.request_amounts(net_destination),
            Some(&destination),
            route_open,
            amounts.rate_park,
            now,
        )
        .map_err(FoldError::from)
}

#[cfg(test)]
mod tests;
