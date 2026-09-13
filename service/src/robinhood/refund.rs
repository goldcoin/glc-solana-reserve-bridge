//! `RhnToGlc` refunds: returning a Robinhood depositor's exact principal
//! when their deposit cannot safely complete to Goldcoin.
//!
//! # What a refund is, and what it is not
//!
//! It is emphatically not a withdrawal. Neither the destination nor the
//! amount is chosen — by an operator, by this service, or by the signers:
//!
//! - The **recipient** is the obligation's own recorded `depositor`,
//!   written on-chain when that user's transfer landed and never mutable
//!   afterwards. This service reads it back from the contract and uses
//!   THAT value.
//! - The **amount** is the obligation's own recorded `amount`, read from
//!   the same place. Not the ledger's gross, not the net, not anything
//!   this service computed.
//! - There is **no fee**. A refund returns what was deposited; charging
//!   for a payout that never happened would be taking money for nothing.
//! - There are **no partial refunds**. The contract compares the amount
//!   exactly and reverts on any difference.
//!
//! Signers choose WHICH obligation to refund, and nothing else. The
//! contract enforces every one of these by explicit comparison, and this
//! module builds the authorization from the contract's own values so that
//! the two can never disagree in the first place.
//!
//! # Mutually exclusive with settlement, twice over
//!
//! On-chain: both `executeRefund` and `executeSettlement` require the
//! obligation to be `Pending`, and both leave a terminal status. Whichever
//! lands first makes the other revert.
//!
//! In this ledger: a request that has a settlement row cannot get a refund
//! row and vice versa — [`begin_refund`] refuses on finding either, and
//! the unique index on `(kind, request_id)` makes a duplicate of either
//! impossible. Two independent mechanisms, because "we refunded a deposit
//! we had already settled" is the failure that loses real money.
//!
//! # When a refund is appropriate
//!
//! When a finalized Robinhood deposit cannot safely complete to Goldcoin:
//! an undeliverable destination, a route that will not open, a reserve
//! that cannot cover it, or an operator's explicit decision after review.
//! It is never automatic. A deposit parked in `ManualReview` stays parked
//! until a human decides between resuming it and refunding it, because
//! those are opposite, irreversible answers to the same question.

use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::evm::{EvmAddress, EvmU256};
use crate::ledger::{
    BeginTxOutcome, Ledger, LedgerError, NewRobinhoodTx, RequestState, RobinhoodTx, RobinhoodTxKind,
};
use crate::routes::Route;

use super::auth::{self, RefundAuth, ACTION_REFUND};
use super::calls::{self, GateError, Obligation};
use super::rpc::{EvmBlockTag, EvmCallRpc, EvmRpc, EvmSubmitRpc};
use super::settlement::{SettlementError, Settler};

/// Why a refund could not be begun.
#[derive(Debug, thiserror::Error)]
pub enum RefundError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Settlement(#[from] Box<SettlementError>),
    #[error(transparent)]
    Gate(#[from] GateError),
    #[error(transparent)]
    Auth(#[from] auth::AuthError),
    #[error(
        "request {request_id} is a {direction} request; only a Robinhood-sourced deposit can be \
         refunded on Robinhood"
    )]
    WrongDirection {
        request_id: i64,
        direction: &'static str,
    },
    #[error(
        "request {request_id} is in state {state}, which is not refundable. A deposit is \
         refunded from ManualReview, after a human has decided against completing it."
    )]
    NotRefundable { request_id: i64, state: String },
    #[error(
        "request {request_id} already has a SETTLEMENT: its obligation was closed as paid out on \
         Goldcoin, and refunding it now would return principal the bridge already delivered"
    )]
    AlreadySettled { request_id: i64 },
    #[error(
        "request {request_id} has a confirmed Goldcoin payout: the depositor has been paid, so \
         the principal is not theirs to have back"
    )]
    AlreadyPaidOut { request_id: i64 },
    #[error(
        "obligation {obligation_index} is {status} on-chain, not Pending — only a Pending \
         obligation can be refunded, and this one has already reached a terminal state"
    )]
    ObligationNotPending {
        obligation_index: u64,
        status: &'static str,
    },
    #[error("request {request_id}: {detail}")]
    Invalid { request_id: i64, detail: String },
}

/// Begins a refund for one request: reads the obligation back from the
/// chain, builds the authorization from ITS values, and collects the
/// quorum.
///
/// # Every refusal below protects the same thing
///
/// A refund and a payout are the two ways a deposit can end, and doing
/// both would be paying twice for one deposit. So this refuses if the
/// request has a settlement row, if it has a confirmed Goldcoin payout,
/// or if the obligation is anything other than `Pending` on-chain — three
/// independent checks against three independent sources of truth.
pub async fn begin_refund<R>(
    settler: &Settler<R>,
    ledger: &mut Ledger,
    request_id: i64,
    now: i64,
) -> Result<i64, RefundError>
where
    R: EvmRpc + EvmCallRpc + EvmSubmitRpc,
{
    // Already begun? Resume rather than start a second one.
    if let Some(existing) = ledger.get_robinhood_tx_for(RobinhoodTxKind::Refund, request_id)? {
        return Ok(existing.id);
    }
    // Ledger-side mutual exclusion, first of two.
    if ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)?
        .is_some()
    {
        return Err(RefundError::AlreadySettled { request_id });
    }

    let request = ledger
        .get_request(request_id)?
        .ok_or(LedgerError::RequestNotFound(request_id))?;
    // Both Robinhood-SOURCED directions: the obligation being returned is
    // the same kind of deposit in the same contract, and the route byte
    // the contract binds is read from that obligation's own storage.
    if !request.direction.source_is_robinhood() {
        return Err(RefundError::WrongDirection {
            request_id,
            direction: request.direction.as_str(),
        });
    }
    let route = Route::from(request.direction);
    // A refund is a deliberate decision made about a parked deposit. It
    // is not reachable from a request that is mid-payout, and it is not
    // reachable from one that already settled.
    if request.state != RequestState::ManualReview {
        return Err(RefundError::NotRefundable {
            request_id,
            state: request.state.as_str().to_string(),
        });
    }
    // Second ledger-side check, against the destination's own payout
    // record: a Goldcoin payout that reached the chain, or a Solana
    // release that was submitted, means the depositor was (or may have
    // been) paid. Both are checked for both directions.
    if let Some(payout) = ledger.get_goldcoin_payout(request_id)? {
        if payout.txid.is_some() {
            return Err(RefundError::AlreadyPaidOut { request_id });
        }
    }
    if ledger.get_destination_txid(request_id)?.is_some() {
        return Err(RefundError::AlreadyPaidOut { request_id });
    }
    // A HELD row (schema v30) refunds only on a recorded `refund`
    // operator decision — the same gate the Solana and Goldcoin refund
    // paths apply. Never a hold-aware bypass in the other direction.
    ledger.refuse_refund_unless_decided(request_id)?;

    let obligation_index = request
        .source_obligation_index
        .ok_or_else(|| RefundError::Invalid {
            request_id,
            detail: "a Robinhood-sourced request must name the obligation it refunds".to_string(),
        })?;

    // The AUTHORITY for both the recipient and the amount. Read from the
    // contract, at `Latest`, immediately before the authorization is
    // built — never from a ledger row, an operator's input, or the event
    // this service decoded.
    let obligation: Obligation = settler
        .deployment_reader()
        .obligation(settler.rpc(), obligation_index, EvmBlockTag::Latest)
        .await
        .map_err(calls::GateError::Read)?;
    if !obligation.is_pending() {
        return Err(RefundError::ObligationNotPending {
            obligation_index,
            status: obligation.status_name(),
        });
    }
    // The obligation's OWN route, read from the chain, must be the one
    // the request was folded on. The contract binds the stored route
    // into the refund digest, so a mismatch would only fail on-chain
    // after consuming a nonce; refusing here names the contradiction.
    if Some(obligation.route) != route.contract_route_id() {
        return Err(RefundError::Invalid {
            request_id,
            detail: format!(
                "the on-chain obligation records route {:#04x}, but this request was folded as \
                 {} ({:#04x})",
                obligation.route,
                route.as_str(),
                route.contract_route_id().unwrap_or(0)
            ),
        });
    }

    let amount =
        RobinhoodAtomic::try_from_u256(obligation.amount).map_err(|e| RefundError::Invalid {
            request_id,
            detail: format!("the obligation's recorded principal is not usable: {e}"),
        })?;

    let deployment = settler.deployment();
    let domain = deployment.domain();
    let identity = auth::obligation_identity(obligation_index);
    let contract_request_id = auth::derive_request_id(ACTION_REFUND, route, domain, &identity)?;

    let signer_epoch = settler
        .deployment_reader()
        .signer_epoch(settler.rpc(), EvmBlockTag::Latest)
        .await
        .map_err(calls::GateError::Read)?;
    let expiry = (now as u64).saturating_add(settler.config_ref().authorization_ttl.as_secs());
    let chains = deployment
        .chains_for(route)
        .expect("preflight verified every contract route's chain pair");

    let authorization = auth::EvmAuthRequest::refund(
        domain,
        RefundAuth {
            route,
            chains,
            token: deployment.token,
            request_id: contract_request_id,
            obligation_index,
            // The obligation's own depositor and principal — the contract
            // compares both and reverts on any difference.
            recipient: obligation.depositor,
            amount,
            signer_epoch,
            expiry,
        },
    );
    let digest = authorization.digest()?;

    let outcome = ledger.begin_robinhood_tx(
        &NewRobinhoodTx {
            kind: RobinhoodTxKind::Refund,
            request_id: Some(request_id),
            rebalance_request_id: None,
            route: Some(route),
            bridge_contract: deployment.bridge_contract.to_bytes(),
            chain_id: deployment.chain_id.get(),
            contract_request_id,
            obligation_index: Some(obligation_index),
            recipient: Some(obligation.depositor.to_bytes()),
            amount_robinhood: Some(amount.to_u256().to_be_bytes()),
            signer_epoch,
            expiry,
            auth_digest: digest,
        },
        now,
    )?;
    let tx_id = match outcome {
        BeginTxOutcome::Created { id } | BeginTxOutcome::Exists { id } => id,
    };

    let quorum = super::signer::collect_quorum(
        settler.signers_ref(),
        &deployment.signers,
        &authorization,
        settler.signer_timeout(),
    )
    .await
    .map_err(|e| RefundError::Settlement(Box::new(SettlementError::Quorum(e))))?;
    ledger.record_robinhood_authorization(tx_id, &quorum.for_storage(), now)?;
    ledger.mark_robinhood_refund_pending(request_id, now)?;
    Ok(tx_id)
}

/// Rebuilds a refund's authorization payload from its stored row.
///
/// Used by the shared calldata builder. The recipient and amount come
/// from the row, which recorded exactly what was read off the chain when
/// the authorization was minted — and the rebuilt payload's digest is
/// then compared against the stored one, so a row that disagrees with the
/// signatures is caught before anything is broadcast.
pub(crate) fn rebuild_refund_auth<R>(
    settler: &Settler<R>,
    tx: &RobinhoodTx,
) -> Result<RefundAuth, SettlementError>
where
    R: EvmRpc + EvmCallRpc + EvmSubmitRpc,
{
    let deployment = settler.deployment();
    let request_id = tx.bridge_request_id()?;
    let route = tx.route.ok_or_else(|| SettlementError::Request {
        request_id,
        detail: "a refund is missing its route".to_string(),
    })?;
    let chains = deployment
        .chains_for(route)
        .ok_or_else(|| SettlementError::Request {
            request_id,
            detail: format!("route {} has no verified chain pair", route.as_str()),
        })?;
    Ok(RefundAuth {
        route,
        chains,
        token: deployment.token,
        request_id: tx.contract_request_id,
        obligation_index: tx
            .obligation_index
            .ok_or_else(|| SettlementError::Request {
                request_id,
                detail: "a refund must name an obligation".to_string(),
            })?,
        recipient: EvmAddress::from_bytes(tx.recipient.ok_or_else(|| {
            SettlementError::Request {
                request_id,
                detail: "a refund must name a recipient".to_string(),
            }
        })?),
        amount: RobinhoodAtomic::try_from_u256(EvmU256::from_be_bytes(
            tx.amount_robinhood
                .ok_or_else(|| SettlementError::Request {
                    request_id,
                    detail: "a refund must name an amount".to_string(),
                })?,
        ))
        .map_err(|e| SettlementError::Request {
            request_id,
            detail: format!("the stored refund amount is not usable: {e}"),
        })?,
        signer_epoch: tx.signer_epoch,
        expiry: tx.expiry,
    })
}

#[cfg(test)]
mod tests;
