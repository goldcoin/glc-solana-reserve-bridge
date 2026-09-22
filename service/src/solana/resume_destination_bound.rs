//! `glc-admin resume-destination-bound`: the operator recovery for a
//! `GlcToSol` request that `Orchestrator::release_out_of_bounds` parked
//! `destination_payout_out_of_bounds` because its locked payout exceeded
//! the Solana program's `per_transfer_limit` at the time — and now fits
//! (docs/40-destination-bound-admission.md; the 2026-09-18 incident,
//! requests 4438 and 4483).
//!
//! # What this module is NOT
//!
//! It is not a settlement implementation. Recovery re-admits the request
//! into the EXISTING release pipeline by transitioning it `ManualReview
//! -> SourceFinalized`; `Orchestrator::tick_release_settlements` then
//! carries it through the same bounds re-check, 2-of-3 attestation,
//! `release_from_reserve` submission and confirmation every other
//! `GlcToSol` request goes through. The entitlement is the STORED gross
//! at the LOCKED quote; no live rate is consulted for it.
//!
//! Nor does it re-implement the eligibility rules: every check lives in
//! [`Ledger::resume_glc_to_sol_destination_bound`] and is reached here
//! only by calling it (for real, audited) or trialling it and rolling it
//! back (the dry run). This module's own job is the ONE chain read the
//! ledger cannot make — the program's live `min_transfer_amount` /
//! `per_transfer_limit` and the mint's decimals — and the operator-facing
//! report.

use crate::admin_api::{audited_mutation, AdminError, AuditedAction, MutationReceipt};
use crate::ledger::{
    BridgeRequest, Ledger, LiveSolanaBounds, ReserveDirection, ResumeDryRunOutcome,
    ResumeManualReviewOutcome,
};
use crate::signing::attestation::fetch_bridge_config;
use crate::solana::accounts;
use crate::solana::rpc::SolanaRpc;

/// The audit-log action name.
pub const AUDIT_ACTION: &str = "resume_destination_bound";

/// Everything an operator reads before deciding, all from live state.
#[derive(Debug, Clone)]
pub struct DestinationBoundDryRunReport {
    pub request: BridgeRequest,
    /// The program's bounds and the mint's decimals, as read this instant.
    pub live: LiveSolanaBounds,
    /// The payout recomputed from the STORED gross and the LOCKED quote,
    /// in mint units — what `release_from_reserve` would transfer. `Err`
    /// when the stored amounts do not verify or the quote is not locked.
    pub locked_payout_mint_units: Result<u64, String>,
    /// Solana reserve figures (mint units): balance, protected minimum,
    /// reserved, pending — the reservation this request still holds is
    /// inside `reserved`/`pending`.
    pub reserve: (u64, u64, u64, u64),
    /// Whether a destination transaction is already recorded.
    pub destination_txid_present: bool,
    /// What an execute would do, from the real function rolled back.
    pub ledger: ResumeDryRunOutcome,
}

impl DestinationBoundDryRunReport {
    pub fn would_resume(&self) -> bool {
        matches!(self.ledger, ResumeDryRunOutcome::WouldResume)
    }
}

/// The one chain read: `bridge_config` and the reserve mint's decimals.
pub async fn read_live_bounds<R: SolanaRpc>(rpc: &R) -> Result<LiveSolanaBounds, String> {
    let config = fetch_bridge_config(rpc)
        .await
        .map_err(|e| format!("could not read the Solana bridge_config: {e}"))?;
    let solana_decimals = accounts::fetch_reserve_mint_decimals(rpc, &config.reserve_token_mint)
        .await
        .map_err(|e| format!("could not read the reserve mint's decimals: {e}"))?;
    Ok(LiveSolanaBounds {
        min_transfer_amount: config.min_transfer_amount,
        per_transfer_limit: config.per_transfer_limit,
        solana_decimals,
    })
}

fn locked_payout(request: &BridgeRequest, live: LiveSolanaBounds) -> Result<u64, String> {
    if request.quote.as_ref().and_then(|q| q.locked_at).is_none() {
        return Err("no locked bridge quote".to_string());
    }
    let breakdown = request
        .verify_breakdown()
        .map_err(|e| format!("stored amounts do not verify against the locked quote: {e}"))?;
    breakdown
        .net
        .to_solana(live.solana_decimals)
        .map(|s| s.0)
        .map_err(|e| format!("locked net is not representable at the mint's precision: {e}"))
}

/// Strictly read-only: one chain read, one ledger trial rolled back.
/// Contacts no signer, loads no keypair, moves no funds, persists nothing.
pub async fn dry_run<R: SolanaRpc>(
    rpc: &R,
    ledger: &mut Ledger,
    request_id: i64,
    now: i64,
) -> Result<DestinationBoundDryRunReport, String> {
    let request = ledger
        .get_request(request_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("bridge request {request_id} not found"))?;
    let live = read_live_bounds(rpc).await?;
    let locked_payout_mint_units = locked_payout(&request, live);
    let reserve = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .map_err(|e| e.to_string())?;
    let destination_txid_present = ledger
        .get_destination_txid(request_id)
        .map_err(|e| e.to_string())?
        .is_some();
    let ledger_outcome = ledger
        .dry_run_resume_glc_to_sol_destination_bound(request_id, live, now)
        .map_err(|e| e.to_string())?;
    Ok(DestinationBoundDryRunReport {
        request,
        live,
        locked_payout_mint_units,
        reserve,
        destination_txid_present,
        ledger: ledger_outcome,
    })
}

/// The audited, atomic re-admission — the ledger method under the same
/// audit discipline as every other resume, recorded as
/// [`AUDIT_ACTION`] with the live bounds it was judged against.
pub fn audited_resume(
    ledger: &mut Ledger,
    request_id: i64,
    live: LiveSolanaBounds,
    note: &str,
    actor: &str,
    now: i64,
) -> Result<(ResumeManualReviewOutcome, MutationReceipt), AdminError> {
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: AUDIT_ACTION,
            target: request_id.to_string(),
            note,
            new_value: None,
        },
        |l| {
            Ok(l.get_request(request_id)?
                .map(|r| r.state.as_str().to_string()))
        },
        |l| {
            l.resume_glc_to_sol_destination_bound(request_id, live, note, actor, now)
                .map_err(AdminError::from)
        },
        |outcome, params| {
            params.new_value = Some(match outcome {
                ResumeManualReviewOutcome::Resumed => format!(
                    "SourceFinalized (live bounds min={} max={} decimals={})",
                    live.min_transfer_amount, live.per_transfer_limit, live.solana_decimals
                ),
                ResumeManualReviewOutcome::AlreadyResumed { state } => {
                    format!("no-op: already resumed (state={})", state.as_str())
                }
            });
        },
    )
}

/// Guarded execution: the chain read, then the atomic audited
/// re-admission, which independently re-runs every ledger check under the
/// write lock against the bounds just read.
pub async fn execute<R: SolanaRpc>(
    rpc: &R,
    ledger: &mut Ledger,
    request_id: i64,
    note: &str,
    actor: &str,
    now: i64,
) -> Result<ResumeManualReviewOutcome, String> {
    let live = read_live_bounds(rpc).await?;
    let (outcome, _receipt) =
        audited_resume(ledger, request_id, live, note, actor, now).map_err(|e| e.to_string())?;
    Ok(outcome)
}

#[cfg(test)]
mod tests;
