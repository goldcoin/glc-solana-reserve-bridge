//! Solana-side indexer: watches `BridgeConfig.obligation_count` at
//! `finalized` commitment and folds newly observed `WithdrawalObligation`
//! accounts into the ledger (docs/03-architecture.md, docs/06-schema.md).
//!
//! # Why this doesn't scan transaction history
//!
//! A `WithdrawalObligation` PDA's address is fully determined by its index
//! (`programs/glc-reserve-bridge/src/constants.rs`'s
//! `SEED_WITHDRAWAL_OBLIGATION` seed), and `BridgeConfig.obligation_count`
//! is the authoritative count of how many exist. So new deposits are
//! discovered by comparing the live count against
//! `Ledger::last_synced_obligation_count` and directly fetching the
//! resulting PDA range — no `getSignaturesForAddress`/`getTransaction`
//! parsing needed. This is simpler, cheaper, and (unlike history scanning)
//! has no pagination/ordering edge cases to get wrong.
//!
//! # Fail-closed behavior
//!
//! - `obligation_count` observed lower than what was already synced is
//!   treated as a hard error, never as "nothing changed" — finalized
//!   commitment is supposed to be monotonic, so this can only mean stale
//!   RPC state, an unexpected redeploy, or a misconfigured endpoint; the
//!   caller must not proceed.
//! - If the account for an index inside `[last_synced, count)` is missing
//!   (RPC returned `None` where the config says it must exist), the sync
//!   cursor is NOT advanced past it — the tick errors and the same range is
//!   retried next tick, rather than silently skipping a real deposit.

use thiserror::Error;

use crate::ledger::{Ledger, LedgerError};

use super::accounts::{self, decode_bridge_config, decode_withdrawal_obligation};
use super::rpc::{SolanaRpc, SolanaRpcError};

#[derive(Debug, Error)]
pub enum SolanaIndexerError {
    #[error("Solana node unavailable: {0}")]
    NodeUnavailable(SolanaRpcError),
    #[error("Solana RPC error: {0}")]
    Rpc(SolanaRpcError),
    #[error(
        "bridge_config account does not exist at {0} — bridge not initialized on this cluster"
    )]
    NotInitialized(solana_sdk::pubkey::Pubkey),
    #[error(
        "observed obligation_count {observed} is LESS than last synced {last_synced} — finalized \
         commitment must be monotonic; refusing to proceed on inconsistent chain state"
    )]
    StaleOrInconsistentChainState { last_synced: u64, observed: u64 },
    #[error("obligation account at index {0} is missing though bridge_config reports it exists")]
    MissingObligationAccount(u64),
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolanaTickOutcome {
    NoNewObligations,
    Folded { count: u64 },
}

const INNER_RETRY_ATTEMPTS: u32 = 3;

pub struct SolanaIndexer<R: SolanaRpc> {
    rpc: R,
    ledger: Ledger,
    /// The rate NEW `SolToGlc` requests price at, resolved by ROUTE from
    /// `[fees]` at config load (`crate::fees::RouteFees`).
    ///
    /// Held as a value this indexer was GIVEN rather than read from a
    /// constant, for the same reason `robinhood::Settler` holds its own:
    /// the rate belongs to this route, and a component that reached for a
    /// global would be one edit away from charging Solana's depositors
    /// Robinhood's price. Snapshotted onto each request at fold time and
    /// immutable thereafter, so an in-flight request keeps settling at
    /// the rate it was created under when this value changes.
    fee_bps: u64,
    /// The `SolToRhn` fold's inputs, present only when that route is
    /// PRICED (`[fees].SolToRhn`) — see [`SolanaIndexer::with_sol_to_rhn`].
    ///
    /// # What decides which route a Solana deposit is on
    ///
    /// The Solana program records no route: `deposit_to_reserve` stores
    /// an opaque destination payload and nothing else. The destination's
    /// SPELLING is what tells the two apart, and it does so structurally
    /// rather than heuristically: a Robinhood destination is the ASCII
    /// text `0x` followed by 40 hex digits, and `0` is not in the base58
    /// alphabet, so no Goldcoin address can ever begin with `0x`. The
    /// same argument [`crate::ledger::TransferAddressFilter`] already
    /// relies on. A payload that begins with `0x` but is not a valid EVM
    /// address (bad length, bad hex, failed EIP-55 checksum) is still a
    /// Robinhood-bound deposit — it is folded as `SolToRhn`, parked as
    /// undeliverable, and refundable on Solana — never a Goldcoin one.
    ///
    /// `None` means classification is OFF and every deposit folds as
    /// `SolToGlc`, exactly as before the route existed.
    sol_to_rhn: Option<SolToRhnFold>,
    /// The source-side gross floor this indexer folds against.
    ///
    /// Always [`crate::min_transfer::SOURCE_MINIMUM_CANONICAL`] in
    /// production — [`SolanaIndexer::new`] sets it unconditionally and no
    /// config key reaches it. A field only so this module's tests, whose
    /// fixtures deposit a few hundred thousand mint-atomic units, can opt
    /// down to the floor they were written against without being
    /// rewritten around a policy none of them is exercising.
    source_minimum: crate::amount_conversion::CanonicalAtomic,
    /// Where every fold strikes its bridge quote (`crate::bridge_rate`).
    /// The fold is the lock for a Solana-sourced deposit, so the quote
    /// written here is the one the request settles at.
    rate_book: crate::bridge_rate::RateBook,
    /// The Robinhood contract's live limits, for the `SolToRhn` fold's
    /// destination-bound check (docs/40-destination-bound-admission.md)
    /// — see [`SolanaIndexer::with_robinhood_destination_limits`]. `None`
    /// = no contract configured; the fold then keeps its pre-existing
    /// behaviour and `Settler::authorize_payout` remains the only check.
    robinhood_limits: Option<RobinhoodDestinationLimits>,
}

/// See [`SolanaIndexer::with_robinhood_destination_limits`].
pub struct RobinhoodDestinationLimits {
    pub source: std::sync::Arc<dyn crate::robinhood::public::RobinhoodContractSource>,
    /// `[bridge_rate] destination_limit_buffer_bps`.
    pub buffer_bps: u64,
}

/// See [`SolanaIndexer::with_sol_to_rhn`].
pub struct SolToRhnFold {
    pub fee_bps: u64,
    pub route_gate: std::sync::Arc<crate::routes::RouteGate>,
}

/// Whether a Solana deposit's opaque destination payload names a
/// Robinhood (EVM) address rather than a Goldcoin one — by its `0x`
/// prefix alone, which no base58 string can carry. Says nothing about
/// whether the address is VALID; that is [`parse_robinhood_destination`]'s
/// job.
pub fn destination_is_robinhood(payload: &[u8]) -> bool {
    payload.starts_with(b"0x")
}

/// Parses a Robinhood-bound destination payload as the EVM address the
/// payout will be sent to: `0x` + 40 hex digits, EIP-55 checksum
/// honoured when the spelling carries one (`EvmAddress`'s own rule), and
/// never the zero address — the EVM burn sink, which `POST /transfers`
/// refuses for `GlcToRhn` for the same reason.
pub fn parse_robinhood_destination(
    payload: &[u8],
) -> Result<crate::evm::address::EvmAddress, String> {
    let text = std::str::from_utf8(payload)
        .map_err(|_| "the destination payload is not valid UTF-8".to_string())?;
    let address = text
        .parse::<crate::evm::address::EvmAddress>()
        .map_err(|e| e.to_string())?;
    if address.is_zero() {
        return Err("the zero address is the EVM burn sink, not a payout destination".to_string());
    }
    Ok(address)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl<R: SolanaRpc> SolanaIndexer<R> {
    pub fn new(rpc: R, ledger: Ledger, fee_bps: u64) -> Self {
        SolanaIndexer {
            rpc,
            ledger,
            fee_bps,
            sol_to_rhn: None,
            source_minimum: crate::min_transfer::SOURCE_MINIMUM_CANONICAL,
            rate_book: crate::bridge_rate::RateBook::fixed_unit(
                crate::bridge_rate::DEFAULT_QUOTE_LIFETIME_SECS,
            ),
            robinhood_limits: None,
        }
    }

    /// Installs the Robinhood contract reader the `SolToRhn` fold checks
    /// its quoted payout against (docs/40-destination-bound-admission.md).
    ///
    /// A Solana deposit is already irreversible when it is folded, so
    /// this is not an admission refusal — it is the EARLIEST park: a
    /// deposit whose quoted net would exceed the contract's `outboundMax`
    /// (less the buffer) is parked `destination_payout_out_of_bounds` at
    /// the fold, before any Robinhood capacity is held for it, instead of
    /// at `Settler::authorize_payout` after it was. The settler's own
    /// check is untouched and remains the second layer. The contract is
    /// read once per tick, and only on a tick that has Robinhood-bound
    /// deposits to fold; an unreadable contract is logged and the fold
    /// proceeds as before (the settler's check still refuses), because a
    /// read failure must not park every deposit of a healthy route.
    pub fn with_robinhood_destination_limits(
        mut self,
        limits: Option<RobinhoodDestinationLimits>,
    ) -> Self {
        self.set_robinhood_destination_limits(limits);
        self
    }

    /// The in-place form of
    /// [`SolanaIndexer::with_robinhood_destination_limits`], for a caller
    /// that has already handed this indexer to the orchestrator.
    pub fn set_robinhood_destination_limits(&mut self, limits: Option<RobinhoodDestinationLimits>) {
        self.robinhood_limits = limits;
    }

    /// Installs the bridge-rate book every fold strikes its quote from
    /// (docs/38-elastic-bridge-rate.md). The daemon calls this with the
    /// configured quote lifetime.
    pub fn with_rate_book(mut self, rate_book: crate::bridge_rate::RateBook) -> Self {
        self.rate_book = rate_book;
        self
    }

    /// Lowers the source-side floor. **Tests only** — see
    /// `crate::api::BridgeApi::with_source_minimum_for_tests`, which
    /// exists for the identical reason and carries the full rationale.
    #[doc(hidden)]
    pub fn with_source_minimum_for_tests(
        mut self,
        minimum: crate::amount_conversion::CanonicalAtomic,
    ) -> Self {
        self.source_minimum = minimum;
        self
    }

    /// Turns on `SolToRhn` classification: a deposit whose destination
    /// begins with `0x` folds as `SolToRhn` at `fold.fee_bps`, payable
    /// only while `fold.route_gate` reports the route open. Without this,
    /// every deposit folds as `SolToGlc` (see the field docs).
    pub fn with_sol_to_rhn(mut self, fold: SolToRhnFold) -> Self {
        self.sol_to_rhn = Some(fold);
        self
    }

    /// The in-place form of [`SolanaIndexer::with_sol_to_rhn`], for a
    /// caller that has already handed this indexer to the orchestrator.
    pub fn set_sol_to_rhn(&mut self, fold: Option<SolToRhnFold>) {
        self.sol_to_rhn = fold;
    }

    async fn call<T, F, Fut>(f: F) -> Result<T, SolanaIndexerError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, SolanaRpcError>>,
    {
        super::rpc::call_with_retry(INNER_RETRY_ATTEMPTS, f)
            .await
            .map_err(|e| {
                if e.is_retriable() {
                    SolanaIndexerError::NodeUnavailable(e)
                } else {
                    SolanaIndexerError::Rpc(e)
                }
            })
    }

    pub async fn tick(&mut self) -> Result<SolanaTickOutcome, SolanaIndexerError> {
        let slot = Self::call(|| self.rpc.get_slot()).await?;

        let config_pda = accounts::bridge_config_pda();
        let account = Self::call(|| self.rpc.get_account(&config_pda))
            .await?
            .ok_or(SolanaIndexerError::NotInitialized(config_pda))?;
        let config = decode_bridge_config(&account.data).map_err(SolanaIndexerError::Rpc)?;

        let last_synced = self.ledger.last_synced_obligation_count()?;
        if config.obligation_count < last_synced {
            return Err(SolanaIndexerError::StaleOrInconsistentChainState {
                last_synced,
                observed: config.obligation_count,
            });
        }
        if config.obligation_count == last_synced {
            self.ledger
                .set_last_synced_obligation_count(last_synced, slot, now_unix())?;
            return Ok(SolanaTickOutcome::NoNewObligations);
        }

        let new_indices: Vec<u64> = (last_synced..config.obligation_count).collect();
        let pdas: Vec<_> = new_indices
            .iter()
            .map(|i| accounts::withdrawal_obligation_pda(*i))
            .collect();
        let fetched = Self::call(|| self.rpc.get_multiple_accounts(&pdas)).await?;

        // Read once per tick, not per obligation — decimals are immutable
        // post-`InitializeMint`, and every obligation folded this tick
        // shares the same reserve mint (docs/20-bridge-fee.md).
        let solana_decimals = Self::call(|| {
            accounts::fetch_reserve_mint_decimals(&self.rpc, &config.reserve_token_mint)
        })
        .await?;

        let now = now_unix();
        // The enablement verdict for `SolToRhn`, read once per tick and
        // applied to every Robinhood-bound deposit folded in it — the same
        // once-per-tick discipline the Robinhood settlement loop keeps.
        let sol_to_rhn_open = self.sol_to_rhn.as_ref().map(|f| {
            f.route_gate
                .is_enabled(&self.ledger, crate::routes::Route::SolToRhn)
        });
        let snaps = fetched
            .into_iter()
            .zip(new_indices.iter())
            .map(|(maybe_account, index)| {
                let account =
                    maybe_account.ok_or(SolanaIndexerError::MissingObligationAccount(*index))?;
                decode_withdrawal_obligation(&account.data).map_err(SolanaIndexerError::Rpc)
            })
            .collect::<Result<Vec<_>, _>>()?;
        // The contract's limits, read ONCE per tick and only when this
        // tick has a Robinhood-bound deposit to check them against
        // (docs/40-destination-bound-admission.md). `None` = no contract
        // configured, or this read failed (logged): the fold then makes
        // no destination-bound decision and the settler's check decides.
        let robinhood_limits = match (&self.robinhood_limits, sol_to_rhn_open) {
            (Some(limits), Some(_))
                if snaps
                    .iter()
                    .any(|s| destination_is_robinhood(&s.glc_address)) =>
            {
                let status = limits.source.state().await;
                let read = crate::api::destination_limits_from(None, &status);
                if read.robinhood_unavailable {
                    tracing::warn!(
                        "Robinhood contract limits unreadable this tick; SolToRhn deposits fold \
                         without the destination-bound check (settlement still enforces it)"
                    );
                }
                Some((read, limits.buffer_bps))
            }
            _ => None,
        };
        for (index, snap) in new_indices.iter().zip(snaps) {
            if let (Some(fold), Some(route_open)) = (&self.sol_to_rhn, sol_to_rhn_open) {
                if destination_is_robinhood(&snap.glc_address) {
                    self.fold_to_robinhood(
                        &snap,
                        fold.fee_bps,
                        solana_decimals,
                        route_open,
                        robinhood_limits.as_ref(),
                        now,
                    )?;
                    continue;
                }
            }
            // `snap.amount` is the raw on-chain obligation's GROSS amount,
            // in the reserve mint's own live decimals. Widening to
            // canonical is always exact; the destination for SolToGlc is
            // Goldcoin, whose native unit already IS canonical, so no
            // further conversion is needed for `net_destination_atomic`
            // (docs/20-bridge-fee.md).
            let gross_canonical = crate::amount_conversion::SolanaAtomic(snap.amount)
                .to_canonical(solana_decimals)
                .map_err(|e| {
                    SolanaIndexerError::Rpc(SolanaRpcError::Malformed(format!(
                        "obligation {index}: {e}"
                    )))
                })?;
            // `SolToGlc`'s own configured rate — never the compiled-in
            // global, which is what this used to read. Struck as a bridge
            // quote (docs/38-elastic-bridge-rate.md): the fold is the
            // lock, and the quote written with the row is the one it
            // settles at.
            //
            // Goldcoin's unit IS canonical: destination scale 1.
            let pricing = crate::bridge_rate::price_final_deposit(
                &self.rate_book,
                crate::routes::Route::SolToGlc,
                gross_canonical,
                self.fee_bps,
                now,
                1,
            )
            .map_err(|e| {
                SolanaIndexerError::Rpc(SolanaRpcError::Malformed(format!(
                    "obligation {index}: {e}"
                )))
            })?;
            let fee_breakdown = pricing.breakdown;
            let amounts = crate::ledger::RequestAmounts {
                gross_atomic: gross_canonical.0,
                fee_bps: fee_breakdown.fee_bps,
                fee_atomic: fee_breakdown.fee.0,
                net_atomic: fee_breakdown.net.0,
                net_destination_atomic: fee_breakdown.net.0,
                quote: pricing.quote,
            };
            // The source-side minimum, against the GROSS this depositor
            // actually sent. The Solana program's own `min_transfer_amount`
            // is the first line of defence, but it is ONE config value
            // serving two roles — the gross floor here, and the NET floor
            // `release_from_reserve` applies — so lowering it far enough
            // for a minimum transfer to be DELIVERABLE necessarily lowers
            // what a deposit may be. That gap is closed here and nowhere
            // else.
            //
            // Parked, never dropped: the deposit is final and the tokens
            // are in the reserve. A recorded request that holds no
            // capacity and carries its reason is what makes it visible
            // and refundable.
            let refusal = crate::min_transfer::enforce_source_minimum_at(
                crate::routes::Route::SolToGlc,
                gross_canonical,
                self.source_minimum,
            )
            .err()
            .map(|e| format!("below source minimum: {e}"))
            // The bridge-rate park (band breach, or a refused quote)
            // ranks after the source-side floor: a deposit under the
            // minimum is reported for the minimum, which is the fact a
            // refund decision rests on.
            .or_else(|| pricing.park.map(str::to_string));
            self.ledger.fold_sol_deposit(
                snap.index,
                amounts,
                snap.requester.to_bytes(),
                &snap.glc_address,
                refusal.as_deref(),
                now,
            )?;
        }

        self.ledger
            .set_last_synced_obligation_count(config.obligation_count, slot, now)?;
        Ok(SolanaTickOutcome::Folded {
            count: new_indices.len() as u64,
        })
    }

    /// Folds one Robinhood-bound obligation as `SolToRhn`.
    ///
    /// The amount path is the `SolToGlc` one up to the net: raw mint
    /// units widened exactly to canonical, the fee at THIS route's rate.
    /// The destination reserve (`RobinhoodReserve`) is accounted in
    /// canonical units, so `net_destination_atomic` is the canonical net;
    /// the 18-decimal widening the payout will perform is exercised here
    /// and discarded purely to prove deliverability before any capacity
    /// is held, exactly as `POST /transfers` does for `GlcToRhn`.
    ///
    /// An undeliverable destination (a `0x` payload that is not a valid
    /// EVM address, or the zero address — the EVM burn sink) folds
    /// PARKED rather than refused: the deposit is real and irreversible,
    /// and the park is what makes it visible and refundable.
    fn fold_to_robinhood(
        &mut self,
        snap: &accounts::WithdrawalObligationSnapshot,
        fee_bps: u64,
        solana_decimals: u8,
        route_open: bool,
        robinhood_limits: Option<&(crate::api::DestinationLimits, u64)>,
        now: i64,
    ) -> Result<(), SolanaIndexerError> {
        let index = snap.index;
        let gross_canonical = crate::amount_conversion::SolanaAtomic(snap.amount)
            .to_canonical(solana_decimals)
            .map_err(|e| {
                SolanaIndexerError::Rpc(SolanaRpcError::Malformed(format!(
                    "obligation {index}: {e}"
                )))
            })?;
        // Robinhood's 18-decimal unit is finer than canonical: scale 1.
        let pricing = crate::bridge_rate::price_final_deposit(
            &self.rate_book,
            crate::routes::Route::SolToRhn,
            gross_canonical,
            fee_bps,
            now,
            1,
        )
        .map_err(|e| {
            SolanaIndexerError::Rpc(SolanaRpcError::Malformed(format!(
                "obligation {index}: {e}"
            )))
        })?;
        let fee_breakdown = pricing.breakdown;
        // Always exact for any canonical amount; still checked rather
        // than assumed, and a failure here is a fold-time refusal to
        // hold capacity for a payout the settler would then refuse.
        fee_breakdown.net.to_robinhood().map_err(|e| {
            SolanaIndexerError::Rpc(SolanaRpcError::Malformed(format!(
                "obligation {index}: net entitlement is not representable at Robinhood's \
                 precision: {e}"
            )))
        })?;
        let amounts = crate::ledger::RequestAmounts {
            gross_atomic: gross_canonical.0,
            fee_bps: fee_breakdown.fee_bps,
            fee_atomic: fee_breakdown.fee.0,
            net_atomic: fee_breakdown.net.0,
            net_destination_atomic: fee_breakdown.net.0,
            quote: pricing.quote,
        };
        let recipient = match parse_robinhood_destination(&snap.glc_address) {
            Ok(address) => Ok(address),
            Err(detail) => Err(format!("undeliverable destination: {detail}")),
        };
        // Same source-side floor, same reason as `SolToGlc` above — this
        // route's source leg is the identical Solana deposit. The
        // destination refusal keeps precedence: a deposit that is both
        // undeliverable and under the minimum is reported for the
        // destination, which is the fact a refund decision rests on.
        let below_minimum = crate::min_transfer::enforce_source_minimum_at(
            crate::routes::Route::SolToRhn,
            gross_canonical,
            self.source_minimum,
        )
        .err()
        .map(|e| format!("below source minimum: {e}"));
        // The destination-bound check at the fold's own locked prices
        // (docs/40-destination-bound-admission.md): the SAME derivation
        // `POST /transfers` admits a Goldcoin deposit with, so a
        // Robinhood-bound Solana deposit whose quoted payout the contract
        // would refuse is parked `destination_payout_out_of_bounds` HERE
        // — before any Robinhood capacity is held — and not first at
        // `Settler::authorize_payout`. A limit that could not be read
        // makes no decision (see `with_robinhood_destination_limits`).
        // A refused rate carries no quote and is parked for the refusal
        // (`pricing.park`); there is then no price to check a bound at.
        let over_destination_bound = robinhood_limits.zip(pricing.quote.as_ref()).and_then(
            |((limits, buffer_bps), quote)| match crate::api::max_transfer_from(
                crate::routes::Route::SolToRhn,
                limits,
                Some(fee_bps),
                Some(quote.rail_prices()),
                *buffer_bps,
            ) {
                crate::api::MaxTransfer::Known(max) if gross_canonical.0 > max.0 => {
                    tracing::warn!(
                        obligation_index = index,
                        gross_canonical = gross_canonical.0,
                        max_transfer_canonical = max.0,
                        "SolToRhn deposit exceeds the destination-bound maximum at its locked \
                         quote; parking at the fold"
                    );
                    Some(
                        crate::ledger::Ledger::MANUAL_REVIEW_REASON_DESTINATION_PAYOUT_OUT_OF_BOUNDS
                            .to_string(),
                    )
                }
                _ => None,
            },
        );
        let refusal = match recipient.as_ref().err() {
            Some(destination) => Some(destination.clone()),
            None => below_minimum
                .or(over_destination_bound)
                .or_else(|| pricing.park.map(str::to_string)),
        };
        self.ledger.fold_sol_deposit_to_robinhood(
            index,
            amounts,
            snap.requester.to_bytes(),
            recipient.as_ref().ok().map(|a| a.to_bytes()),
            &snap.glc_address,
            route_open,
            refusal.as_deref(),
            now,
        )?;
        Ok(())
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }
}

#[cfg(test)]
mod tests;
