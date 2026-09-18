//! The READ-ONLY Robinhood contract and submitter reads the admin API's
//! `GET /routes` and `GET /submitters` need (docs/39-admin-console-v2.md).
//!
//! # Why the admin API now reads the contract at all
//!
//! The admin API deliberately held no Robinhood RPC client for a long
//! time: the contract's `routeEnabled` flags were readable only through
//! `glc-admin robinhood-preflight`, which the admin console reached by
//! spawning a root-side process through `sudo` on every refresh. An
//! operator console that polls every few seconds cannot be built on
//! that, and a console that does not read the contract cannot tell an
//! operator that a route they opened locally is still closed on chain —
//! which is exactly the class of confusion (local pause versus on-chain
//! pause) the 2026-09-12 incident was made of.
//!
//! # What this can and cannot do
//!
//! Every call here is an `eth_call` or an `eth_getBalance` through the
//! same read-only [`BridgeReader`] the public API and preflight use, plus
//! the submitter's balance read preflight already performs. There is no
//! signer, no key, no transaction builder, and nothing that touches
//! [`crate::routes::RouteGate`] or the ledger. Reporting that a route is
//! enabled ON THE CONTRACT enables nothing in this service, and reporting
//! it disabled closes nothing: the contract's flags are enforced at
//! broadcast time by `crate::robinhood::calls`, exactly as before.
//!
//! # Fail-honest
//!
//! A read that does not complete is reported as UNREAD (`None`), never
//! as "enabled" and never as "paused". The console renders unread as
//! unread, and the availability model keeps the public verdict rather
//! than inventing a contract-side blocker it could not observe.

use std::future::Future;
use std::pin::Pin;

use crate::evm::{EvmAddress, EvmU256};
use crate::robinhood::calls::BridgeReader;
use crate::robinhood::rpc::{EvmBlockTag, EvmCallRpc, EvmSubmitRpc};
use crate::routes::Route;

/// One consistent reading of the contract flags the operator console
/// shows beside the local gates. Every field came from THIS read; a read
/// that could not obtain all of them yields no struct at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodContractFlags {
    /// `depositsPaused()` — governance's inbound kill switch. Gates every
    /// Robinhood-SOURCED route (`RhnToGlc`, `RhnToSol`).
    pub deposits_paused: bool,
    /// `payoutsPaused()` — the outbound one. Gates every Robinhood-BOUND
    /// route (`GlcToRhn`, `SolToRhn`).
    pub payouts_paused: bool,
    /// `routeEnabled(route)` for each route the contract models, in
    /// [`Route::ALL`] order restricted to those with a
    /// [`Route::contract_route_id`].
    pub route_enabled: Vec<(Route, bool)>,
    /// Unix seconds at which the read completed.
    pub read_at: i64,
}

impl RobinhoodContractFlags {
    /// The contract's own flag for `route`, or `None` for a route the
    /// contract does not model.
    pub fn route_enabled(&self, route: Route) -> Option<bool> {
        self.route_enabled
            .iter()
            .find(|(r, _)| *r == route)
            .map(|(_, enabled)| *enabled)
    }
}

/// An object-safe, read-only source of Robinhood facts for the admin
/// API. Object-safe for the same reason
/// [`crate::robinhood::public::RobinhoodContractSource`] is: the
/// Robinhood half of a deployment is optional, and [`super::AdminApi`]
/// must be constructible without naming a phantom RPC type.
pub trait RobinhoodAdminReader: Send + Sync {
    /// The contract's pause and route flags, or `None` when any of the
    /// reads did not complete.
    fn contract_flags(
        &self,
    ) -> Pin<Box<dyn Future<Output = Option<RobinhoodContractFlags>> + Send + '_>>;
    /// The configured submitter's address and live native balance (wei),
    /// with the deployment's `min_submitter_balance_wei`. The balance is
    /// `None` when the read did not complete; the address and minimum
    /// are configuration and always present.
    fn submitter(&self) -> Pin<Box<dyn Future<Output = RobinhoodSubmitterRead> + Send + '_>>;
}

/// See [`RobinhoodAdminReader::submitter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodSubmitterRead {
    pub address: EvmAddress,
    pub balance_wei: Option<EvmU256>,
    pub min_balance_wei: EvmU256,
}

/// The production [`RobinhoodAdminReader`]: live reads against the
/// configured `GlcRobinhoodBridge` and the configured submitter address.
///
/// `EvmSubmitRpc` is bound for exactly one method, `balance` — the same
/// read `robinhood::preflight`'s `submitter_funded` check performs. This
/// type holds no key and never calls `send_raw_transaction`.
pub struct LiveRobinhoodAdminReader<R> {
    rpc: R,
    reader: BridgeReader,
    submitter: EvmAddress,
    min_submitter_balance_wei: EvmU256,
}

impl<R: EvmCallRpc + EvmSubmitRpc + Send + Sync> LiveRobinhoodAdminReader<R> {
    pub fn new(
        rpc: R,
        bridge_contract: EvmAddress,
        submitter: EvmAddress,
        min_submitter_balance_wei: EvmU256,
    ) -> Self {
        LiveRobinhoodAdminReader {
            rpc,
            reader: BridgeReader::new(bridge_contract),
            submitter,
            min_submitter_balance_wei,
        }
    }

    async fn read_flags(
        &self,
    ) -> Result<RobinhoodContractFlags, crate::robinhood::calls::ContractReadError> {
        let deposits_paused = self
            .reader
            .deposits_paused(&self.rpc, EvmBlockTag::Latest)
            .await?;
        let payouts_paused = self
            .reader
            .payouts_paused(&self.rpc, EvmBlockTag::Latest)
            .await?;
        let mut route_enabled = Vec::with_capacity(4);
        for route in Route::ALL {
            let Some(byte) = route.contract_route_id() else {
                continue;
            };
            let enabled = self
                .reader
                .route_enabled(&self.rpc, byte, EvmBlockTag::Latest)
                .await?;
            route_enabled.push((route, enabled));
        }
        Ok(RobinhoodContractFlags {
            deposits_paused,
            payouts_paused,
            route_enabled,
            read_at: super::now_unix(),
        })
    }
}

impl<R: EvmCallRpc + EvmSubmitRpc + Send + Sync> RobinhoodAdminReader
    for LiveRobinhoodAdminReader<R>
{
    fn contract_flags(
        &self,
    ) -> Pin<Box<dyn Future<Output = Option<RobinhoodContractFlags>> + Send + '_>> {
        Box::pin(async move {
            match self.read_flags().await {
                Ok(flags) => Some(flags),
                Err(e) => {
                    // Logged (redacted by the tracing layer's own rules),
                    // not returned: an RPC error can name the endpoint.
                    tracing::debug!(
                        error = %e,
                        "admin Robinhood contract read failed; reporting the flags as unread"
                    );
                    None
                }
            }
        })
    }

    fn submitter(&self) -> Pin<Box<dyn Future<Output = RobinhoodSubmitterRead> + Send + '_>> {
        Box::pin(async move {
            let balance_wei = match self.rpc.balance(self.submitter).await {
                Ok(b) => Some(b),
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "admin Robinhood submitter balance read failed; reporting as unread"
                    );
                    None
                }
            };
            RobinhoodSubmitterRead {
                address: self.submitter,
                balance_wei,
                min_balance_wei: self.min_submitter_balance_wei,
            }
        })
    }
}
