//! Bridge quote math and the bridge-rate book (docs/38-elastic-bridge-rate.md).
//!
//! # What a bridge quote is
//!
//! Until this module existed, a request's gross, fee and net were one asset
//! in one unit: `net = gross - fee`, with the only source→destination
//! transform being a decimal conversion (`amount_conversion`). A bridge
//! quote generalises that by one factor — the **bridge rate**, the ratio of
//! the source rail's price to the destination rail's price — and fixes the
//! order in which the three figures are derived:
//!
//! ```text
//! gross_out = floor(gross_in * source_price_e12 / destination_price_e12)
//! fee_out   = floor(gross_out * fee_bps / 10_000)      // the existing fee rule
//! net_out   = gross_out - fee_out
//! ```
//!
//! `gross_in` is the amount the depositor actually sent (anchored to the
//! real deposit exactly as before); `gross_out`, `fee_out` and `net_out` are
//! denominated in the DESTINATION asset, still in the canonical 8-decimal
//! accounting unit. Both prices are fixed-point integers scaled by
//! [`PRICE_SCALE`]; every intermediate product is `u128` and every step is
//! checked, so two processes given the same integers always produce the
//! same integers. No floating point appears anywhere in this module.
//!
//! # Phase 2A: the rate is exactly 1.0
//!
//! This phase ships the math, the persistence and the verification, with
//! both prices pinned to [`PRICE_SCALE`] ([`RateBook::fixed_unit`]). At a
//! unit rate `gross_out == gross_in`, so the fee and net a route produces
//! are bit-for-bit what it produced before — the signer messages, the
//! reserve accounting and the API amounts are unchanged. What changes is
//! that every new request now CARRIES its quote (`bridge_requests.quote_*`,
//! schema v37) and every settlement path re-derives its amounts from that
//! persisted quote ([`verify_quoted_breakdown`]) rather than from the fee
//! rule alone. Phase 2B replaces the fixed prices with live rail prices and
//! adds the halt/band/staleness gates; nothing in a settlement path has to
//! change for that, because the quote a request settles at is the one
//! persisted when its deposit was locked, never a live read.
//!
//! # When a quote is locked
//!
//! - Goldcoin-sourced routes (`GlcToSol`, `GlcToRhn`): the quote returned by
//!   `POST /transfers` is INDICATIVE and is stored on the row unlocked. The
//!   canonical settlement quote is locked when the deposit is first observed
//!   in a block (`Ledger::record_glc_deposit_observed_from`), and unlocked
//!   again if that block is orphaned, so a re-observation re-locks.
//! - Solana- and Robinhood-sourced routes: the fold IS the lock — the
//!   deposit is already final when the row is created.
//!
//! A quoted row whose quote is not locked cannot settle
//! ([`ConversionError::QuoteNotLocked`]).
//!
//! # Fee accounting under a quote (founder decision J-5, option a)
//!
//! `fee_out` is the bridge fee in destination-asset canonical units, and it
//! is the figure `bridge_requests.fee_amount_atomic` stores and
//! `reserve_ledger.accrued_fees_atomic` accrues. The fee is still physically
//! retained on the SOURCE reserve, exactly as before; what the accrued-fee
//! figure now reports is that retention valued in the destination asset at
//! the request's own quoted rate. At a unit rate the two are identical.
//! There is deliberately no second, source-denominated fee figure.

//!
//! # Phase 2B: the live book
//!
//! [`RateBook::live`] replaces the fixed prices with a [`live::LiveBook`]:
//! one smoothed USD price per rail ([`smoothing`]), fed by the three
//! verified price feeds ([`feeds`]), with the staleness, warm-up and band
//! verdicts ([`live`]). The pricing sites are unchanged in shape — they
//! still call [`RateBook::quote`] — but a quote can now be REFUSED
//! ([`RateError::Refused`]) or struck under a band breach
//! ([`StruckQuote::band`]), and the destination-precision flooring of
//! founder decision J-7 is part of the quote itself
//! (`destination_scale`, [`BridgeQuote::dust_out`]).

pub mod bigmath;
pub mod decimal;
pub mod feeds;
pub mod live;
pub mod smoothing;

use std::sync::Arc;

use crate::amount_conversion::{
    compute_fee_at_bps, verify_fee_breakdown, CanonicalAtomic, ConversionError, FeeBreakdown,
    BPS_DENOMINATOR,
};
use crate::routes::Route;

pub use live::{LiveBook, LiveRateConfig, RateRefusal, RouteRate};

/// Fixed-point scale of every rail price: a price of exactly 1.0 is
/// `PRICE_SCALE`. Twelve decimals is enough headroom for any real per-unit
/// price this bridge will see while keeping a BTC-quoted USD price (~1e5)
/// far inside `u64`.
pub const PRICE_SCALE: u64 = 1_000_000_000_000;

/// The default lifetime of a quote, in seconds, when the config's
/// `[bridge_rate]` section is absent. Metadata only in Phase 2A: nothing
/// reads `quote_expires_at` to make a decision.
pub const DEFAULT_QUOTE_LIFETIME_SECS: i64 = 60;

/// The two rail prices a quote is derived from, plus the timestamps of the
/// feed reads they came from (for audit; at a fixed rate these are simply
/// the quoting instant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RailPrices {
    pub source_price_e12: u64,
    pub destination_price_e12: u64,
    pub source_feed_at: i64,
    pub destination_feed_at: i64,
}

impl RailPrices {
    /// A unit rate (`1.0`) on both rails, read "now".
    pub const fn unit(now: i64) -> RailPrices {
        RailPrices {
            source_price_e12: PRICE_SCALE,
            destination_price_e12: PRICE_SCALE,
            source_feed_at: now,
            destination_feed_at: now,
        }
    }
}

/// One fully derived bridge quote: the prices it was struck at, the amounts
/// they produce for `gross_in`, and when it was struck. This is what a
/// pricing site persists onto a new request (`ledger::RequestAmounts::quote`)
/// and what a deposit observation locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeQuote {
    pub source_price_e12: u64,
    pub destination_price_e12: u64,
    /// The amount the depositor sends (source asset, canonical units).
    pub gross_in: CanonicalAtomic,
    /// `gross_in` valued in the destination asset at the bridge rate.
    pub gross_out: CanonicalAtomic,
    pub fee_bps: u64,
    /// The bridge fee, destination asset, canonical units.
    pub fee_out: CanonicalAtomic,
    /// `gross_out - fee_out - dust_out`: what the destination reserve
    /// owes, already a whole multiple of the destination rail's unit.
    pub net_out: CanonicalAtomic,
    /// The residual below one destination atomic unit that flooring the
    /// net to the destination's precision leaves with the bridge (founder
    /// decision J-7). Always `0 <= dust_out < destination_scale`, and
    /// always `0` at a unit rate on an exactly-representable amount.
    pub dust_out: CanonicalAtomic,
    pub quoted_at: i64,
    pub quote_expires_at: i64,
    pub source_feed_at: i64,
    pub destination_feed_at: i64,
}

impl BridgeQuote {
    /// The quote's amounts in the shape every settlement path already
    /// consumes. `gross` here is `gross_out` — the destination-asset figure
    /// the fee and net were derived from — never `gross_in`.
    /// The rail prices this quote was struck at, as the [`RailPrices`]
    /// [`max_source_for_destination_limit`] takes — so a maximum derived
    /// beside a quote is derived at exactly the quote's own prices.
    pub fn rail_prices(&self) -> RailPrices {
        RailPrices {
            source_price_e12: self.source_price_e12,
            destination_price_e12: self.destination_price_e12,
            source_feed_at: self.source_feed_at,
            destination_feed_at: self.destination_feed_at,
        }
    }

    pub fn breakdown(&self) -> FeeBreakdown {
        FeeBreakdown {
            gross: self.gross_out,
            fee_bps: self.fee_bps,
            fee: self.fee_out,
            net: self.net_out,
        }
    }

    /// The bridge rate as a plain decimal string with twelve places
    /// (`"1.000000000000"`), derived from the two prices by integer
    /// arithmetic only. For display; never parsed back.
    pub fn rate_display(&self) -> String {
        format_rate_e12(self.source_price_e12, self.destination_price_e12)
    }

    /// Whether this quote is a unit rate — the only rate Phase 2A produces.
    pub fn is_unit_rate(&self) -> bool {
        self.source_price_e12 == self.destination_price_e12
    }
}

/// `floor(gross_in * source_price_e12 / destination_price_e12)`, the
/// destination-asset value of a source-asset amount at the bridge rate.
/// The one place the rate is applied; everything else derives from its
/// result through the unchanged fee rule.
pub fn gross_out_at_rate(
    gross_in: CanonicalAtomic,
    source_price_e12: u64,
    destination_price_e12: u64,
) -> Result<CanonicalAtomic, ConversionError> {
    if source_price_e12 == 0 || destination_price_e12 == 0 {
        return Err(ConversionError::InvalidBridgePrice {
            source_price_e12,
            destination_price_e12,
        });
    }
    // u64 * u64 < 2^128, and the divisor is nonzero, so neither the
    // multiplication nor the division can fail here; only the narrowing
    // back to u64 can.
    let scaled = u128::from(gross_in.0) * u128::from(source_price_e12);
    let out = scaled / u128::from(destination_price_e12);
    u64::try_from(out)
        .map(CanonicalAtomic)
        .map_err(|_| ConversionError::Overflow(gross_in.0))
}

/// The quote derivation from `gross_in` at the given prices and fee rate:
/// `gross_out`, then the fee rule on `gross_out`, then the net — floored
/// to a whole multiple of `destination_scale` (founder decision J-7).
///
/// `destination_scale` is `10^(8 − destination_decimals)` — the number of
/// canonical atomic units in one destination atomic unit: `100` for the
/// 6-decimal Solana mint, `1` for Goldcoin (8 = 8) and for Robinhood
/// (18 > 8: widening is exact). The floored-off residual is returned
/// separately; it satisfies `0 <= dust < destination_scale` by
/// construction, so it is never a whole destination unit and never
/// reaches the recipient.
pub fn quoted_breakdown(
    gross_in: CanonicalAtomic,
    source_price_e12: u64,
    destination_price_e12: u64,
    fee_bps: u64,
    destination_scale: u64,
) -> Result<QuotedBreakdown, ConversionError> {
    if destination_scale == 0 {
        return Err(ConversionError::InvalidDestinationScale);
    }
    let gross_out = gross_out_at_rate(gross_in, source_price_e12, destination_price_e12)?;
    let fb = compute_fee_at_bps(gross_out, fee_bps)?;
    let dust = fb.net.0 % destination_scale;
    Ok(QuotedBreakdown {
        gross_out: fb.gross,
        fee_bps: fb.fee_bps,
        fee_out: fb.fee,
        net_out: CanonicalAtomic(fb.net.0 - dust),
        dust_out: CanonicalAtomic(dust),
    })
}

/// [`quoted_breakdown`]'s result: `gross_out == fee_out + net_out +
/// dust_out` by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotedBreakdown {
    pub gross_out: CanonicalAtomic,
    pub fee_bps: u64,
    pub fee_out: CanonicalAtomic,
    pub net_out: CanonicalAtomic,
    pub dust_out: CanonicalAtomic,
}

impl QuotedBreakdown {
    /// The settlement shape: `gross` is `gross_out`, `net` is the floored
    /// net that settles.
    pub fn as_fee_breakdown(&self) -> FeeBreakdown {
        FeeBreakdown {
            gross: self.gross_out,
            fee_bps: self.fee_bps,
            fee: self.fee_out,
            net: self.net_out,
        }
    }
}

/// Strikes a full [`BridgeQuote`] for `gross_in` at `prices`, fixing its
/// `quoted_at`/`quote_expires_at` from `now` and `quote_lifetime_secs`,
/// and flooring the net to `destination_scale`.
pub fn compute_bridge_quote(
    gross_in: CanonicalAtomic,
    prices: RailPrices,
    fee_bps: u64,
    now: i64,
    quote_lifetime_secs: i64,
    destination_scale: u64,
) -> Result<BridgeQuote, ConversionError> {
    let breakdown = quoted_breakdown(
        gross_in,
        prices.source_price_e12,
        prices.destination_price_e12,
        fee_bps,
        destination_scale,
    )?;
    Ok(BridgeQuote {
        source_price_e12: prices.source_price_e12,
        destination_price_e12: prices.destination_price_e12,
        gross_in,
        gross_out: breakdown.gross_out,
        fee_bps,
        fee_out: breakdown.fee_out,
        net_out: breakdown.net_out,
        dust_out: breakdown.dust_out,
        quoted_at: now,
        quote_expires_at: now.saturating_add(quote_lifetime_secs),
        source_feed_at: prices.source_feed_at,
        destination_feed_at: prices.destination_feed_at,
    })
}

/// The quoted twin of [`verify_fee_breakdown`], and the ONLY way a quoted
/// request's amounts reach a settlement: re-derives `gross_out`, fee and
/// the floored net from the persisted `gross_in`, prices, `fee_bps` and
/// the destination scale, and refuses with
/// [`ConversionError::QuoteMismatch`] unless all three stored figures
/// agree exactly. The returned breakdown is the freshly recomputed one;
/// the stored figures are only ever compared against, never used.
#[allow(clippy::too_many_arguments)]
pub fn verify_quoted_breakdown(
    gross_in: u64,
    stored_source_price_e12: u64,
    stored_destination_price_e12: u64,
    stored_fee_bps: u64,
    stored_gross_out: u64,
    stored_fee_out: u64,
    stored_net_out: u64,
    destination_scale: u64,
) -> Result<FeeBreakdown, ConversionError> {
    let qb = quoted_breakdown(
        CanonicalAtomic(gross_in),
        stored_source_price_e12,
        stored_destination_price_e12,
        stored_fee_bps,
        destination_scale,
    )?;
    let fb = qb.as_fee_breakdown();
    if fb.gross.0 != stored_gross_out || fb.fee.0 != stored_fee_out || fb.net.0 != stored_net_out {
        return Err(ConversionError::QuoteMismatch {
            gross_in,
            source_price_e12: stored_source_price_e12,
            destination_price_e12: stored_destination_price_e12,
            stored_gross_out,
            recomputed_gross_out: fb.gross.0,
            stored_fee: stored_fee_out,
            recomputed_fee: fb.fee.0,
            stored_net: stored_net_out,
            recomputed_net: fb.net.0,
        });
    }
    Ok(fb)
}

/// The quote a request persisted, exactly as its row carries it
/// (`bridge_requests.quote_*`, schema v37). `None` on a `BridgeRequest`
/// means a LEGACY row — created before v37 — which verifies through
/// [`verify_fee_breakdown`] at an implicit unit rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistedQuote {
    pub source_price_e12: u64,
    pub destination_price_e12: u64,
    pub gross_out_atomic: u64,
    pub quoted_at: i64,
    pub quote_expires_at: i64,
    pub source_feed_at: i64,
    pub destination_feed_at: i64,
    /// When the quote became the settlement quote. `None` = still the
    /// indicative quote a Goldcoin-sourced request was created with (its
    /// deposit has not been observed, or the observing block was orphaned).
    pub locked_at: Option<i64>,
}

impl PersistedQuote {
    pub fn is_locked(&self) -> bool {
        self.locked_at.is_some()
    }
}

/// THE canonical amount verification for a request row, quoted or legacy.
///
/// Every settlement, recovery and reconciliation path calls this (through
/// `BridgeRequest::verify_breakdown`) instead of choosing between the two
/// verifiers itself:
///
/// - a quoted row must be LOCKED and must reconcile under
///   [`verify_quoted_breakdown`] at `destination_scale`;
/// - a legacy row (no quote) reconciles under [`verify_fee_breakdown`],
///   exactly as it did before v37.
///
/// The returned breakdown's `net` is what settles, in both cases.
pub fn verify_request_amounts(
    quote: Option<&PersistedQuote>,
    gross_amount_atomic: u64,
    fee_bps: u64,
    fee_amount_atomic: u64,
    net_amount_atomic: u64,
    destination_scale: u64,
) -> Result<FeeBreakdown, ConversionError> {
    match quote {
        None => verify_fee_breakdown(
            gross_amount_atomic,
            fee_bps,
            fee_amount_atomic,
            net_amount_atomic,
        ),
        Some(q) => {
            if !q.is_locked() {
                return Err(ConversionError::QuoteNotLocked {
                    quoted_at: q.quoted_at,
                });
            }
            verify_quoted_breakdown(
                gross_amount_atomic,
                q.source_price_e12,
                q.destination_price_e12,
                fee_bps,
                q.gross_out_atomic,
                fee_amount_atomic,
                net_amount_atomic,
                destination_scale,
            )
        }
    }
}

/// The net a request WOULD owe for some independently observed gross —
/// the cross-check the Solana completion attestation runs against the
/// on-chain obligation amount. Quoted rows price the gross at their own
/// persisted rate (floored to `destination_scale`); legacy rows at the
/// fee rule alone.
pub fn expected_net_for_gross(
    quote: Option<&PersistedQuote>,
    gross_in: CanonicalAtomic,
    fee_bps: u64,
    destination_scale: u64,
) -> Result<CanonicalAtomic, ConversionError> {
    match quote {
        None => Ok(compute_fee_at_bps(gross_in, fee_bps)?.net),
        Some(q) => Ok(quoted_breakdown(
            gross_in,
            q.source_price_e12,
            q.destination_price_e12,
            fee_bps,
            destination_scale,
        )?
        .net_out),
    }
}

/// `10^(8 − destination_decimals)`: the canonical atomic units in one
/// destination atomic unit, or `1` when the destination is at least as
/// fine as canonical (Goldcoin 8, Robinhood 18).
pub fn destination_scale_for_decimals(destination_decimals: u8) -> u64 {
    let canonical = crate::amount_conversion::GOLDCOIN_DECIMALS as u8;
    if destination_decimals >= canonical {
        1
    } else {
        10u64.pow(u32::from(canonical - destination_decimals))
    }
}

/// Where a pricing site gets its rail prices from.
///
/// Two modes, one call shape. [`RateBook::fixed_unit`] answers `1.0` for
/// every route, every time (Phase 2A, tests, a staging deployment with
/// no feeds). [`RateBook::live`] answers from the smoothed rail prices
/// and can refuse ([`RateError::Refused`]) or flag a band breach. Every
/// pricing site — `POST /transfers`, `GET /quote`, the three deposit
/// folds, the Goldcoin deposit observation — calls [`RateBook::quote`]
/// and handles the same three outcomes, so no site can quote what
/// another refuses.
#[derive(Debug, Clone)]
pub struct RateBook {
    quote_lifetime_secs: i64,
    mode: RateMode,
}

#[derive(Debug, Clone)]
enum RateMode {
    FixedUnit,
    Live(Arc<LiveBook>),
}

/// Why [`RateBook::quote`] produced nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RateError {
    /// The live book has no usable rate for this route right now — a
    /// halt condition, never a fallback.
    #[error("{0}")]
    Refused(#[from] RateRefusal),
    /// The prices are fine but the amount is not (overflow, a fee rate
    /// the fee rule rejects, a zero destination scale).
    #[error("{0}")]
    Conversion(#[from] ConversionError),
}

impl RateError {
    /// The stable reason string for the API and the ManualReview note.
    pub fn reason(&self) -> &'static str {
        match self {
            RateError::Refused(r) => r.reason(),
            RateError::Conversion(_) => "bridge_quote_invalid",
        }
    }
}

/// A quote the book struck, with the band verdict it was struck under.
/// `band` is `Some` when the route's rate has moved further than the
/// configured band from one window ago: the quote is real and must be
/// persisted with any deposit it prices, but that deposit is parked
/// (`bridge_rate_band_exceeded`) rather than paid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StruckQuote {
    pub quote: BridgeQuote,
    pub band: Option<BandBreach>,
    /// The live route rate the quote came from; `None` at a fixed rate.
    pub route_rate: Option<RouteRate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BandBreach {
    pub movement_bps: u64,
    pub band_bps: u64,
}

impl RateBook {
    /// A book that answers `1.0` for every route.
    pub fn fixed_unit(quote_lifetime_secs: i64) -> RateBook {
        RateBook {
            quote_lifetime_secs,
            mode: RateMode::FixedUnit,
        }
    }

    /// A book backed by live smoothed rail prices.
    pub fn live(quote_lifetime_secs: i64, book: Arc<LiveBook>) -> RateBook {
        RateBook {
            quote_lifetime_secs,
            mode: RateMode::Live(book),
        }
    }

    pub fn quote_lifetime_secs(&self) -> i64 {
        self.quote_lifetime_secs
    }

    pub fn is_live(&self) -> bool {
        matches!(self.mode, RateMode::Live(_))
    }

    /// The live book, for operators' read-only status.
    pub fn live_book(&self) -> Option<&Arc<LiveBook>> {
        match &self.mode {
            RateMode::Live(book) => Some(book),
            RateMode::FixedUnit => None,
        }
    }

    /// The route's rate as of `now`: the prices a quote would be struck
    /// at, plus the live route rate (`None` at a fixed unit rate).
    pub fn route_status(
        &self,
        route: Route,
        now: i64,
    ) -> Result<(RailPrices, Option<RouteRate>), RateRefusal> {
        match &self.mode {
            RateMode::FixedUnit => Ok((RailPrices::unit(now), None)),
            RateMode::Live(book) => {
                let rate = book.route_rate(route, now)?;
                Ok((rate.prices, Some(rate)))
            }
        }
    }

    /// Strikes a quote for `gross_in` on `route` at `fee_bps`, flooring
    /// the net to `destination_scale` (see [`quoted_breakdown`]).
    pub fn quote(
        &self,
        route: Route,
        gross_in: CanonicalAtomic,
        fee_bps: u64,
        now: i64,
        destination_scale: u64,
    ) -> Result<StruckQuote, RateError> {
        let (prices, route_rate) = self.route_status(route, now)?;
        let quote = compute_bridge_quote(
            gross_in,
            prices,
            fee_bps,
            now,
            self.quote_lifetime_secs,
            destination_scale,
        )?;
        let band = route_rate.filter(|r| r.band_exceeded).map(|r| BandBreach {
            movement_bps: r.movement_bps,
            band_bps: r.band_bps,
        });
        Ok(StruckQuote {
            quote,
            band,
            route_rate,
        })
    }
}

/// What a deposit fold does with the book's answer (docs/38-elastic-
/// bridge-rate.md, Phase 2B "Band check" and "Staleness / bad feed").
///
/// A Solana- or Robinhood-sourced deposit is already final when it is
/// folded, so the fold can never "refuse" it — it can only decide
/// whether the row is payable. This is the one place that decision is
/// made from a [`RateBook`] answer, so every fold parks for the same
/// reasons with the same rows:
///
/// - the book struck a quote → the row carries it, LOCKED at the fold;
///   under a band breach the row is additionally parked
///   `bridge_rate_band_exceeded`, keeping that locked quote (an operator
///   resume settles at it, never at a live rate);
/// - the book refused (feed unavailable / stale / warming up) → the row
///   carries NO quote and is parked under the refusal's reason. Its
///   fee/net columns hold the unit-rate figures purely as a record of
///   the deposit; nothing can settle them (no quote → no lock) and the
///   row's exit is a refund.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldPricing {
    /// The quote to persist (`RequestAmounts::quote`), if one was struck.
    pub quote: Option<BridgeQuote>,
    /// The figures to store: the quote's own, or the unit-rate record.
    pub breakdown: FeeBreakdown,
    /// The ManualReview reason the fold must park under, if any.
    pub park: Option<&'static str>,
    /// The band breach, when that is the reason.
    pub band: Option<BandBreach>,
}

/// Prices a FINAL deposit for a fold — see [`FoldPricing`]. A
/// [`RateError::Conversion`] (an amount the arithmetic itself refuses)
/// is returned as-is: it is a bug or a broken config, not a market
/// condition, and the tick must fail loudly rather than park.
pub fn price_final_deposit(
    book: &RateBook,
    route: Route,
    gross_in: CanonicalAtomic,
    fee_bps: u64,
    now: i64,
    destination_scale: u64,
) -> Result<FoldPricing, ConversionError> {
    match book.quote(route, gross_in, fee_bps, now, destination_scale) {
        Ok(struck) => Ok(FoldPricing {
            quote: Some(struck.quote),
            breakdown: struck.quote.breakdown(),
            park: struck.band.map(|_| live::REASON_BAND_EXCEEDED),
            band: struck.band,
        }),
        Err(RateError::Refused(refusal)) => Ok(FoldPricing {
            quote: None,
            breakdown: compute_fee_at_bps(gross_in, fee_bps)?,
            park: Some(refusal.reason()),
            band: None,
        }),
        Err(RateError::Conversion(e)) => Err(e),
    }
}

/// `source / destination` rendered with twelve decimal places by integer
/// arithmetic. A zero destination price (which no quote can carry — see
/// [`gross_out_at_rate`]) renders as `"0.000000000000"` rather than
/// panicking, so a display path can never take a process down.
pub fn format_rate_e12(source_price_e12: u64, destination_price_e12: u64) -> String {
    if destination_price_e12 == 0 {
        return "0.000000000000".to_string();
    }
    let scaled =
        u128::from(source_price_e12) * u128::from(PRICE_SCALE) / u128::from(destination_price_e12);
    let whole = scaled / u128::from(PRICE_SCALE);
    let frac = scaled % u128::from(PRICE_SCALE);
    format!("{whole}.{frac:012}")
}

// ------------------------------------------- destination-bound admission --

/// The default `destination_limit_buffer_bps`: one rate band (the
/// configured `rate_band_pct`, 25 % in production → 2500 bps). See
/// [`buffered_destination_limit`] for why one band is the right unit.
pub const DEFAULT_DESTINATION_LIMIT_BUFFER_BPS: u64 = 2_500;

/// The largest quoted destination net this deployment ADMITS against a
/// destination-chain limit of `limit_canonical`:
/// `⌊limit · (10000 − buffer_bps) / 10000⌋`, canonical units.
///
/// # Why a buffer, and why one band is enough
///
/// A Goldcoin-sourced request is quoted at `POST /transfers` and LOCKED
/// at its first deposit observation (docs/38, J-4) — the payout the
/// destination chain finally sees is struck at the observation-time
/// rate, not the quote-time rate. The destination chain's limit (the
/// Solana program's `per_transfer_limit`, the Robinhood contract's
/// `outboundMax`) is checked again before any signer is asked
/// (`Orchestrator::release_out_of_bounds`, `Settler::authorize_payout`)
/// and a breach parks the request `destination_payout_out_of_bounds`.
/// Admitting a request whose quote sits exactly at the limit therefore
/// admits a request that any upward rate move turns unpayable.
///
/// The live book bounds the rate's movement between consecutive price
/// windows to `rate_band_bps` (`RouteRate::band_exceeded` parks a
/// deposit that moved further). A deposit observed within one window of
/// its quote can thus have moved at most one band, so a buffer of one
/// band keeps every such order payable. A deposit observed later can
/// have drifted further (each window is bounded, the sum is not); that
/// remains the settlement check's job — the buffer makes the park rare,
/// it cannot and must not make it unreachable.
///
/// A buffer of `10000` bps (or more) admits nothing; `0` admits up to the
/// limit exactly.
pub fn buffered_destination_limit(
    limit_canonical: CanonicalAtomic,
    buffer_bps: u64,
) -> CanonicalAtomic {
    let keep = BPS_DENOMINATOR.saturating_sub(buffer_bps);
    let scaled = u128::from(limit_canonical.0) * u128::from(keep) / u128::from(BPS_DENOMINATOR);
    // keep ≤ BPS_DENOMINATOR, so the product / denominator ≤ the input.
    CanonicalAtomic(scaled as u64)
}

/// **The one canonical maximum.** The largest source amount (canonical
/// units, the figure a depositor sends) whose quoted destination net —
/// derived by [`quoted_breakdown`], the SAME integer arithmetic every
/// quote, lock and settlement verification uses — does not exceed
/// [`buffered_destination_limit`]`(limit_canonical, buffer_bps)`.
///
/// `limit_canonical` is the destination chain's per-transfer limit
/// already converted to canonical units by the caller (the Solana
/// program's `per_transfer_limit` widened from the mint's decimals; the
/// Robinhood contract's `outboundMax` floored from 18 dp). The
/// destination precision floor (J-7) is applied through
/// `destination_scale` exactly as the quote applies it.
///
/// Found by binary search over the monotone (non-decreasing) integer
/// function `gross_in ↦ net_out`, never by rearranging the formula —
/// so the two can never disagree by a rounding unit. Pinned by
/// `tests::max_source_is_the_exact_boundary`: `net_out(max) ≤ buffered`
/// and `net_out(max + 1) > buffered` whenever `max > 0`.
///
/// Returns `0` when no deliverable source amount fits (a buffer of
/// 100 %, a fee of 100 %, a zero limit, or a buffered limit below one
/// destination unit — the net would floor to nothing). Otherwise the
/// net at the maximum is at least one destination unit.
pub fn max_source_for_destination_limit(
    limit_canonical: CanonicalAtomic,
    prices: RailPrices,
    fee_bps: u64,
    destination_scale: u64,
    buffer_bps: u64,
) -> Result<CanonicalAtomic, ConversionError> {
    if destination_scale == 0 {
        return Err(ConversionError::InvalidDestinationScale);
    }
    if fee_bps > BPS_DENOMINATOR {
        return Err(ConversionError::FeeBpsOutOfRange {
            fee_bps,
            max: BPS_DENOMINATOR,
        });
    }
    if prices.source_price_e12 == 0 || prices.destination_price_e12 == 0 {
        return Err(ConversionError::InvalidBridgePrice {
            source_price_e12: prices.source_price_e12,
            destination_price_e12: prices.destination_price_e12,
        });
    }
    let buffered = buffered_destination_limit(limit_canonical, buffer_bps);
    // Below one destination unit nothing DELIVERABLE fits: a gross that
    // nets to zero after the floor would technically "fit" a limit of
    // 0..scale-1, and no bridge admits a transfer that pays nothing.
    if buffered.0 < destination_scale || fee_bps == BPS_DENOMINATOR {
        return Ok(CanonicalAtomic(0));
    }
    // `net_out` is non-decreasing in `gross_in` (every step is a floor of
    // a non-decreasing function), and an overflow while computing it can
    // only happen above any admissible amount — so "fits" is a monotone
    // predicate and a binary search is exact.
    let fits = |gross_in: u64| -> bool {
        match quoted_breakdown(
            CanonicalAtomic(gross_in),
            prices.source_price_e12,
            prices.destination_price_e12,
            fee_bps,
            destination_scale,
        ) {
            Ok(qb) => qb.net_out.0 <= buffered.0,
            Err(_) => false,
        }
    };
    let (mut lo, mut hi) = (0u64, u64::MAX);
    if fits(hi) {
        return Ok(CanonicalAtomic(hi));
    }
    // Invariant: fits(lo), !fits(hi).
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(CanonicalAtomic(lo))
}

#[cfg(test)]
mod tests;
