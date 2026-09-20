use super::*;
use crate::amount_conversion::{compute_fee_at_bps, BRIDGE_FEE_BPS};

const GLC: u64 = 100_000_000;

fn prices(source_price_e12: u64, destination_price_e12: u64) -> RailPrices {
    RailPrices {
        source_price_e12,
        destination_price_e12,
        source_feed_at: 1_000,
        destination_feed_at: 1_000,
    }
}

// ---------------------------------------------------------------------
// Rate 1.0 — the Phase 2A production rate — is the fee rule, exactly.
// ---------------------------------------------------------------------

#[test]
fn a_unit_rate_reproduces_the_fee_rule_bit_for_bit() {
    // The fee rule itself bounds gross at `u64::MAX / 10_000`; the last
    // value sits just inside it.
    for gross in [
        1u64,
        33,
        34,
        103,
        500_000,
        100 * GLC,
        20_000 * GLC,
        u64::MAX / 10_000,
    ] {
        for fee_bps in [0u64, 100, 300, 600, 9_999, 10_000] {
            let legacy = compute_fee_at_bps(CanonicalAtomic(gross), fee_bps).unwrap();
            let quoted =
                quoted_breakdown(CanonicalAtomic(gross), PRICE_SCALE, PRICE_SCALE, fee_bps, 1)
                    .unwrap();
            assert_eq!(
                quoted.as_fee_breakdown(),
                legacy,
                "gross {gross} at {fee_bps} bps"
            );
            assert_eq!(quoted.dust_out.0, 0);
        }
    }
}

#[test]
fn the_fixed_unit_book_quotes_every_route_at_one() {
    let book = RateBook::fixed_unit(60);
    for route in [
        Route::GlcToSol,
        Route::SolToGlc,
        Route::GlcToRhn,
        Route::RhnToGlc,
        Route::SolToRhn,
        Route::RhnToSol,
    ] {
        let struck = book
            .quote(
                route,
                CanonicalAtomic(1_000 * GLC),
                BRIDGE_FEE_BPS,
                1_700_000_000,
                1,
            )
            .unwrap();
        assert!(struck.band.is_none());
        assert!(
            struck.route_rate.is_none(),
            "a fixed book has no live route rate"
        );
        let q = struck.quote;
        assert!(q.is_unit_rate());
        assert_eq!(q.rate_display(), "1.000000000000");
        assert_eq!(q.gross_in, q.gross_out);
        assert_eq!(q.fee_out.0, 30 * GLC);
        assert_eq!(q.net_out.0, 970 * GLC);
        assert_eq!(q.quoted_at, 1_700_000_000);
        assert_eq!(q.quote_expires_at, 1_700_000_060);
        assert_eq!(q.source_feed_at, 1_700_000_000);
        assert_eq!(q.destination_feed_at, 1_700_000_000);
        assert_eq!(
            q.breakdown(),
            compute_fee_at_bps(CanonicalAtomic(1_000 * GLC), BRIDGE_FEE_BPS).unwrap()
        );
    }
}

// ---------------------------------------------------------------------
// Synthetic rates — the math Phase 2B will drive with live prices.
// ---------------------------------------------------------------------

#[test]
fn rate_0_8_values_the_deposit_at_four_fifths_before_the_fee() {
    // source 0.80, destination 1.00
    let q = compute_bridge_quote(
        CanonicalAtomic(1_000 * GLC),
        prices(800_000_000_000, PRICE_SCALE),
        300,
        0,
        60,
        1,
    )
    .unwrap();
    assert_eq!(q.rate_display(), "0.800000000000");
    assert_eq!(q.gross_out.0, 800 * GLC);
    assert_eq!(q.fee_out.0, 24 * GLC);
    assert_eq!(q.net_out.0, 776 * GLC);
    assert_eq!(q.gross_out.0, q.fee_out.0 + q.net_out.0);
}

#[test]
fn rate_1_25_values_the_deposit_at_five_quarters_before_the_fee() {
    // source 1.25, destination 1.00 — and the same rate spelled as
    // 1.00 / 0.80, which must be the identical quote.
    let a = compute_bridge_quote(
        CanonicalAtomic(1_000 * GLC),
        prices(1_250_000_000_000, PRICE_SCALE),
        300,
        0,
        60,
        1,
    )
    .unwrap();
    let b = compute_bridge_quote(
        CanonicalAtomic(1_000 * GLC),
        prices(PRICE_SCALE, 800_000_000_000),
        300,
        0,
        60,
        1,
    )
    .unwrap();
    assert_eq!(a.rate_display(), "1.250000000000");
    assert_eq!(b.rate_display(), "1.250000000000");
    assert_eq!(a.gross_out.0, 1_250 * GLC);
    assert_eq!(a.fee_out.0, 37 * GLC + 50_000_000);
    assert_eq!(a.net_out.0, 1_212 * GLC + 50_000_000);
    assert_eq!(
        (a.gross_out, a.fee_out, a.net_out),
        (b.gross_out, b.fee_out, b.net_out)
    );
}

#[test]
fn a_non_terminating_rate_floors_the_destination_gross_and_never_rounds_up() {
    // 1/3: source 1.00, destination 3.00.
    let q = compute_bridge_quote(
        CanonicalAtomic(1_000 * GLC),
        prices(PRICE_SCALE, 3 * PRICE_SCALE),
        300,
        0,
        60,
        1,
    )
    .unwrap();
    assert_eq!(q.rate_display(), "0.333333333333");
    // 100_000_000_000 / 3 = 33_333_333_333.33.. -> floored
    assert_eq!(q.gross_out.0, 33_333_333_333);
    assert_eq!(q.fee_out.0, 999_999_999); // floor(33_333_333_333 * 0.03)
    assert_eq!(q.net_out.0, 32_333_333_334);
    assert_eq!(q.gross_out.0, q.fee_out.0 + q.net_out.0);
    // One atomic unit of source at 1/3 is worth nothing at the
    // destination — floored to zero, never rounded to one.
    let dust = quoted_breakdown(CanonicalAtomic(1), PRICE_SCALE, 3 * PRICE_SCALE, 300, 1).unwrap();
    assert_eq!(dust.gross_out.0, 0);
    assert_eq!(dust.net_out.0, 0);
}

#[test]
fn gross_out_is_derived_by_integer_arithmetic_only_and_is_deterministic() {
    // A small LCG over (amount, prices, fee): the same integers must give
    // the same integers, run after run, and every result must satisfy the
    // structural identities.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..10_000 {
        // Bounded so `gross_out * fee_bps` stays inside the fee rule's own
        // u64 headroom: gross_in < 1e14, rate <= 10 -> gross_out < 1e15.
        let gross_in = next() % (1_000_000 * GLC);
        let src = 1 + next() % (10 * PRICE_SCALE);
        let dst = PRICE_SCALE + next() % (10 * PRICE_SCALE);
        let fee_bps = next() % 10_001;
        let first = quoted_breakdown(CanonicalAtomic(gross_in), src, dst, fee_bps, 1).unwrap();
        let second = quoted_breakdown(CanonicalAtomic(gross_in), src, dst, fee_bps, 1).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.gross_out.0,
            first.fee_out.0 + first.net_out.0 + first.dust_out.0
        );
        assert_eq!(first.dust_out.0, 0, "scale 1 never floors anything off");
        let expected_gross_out = (u128::from(gross_in) * u128::from(src) / u128::from(dst)) as u64;
        assert_eq!(first.gross_out.0, expected_gross_out);
        assert_eq!(
            u128::from(first.fee_out.0),
            u128::from(expected_gross_out) * u128::from(fee_bps) / 10_000
        );
        assert!(verify_quoted_breakdown(
            gross_in,
            src,
            dst,
            fee_bps,
            first.gross_out.0,
            first.fee_out.0,
            first.net_out.0,
            1
        )
        .is_ok());
    }
}

// ---------------------------------------------------------------------
// Refusals.
// ---------------------------------------------------------------------

#[test]
fn a_zero_price_on_either_rail_is_refused_before_any_amount_is_derived() {
    assert!(matches!(
        gross_out_at_rate(CanonicalAtomic(1), 0, PRICE_SCALE),
        Err(ConversionError::InvalidBridgePrice { .. })
    ));
    assert!(matches!(
        gross_out_at_rate(CanonicalAtomic(1), PRICE_SCALE, 0),
        Err(ConversionError::InvalidBridgePrice { .. })
    ));
    assert!(matches!(
        RateBook::fixed_unit(60).quote(Route::GlcToSol, CanonicalAtomic(5), 10_001, 0, 1),
        Err(RateError::Conversion(
            ConversionError::FeeBpsOutOfRange { .. }
        ))
    ));
}

#[test]
fn a_gross_out_that_does_not_fit_u64_is_an_overflow_not_a_wrap() {
    // u64::MAX at a rate of 2.0 overflows; at 1.0 it does not.
    assert!(matches!(
        gross_out_at_rate(CanonicalAtomic(u64::MAX), 2 * PRICE_SCALE, PRICE_SCALE),
        Err(ConversionError::Overflow(_))
    ));
    assert_eq!(
        gross_out_at_rate(CanonicalAtomic(u64::MAX), PRICE_SCALE, PRICE_SCALE).unwrap(),
        CanonicalAtomic(u64::MAX)
    );
    // The intermediate product of two u64s cannot itself overflow u128.
    assert_eq!(
        gross_out_at_rate(CanonicalAtomic(u64::MAX), u64::MAX, u64::MAX).unwrap(),
        CanonicalAtomic(u64::MAX)
    );
}

// ---------------------------------------------------------------------
// Verification: every stored figure must reproduce, or nothing settles.
// ---------------------------------------------------------------------

fn locked(gross_in: u64, src: u64, dst: u64, fee_bps: u64) -> (PersistedQuote, u64, u64) {
    let fb = quoted_breakdown(CanonicalAtomic(gross_in), src, dst, fee_bps, 1).unwrap();
    (
        PersistedQuote {
            source_price_e12: src,
            destination_price_e12: dst,
            gross_out_atomic: fb.gross_out.0,
            quoted_at: 100,
            quote_expires_at: 160,
            source_feed_at: 100,
            destination_feed_at: 100,
            locked_at: Some(100),
        },
        fb.fee_out.0,
        fb.net_out.0,
    )
}

#[test]
fn a_consistent_quoted_row_verifies_and_the_recomputed_net_is_what_settles() {
    let (q, fee, net) = locked(1_000 * GLC, 800_000_000_000, PRICE_SCALE, 300);
    let fb = verify_request_amounts(Some(&q), 1_000 * GLC, 300, fee, net, 1).unwrap();
    assert_eq!(fb.gross.0, 800 * GLC);
    assert_eq!(fb.net.0, 776 * GLC);
}

#[test]
fn tampering_with_any_persisted_quote_figure_is_refused() {
    let (q, fee, net) = locked(1_000 * GLC, PRICE_SCALE, PRICE_SCALE, 300);
    let ok = |q: &PersistedQuote, fee: u64, net: u64| {
        verify_request_amounts(Some(q), 1_000 * GLC, 300, fee, net, 1)
    };
    assert!(ok(&q, fee, net).is_ok());

    let mismatch = |r: Result<FeeBreakdown, ConversionError>| {
        matches!(r, Err(ConversionError::QuoteMismatch { .. }))
    };
    // gross_out
    let mut t = q;
    t.gross_out_atomic += 1;
    assert!(mismatch(ok(&t, fee, net)));
    // fee (net untouched -> gross != fee + net)
    assert!(mismatch(ok(&q, fee - 1, net)));
    // net
    assert!(mismatch(ok(&q, fee, net + 1)));
    // a rewritten source price that no longer produces the stored gross_out
    let mut t = q;
    t.source_price_e12 = 1_250_000_000_000;
    assert!(mismatch(ok(&t, fee, net)));
    // a rewritten destination price, same
    let mut t = q;
    t.destination_price_e12 = 800_000_000_000;
    assert!(mismatch(ok(&t, fee, net)));
    // a consistent rewrite of EVERYTHING at a different rate still cannot
    // pass off the old fee/net: the stored fee/net were struck at 1.0
    let mut t = q;
    t.source_price_e12 = 1_250_000_000_000;
    t.gross_out_atomic = 1_250 * GLC;
    assert!(mismatch(ok(&t, fee, net)));
}

#[test]
fn an_unlocked_quote_cannot_settle() {
    let (mut q, fee, net) = locked(1_000 * GLC, PRICE_SCALE, PRICE_SCALE, 300);
    q.locked_at = None;
    assert!(matches!(
        verify_request_amounts(Some(&q), 1_000 * GLC, 300, fee, net, 1),
        Err(ConversionError::QuoteNotLocked { quoted_at: 100 })
    ));
}

#[test]
fn a_legacy_row_verifies_exactly_as_before_v37() {
    let fb = compute_fee_at_bps(CanonicalAtomic(500_000), 600).unwrap();
    assert_eq!(
        verify_request_amounts(None, 500_000, 600, fb.fee.0, fb.net.0, 1).unwrap(),
        fb
    );
    assert!(matches!(
        verify_request_amounts(None, 500_000, 600, fb.fee.0 + 1, fb.net.0 - 1, 1),
        Err(ConversionError::AccountingMismatch { .. })
    ));
}

#[test]
fn the_expected_net_for_an_observed_gross_is_priced_at_the_persisted_rate() {
    let (q, _, _) = locked(1_000 * GLC, 800_000_000_000, PRICE_SCALE, 300);
    // Quoted: the on-chain gross is valued at 0.8 first.
    assert_eq!(
        expected_net_for_gross(Some(&q), CanonicalAtomic(1_000 * GLC), 300, 1).unwrap(),
        CanonicalAtomic(776 * GLC)
    );
    // Legacy: the fee rule alone.
    assert_eq!(
        expected_net_for_gross(None, CanonicalAtomic(1_000 * GLC), 300, 1).unwrap(),
        CanonicalAtomic(970 * GLC)
    );
}

#[test]
fn the_rate_renders_with_twelve_places_by_integer_arithmetic() {
    assert_eq!(format_rate_e12(PRICE_SCALE, PRICE_SCALE), "1.000000000000");
    assert_eq!(format_rate_e12(1, PRICE_SCALE), "0.000000000001");
    assert_eq!(
        format_rate_e12(123 * PRICE_SCALE, PRICE_SCALE),
        "123.000000000000"
    );
    assert_eq!(format_rate_e12(PRICE_SCALE, 0), "0.000000000000");
}

// ---------------------------------------------------------------------
// Phase 2A has no feed: the book is pure arithmetic over fixed prices.
// ---------------------------------------------------------------------

#[test]
fn the_quote_math_and_the_book_never_reach_a_network() {
    let source = include_str!("../bridge_rate.rs");
    for forbidden in [
        "reqwest",
        "http://",
        "https://",
        "tokio::",
        "RpcClient",
        "eth_call",
    ] {
        assert!(
            !source.contains(forbidden),
            "bridge_rate.rs must not reach a network in Phase 2A (found {forbidden:?})"
        );
    }
    assert_eq!(
        RateBook::fixed_unit(60)
            .route_status(Route::RhnToSol, 7)
            .unwrap(),
        (RailPrices::unit(7), None)
    );
}

// ---------------------------------------------------------------------
// Destination-precision flooring (founder decision J-7).
// ---------------------------------------------------------------------

#[test]
fn a_non_unit_rate_floors_the_net_to_the_destination_unit_and_keeps_the_dust() {
    // 1 000 GLC at 1/3 to a 6-decimal destination (scale 100): the raw
    // net 32_333_333_334 floors to 32_333_333_300; the 34 canonical
    // atomic units below one mint unit stay with the bridge.
    let q = compute_bridge_quote(
        CanonicalAtomic(1_000 * GLC),
        prices(PRICE_SCALE, 3 * PRICE_SCALE),
        300,
        0,
        60,
        100,
    )
    .unwrap();
    assert_eq!(q.gross_out.0, 33_333_333_333);
    assert_eq!(q.fee_out.0, 999_999_999);
    assert_eq!(q.net_out.0, 32_333_333_300);
    assert_eq!(q.dust_out.0, 34);
    assert_eq!(q.gross_out.0, q.fee_out.0 + q.net_out.0 + q.dust_out.0);
    // The persisted triple verifies at the same scale, and at no other.
    let ok = verify_quoted_breakdown(
        1_000 * GLC,
        PRICE_SCALE,
        3 * PRICE_SCALE,
        300,
        q.gross_out.0,
        q.fee_out.0,
        q.net_out.0,
        100,
    );
    assert_eq!(ok.unwrap().net.0, 32_333_333_300);
    assert!(matches!(
        verify_quoted_breakdown(
            1_000 * GLC,
            PRICE_SCALE,
            3 * PRICE_SCALE,
            300,
            q.gross_out.0,
            q.fee_out.0,
            q.net_out.0,
            1
        ),
        Err(ConversionError::QuoteMismatch { .. })
    ));
    // A unit rate on an exactly-representable amount floors nothing.
    let unit = compute_bridge_quote(
        CanonicalAtomic(500_000),
        RailPrices::unit(0),
        300,
        0,
        60,
        100,
    )
    .unwrap();
    assert_eq!(unit.dust_out.0, 0);
    assert_eq!(unit.net_out.0, 485_000);
}

#[test]
fn the_dust_is_always_below_one_destination_unit_and_never_reaches_the_net() {
    let mut state: u64 = 0xD1B5_4A32_D192_ED03;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..10_000 {
        let gross_in = next() % (1_000_000 * GLC);
        let src = 1 + next() % (10 * PRICE_SCALE);
        let dst = PRICE_SCALE + next() % (10 * PRICE_SCALE);
        let fee_bps = next() % 10_001;
        let scale = [1u64, 10, 100, 1_000][(next() % 4) as usize];
        let qb = quoted_breakdown(CanonicalAtomic(gross_in), src, dst, fee_bps, scale).unwrap();
        assert!(
            qb.dust_out.0 < scale,
            "dust {} at scale {scale}",
            qb.dust_out.0
        );
        assert_eq!(
            qb.net_out.0 % scale,
            0,
            "net is a whole number of destination units"
        );
        assert_eq!(qb.gross_out.0, qb.fee_out.0 + qb.net_out.0 + qb.dust_out.0);
        let raw = compute_fee_at_bps(qb.gross_out, fee_bps).unwrap();
        assert!(
            qb.net_out.0 <= raw.net.0,
            "flooring never pays more than the raw net"
        );
        assert_eq!(qb.fee_out, raw.fee, "the fee is untouched by the floor");
        assert_eq!(destination_scale_for_decimals(6), 100);
        assert_eq!(destination_scale_for_decimals(8), 1);
        assert_eq!(destination_scale_for_decimals(18), 1);
    }
}

#[test]
fn a_zero_destination_scale_is_refused() {
    assert!(matches!(
        quoted_breakdown(CanonicalAtomic(1), PRICE_SCALE, PRICE_SCALE, 300, 0),
        Err(ConversionError::InvalidDestinationScale)
    ));
}

// ---------------------------------------------------------------------
// The live book behind the same `quote` call.
// ---------------------------------------------------------------------

#[test]
fn a_live_book_refuses_until_warm_then_quotes_and_flags_a_breach() {
    use crate::bridge_rate::smoothing::Sample;
    use crate::routes::Chain;
    let live = std::sync::Arc::new(LiveBook::new(LiveRateConfig {
        price_window_secs: 360,
        price_staleness_secs: 120,
        rate_band_bps: 2_500,
    }));
    let book = RateBook::live(60, std::sync::Arc::clone(&live));
    assert!(book.is_live());
    let refused = book
        .quote(Route::GlcToSol, CanonicalAtomic(GLC), 300, 0, 100)
        .unwrap_err();
    assert!(matches!(
        refused,
        RateError::Refused(RateRefusal::FeedUnavailable { .. })
    ));
    assert_eq!(refused.reason(), "bridge_rate_feed_unavailable");
    let feed = |chain: Chain, from: i64, to: i64, price: u64| {
        let mut t = from;
        while t <= to {
            live.record_sample(
                chain,
                Sample {
                    feed_at: t,
                    observed_at: t,
                    price_e12: price,
                },
            );
            t += 30;
        }
    };
    feed(Chain::Goldcoin, 0, 359, PRICE_SCALE);
    feed(Chain::Goldcoin, 360, 720, 2 * PRICE_SCALE);
    feed(Chain::Solana, 0, 720, PRICE_SCALE);
    assert!(matches!(
        book.quote(Route::GlcToSol, CanonicalAtomic(GLC), 300, 700, 100),
        Err(RateError::Refused(RateRefusal::WarmingUp { .. }))
    ));
    // Warm at t=720: Goldcoin doubled against the reference -> 100% move,
    // a breach; the quote is nonetheless struck at the live 2.0 rate.
    let struck = book
        .quote(Route::GlcToSol, CanonicalAtomic(GLC), 300, 720, 100)
        .unwrap();
    assert_eq!(struck.quote.rate_display(), "2.000000000000");
    assert_eq!(struck.quote.gross_out.0, 2 * GLC);
    assert_eq!(struck.quote.source_feed_at, 720);
    let breach = struck.band.expect("a 100% move breaches a 25% band");
    assert_eq!(breach.movement_bps, 10_000);
    assert_eq!(breach.band_bps, 2_500);
    assert!(struck.route_rate.unwrap().band_exceeded);
    // The reciprocal route is priced at 0.5 and breaches by 50%.
    let inverse = book
        .quote(Route::SolToGlc, CanonicalAtomic(2 * GLC), 300, 720, 1)
        .unwrap();
    assert_eq!(inverse.quote.gross_out.0, GLC);
    assert_eq!(inverse.band.unwrap().movement_bps, 5_000);
}

// ---------------------------------------------------------------------
// The destination-bound maximum (docs/40-destination-bound-admission.md).
// ---------------------------------------------------------------------

/// The live prices at which requests 4438 and 4483 were quoted on
/// 2026-09-18 (`POST /quote` reproduction in the incident report): GLC
/// (Goldcoin) at 731_245_672 e12, GLC (Solana) at 43_669_983 e12 — a
/// rate of ~16.7 Solana units per Goldcoin unit.
const INCIDENT_PRICES: RailPrices = RailPrices {
    source_price_e12: 731_245_672,
    destination_price_e12: 43_669_983,
    source_feed_at: 1_000,
    destination_feed_at: 1_000,
};
/// The program's `per_transfer_limit`: 50_000 GLC at the mint's 6
/// decimals = 50_000_000_000 mint units, widened to canonical.
const INCIDENT_LIMIT_CANONICAL: u64 = 50_000 * GLC;
const SOLANA_SCALE: u64 = 100;

fn net_at(gross: u64, prices: RailPrices, fee_bps: u64, scale: u64) -> Option<u64> {
    quoted_breakdown(
        CanonicalAtomic(gross),
        prices.source_price_e12,
        prices.destination_price_e12,
        fee_bps,
        scale,
    )
    .ok()
    .map(|q| q.net_out.0)
}

/// `max` is a boundary, not an estimate: the net at `max` fits and the
/// net one unit above does not.
fn assert_exact_boundary(
    limit: u64,
    prices: RailPrices,
    fee_bps: u64,
    scale: u64,
    buffer_bps: u64,
) -> u64 {
    let buffered = buffered_destination_limit(CanonicalAtomic(limit), buffer_bps).0;
    let max = max_source_for_destination_limit(
        CanonicalAtomic(limit),
        prices,
        fee_bps,
        scale,
        buffer_bps,
    )
    .unwrap()
    .0;
    if buffered < scale || fee_bps == BPS_DENOMINATOR {
        assert_eq!(
            max, 0,
            "nothing deliverable fits a buffered limit of {buffered}"
        );
        return 0;
    }
    let at = net_at(max, prices, fee_bps, scale).expect("the maximum itself quotes");
    assert!(
        at <= buffered,
        "net at max {max} is {at}, above the buffered limit {buffered}"
    );
    // A maximum of zero is a real answer at an extreme rate: one canonical
    // unit already nets above the limit. Otherwise the net at the maximum
    // is deliverable — at least one destination unit.
    if max > 0 {
        assert!(at >= scale, "the net at the maximum ({at}) is deliverable");
    }
    if max < u64::MAX {
        // One unit more either does not quote at all (overflow) or nets
        // above the buffered limit.
        if let Some(above) = net_at(max + 1, prices, fee_bps, scale) {
            assert!(
                above > buffered,
                "net at max+1 ({}) is {above}, still within the buffered limit {buffered}",
                max + 1
            );
        }
    }
    max
}

#[test]
fn the_buffer_reduces_the_limit_by_basis_points_and_never_rounds_up() {
    assert_eq!(
        buffered_destination_limit(CanonicalAtomic(10_000), 0).0,
        10_000
    );
    assert_eq!(
        buffered_destination_limit(CanonicalAtomic(10_000), 2_500).0,
        7_500
    );
    assert_eq!(
        buffered_destination_limit(CanonicalAtomic(10_000), 10_000).0,
        0
    );
    assert_eq!(
        buffered_destination_limit(CanonicalAtomic(10_000), 20_000).0,
        0
    );
    // 7 × 0.75 = 5.25 → 5, never 6.
    assert_eq!(buffered_destination_limit(CanonicalAtomic(7), 2_500).0, 5);
    assert_eq!(
        buffered_destination_limit(CanonicalAtomic(u64::MAX), 2_500).0,
        (u128::from(u64::MAX) * 7_500 / 10_000) as u64
    );
    assert_eq!(DEFAULT_DESTINATION_LIMIT_BUFFER_BPS, 2_500);
}

/// At a unit rate the maximum is the fee rule inverted, and the
/// destination floor (J-7) is honoured: a limit of 1_000 at scale 100
/// admits a gross of 1_099 (net 1_099 → floored 1_000) but not 1_100.
#[test]
fn at_a_unit_rate_the_maximum_inverts_the_fee_and_honours_the_floor() {
    let unit = prices(PRICE_SCALE, PRICE_SCALE);
    assert_eq!(assert_exact_boundary(1_000, unit, 0, 1, 0), 1_000);
    assert_eq!(assert_exact_boundary(1_000, unit, 0, 100, 0), 1_099);
    // 300 bps: net(g) = g − ⌊g·300/10000⌋. net(1030) = 1030 − 30 = 1000;
    // net(1031) = 1031 − 30 = 1001.
    assert_eq!(assert_exact_boundary(1_000, unit, 300, 1, 0), 1_030);
    // With the established 25 % buffer the limit 1_000 admits 750 net:
    // net(773) = 773 − 23 = 750; net(774) = 774 − 23 = 751.
    assert_eq!(assert_exact_boundary(1_000, unit, 300, 1, 2_500), 773);
}

/// The 2026-09-18 figures, pinned: at the incident's live rate a 50_000
/// GLC limit admits at most 3_078.34991410 GLC (buffer 0) and, with the
/// established 25 % band as buffer, 2_308.76243559 GLC — and 50_000 GLC
/// (what 4438 and 4483 sent) is far outside both.
#[test]
fn the_incident_rate_yields_the_reported_maximum_and_refuses_the_incident_amount() {
    let unbuffered = assert_exact_boundary(
        INCIDENT_LIMIT_CANONICAL,
        INCIDENT_PRICES,
        300,
        SOLANA_SCALE,
        0,
    );
    assert_eq!(unbuffered, 307_834_991_410);
    let buffered = assert_exact_boundary(
        INCIDENT_LIMIT_CANONICAL,
        INCIDENT_PRICES,
        300,
        SOLANA_SCALE,
        DEFAULT_DESTINATION_LIMIT_BUFFER_BPS,
    );
    assert_eq!(buffered, 230_876_243_559);
    assert!(buffered < unbuffered);
    // The incident amount nets to ~812_123.40 GLC (Solana) — 16× the limit.
    let incident_net = net_at(50_000 * GLC, INCIDENT_PRICES, 300, SOLANA_SCALE).unwrap();
    assert!(incident_net > INCIDENT_LIMIT_CANONICAL);
    assert_eq!(incident_net, 81_212_340_046_000);
    assert!(50_000 * GLC > unbuffered);
}

/// A buffer of 100 % (or a zero limit, or a limit below one destination
/// unit) admits nothing; a fee of 100 % admits nothing.
#[test]
fn a_maximum_of_zero_when_nothing_could_fit() {
    let unit = prices(PRICE_SCALE, PRICE_SCALE);
    assert_eq!(assert_exact_boundary(1_000, unit, 300, 1, 10_000), 0);
    assert_eq!(assert_exact_boundary(0, unit, 300, 1, 0), 0);
    assert_eq!(assert_exact_boundary(99, unit, 0, 100, 0), 0);
    assert_eq!(assert_exact_boundary(1_000, unit, 10_000, 1, 0), 0);
    // Invalid inputs are refused, never answered with a number.
    assert!(max_source_for_destination_limit(CanonicalAtomic(1), unit, 300, 0, 0).is_err());
    assert!(max_source_for_destination_limit(CanonicalAtomic(1), unit, 10_001, 1, 0).is_err());
    assert!(max_source_for_destination_limit(CanonicalAtomic(1), prices(0, 1), 300, 1, 0).is_err());
}

/// The integer property test: across prices spanning six orders of
/// magnitude each way, every fee the config admits, every destination
/// precision and limits from one unit to the whole canonical range, the
/// maximum is ALWAYS the exact boundary. Deterministic (a fixed
/// xorshift stream), so a failure reproduces.
#[test]
fn max_source_is_the_exact_boundary() {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut cases = 0u32;
    for _ in 0..4_000 {
        // Prices between 1e6 and 1e12 e12 (1e-6 .. 1.0 of the reference
        // asset), so the rate ranges over 1e-6 .. 1e6.
        let src = 10u64.pow(6 + (next() % 7) as u32) + next() % 1_000_000;
        let dst = 10u64.pow(6 + (next() % 7) as u32) + next() % 1_000_000;
        let fee_bps = [0u64, 1, 100, 300, 450, 600, 2_500, 9_999][(next() % 8) as usize];
        let scale = [1u64, 10, 100, 1_000_000][(next() % 4) as usize];
        let buffer_bps = [0u64, 1, 2_500, 5_000, 9_999][(next() % 5) as usize];
        let limit = match next() % 4 {
            0 => next() % 1_000,
            1 => next() % (1_000 * GLC),
            2 => next() % (100_000_000 * GLC),
            _ => next(),
        };
        assert_exact_boundary(limit, prices(src, dst), fee_bps, scale, buffer_bps);
        cases += 1;
    }
    assert_eq!(cases, 4_000);
    // And the two production shapes, at the extremes of the limit range.
    for limit in [
        1u64,
        100,
        20_000 * GLC,
        50_000 * GLC,
        u64::MAX / 2,
        u64::MAX,
    ] {
        assert_exact_boundary(limit, INCIDENT_PRICES, 300, SOLANA_SCALE, 2_500);
        assert_exact_boundary(limit, prices(PRICE_SCALE, PRICE_SCALE), 600, 1, 2_500);
    }
}

// ---------------------------------------------------------------------
// The two parked requests (4438, 4483) against candidate program limits.
// ---------------------------------------------------------------------

/// Requests 4438 and 4483 as the ledger holds them (locked quotes read
/// from `GET /transfers/{id}` on 2026-09-20): 50_000 GLC gross, 300 bps,
/// locked rail prices, and the nets the settlement path must pay. Both
/// were parked `destination_payout_out_of_bounds` against a 50_000 GLC
/// (Solana) `per_transfer_limit`. This pins, for each candidate limit,
/// (a) the SETTLEMENT verdict — `Orchestrator::release_out_of_bounds`
/// compares the locked net in mint units to the raw limit, no buffer —
/// and (b) the ADMISSION verdict a NEW identical request would get from
/// docs/40's buffered check. The quote arithmetic is reproduced from the
/// locked prices bit for bit, so the ledger figures are the oracle.
#[test]
fn requests_4438_and_4483_against_candidate_per_transfer_limits() {
    let fee_bps = 300;
    let gross = CanonicalAtomic(50_000 * GLC);
    let cases = [
        (
            4438u32,
            prices(983_906_422, 50_389_216),
            94_701_734_329_400u64,
        ),
        (4483, prices(772_204_750, 45_187_267), 82_881_601_082_400),
    ];
    for (id, p, locked_net) in cases {
        let q = quoted_breakdown(
            gross,
            p.source_price_e12,
            p.destination_price_e12,
            fee_bps,
            SOLANA_SCALE,
        )
        .unwrap();
        assert_eq!(
            q.net_out.0, locked_net,
            "request {id}: the locked quote reproduces"
        );
        let net_mint_units = locked_net / SOLANA_SCALE;
        // (a) settlement: pays iff net ≤ per_transfer_limit (mint units).
        for (limit_mint_units, pays) in [
            (50_000_000_000u64, false), // today: parked (the incident)
            (1_000_000_000_000, true),  // 1_000_000 GLC (Solana)
            (2_000_000_000_000, true),  // 2_000_000 GLC (Solana)
        ] {
            assert_eq!(
                net_mint_units <= limit_mint_units,
                pays,
                "request {id}: settlement at limit {limit_mint_units}"
            );
        }
        // (b) admission of a NEW identical request under docs/40, at the
        // established 25 % buffer and with no buffer.
        for (limit_mint_units, buffer_bps, admitted) in [
            (1_000_000_000_000u64, 2_500u64, false), // 947k / 829k > 750k
            (1_000_000_000_000, 0, true),            // both ≤ 1_000_000
            (2_000_000_000_000, 2_500, true),        // both ≤ 1_500_000
            (2_000_000_000_000, 0, true),
        ] {
            let max = max_source_for_destination_limit(
                CanonicalAtomic(limit_mint_units * SOLANA_SCALE),
                p,
                fee_bps,
                SOLANA_SCALE,
                buffer_bps,
            )
            .unwrap()
            .0;
            assert_eq!(
                gross.0 <= max,
                admitted,
                "request {id}: admission at limit {limit_mint_units} buffer {buffer_bps} (max {max})"
            );
        }
    }
    // The exact nets, in mint units, beside the candidates.
    assert_eq!(94_701_734_329_400 / SOLANA_SCALE, 947_017_343_294);
    assert_eq!(82_881_601_082_400 / SOLANA_SCALE, 828_816_010_824);
}
