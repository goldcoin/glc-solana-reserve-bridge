//! Canonical Goldcoin<->Solana atomic-amount conversion policy (Task 3,
//! docs/18-token-2022-support.md).
//!
//! Goldcoin's native chain uses 8 decimals (Bitcoin-fork convention,
//! docs/goldcoin-rpc-notes.md — verified against a real Goldcoin node).
//! The canonical Solana GLC Token-2022 mint uses 6 decimals, verified
//! read-only against mainnet — but this module never hardcodes that
//! value: callers pass the live `decimals` they read from the mint
//! account, exactly as the on-chain program itself does
//! (`release_from_reserve`/`deposit_to_reserve` both read
//! `reserve_mint.decimals` live rather than trusting a compile-time
//! constant). Both chains represent the SAME underlying asset in
//! DIFFERENT atomic units, and this module is the only place a raw
//! atomic amount is allowed to cross from one chain's units to the
//! other's.
//!
//! # Policy
//!
//! Converting to a chain with MORE decimals (finer-grained units) is
//! always exact: multiply by the scale factor. Converting to a chain with
//! FEWER decimals (coarser-grained units) is exact ONLY when the source
//! amount's low-order digits beyond the destination's precision are all
//! zero; otherwise the amount cannot be represented exactly on the
//! destination side and conversion is REJECTED — never silently rounded
//! or truncated. Rounding down would permanently strand the depositor's
//! entitlement to the remainder inside the reserve; rounding up would
//! create GLC that was never deposited anywhere. Both are exactly the
//! "silently round value in a way that creates or destroys user
//! entitlement" failure Task 3 explicitly forbids — so this module fails
//! closed instead.

use std::cmp::Ordering;

/// Robinhood Chain's 18-decimal atomic unit and its exact conversion to
/// and from [`CanonicalAtomic`]. A third unit alongside [`CanonicalAtomic`]
/// and [`SolanaAtomic`], separated out because it is the only one that
/// does not fit a `u64` — see that module's docs.
pub mod robinhood;

/// Goldcoin's native-chain atomic-unit precision. Fixed by the Goldcoin
/// protocol itself (a Bitcoin fork), unlike the Solana side's decimals,
/// which are read live from the mint and passed into every function here.
pub const GOLDCOIN_DECIMALS: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConversionError {
    #[error(
        "amount {amount} atomic unit(s) at {from_decimals} decimals cannot be represented \
         exactly at {to_decimals} decimals — converting would silently strand/create {remainder} \
         atomic unit(s) of precision the destination chain cannot express"
    )]
    NotExactlyRepresentable {
        amount: u64,
        from_decimals: u32,
        to_decimals: u32,
        remainder: u64,
    },
    #[error("amount {0} overflows u64 when converted to the destination's finer precision")]
    Overflow(u64),
    #[error(
        "recomputed fee breakdown for gross {gross} disagrees with the stored ledger record \
         (stored fee {stored_fee}, recomputed {recomputed_fee}; stored net {stored_net}, \
         recomputed {recomputed_net}) — refusing to settle on an inconsistent accounting record"
    )]
    AccountingMismatch {
        gross: u64,
        stored_fee: u64,
        recomputed_fee: u64,
        stored_net: u64,
        recomputed_net: u64,
    },
    #[error(
        "fee_bps {fee_bps} is above {max} basis points (100%): a fee larger than the gross \
         amount would make the net entitlement negative, which this bridge's unsigned \
         accounting cannot represent and must never be asked to"
    )]
    FeeBpsOutOfRange { fee_bps: u64, max: u64 },
}

/// Converts `amount` from `from_decimals` atomic units to `to_decimals`
/// atomic units, representing the identical real-world GLC quantity.
/// Deterministic; never rounds. See module docs for the exactness policy.
pub fn convert_atomic_amount(
    amount: u64,
    from_decimals: u32,
    to_decimals: u32,
) -> Result<u64, ConversionError> {
    match from_decimals.cmp(&to_decimals) {
        Ordering::Equal => Ok(amount),
        Ordering::Less => {
            // Widening: destination is finer-grained, always exact.
            let scale = 10u64
                .checked_pow(to_decimals - from_decimals)
                .ok_or(ConversionError::Overflow(amount))?;
            amount
                .checked_mul(scale)
                .ok_or(ConversionError::Overflow(amount))
        }
        Ordering::Greater => {
            // Narrowing: destination is coarser-grained; exact only if
            // `amount` is already a multiple of the scale factor.
            let scale = 10u64
                .checked_pow(from_decimals - to_decimals)
                .ok_or(ConversionError::Overflow(amount))?;
            let remainder = amount % scale;
            if remainder != 0 {
                return Err(ConversionError::NotExactlyRepresentable {
                    amount,
                    from_decimals,
                    to_decimals,
                    remainder,
                });
            }
            Ok(amount / scale)
        }
    }
}

/// Converts a Goldcoin-atomic amount (8 decimals) into the equivalent
/// Solana-atomic amount at the mint's live `solana_decimals`. This is the
/// GLC->Solana release direction; today's canonical mint (6 decimals) is
/// a narrowing conversion, so it fails closed on any amount not divisible
/// by 100 rather than losing or fabricating GLC.
pub fn goldcoin_to_solana_atomic(
    goldcoin_atomic: u64,
    solana_decimals: u8,
) -> Result<u64, ConversionError> {
    convert_atomic_amount(
        goldcoin_atomic,
        GOLDCOIN_DECIMALS,
        u32::from(solana_decimals),
    )
}

/// Converts a Solana-atomic amount at the mint's live `solana_decimals`
/// into the equivalent Goldcoin-atomic amount (8 decimals). This is the
/// Solana->Goldcoin deposit direction; today's canonical mint (6
/// decimals) is a widening conversion and always exact.
pub fn solana_to_goldcoin_atomic(
    solana_atomic: u64,
    solana_decimals: u8,
) -> Result<u64, ConversionError> {
    convert_atomic_amount(solana_atomic, u32::from(solana_decimals), GOLDCOIN_DECIMALS)
}

// ------------------------------------------------------------------------
// Canonical accounting unit + bridge fee (fee/reserve-capacity accounting
// pass, docs/20-bridge-fee.md).
//
// Every ledger-level bridge obligation, reserve-capacity figure, gross
// amount, fee amount, and net amount is denominated in ONE canonical unit:
// [`CanonicalAtomic`], 8 decimals — numerically identical to Goldcoin's own
// native atomic unit (`GOLDCOIN_DECIMALS`), the finer of the two real,
// verified chain precisions today. This is what
// `docs/18-token-2022-support.md`'s flagged "reserve-capacity accounting-
// unit gap" resolves to: `bridge_requests`' gross/fee/net columns are
// always canonical, for both directions, never source-chain-native.
//
// [`SolanaAtomic`] is the OTHER unit that ever appears in this accounting
// pipeline: the reserve mint's own live decimals, needed only at the two
// points a canonical amount must become (or came from) a real Solana
// transfer amount. The two types deliberately do not implement any
// arithmetic or comparison against each other — Rust's type system itself
// is what makes `SolanaAtomic(5) < CanonicalAtomic(5)` a compile error
// rather than a silent unit-confusion bug, satisfying the "structurally
// difficult or impossible" requirement without runtime checks.
// ------------------------------------------------------------------------

/// Basis-point denominator: `fee_bps / BPS_DENOMINATOR` is the fee rate.
pub const BPS_DENOMINATOR: u64 = 10_000;

/// The bridge's HISTORICAL default fee rate: exactly 3.00%.
///
/// No longer the rate anything charges. Fees are per ROUTE
/// ([`crate::fees::RouteFees`], the config's `[fees]` table), and every
/// pricing call site resolves its rate by `Route`. This constant survives
/// as exactly one thing: the value the documented MIGRATION FALLBACK gives
/// a route when a config has no `[fees]` section at all, so an unmodified
/// production config keeps pricing identically after the upgrade
/// (`crate::config`'s `resolve_route_fees`).
///
/// The rate that prices a request is snapshotted onto it
/// (`bridge_requests.fee_bps`) at creation/fold time, and that snapshot —
/// never this constant, and never the config's current value — governs the
/// request's validation and settlement from then on
/// ([`verify_fee_breakdown`]), so an in-flight request survives a rate
/// change.
pub const BRIDGE_FEE_BPS: u64 = 300;

/// An amount in the ledger's canonical accounting unit (8 decimals,
/// numerically identical to Goldcoin's own native atomic unit). See the
/// module-level section above.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalAtomic(pub u64);

/// An amount in the Solana reserve mint's own live decimals. Never
/// compared or combined with [`CanonicalAtomic`] directly — convert first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SolanaAtomic(pub u64);

impl CanonicalAtomic {
    pub const ZERO: CanonicalAtomic = CanonicalAtomic(0);

    pub fn checked_add(self, rhs: CanonicalAtomic) -> Result<CanonicalAtomic, ConversionError> {
        self.0
            .checked_add(rhs.0)
            .map(CanonicalAtomic)
            .ok_or(ConversionError::Overflow(self.0))
    }

    pub fn checked_sub(self, rhs: CanonicalAtomic) -> Result<CanonicalAtomic, ConversionError> {
        self.0
            .checked_sub(rhs.0)
            .map(CanonicalAtomic)
            .ok_or(ConversionError::Overflow(self.0))
    }

    /// Converts to the reserve mint's own live decimals. Delegates to
    /// [`goldcoin_to_solana_atomic`] — the one, canonical conversion
    /// implementation; this is a typed wrapper around it, not a second one.
    pub fn to_solana(self, solana_decimals: u8) -> Result<SolanaAtomic, ConversionError> {
        goldcoin_to_solana_atomic(self.0, solana_decimals).map(SolanaAtomic)
    }
}

impl SolanaAtomic {
    pub const ZERO: SolanaAtomic = SolanaAtomic(0);

    pub fn checked_add(self, rhs: SolanaAtomic) -> Result<SolanaAtomic, ConversionError> {
        self.0
            .checked_add(rhs.0)
            .map(SolanaAtomic)
            .ok_or(ConversionError::Overflow(self.0))
    }

    pub fn checked_sub(self, rhs: SolanaAtomic) -> Result<SolanaAtomic, ConversionError> {
        self.0
            .checked_sub(rhs.0)
            .map(SolanaAtomic)
            .ok_or(ConversionError::Overflow(self.0))
    }

    /// Converts to the ledger's canonical unit. Delegates to
    /// [`solana_to_goldcoin_atomic`] — see [`CanonicalAtomic::to_solana`].
    pub fn to_canonical(self, solana_decimals: u8) -> Result<CanonicalAtomic, ConversionError> {
        solana_to_goldcoin_atomic(self.0, solana_decimals).map(CanonicalAtomic)
    }
}

/// A fully reconciled gross/fee/net breakdown, all in canonical units:
/// `gross == fee + net` holds BY CONSTRUCTION (`net` is derived as
/// `gross - fee`, never computed independently), so this type cannot
/// represent an inconsistent state — see [`compute_fee`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeBreakdown {
    pub gross: CanonicalAtomic,
    pub fee_bps: u64,
    pub fee: CanonicalAtomic,
    pub net: CanonicalAtomic,
}

/// Computes the bridge fee for `gross` (canonical units) at the fixed
/// [`BRIDGE_FEE_BPS`] rate. `fee` is floored (rounds toward the user, in
/// the user's favor, never up) via checked integer arithmetic — never
/// floating point. `net = gross - fee` is derived, not independently
/// computed, so `gross == fee + net` is a structural guarantee of
/// [`FeeBreakdown`], not something callers must separately verify.
///
/// A `gross` too small for `BRIDGE_FEE_BPS` of it to matter (e.g. `gross <
/// BPS_DENOMINATOR / BRIDGE_FEE_BPS`) still succeeds here — `fee` is
/// simply `0` and `net == gross`. Whether a given gross amount can
/// actually SETTLE end to end (i.e. whether `net` survives the
/// destination chain's own decimal precision exactly) is a SEPARATE
/// question, checked at the point `net` is converted to the destination
/// chain's native unit (see docs/20-bridge-fee.md's "smallest
/// mathematically valid gross amount" analysis) — `compute_fee` itself
/// never rejects an amount on exactness grounds, only on overflow.
pub fn compute_fee(gross: CanonicalAtomic) -> Result<FeeBreakdown, ConversionError> {
    compute_fee_at_bps(gross, BRIDGE_FEE_BPS)
}

/// [`compute_fee`] at an explicit rate — the variant every real caller
/// uses.
///
/// Two callers, two reasons:
///
/// - **Pricing a NEW request** at the rate its ROUTE is configured with
///   ([`crate::fees::RouteFees::fee_bps`]).
/// - **Re-deriving an EXISTING request's breakdown** from its own
///   `fee_bps` snapshot, which was fixed at creation/fold time and is
///   immutable historical accounting thereafter ([`verify_fee_breakdown`]).
///
/// # The only rate rule left here is arithmetic
///
/// `fee_bps` must be in `0..=`[`BPS_DENOMINATOR`], and that bound is not a
/// policy choice — it is where the arithmetic below stops being able to
/// produce an answer. `fee = floor(gross * fee_bps / 10_000)`, and
/// `net = gross - fee` is computed on `u64`: above 10,000 bps the fee
/// exceeds the gross and the subtraction would underflow, i.e. the net
/// entitlement would be negative, which this bridge's unsigned accounting
/// cannot represent. So the check is stated explicitly and returns
/// [`ConversionError::FeeBpsOutOfRange`], rather than being left to
/// surface as a misleading `Overflow`.
///
/// # What used to be here, and why it is gone
///
/// This function used to additionally refuse any rate outside a
/// compiled-in allowlist of rates the protocol had previously charged.
/// That made a fee a CODE artefact: moving a route to a rate nobody had
/// used before required editing and releasing the binary, even though the
/// value is configuration. Fees are now genuinely config-driven and
/// validated by range alone.
///
/// The fail-closed accounting protection that remains — and it is the one
/// that actually catches a tampered ledger row — is
/// [`verify_fee_breakdown`]: the stored fee and net must reconcile
/// EXACTLY against the stored gross and rate, and the settlement is always
/// built from the freshly recomputed figures rather than the stored ones.
/// A row whose three amounts do not agree is refused whatever rate it
/// claims. See docs/20-bridge-fee.md.
pub fn compute_fee_at_bps(
    gross: CanonicalAtomic,
    fee_bps: u64,
) -> Result<FeeBreakdown, ConversionError> {
    if fee_bps > BPS_DENOMINATOR {
        return Err(ConversionError::FeeBpsOutOfRange {
            fee_bps,
            max: BPS_DENOMINATOR,
        });
    }
    let scaled = gross
        .0
        .checked_mul(fee_bps)
        .ok_or(ConversionError::Overflow(gross.0))?;
    let fee = scaled / BPS_DENOMINATOR; // floor; BPS_DENOMINATOR is a nonzero constant
    let net = gross
        .0
        .checked_sub(fee)
        .ok_or(ConversionError::Overflow(gross.0))?;
    Ok(FeeBreakdown {
        gross,
        fee_bps,
        fee: CanonicalAtomic(fee),
        net: CanonicalAtomic(net),
    })
}

/// Recomputes the fee breakdown for `gross_atomic` from scratch — at the
/// request's own STORED `fee_bps` snapshot, not the current compiled-in
/// rate — and asserts it matches `stored_fee_atomic`/`stored_net_atomic`,
/// the ledger's own persisted record for the request
/// (docs/20-bridge-fee.md's "FAIL CLOSED on accounting inconsistencies"
/// requirement). Every settlement-construction call site (attestation
/// signing, release-instruction building, the Goldcoin payout plan,
/// payout recovery) uses this rather than trusting the stored fee/net
/// columns directly: the RETURNED breakdown is always the freshly
/// recomputed one, never the stored one, so even if the stored figures
/// were somehow tampered with in the database, they are never actually
/// used to build a real settlement — only compared against, and rejected
/// on mismatch.
///
/// Using the stored snapshot rate is what lets an in-flight request
/// created under an earlier fee policy keep settling after the configured
/// rate changes (the production #818 class of bug: a 6%-era request must
/// not be re-judged against 3%).
///
/// **This is the fee-bypass protection.** The stored `fee_bps`,
/// `fee_atomic` and `net_atomic` must reconcile EXACTLY against the stored
/// gross — a row where they do not is refused — and the breakdown this
/// returns is always the freshly recomputed one, so the stored figures are
/// only ever compared against, never used to build a settlement. The rate
/// itself is bounded by arithmetic alone (`0..=`[`BPS_DENOMINATOR`]); it is
/// no longer checked against a compiled-in list of previously-charged
/// rates, because a fee is configuration and must not require a release to
/// change. See docs/20-bridge-fee.md.
pub fn verify_fee_breakdown(
    gross_atomic: u64,
    stored_fee_bps: u64,
    stored_fee_atomic: u64,
    stored_net_atomic: u64,
) -> Result<FeeBreakdown, ConversionError> {
    let fb = compute_fee_at_bps(CanonicalAtomic(gross_atomic), stored_fee_bps)?;
    if fb.fee.0 != stored_fee_atomic || fb.net.0 != stored_net_atomic {
        return Err(ConversionError::AccountingMismatch {
            gross: gross_atomic,
            stored_fee: stored_fee_atomic,
            recomputed_fee: fb.fee.0,
            stored_net: stored_net_atomic,
            recomputed_net: fb.net.0,
        });
    }
    Ok(fb)
}

/// The largest GROSS whose net at `fee_bps` does not exceed `max_net` —
/// the inverse of [`compute_fee_at_bps`], for rendering "up to N GLC"
/// from a net-denominated capacity figure.
///
/// Exact by construction rather than by formula: the closed-form
/// estimate `max_net * 10000 / (10000 - fee_bps)` is only a starting
/// point, because [`compute_fee_at_bps`] floors the fee and so its net is
/// a step function of the gross. The estimate is then walked to the
/// exact boundary against the real function — `net(gross) <= max_net`
/// and `net(gross + 1) > max_net` — so the two can never disagree by a
/// rounding unit. `0` for a zero `max_net`; a `fee_bps` of 100% (no net
/// is ever delivered) also answers `0` rather than dividing by zero.
pub fn max_gross_for_net_at_bps(
    max_net: CanonicalAtomic,
    fee_bps: u64,
) -> Result<CanonicalAtomic, ConversionError> {
    if fee_bps > BPS_DENOMINATOR {
        return Err(ConversionError::FeeBpsOutOfRange {
            fee_bps,
            max: BPS_DENOMINATOR,
        });
    }
    if max_net.0 == 0 || fee_bps == BPS_DENOMINATOR {
        return Ok(CanonicalAtomic(0));
    }
    let net_of =
        |gross: u64| compute_fee_at_bps(CanonicalAtomic(gross), fee_bps).map(|fb| fb.net.0);
    let estimate =
        (max_net.0 as u128) * (BPS_DENOMINATOR as u128) / ((BPS_DENOMINATOR - fee_bps) as u128);
    let mut gross = u64::try_from(estimate).map_err(|_| ConversionError::Overflow(max_net.0))?;
    // Walk down while the estimate delivers too much, then up while the
    // next unit would still fit — each loop moves at most a handful of
    // atomic units, since the estimate is exact up to fee flooring.
    while gross > 0 && net_of(gross)? > max_net.0 {
        gross -= 1;
    }
    while let Some(next) = gross.checked_add(1) {
        match net_of(next) {
            Ok(net) if net <= max_net.0 => gross = next,
            _ => break,
        }
    }
    Ok(CanonicalAtomic(gross))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_decimals_is_the_identity() {
        assert_eq!(convert_atomic_amount(12345, 6, 6), Ok(12345));
    }

    #[test]
    fn widening_multiplies_exactly() {
        // 6 -> 8 decimals: scale by 100.
        assert_eq!(convert_atomic_amount(1, 6, 8), Ok(100));
        assert_eq!(convert_atomic_amount(123_456, 6, 8), Ok(12_345_600));
    }

    #[test]
    fn narrowing_divides_exactly_when_the_remainder_is_zero() {
        // 8 -> 6 decimals: scale by 100. 2.5 GLC at 8 decimals ->
        // 2.5 GLC at 6 decimals.
        assert_eq!(convert_atomic_amount(250_000_000, 8, 6), Ok(2_500_000));
    }

    #[test]
    fn narrowing_rejects_a_nonzero_remainder_rather_than_rounding() {
        // 8 -> 6 decimals: 0.000000_01 GLC (1 atomic unit at 8 decimals)
        // has no exact representation at 6 decimals.
        let err = convert_atomic_amount(1, 8, 6).unwrap_err();
        assert_eq!(
            err,
            ConversionError::NotExactlyRepresentable {
                amount: 1,
                from_decimals: 8,
                to_decimals: 6,
                remainder: 1,
            }
        );
    }

    #[test]
    fn narrowing_rejects_a_partial_remainder_not_just_the_full_scale() {
        // 250_000_001 at 8 decimals is 2.50000001 GLC — the trailing
        // "01" cannot survive a narrowing to 6 decimals.
        let err = convert_atomic_amount(250_000_001, 8, 6).unwrap_err();
        assert_eq!(
            err,
            ConversionError::NotExactlyRepresentable {
                amount: 250_000_001,
                from_decimals: 8,
                to_decimals: 6,
                remainder: 1,
            }
        );
    }

    #[test]
    fn goldcoin_to_solana_matches_the_canonical_six_decimal_mint() {
        // 2.5 GLC: glc_to_atomic gives 250_000_000 at 8 decimals.
        assert_eq!(goldcoin_to_solana_atomic(250_000_000, 6), Ok(2_500_000));
    }

    #[test]
    fn solana_to_goldcoin_matches_the_canonical_six_decimal_mint() {
        assert_eq!(solana_to_goldcoin_atomic(2_500_000, 6), Ok(250_000_000));
    }

    #[test]
    fn goldcoin_to_solana_rejects_dust_finer_than_the_solana_mint_can_hold() {
        // 1 atomic Goldcoin unit (1e-8 GLC) cannot be represented at 6
        // decimals (1e-6 GLC granularity) — must reject, not truncate to 0
        // (which would silently destroy the depositor's entitlement).
        assert!(goldcoin_to_solana_atomic(1, 6).is_err());
    }

    #[test]
    fn round_trip_is_lossless_for_amounts_expressible_in_both_precisions() {
        // Any amount that is a whole multiple of 0.01 GLC (100 atomic
        // units at 8 decimals) round-trips exactly through 6 decimals and
        // back, for every amount up to a reasonable bound — a property
        // check, not just a couple of fixed examples.
        for whole_hundredths in 0u64..=10_000 {
            let goldcoin_atomic = whole_hundredths * 100;
            let solana_atomic = goldcoin_to_solana_atomic(goldcoin_atomic, 6).unwrap();
            let back = solana_to_goldcoin_atomic(solana_atomic, 6).unwrap();
            assert_eq!(back, goldcoin_atomic);
        }
    }

    #[test]
    fn exact_conversion_never_changes_total_value_at_the_boundary_of_u64() {
        // A large, realistic reserve-sized amount that is exactly
        // representable must convert without overflow or loss.
        let goldcoin_atomic = 1_000_000_000_000u64; // 10,000 GLC
        let solana_atomic = goldcoin_to_solana_atomic(goldcoin_atomic, 6).unwrap();
        assert_eq!(solana_atomic, 10_000_000_000);
        assert_eq!(
            solana_to_goldcoin_atomic(solana_atomic, 6).unwrap(),
            goldcoin_atomic
        );
    }

    // --------------------------------------------------------------- fee --

    #[test]
    fn hundred_glc_gross_charges_three_glc_fee() {
        // 100 GLC gross -> 3 GLC fee -> 97 GLC net.
        let gross = CanonicalAtomic(100 * 100_000_000);
        let fb = compute_fee(gross).unwrap();
        assert_eq!(fb.fee, CanonicalAtomic(3 * 100_000_000));
        assert_eq!(fb.net, CanonicalAtomic(97 * 100_000_000));
        assert_eq!(fb.fee_bps, 300);
    }

    #[test]
    fn thousand_glc_gross_charges_thirty_glc_fee() {
        // 1,000 GLC gross -> 30 GLC fee -> 970 GLC net.
        let gross = CanonicalAtomic(1_000 * 100_000_000);
        let fb = compute_fee(gross).unwrap();
        assert_eq!(fb.fee, CanonicalAtomic(30 * 100_000_000));
        assert_eq!(fb.net, CanonicalAtomic(970 * 100_000_000));
    }

    #[test]
    fn twenty_thousand_glc_gross_nets_nineteen_thousand_four_hundred() {
        // The production per-transfer maximum: 20,000 GLC gross -> 600 GLC
        // fee (3%) -> 19,400 GLC net, exact at canonical precision and
        // exactly representable at the reserve mint's 6-decimal precision
        // (20,000 GLC = 20_000_000_000 Solana-atomic units at 6 decimals).
        let gross = CanonicalAtomic(20_000 * 100_000_000);
        let fb = compute_fee(gross).unwrap();
        assert_eq!(fb.fee, CanonicalAtomic(600 * 100_000_000));
        assert_eq!(fb.net, CanonicalAtomic(19_400 * 100_000_000));
        assert_eq!(gross.to_solana(6).unwrap(), SolanaAtomic(20_000_000_000));
        assert_eq!(
            fb.net.to_solana(6).unwrap(),
            SolanaAtomic(19_400_000_000),
            "the max-transfer net must be exactly deliverable at 6 decimals"
        );
    }

    #[test]
    fn minimum_gross_for_a_99_glc_net_at_3_percent() {
        // The production minimum is defined by its NET: the smallest gross
        // whose net is >= 99 GLC. Continuous algebra says 99 / 0.97 =
        // 102.06185567... GLC; with the fee FLOORED in the user's favor,
        // brute force over canonical (8-decimal) precision lands one
        // atomic unit lower: 10_206_185_566 is the smallest gross that
        // nets exactly 99 GLC. Derived here by search, not pinned, so a
        // fee-formula regression shows up as a wrong minimum. On-chain,
        // `min_transfer_amount` stays 99 GLC (the NET-side floor); what
        // moves with the fee is the UI-facing GROSS entry minimum it
        // derives from that floor.
        let smallest = (10_206_000_000u64..10_207_000_000)
            .find(|&gross| compute_fee(CanonicalAtomic(gross)).unwrap().net.0 >= 99 * 100_000_000)
            .expect("a valid minimum must exist in this range");
        assert_eq!(smallest, 10_206_185_566);
        assert_eq!(
            compute_fee(CanonicalAtomic(smallest)).unwrap().net,
            CanonicalAtomic(99 * 100_000_000)
        );
        // The UI-facing gross entry minimum, quantized UP to 6-decimal
        // input precision (102.061856 GLC), must also net >= 99 GLC and
        // be exactly representable at the mint's 6 decimals.
        let ui_minimum = CanonicalAtomic(10_206_185_600);
        let fb = compute_fee(ui_minimum).unwrap();
        assert!(fb.net.0 >= 99 * 100_000_000);
        assert_eq!(
            ui_minimum.to_solana(6).unwrap(),
            SolanaAtomic(102_061_856),
            "the UI-facing minimum must be exact at the mint's 6 decimals"
        );
    }

    #[test]
    fn gross_always_equals_fee_plus_net_by_construction() {
        for gross in [1u64, 2, 99, 100, 101, 12_345, 1_000_000, 999_999_999] {
            let fb = compute_fee(CanonicalAtomic(gross)).unwrap();
            assert_eq!(fb.fee.0 + fb.net.0, fb.gross.0);
        }
    }

    #[test]
    fn fee_never_exceeds_gross_and_net_is_never_negative() {
        // At 300 bps (< BPS_DENOMINATOR), fee can never reach gross, so net
        // can never underflow — but assert it explicitly as a property,
        // not just an implementation detail of the formula.
        for gross in [0u64, 1, 50, 99, 100, u64::MAX / BRIDGE_FEE_BPS] {
            let fb = compute_fee(CanonicalAtomic(gross)).unwrap();
            assert!(fb.fee.0 <= fb.gross.0);
            assert!(fb.net.0 <= fb.gross.0);
        }
    }

    #[test]
    fn fee_rounds_down_never_up() {
        // gross=1: 1*300/10000 = 0.03 -> floors to 0, never rounds up to 1
        // (rounding up would charge a fee larger than 3% of the amount).
        let fb = compute_fee(CanonicalAtomic(1)).unwrap();
        assert_eq!(fb.fee, CanonicalAtomic::ZERO);
        assert_eq!(fb.net, CanonicalAtomic(1));
    }

    #[test]
    fn fee_computation_overflows_closed_rather_than_wrapping() {
        // gross large enough that gross * BRIDGE_FEE_BPS overflows u64.
        let gross = CanonicalAtomic(u64::MAX / 10); // *300 overflows u64
        assert_eq!(compute_fee(gross), Err(ConversionError::Overflow(gross.0)));
    }

    #[test]
    fn very_large_valid_gross_computes_without_overflow() {
        // The largest gross for which gross * BRIDGE_FEE_BPS still fits in
        // u64.
        let gross = CanonicalAtomic(u64::MAX / BRIDGE_FEE_BPS);
        let fb = compute_fee(gross).unwrap();
        assert_eq!(fb.fee.0 + fb.net.0, fb.gross.0);
    }

    #[test]
    fn typed_units_reject_mixing_at_compile_time() {
        // This test's real assertion is that the file compiles at all:
        // CanonicalAtomic and SolanaAtomic have no shared arithmetic/
        // comparison impl, so `CanonicalAtomic(1) == SolanaAtomic(1)` (or
        // any direct comparison/addition between the two) is a compile
        // error, not a runtime bug. Nothing to execute; documents the
        // property under test.
        let c = CanonicalAtomic(500_000_000);
        let s = c.to_solana(6).unwrap();
        assert_eq!(s, SolanaAtomic(5_000_000));
        assert_eq!(s.to_canonical(6).unwrap(), c);
    }

    /// GLC -> Solana direction: the smallest canonical gross amount whose
    /// NET (after the 3% fee) survives conversion to the canonical mint's
    /// 6-decimal precision exactly, per docs/20-bridge-fee.md's
    /// "mathematically smallest valid gross amount" analysis. Computed
    /// here by brute force rather than hardcoded, so this test would catch
    /// a regression in either the fee formula or the conversion policy,
    /// not just pin a number.
    #[test]
    fn smallest_valid_glc_to_solana_gross_is_103_canonical_atomic_units() {
        let solana_decimals = 6u8;
        let smallest = (1u64..10_000)
            .find(|&gross| {
                let fb = compute_fee(CanonicalAtomic(gross)).unwrap();
                fb.net.0 > 0 && fb.net.to_solana(solana_decimals).is_ok()
            })
            .expect("a valid gross must exist well within this search bound");
        assert_eq!(smallest, 103);

        // One canonical atomic unit below is invalid for either reason
        // (net==0 or non-exact conversion) for every smaller gross.
        for gross in 1..smallest {
            let fb = compute_fee(CanonicalAtomic(gross)).unwrap();
            let valid = fb.net.0 > 0 && fb.net.to_solana(solana_decimals).is_ok();
            assert!(
                !valid,
                "gross {gross} should not be a valid GLC->Solana amount"
            );
        }
    }

    /// Solana -> GLC direction: canonical is Goldcoin-native already, so
    /// converting gross (Solana-native, widening) to canonical is always
    /// exact — the only constraint is `net > 0`. Smallest valid Solana
    /// gross is therefore 1 atomic unit (1e-6 GLC).
    #[test]
    fn smallest_valid_solana_to_glc_gross_is_one_solana_atomic_unit() {
        let solana_decimals = 6u8;
        let smallest = (1u64..10_000)
            .find(|&gross_solana| {
                let gross_canonical = SolanaAtomic(gross_solana)
                    .to_canonical(solana_decimals)
                    .unwrap();
                let fb = compute_fee(gross_canonical).unwrap();
                fb.net.0 > 0
            })
            .expect("a valid gross must exist well within this search bound");
        assert_eq!(smallest, 1);
    }

    // --------------------------------------------------- verify_fee_breakdown --

    #[test]
    fn verify_fee_breakdown_accepts_a_correctly_reconciled_record() {
        let fb = verify_fee_breakdown(100_000, BRIDGE_FEE_BPS, 3_000, 97_000).unwrap();
        assert_eq!(fb.fee.0, 3_000);
        assert_eq!(fb.net.0, 97_000);
    }

    #[test]
    fn verify_fee_breakdown_rejects_a_tampered_fee_amount() {
        // Real breakdown for gross=100_000 is fee=3_000/net=97_000; an
        // attacker (or corrupted row) claims a smaller fee while leaving net
        // untouched — `gross == fee + net` would then also be violated, but
        // this must fail closed on the fee mismatch itself, not rely on
        // that secondary check.
        let result = verify_fee_breakdown(100_000, BRIDGE_FEE_BPS, 1_500, 97_000);
        assert!(matches!(
            result,
            Err(ConversionError::AccountingMismatch {
                gross: 100_000,
                stored_fee: 1_500,
                recomputed_fee: 3_000,
                stored_net: 97_000,
                recomputed_net: 97_000,
            })
        ));
    }

    #[test]
    fn verify_fee_breakdown_rejects_a_tampered_net_amount() {
        // Fee left correct but net inflated — the classic "keep the fee
        // small so it looks plausible, inflate what you actually receive"
        // tamper attempt.
        let result = verify_fee_breakdown(100_000, BRIDGE_FEE_BPS, 3_000, 100_000);
        assert!(matches!(
            result,
            Err(ConversionError::AccountingMismatch {
                gross: 100_000,
                stored_fee: 3_000,
                recomputed_fee: 3_000,
                stored_net: 100_000,
                recomputed_net: 97_000,
            })
        ));
    }

    #[test]
    fn verify_fee_breakdown_rejects_gross_ne_fee_plus_net() {
        // Both fee and net are individually wrong in a way that doesn't
        // even sum back to the claimed gross.
        let result = verify_fee_breakdown(100_000, BRIDGE_FEE_BPS, 7_000, 94_000);
        assert!(result.is_err());
    }

    #[test]
    fn verify_fee_breakdown_rejects_a_tampered_fee_bps_effect() {
        // A fee/net pair consistent with a DIFFERENT rate than the stored
        // snapshot claims (here: 6%-rate figures against a 300-bps
        // snapshot) must fail closed — the stored fee/net are validated
        // against the stored fee_bps itself, never against whichever rate
        // happens to make them look consistent.
        let wrong_bps_fee = 100_000 * 600 / 10_000; // what 6% would have charged
        let wrong_bps_net = 100_000 - wrong_bps_fee;
        let result = verify_fee_breakdown(100_000, 300, wrong_bps_fee, wrong_bps_net);
        assert!(matches!(
            result,
            Err(ConversionError::AccountingMismatch { .. })
        ));
    }

    #[test]
    fn verify_fee_breakdown_validates_a_600_bps_era_request_at_its_own_snapshot() {
        // Production request #818's exact figures: created at 6%, still
        // in flight when the compiled-in rate became 3%. At its own
        // stored snapshot it reconciles exactly and must keep settling.
        let fb = verify_fee_breakdown(20_000_000_000, 600, 1_200_000_000, 18_800_000_000).unwrap();
        assert_eq!(fb.fee_bps, 600);
        assert_eq!(fb.fee.0, 1_200_000_000);
        assert_eq!(fb.net.0, 18_800_000_000);
        // The same row judged at the CURRENT rate is exactly the refusal
        // production hit — proving the snapshot rate, not the compiled-in
        // rate, is what must govern historical validation.
        assert!(matches!(
            verify_fee_breakdown(
                20_000_000_000,
                BRIDGE_FEE_BPS,
                1_200_000_000,
                18_800_000_000
            ),
            Err(ConversionError::AccountingMismatch {
                recomputed_fee: 600_000_000,
                recomputed_net: 19_400_000_000,
                ..
            })
        ));
    }

    #[test]
    fn any_arithmetically_valid_snapshot_rate_settles_when_its_figures_reconcile() {
        // Replaces a test that required the snapshot to be one of three
        // compiled-in rates. A fee is CONFIGURATION: a request created at
        // a rate nobody had used before must settle, or changing a fee
        // would mean releasing a binary.
        //
        // What still fails closed is the thing that actually catches a
        // tampered row — figures that do not reconcile — and that is
        // asserted immediately below and in the two tests after it.
        for bps in [0u64, 1, 137, 300, 400, 599, 600, 1_234, 9_999, 10_000] {
            let fee = 100_000 * bps / 10_000;
            let net = 100_000 - fee;
            let breakdown = verify_fee_breakdown(100_000, bps, fee, net)
                .unwrap_or_else(|e| panic!("{bps} bps must settle when it reconciles: {e}"));
            assert_eq!(breakdown.fee_bps, bps);
            assert_eq!(breakdown.fee.0, fee);
            assert_eq!(breakdown.net.0, net);
            assert_eq!(breakdown.gross.0, breakdown.fee.0 + breakdown.net.0);
        }

        // ...and an internally INCONSISTENT row is still refused, at every
        // one of those rates.
        for bps in [0u64, 300, 400, 10_000] {
            let fee = 100_000 * bps / 10_000;
            assert!(
                matches!(
                    verify_fee_breakdown(100_000, bps, fee + 1, 100_000 - fee),
                    Err(ConversionError::AccountingMismatch { .. })
                ),
                "{bps} bps with a doctored fee must fail closed"
            );
        }
    }

    #[test]
    fn a_snapshot_rate_above_one_hundred_percent_fails_closed_by_arithmetic() {
        // The one rate rule left: above 10,000 bps the fee exceeds the
        // gross and the net entitlement would be negative, which unsigned
        // accounting cannot represent. Refused with a message that says
        // so, rather than surfacing as a misleading `Overflow`.
        for bps in [10_001u64, 20_000, u64::MAX] {
            assert!(
                matches!(
                    verify_fee_breakdown(100_000, bps, 0, 0),
                    Err(ConversionError::FeeBpsOutOfRange { max: 10_000, .. })
                ),
                "{bps} bps must be refused as out of range"
            );
        }
    }

    #[test]
    fn the_boundary_rates_behave_exactly_as_the_arithmetic_says() {
        // 0 bps: the route is free — fee 0, net == gross.
        let free = compute_fee_at_bps(CanonicalAtomic(100_000), 0).unwrap();
        assert_eq!(free.fee.0, 0);
        assert_eq!(free.net.0, 100_000);

        // 10,000 bps: the whole gross is the fee and net is 0. Arithmetically
        // valid and therefore accepted HERE; `crate::fees` refuses to
        // CONFIGURE it, because a route that can never deliver anything is
        // not a route.
        let all = compute_fee_at_bps(CanonicalAtomic(100_000), 10_000).unwrap();
        assert_eq!(all.fee.0, 100_000);
        assert_eq!(all.net.0, 0);
        assert_eq!(all.gross.0, all.fee.0 + all.net.0);

        // One past it is refused.
        assert!(matches!(
            compute_fee_at_bps(CanonicalAtomic(100_000), 10_001),
            Err(ConversionError::FeeBpsOutOfRange {
                fee_bps: 10_001,
                max: 10_000
            })
        ));
    }

    #[test]
    fn the_fee_math_is_exact_and_floored_at_an_arbitrary_non_historical_rate() {
        // 400 bps (4%) — a rate the old allowlist refused outright.
        let fb = compute_fee_at_bps(CanonicalAtomic(1_000_000_000), 400).unwrap();
        assert_eq!(fb.fee.0, 40_000_000);
        assert_eq!(fb.net.0, 960_000_000);
        assert_eq!(fb.gross.0, fb.fee.0 + fb.net.0);

        // Flooring still rounds toward the user, never up: 4% of 25 is
        // 1.0 exactly, of 26 is 1.04 -> 1.
        assert_eq!(
            compute_fee_at_bps(CanonicalAtomic(25), 400).unwrap().fee.0,
            1
        );
        assert_eq!(
            compute_fee_at_bps(CanonicalAtomic(26), 400).unwrap().fee.0,
            1
        );
        // And an amount too small for the rate to bite pays nothing.
        assert_eq!(
            compute_fee_at_bps(CanonicalAtomic(24), 400).unwrap().fee.0,
            0
        );
        assert_eq!(
            compute_fee_at_bps(CanonicalAtomic(24), 400).unwrap().net.0,
            24
        );
    }

    #[test]
    fn corrupted_600_bps_era_figures_still_fail_closed_at_their_own_snapshot() {
        // Historical snapshot honored does NOT mean historical rows are
        // trusted: 6%-era figures that do not reconcile against the 600
        // bps snapshot keep being refused.
        assert!(matches!(
            verify_fee_breakdown(20_000_000_000, 600, 1_100_000_000, 18_900_000_000),
            Err(ConversionError::AccountingMismatch { .. })
        ));
    }

    // ------------------------------------------ typed checked_add/checked_sub --

    #[test]
    fn canonical_checked_add_overflows_closed_rather_than_wrapping() {
        let result = CanonicalAtomic(u64::MAX).checked_add(CanonicalAtomic(1));
        assert!(matches!(result, Err(ConversionError::Overflow(_))));
    }

    #[test]
    fn canonical_checked_sub_underflows_closed_rather_than_wrapping() {
        let result = CanonicalAtomic(0).checked_sub(CanonicalAtomic(1));
        assert!(matches!(result, Err(ConversionError::Overflow(_))));
    }

    #[test]
    fn solana_checked_add_overflows_closed_rather_than_wrapping() {
        let result = SolanaAtomic(u64::MAX).checked_add(SolanaAtomic(1));
        assert!(matches!(result, Err(ConversionError::Overflow(_))));
    }

    #[test]
    fn solana_checked_sub_underflows_closed_rather_than_wrapping() {
        let result = SolanaAtomic(0).checked_sub(SolanaAtomic(1));
        assert!(matches!(result, Err(ConversionError::Overflow(_))));
    }

    /// `max_gross_for_net_at_bps` is the exact inverse of
    /// `compute_fee_at_bps`: the answer's net fits, one more unit's does
    /// not — across rates and around the flooring boundaries.
    #[test]
    fn max_gross_for_net_is_the_exact_inverse_of_the_fee() {
        for fee_bps in [0u64, 1, 300, 600, 2_500, 9_999] {
            for max_net in [1u64, 2, 99, 100, 101, 9_400, 47_000, 4_700_000_000] {
                let gross = max_gross_for_net_at_bps(CanonicalAtomic(max_net), fee_bps)
                    .unwrap()
                    .0;
                let net = compute_fee_at_bps(CanonicalAtomic(gross), fee_bps)
                    .unwrap()
                    .net
                    .0;
                assert!(
                    net <= max_net,
                    "fee {fee_bps} net {max_net}: gross {gross} nets {net}"
                );
                let next = compute_fee_at_bps(CanonicalAtomic(gross + 1), fee_bps)
                    .unwrap()
                    .net
                    .0;
                assert!(
                    next > max_net,
                    "fee {fee_bps} net {max_net}: gross {gross} is not maximal"
                );
            }
        }
        // The production shape: 47,000 GLC net at 6% is exactly 50,000 GLC gross.
        assert_eq!(
            max_gross_for_net_at_bps(CanonicalAtomic(4_700_000_000_000), 600).unwrap(),
            CanonicalAtomic(5_000_000_000_000)
        );
    }

    #[test]
    fn max_gross_for_net_edge_cases() {
        assert_eq!(
            max_gross_for_net_at_bps(CanonicalAtomic(0), 600).unwrap(),
            CanonicalAtomic(0)
        );
        assert_eq!(
            max_gross_for_net_at_bps(CanonicalAtomic(1_000), BPS_DENOMINATOR).unwrap(),
            CanonicalAtomic(0)
        );
        assert!(matches!(
            max_gross_for_net_at_bps(CanonicalAtomic(1_000), BPS_DENOMINATOR + 1),
            Err(ConversionError::FeeBpsOutOfRange { .. })
        ));
    }
}
