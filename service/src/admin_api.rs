//! The authenticated, privately-bound admin control plane
//! (docs/27-admin-control-plane.md).
//!
//! # Relationship to the public API's boundary
//!
//! [`crate::api`]'s module docs state that the public API never exposes
//! privileged admin operations — that boundary is unchanged: nothing here
//! is reachable through the public listener. This module is the
//! deliberate, separately-bound counterpart for operators: its listener
//! only starts when `service.admin_bind_addr` is configured (bind it
//! privately — localhost or an internal interface behind the operators'
//! reverse proxy, never a public address), and every request must carry a
//! per-operator bearer token ([`auth::OperatorRegistry`]).
//!
//! # What this exposes, and what it structurally cannot do
//!
//! Read-only views an operator needs (reserve health, on-chain
//! `BridgeConfig`/rolling-window state, the ManualReview backlog, the
//! rebalance workflow, the admin audit log, the fixed fee rate), plus the
//! LOCAL mutations `glc-admin` already supports through validated
//! `Ledger` logic: local pause/unpause per reserve direction, admission
//! open/close (through [`guard::open_admission_guarded`] — the same
//! invariant + UTXO-liquidity gates the CLI applies, never bypassable),
//! resume-manual-review (via `Ledger::resume_manual_review_sol_to_glc`,
//! whose internal safety checks — including the unconditional
//! source-wallet/recipient rate-limit re-checks — are never reimplemented
//! or pre-filtered here), and the rebalance request workflow
//! (`record-executed` records an out-of-band transaction reference
//! string; nothing here constructs or broadcasts anything).
//!
//! # The ONE fund-moving exception (added 2026-09-03)
//!
//! This API was originally, and deliberately, incapable of moving funds
//! at all. That is no longer literally true, and this section states the
//! exception precisely rather than leaving the old claim standing.
//!
//! `POST /refunds/glc/{request_id}/execute` — the GOLDCOIN-side refund of
//! a `GlcToSol` request parked in `ManualReview` — signs and broadcasts a
//! real vault transaction. It is the only such route, and it is fenced:
//!
//! - It exists only when the daemon injected a refund executor
//!   ([`AdminApi::with_refund_executor`]). Absent that, the route reports
//!   "not wired" — every other construction of [`AdminApi`] remains
//!   structurally incapable of moving funds.
//! - It requires the operator's token to carry an explicit, separately
//!   granted capability (`may_execute_glc_refunds`). **An ordinary admin
//!   token is not sufficient**, so a leaked read-only token cannot spend
//!   from the vault.
//! - It refuses outright when NO operator has the capability, so a
//!   deployment that never opted in cannot move funds through this API.
//! - It requires the local `GoldcoinReserve` pause, and never engages or
//!   clears it.
//! - Its request body is a request id and a mandatory note. There is no
//!   destination, amount, fee, transaction, signer or override parameter
//!   to supply; every such value is derived server-side from chain
//!   evidence and re-verified immediately before signing.
//! - Signing uses the daemon's existing 2-of-3 vault signers. No keypair
//!   is loaded here, no hot wallet exists, and the threshold is unchanged.
//!
//! Everything else below still holds exactly as written.
//!
//! # What this exposes, and what it otherwise cannot do
//!
//! Apart from that one route, it never touches [`crate::signing`], never
//! loads or holds any keypair, and has no path that executes a command or
//! submits a transaction — including for SOLANA ManualReview refunds,
//! whose read-only listing and dry run are served here (`GET /refunds`,
//! `GET /refunds/{id}/dry-run`) while execution stays a
//! [`cli_command`]-generated `glc-admin` line for an operator to run with
//! their own keypair. The
//! on-chain admin instructions (`set_paused`/`set_limit`/
//! `reset_rolling_volume_window`) remain CLI-only, gated by possession of
//! the admin keypair on the operator's own machine: for those, this
//! module only serves read-only state plus [`cli_command`]-generated
//! `glc-admin` command lines (labeled "CLI approval required") with the
//! atomic-unit conversions done server-side, in the same Rust code the
//! daemon itself trusts.
//!
//! Every mutation requires a non-empty `note` and is recorded —
//! successes AND refusals — in the append-only `admin_audit_log`
//! (schema v15, [`crate::ledger::Ledger::append_admin_audit`]) under the
//! operator identity the bearer token resolved to.
//!
//! # Browser sessions live elsewhere
//!
//! This API is bearer-only by design: it never sets or reads cookies and
//! never answers CORS preflight, so a browser's ambient credentials can
//! never authorize anything here (CSRF against it is structurally
//! impossible, not just mitigated). Requests carrying a `Cookie` or
//! `Origin` header are rejected outright — a legitimate caller is the
//! admin UI's server-side proxy (which holds the operator's token
//! server-side), `curl`, or another non-browser client.

pub mod auth;
pub mod cli_command;
pub mod glc_refund_exec;
pub mod guard;

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

use crate::ledger::SolanaRefundState;
use crate::ledger::{
    AdminAuditEntry, AdminAuditFilter, AdminAuditOutcome, AdminAuditRow, Direction, Ledger,
    LedgerError, RebalanceKind, RebalanceRequest, RequestState, ReserveDirection,
};
use crate::ops::reserve_health;
use crate::solana::accounts;
use crate::solana::rpc::SolanaRpc;

use auth::OperatorRegistry;

// ------------------------------------------------------------- errors --

#[derive(Debug)]
pub enum AdminError {
    /// Malformed input (bad JSON, unknown direction, empty note): 400.
    BadRequest(String),
    /// The target row does not exist: 404.
    NotFound(String),
    /// A validated refusal from the underlying business logic (invariant
    /// does not hold, wrong state, rate-limited, ...): 409. The message
    /// is the same operator-facing text `glc-admin` would print.
    Conflict(String),
    /// Ledger/storage failure: 500.
    Ledger(String),
    /// The Solana RPC read failed or returned something undecodable: 503.
    Upstream(String),
}

impl AdminError {
    fn status(&self) -> StatusCode {
        match self {
            AdminError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AdminError::NotFound(_) => StatusCode::NOT_FOUND,
            AdminError::Conflict(_) => StatusCode::CONFLICT,
            AdminError::Ledger(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AdminError::Upstream(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdminError::BadRequest(m)
            | AdminError::NotFound(m)
            | AdminError::Conflict(m)
            | AdminError::Ledger(m)
            | AdminError::Upstream(m) => f.write_str(m),
        }
    }
}

/// Every non-`Sqlite` `LedgerError` is a validated refusal whose message
/// is already written for an operator (`glc-admin` prints them verbatim
/// today) — surface those as 409. Raw storage errors are 500 and their
/// detail stays out of the response body (a SQLite message can embed the
/// database path).
impl From<LedgerError> for AdminError {
    fn from(e: LedgerError) -> Self {
        match e {
            LedgerError::Sqlite(_) => AdminError::Ledger("ledger storage error".to_string()),
            LedgerError::RequestNotFound(id) => {
                AdminError::NotFound(format!("bridge request {id} not found"))
            }
            LedgerError::RebalanceNotFound(id) => {
                AdminError::NotFound(format!("rebalance request {id} not found"))
            }
            other => AdminError::Conflict(other.to_string()),
        }
    }
}

// ------------------------------------------------------- request types --

fn parse_reserve_direction(s: &str) -> Result<ReserveDirection, AdminError> {
    match s {
        "goldcoin" => Ok(ReserveDirection::GoldcoinReserve),
        "solana" => Ok(ReserveDirection::SolanaReserve),
        other => Err(AdminError::BadRequest(format!(
            "unknown direction {other:?} (expected goldcoin|solana)"
        ))),
    }
}

/// `direction` for the `/rebalances` family: the two reserves
/// [`parse_reserve_direction`] knows plus `robinhood`, which is a
/// rebalanceable reserve now that `GlcRobinhoodBridge.executeTreasuryWithdraw`
/// exists and `glc-admin robinhood-treasury-withdraw` drives it.
///
/// Separate from [`parse_reserve_direction`] because `pause`/`unpause`
/// still do not take `robinhood` — the local Robinhood gate has its own
/// command — and folding the two would give one of them the wrong answer.
fn parse_rebalance_direction(s: &str) -> Result<ReserveDirection, AdminError> {
    match s {
        "robinhood" => Ok(ReserveDirection::RobinhoodReserve),
        other => parse_reserve_direction(other).map_err(|_| {
            AdminError::BadRequest(format!(
                "unknown direction {other:?} (expected goldcoin|solana|robinhood)"
            ))
        }),
    }
}

fn require_note(note: &str) -> Result<&str, AdminError> {
    let trimmed = note.trim();
    if trimmed.is_empty() {
        return Err(AdminError::BadRequest(
            "a non-empty note is required for every admin mutation".to_string(),
        ));
    }
    Ok(trimmed)
}

#[derive(Debug, Deserialize)]
pub struct DirectionNoteInput {
    pub direction: String,
    pub note: String,
}

#[derive(Debug, Deserialize)]
pub struct NoteInput {
    pub note: String,
}

#[derive(Debug, Deserialize)]
pub struct RebalanceProposeInput {
    pub direction: String,
    /// `deposit` or `withdraw`.
    pub kind: String,
    pub amount_atomic: u64,
    pub required_approvals: u32,
    pub note: String,
}

#[derive(Debug, Deserialize)]
pub struct RebalanceRecordExecutedInput {
    pub tx_reference: String,
    pub note: String,
}

#[derive(Debug, Deserialize)]
pub struct RebalanceConfirmInput {
    pub observed_amount_atomic: u64,
    pub note: String,
}

// ------------------------------------------------------ response types --

/// One mutation's receipt: what the audit log now holds for it.
#[derive(Debug, Serialize)]
pub struct MutationReceipt {
    pub audit_id: i64,
    pub action: String,
    pub target: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DirectionStatusView {
    pub direction: String,
    pub paused: bool,
    pub pause_reason: Option<String>,
    pub admission_closed: bool,
    pub admission_reason: Option<String>,
    /// The AUTOMATIC confirmed-liquidity gate, reported alongside — never
    /// merged into — the operator-only `admission_closed` above, so an
    /// operator console can always show WHICH of the two is holding new
    /// SolToGlc obligations back (docs/09-runbook.md's
    /// "Confirmed-liquidity admission safety buffer"). Always `false` for
    /// `glc-to-sol`.
    pub liquidity_admission_closed: bool,
    pub manual_review_count: usize,
}

#[derive(Debug, Serialize)]
pub struct AdminStatusView {
    pub glc_to_sol: DirectionStatusView,
    pub sol_to_glc: DirectionStatusView,
    pub post_finality_reorg_events: i64,
    /// Per-route Robinhood availability. EMPTY on every deployment that
    /// has not configured Robinhood, so an existing operator console sees
    /// exactly the response it always did.
    ///
    /// Additive: no existing field changed shape or meaning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub robinhood_routes: Vec<RobinhoodRouteView>,
    /// The ROUTE-SCOPED admission gate for each inbound-to-Goldcoin
    /// route (schema v25's `route_admission`), reported SEPARATELY from
    /// the reserve-wide `paused`/`admission_closed` that
    /// `GET /admin/reserve-health` carries.
    ///
    /// The separation is the point, and it is what lets an admin UI
    /// drive the two axes independently. A reserve-wide pause and a
    /// route-level closure produce the same user-visible outcome on a
    /// given route but have completely different blast radii and
    /// completely different remedies (`glc-admin unpause --direction
    /// goldcoin` versus `glc-admin route-admission-open --route ...`),
    /// so a console that rendered one number for both would tell an
    /// operator to run the wrong command.
    ///
    /// EMPTY on a pre-v25 ledger — the table does not exist, which is a
    /// different fact from "both routes are open" and is reported as
    /// absence rather than as defaults, the same discipline
    /// `Ledger::route_ledger_rows` follows.
    ///
    /// Additive: no existing field changed shape or meaning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route_admission: Vec<RouteAdmissionStatusView>,
}

/// One inbound-to-Goldcoin route's ROUTE-SCOPED admission gate, beside
/// the reserve-wide gates it is ANDed with.
///
/// Every field an operator needs to answer "why is this route closed,
/// and which command reopens it" without correlating two responses:
/// the route's own flag, the reserve-wide flags it shares with the
/// other inbound route, and the resulting verdict.
#[derive(Debug, Serialize)]
pub struct RouteAdmissionStatusView {
    pub route: String,
    /// The reserve this route settles out of, whose reserve-wide gates
    /// the next two fields report. `GoldcoinReserve` for both inbound
    /// routes today — which is exactly why the route-scoped flag exists.
    pub destination_reserve: String,
    /// THIS ROUTE's own gate. `true` means an operator ran
    /// `route-admission-close` on this route specifically.
    pub route_admission_closed: bool,
    /// Operator context recorded with the closure, last-write-wins. The
    /// authoritative history is `admin_audit_log`.
    pub route_admission_closed_reason: Option<String>,
    /// Unix seconds the route's own flag was last written.
    pub route_admission_updated_at: i64,
    /// The RESERVE-WIDE emergency stop, shared with every other route
    /// drawing on the same reserve. Repeated here so the two axes can be
    /// read side by side; the authoritative per-reserve view is
    /// `GET /admin/reserve-health`.
    pub reserve_paused: bool,
    /// The RESERVE-WIDE operator admission switch, likewise shared.
    pub reserve_admission_closed: bool,
    /// `true` only when the route's own gate AND every reserve-wide gate
    /// would admit a minimum-sized deposit right now — the same verdict
    /// `GET /chains` publishes as `available`, computed from the same
    /// [`crate::ledger::InboundAdmissionGates`] both folds gate on, so
    /// the operator view and the public view cannot disagree.
    pub admits_now: bool,
    /// Which gate refuses, ranked most specific first, or `null` when
    /// none does. Operator-facing detail deliberately absent from the
    /// public `/chains` response.
    pub blocker: Option<String>,
}

/// One Robinhood route, decomposed into the gates that decide it.
///
/// # Why this is not on the PUBLIC `/chains`
///
/// `api::RouteView` deliberately gives end users one cause-agnostic
/// message ([`crate::routes::RouteGateError::UNAVAILABLE_MESSAGE`]) and
/// never names which gate refused — that boundary is unchanged, and this
/// view does not relax it. An operator debugging a route that will not
/// open needs the opposite, and gets it here, behind the authenticated,
/// privately-bound admin listener.
///
/// It carries no RPC URL, no endpoint host and no credential: those are
/// not route state, and this response is read by the admin UI's proxy.
#[derive(Debug, Serialize)]
pub struct RobinhoodRouteView {
    pub route: String,
    pub source_chain: String,
    pub destination_chain: String,
    /// Whether settlement machinery exists at all. `false` for
    /// `SolToRhn`/`RhnToSol`, which have no ledger `Direction` and whose
    /// spelling the database's own CHECK constraint cannot store.
    pub implemented: bool,
    /// The service-side three-place AND: config, ledger, adapter.
    pub service_enabled: bool,
    /// The contract's own `routeEnabled(route)`. `null` when no live read
    /// supplied one — never assumed in either direction.
    pub contract_route_enabled: Option<bool>,
    /// Enabled by BOTH gates and healthy. The only field to read as "a
    /// transfer could happen".
    pub effective_available: bool,
    /// Which gate refused, in operator terms.
    pub disabled_reason: Option<String>,
    /// Why an enabled route is nonetheless unusable — a halted indexer, a
    /// chain-id disagreement, an unformable signer quorum. Separate from
    /// `disabled_reason` so an operator is not sent to look at
    /// configuration when the problem is the chain.
    pub health_reason: Option<String>,
    /// The largest SINGLE transfer this route is approved for, in
    /// canonical 8-decimal units — the same unit every other amount on
    /// this API uses, NOT Robinhood's native 18.
    ///
    /// # What this number is
    ///
    /// The configured `[robinhood.policy].per_transfer_limit`, read
    /// through [`crate::chain_policy::ChainPolicy`]. Policy is keyed by
    /// CHAIN, so every Robinhood route reports the same figure; it is
    /// repeated per route rather than hoisted because it is a fact about
    /// what each route will accept, and that is where an operator looks
    /// for it.
    ///
    /// # What it is NOT
    ///
    /// Not a rolling figure. The rolling 24-hour window is a SEPARATE
    /// ceiling with its own accounting, its own two directions and its
    /// own reset, and this value neither bounds nor is bounded by what
    /// remains of it: a route with its whole daily budget free still
    /// refuses a single transfer above this, and a route well under this
    /// still refuses one that would overrun the window. Anything
    /// displaying the two must keep them visibly apart.
    ///
    /// Not the enforcement layer either. `GlcRobinhoodBridge` holds
    /// `inboundMax`/`outboundMax` and is what actually reverts an
    /// oversized transfer; this is the operator's STATEMENT of what those
    /// are believed to hold, and `glc-admin robinhood-preflight` is what
    /// compares the two and reports any divergence. Reading a live
    /// `limits()` would need an `eth_call`, and this API deliberately
    /// holds no Robinhood RPC client.
    ///
    /// `null` when no `[robinhood.policy]` section is configured — never
    /// zero, which would say the route accepts nothing.
    pub per_transfer_limit_atomic: Option<u64>,
    /// The smallest SINGLE transfer this route accepts, canonical 8dp —
    /// the source-side GROSS floor, and the counterpart to
    /// `per_transfer_limit_atomic` above.
    ///
    /// # Where it comes from, and why it is never `null`
    ///
    /// `crate::min_transfer::SOURCE_MINIMUM_CANONICAL`: one policy figure
    /// for every route, compiled in rather than configured, and the same
    /// value `POST /transfers` and `POST /quote` admit against. Unlike
    /// the maximum beside it there is no `[robinhood.policy]` section to
    /// be absent, so this is always present — an operator reading this
    /// table always learns the floor even on a deployment that has stated
    /// no limits of its own.
    ///
    /// # It is NOT a chain figure, and the distinction is the point
    ///
    /// `GlcRobinhoodBridge` holds `inboundMin` and `outboundMin`, and
    /// neither is this. `outboundMin` bounds the NET payout, after the
    /// fee — so it is not a statement about what a user may send, and
    /// reporting it in this column would put a number in front of an
    /// operator that no user-facing surface applies. Whether the
    /// contract's floors leave room for a policy-minimum transfer to be
    /// DELIVERED is a separate question with its own answer:
    /// `glc-admin robinhood-preflight`'s
    /// `policy_source_minimum_deliverable` check.
    pub min_transfer_atomic: u64,
}

/// The Robinhood reserve, reported as a THIRD independent reserve.
#[derive(Debug, Serialize)]
pub struct RobinhoodReserveView {
    /// Canonical 8-decimal units, like every other reserve row in this
    /// API — NOT Robinhood's native 18 decimals.
    pub balance_atomic: u64,
    pub protected_minimum_atomic: u64,
    pub reserved_liquidity_atomic: u64,
    /// Liquidity committed to SourceFinalized-or-later requests: the
    /// pending outbound obligations this reserve owes.
    pub pending_obligations_atomic: u64,
    pub accrued_fees_atomic: u64,
    /// Signed — a negative value is itself diagnostic and is not clamped.
    pub available_capacity_atomic: i64,
    pub invariant_holds: bool,
    pub paused: bool,
}

#[derive(Debug, Serialize)]
pub struct ReserveHealthView {
    pub direction: String,
    /// Native reserve unit: 8-decimal Goldcoin atomic for `goldcoin`,
    /// 6-decimal mint atomic for `solana` (docs/09-runbook.md's unit
    /// trap).
    pub total_reserve_balance: u64,
    pub protected_minimum: u64,
    pub reserved_liquidity: u64,
    pub pending_obligations: u64,
    pub accrued_fees: u64,
    pub immature_vault_utxo_total: u64,
    pub mature_available_atomic: u64,
    pub available_utxo_count: u32,
    pub utxo_pool_warning: bool,
    pub paused: bool,
    pub admission_closed: bool,
    pub liquidity_admission_closed: bool,
    /// Confirmed unreserved headroom and the thresholds it is judged
    /// against — signed, because a negative value is itself diagnostic
    /// (see `Ledger::available_capacity`). `(0, 0)` thresholds mean the
    /// buffer is disabled on this deployment.
    pub confirmed_admission_headroom: i64,
    pub admission_buffer_atomic: i64,
    pub admission_reopen_atomic: i64,
    pub invariant_holds: bool,
}

#[derive(Debug, Serialize)]
pub struct RollingWindowView {
    /// `glc-to-sol` (release window) or `sol-to-glc` (deposit window).
    pub window: String,
    pub window_start: i64,
    pub window_total: u64,
    /// Remaining capacity in the CURRENT bucket, mirroring the on-chain
    /// arithmetic exactly (`accounts::rolling_volume_remaining`).
    pub remaining: u64,
}

#[derive(Debug, Serialize)]
pub struct OnchainView {
    pub paused: bool,
    pub release_paused: bool,
    pub deposit_paused: bool,
    /// All limits in the reserve mint's own atomic units — see
    /// `reserve_mint_decimals` for how many decimals that is (read LIVE
    /// from the mint, never assumed).
    pub min_transfer_amount: u64,
    pub per_transfer_limit: u64,
    pub protected_minimum: u64,
    pub rolling_volume_limit: u64,
    pub rolling_window_seconds: i64,
    pub obligation_count: u64,
    /// The reserve mint's live decimals; `None` only before
    /// `initialize_reserve_vault` has configured a mint.
    pub reserve_mint_decimals: Option<u8>,
    pub rolling_windows: Vec<RollingWindowView>,
}

/// The fee is a compile-time constant (docs/20-bridge-fee.md's
/// "Staged fee-change process") — this view is deliberately read-only
/// and there is no endpoint that can change it.
#[derive(Debug, Serialize)]
pub struct RouteFeeEntry {
    pub route: &'static str,
    pub fee_bps: u64,
    pub fee_percent_display: String,
}

/// Every executable route's configured fee.
///
/// Was a single `bridge_fee_bps` describing itself as a "compile-time
/// setting" — true when there was one global rate, and misleading the
/// moment Robinhood was priced separately. Fees are now per route and
/// come from the config's `[fees]` table, so this reports the table.
#[derive(Debug, Serialize)]
pub struct FeeView {
    pub routes: Vec<RouteFeeEntry>,
    pub provenance: &'static str,
}

pub fn fee_view(route_fees: &crate::fees::RouteFees) -> FeeView {
    FeeView {
        routes: route_fees
            .iter()
            .map(|(route, fee_bps)| RouteFeeEntry {
                route: route.as_str(),
                fee_bps,
                fee_percent_display: cli_command::format_atomic_as_decimal_string(fee_bps, 2),
            })
            .collect(),
        provenance: "Config `[fees]`, one rate per executable route — changed with \
                     `glc-admin fees-set` and a deliberate daemon restart",
    }
}

#[derive(Debug, Serialize)]
pub struct ManualReviewItemView {
    pub request_id: i64,
    pub direction: String,
    pub reason: Option<String>,
    /// Canonical (8-decimal) units.
    pub gross_amount_atomic: u64,
    pub net_amount_atomic: u64,
    pub created_at: i64,
    /// Unix time until which the DESTINATION wallet's rolling-24h window
    /// (`ledger::wallet_window`) would refuse a resume, when one applies
    /// right now — on every route. Field name kept from when only the
    /// Goldcoin recipient had one.
    pub recipient_rate_limited_until: Option<i64>,
    /// Unix time until which the SOURCE wallet's rolling-24h window would
    /// refuse a resume, when one applies right now — on every route.
    pub source_wallet_rate_limited_until: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ManualReviewView {
    pub requests: Vec<ManualReviewItemView>,
}

/// One row of the ManualReview refund table: either a refund CANDIDATE (a
/// `SolToGlc` request still in `ManualReview` whose park reason is on
/// [`Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS`] and which has no refund
/// row yet) or an existing refund lifecycle row at any stage.
///
/// Purely a projection of already-persisted state — every field is read
/// from the ledger. Neither the amount nor the destination is ever
/// accepted from a caller: the destination only exists here once
/// `begin_solana_refund` has derived and recorded it from the verified
/// on-chain `WithdrawalObligation.requester`.
#[derive(Debug, Serialize)]
pub struct RefundItemView {
    pub request_id: i64,
    /// `bridge_requests.state` — `ManualReview`, `RefundPending`,
    /// `RefundBroadcast`, or `Refunded`.
    pub request_state: String,
    pub direction: String,
    pub manual_review_reason: Option<String>,
    /// Canonical (8-decimal) gross deposit — exactly what a refund
    /// returns; no fee is deducted (docs/09-runbook.md).
    pub gross_amount_atomic: u64,
    /// The same quantity as a GLC decimal string, for display only.
    pub gross_amount_display_glc: String,
    pub source_obligation_index: Option<u64>,
    /// Original depositor, base58. From the verified on-chain obligation.
    pub requester: Option<String>,
    /// Refund destination, base58 — present once a refund row exists.
    /// Always the depositor's canonical ATA, never operator-supplied.
    pub destination_token_account: Option<String>,
    /// `Pending` | `Broadcast` | `Confirmed`; `None` for a candidate.
    pub refund_state: Option<String>,
    pub refund_signature: Option<String>,
    pub refund_nonce: Option<u64>,
    /// The reserve mint's own native atomic units.
    pub refund_amount_solana_atomic: Option<u64>,
    pub refund_note: Option<String>,
    pub refund_created_by: Option<String>,
    pub refund_created_at: Option<i64>,
    pub refund_broadcast_at: Option<i64>,
    pub refund_confirmed_at: Option<i64>,
    /// The refund confirmed and the request is `Refunded` — terminal.
    /// The console must never offer a refund or resume action on a
    /// terminal row.
    pub terminal: bool,
    /// Whether offering a dry run for this row is meaningful right now.
    /// False for terminal rows.
    pub dry_run_available: bool,
}

#[derive(Debug, Serialize)]
pub struct RefundsView {
    pub refunds: Vec<RefundItemView>,
}

/// Canonical ledger amounts are 8-decimal Goldcoin-native units
/// (docs/20-bridge-fee.md). The refunded GLC quantity is the same whether
/// expressed canonically or in the mint's own 6 decimals, so the listing
/// renders it from the canonical gross and needs no live mint read.
const CANONICAL_DISPLAY_DECIMALS: u8 = 8;

/// Base58 for a 32-byte on-chain address, so views never leak raw byte
/// arrays into JSON.
fn base58(bytes: &[u8; 32]) -> String {
    solana_sdk::pubkey::Pubkey::from(*bytes).to_string()
}

/// One named safety check from the dry run, projected for display.
#[derive(Debug, Serialize)]
pub struct RefundCheckView {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    /// True only for the global-pause check: an operator precondition for
    /// executing, not a property of the request. The console reports it
    /// separately so a not-yet-engaged pause never reads as the request
    /// being ineligible.
    pub is_execute_precondition: bool,
}

/// The chain-derived refund plan — every value DERIVED from the verified
/// on-chain obligation and live `BridgeConfig`, never caller-supplied.
#[derive(Debug, Serialize)]
pub struct RefundPlanView {
    pub obligation_index: u64,
    pub obligation_pda: String,
    pub requester: String,
    pub destination_token_account: String,
    pub destination_exists: bool,
    pub reserve_mint: String,
    pub token_program: String,
    pub mint_decimals: u8,
    /// Native (mint-decimal) units, and the same quantity as GLC.
    pub amount_solana_atomic: u64,
    pub amount_display_glc: String,
    pub gross_canonical_atomic: u64,
    pub refund_nonce: u64,
    pub nonce_pda: String,
    pub nonce_pda_exists: bool,
    pub attestation_epoch: u64,
    pub attestation_threshold: u8,
    pub attestation_key_count: usize,
    pub bridge_paused: bool,
    pub protected_minimum: u64,
    pub reserve_token_account: String,
    pub reserve_balance: u64,
    pub reserve_balance_after: u64,
}

/// The reserve-safety check: stricter than the on-chain floor, because it
/// also excludes liquidity reserved for GlcToSol releases and every other
/// still-open refund.
#[derive(Debug, Serialize)]
pub struct RefundCapacityView {
    pub amount_solana_atomic: u64,
    pub total_reserve_balance: i64,
    pub protected_minimum: i64,
    pub reserved_liquidity: i64,
    pub other_open_refunds_atomic: i64,
    pub ok: bool,
}

/// Result of the strict read-only refund dry run — the identical
/// `solana::refund::dry_run_refund` the `glc-admin refund-manual-review`
/// dry run uses, projected to JSON. Contacts no signer, loads no keypair,
/// writes nothing, broadcasts nothing.
#[derive(Debug, Serialize)]
pub struct RefundDryRunView {
    pub request_id: i64,
    pub request_state: String,
    pub manual_review_reason: Option<String>,
    /// `None` when the chain-side verification failed; `plan_error` then
    /// carries the fail-closed reason, which is itself a failed check.
    pub plan: Option<RefundPlanView>,
    pub plan_error: Option<String>,
    pub capacity: Option<RefundCapacityView>,
    pub checks: Vec<RefundCheckView>,
    /// Every REQUEST-level check passes.
    pub eligible_ignoring_pause: bool,
    /// The on-chain global pause is currently engaged.
    pub pause_engaged: bool,
    /// Executing right now would proceed (eligible AND paused), or the
    /// refund already confirmed and executing is a safe no-op.
    pub would_execute: bool,
    pub already_refunded: bool,
    /// Operator-facing one-line verdict, worded exactly as the CLI's.
    pub verdict: String,
}

/// Projects the shared [`crate::solana::refund::RefundDryRunReport`] into
/// its JSON view. Pure — it adds no eligibility logic of its own; every
/// boolean here comes from the report the refund module produced.
fn refund_dry_run_view(report: crate::solana::refund::RefundDryRunReport) -> RefundDryRunView {
    let verdict = if report.already_refunded {
        "ALREADY REFUNDED — terminal; executing would report the existing transaction and \
         change nothing"
    } else if report.would_execute {
        "ELIGIBLE — executing would proceed (all checks re-run against fresh state first)"
    } else if report.eligible_ignoring_pause {
        "ELIGIBLE, PENDING GLOBAL PAUSE — every request-level check passes. Engage the \
         on-chain global pause, then run the generated command; unpause explicitly afterwards"
    } else {
        "NOT ELIGIBLE — execution would refuse (no override exists)"
    }
    .to_string();

    let plan_error = report.plan.as_ref().err().cloned();
    let plan = report.plan.ok().map(|p| RefundPlanView {
        obligation_index: p.obligation_index,
        obligation_pda: p.obligation_pda.to_string(),
        requester: p.requester.to_string(),
        destination_token_account: p.destination_token_account.to_string(),
        destination_exists: p.destination_exists,
        reserve_mint: p.reserve_mint.to_string(),
        token_program: p.token_program.to_string(),
        mint_decimals: p.mint_decimals,
        amount_solana_atomic: p.amount_solana_atomic,
        amount_display_glc: cli_command::format_atomic_as_decimal_string(
            p.amount_solana_atomic,
            // Live mint decimals, bounded exactly as `cli_command`
            // bounds every chain-fed decimals value before formatting.
            p.mint_decimals.min(19),
        ),
        gross_canonical_atomic: p.gross_canonical_atomic,
        refund_nonce: p.nonce,
        nonce_pda: p.nonce_pda.to_string(),
        nonce_pda_exists: p.nonce_pda_exists,
        attestation_epoch: p.attestation_epoch,
        attestation_threshold: p.attestation_threshold,
        attestation_key_count: p.attestation_keys.len(),
        bridge_paused: p.bridge_paused,
        protected_minimum: p.protected_minimum,
        reserve_token_account: p.reserve_token_account.to_string(),
        reserve_balance: p.reserve_balance,
        reserve_balance_after: p.reserve_balance.saturating_sub(p.amount_solana_atomic),
    });

    RefundDryRunView {
        request_id: report.request.id,
        request_state: report.request.state.as_str().to_string(),
        manual_review_reason: report.db_checks.manual_review_reason.clone(),
        plan,
        plan_error,
        capacity: report.capacity.map(|c| RefundCapacityView {
            amount_solana_atomic: c.amount_solana_atomic,
            total_reserve_balance: c.total_reserve_balance,
            protected_minimum: c.protected_minimum,
            reserved_liquidity: c.reserved_liquidity,
            other_open_refunds_atomic: c.other_open_refunds_atomic,
            ok: c.ok,
        }),
        checks: report
            .checks
            .into_iter()
            .map(|c| RefundCheckView {
                name: c.name.to_string(),
                ok: c.ok,
                detail: c.detail,
                is_execute_precondition: c.is_execute_precondition,
            })
            .collect(),
        eligible_ignoring_pause: report.eligible_ignoring_pause,
        pause_engaged: report.pause_engaged,
        would_execute: report.would_execute,
        already_refunded: report.already_refunded,
        verdict,
    }
}

#[derive(Debug, Serialize)]
pub struct RebalanceView {
    pub id: i64,
    pub direction: String,
    pub kind: String,
    pub amount_atomic: u64,
    pub state: String,
    pub reason: String,
    pub requested_by: String,
    pub requested_at: i64,
    pub required_approvals: u32,
    pub approved_by: Vec<String>,
    pub approved_at: Option<i64>,
    pub tx_reference: Option<String>,
    pub executed_at: Option<i64>,
    pub observed_amount_atomic: Option<u64>,
    pub confirmed_at: Option<i64>,
    pub failure_reason: Option<String>,
}

impl RebalanceView {
    fn from_request(r: RebalanceRequest) -> Self {
        RebalanceView {
            id: r.id,
            // The SAME slugs this API accepts as input
            // (`parse_reserve_direction`, `RebalanceProposeInput.kind`),
            // so a value read from a response round-trips into a request
            // body — and an explicit mapping, never `{:?}`, so a Rust
            // enum rename can't silently change the wire format.
            direction: direction_name(r.direction).to_string(),
            kind: match r.kind {
                RebalanceKind::Deposit => "deposit".to_string(),
                RebalanceKind::Withdraw => "withdraw".to_string(),
            },
            amount_atomic: r.amount_atomic,
            state: r.state.as_str().to_string(),
            reason: r.reason,
            requested_by: r.requested_by,
            requested_at: r.requested_at,
            required_approvals: r.required_approvals,
            approved_by: r.approved_by,
            approved_at: r.approved_at,
            tx_reference: r.tx_reference,
            executed_at: r.executed_at,
            observed_amount_atomic: r.observed_amount_atomic,
            confirmed_at: r.confirmed_at,
            failure_reason: r.failure_reason,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RebalanceStatusView {
    pub direction: String,
    pub severity: String,
    pub total_reserve_balance: u64,
    pub protected_minimum: u64,
    pub target_reserve: u64,
    pub warning_reserve: u64,
    pub critical_reserve: u64,
    pub suggested_deposit_atomic: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct RebalancesView {
    pub assessments: Vec<RebalanceStatusView>,
    pub requests: Vec<RebalanceView>,
}

#[derive(Debug, Serialize)]
pub struct AuditRowView {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub note: String,
    pub outcome: String,
    pub error: Option<String>,
}

impl AuditRowView {
    fn from_row(r: AdminAuditRow) -> Self {
        let (outcome, error) = match r.outcome {
            AdminAuditOutcome::Success => ("success".to_string(), None),
            AdminAuditOutcome::Error(e) => ("error".to_string(), Some(e)),
        };
        AuditRowView {
            id: r.id,
            at: r.at,
            actor: r.actor,
            action: r.action,
            target: r.target,
            old_value: r.old_value,
            new_value: r.new_value,
            note: r.note,
            outcome,
            error,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AuditLogView {
    pub rows: Vec<AuditRowView>,
}

#[derive(Debug, Serialize)]
pub struct WhoamiView {
    pub operator: String,
}

// -------------------------------------------------------------- trait --

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The admin API's data/mutation boundary, mirroring
/// [`crate::api::ApiSource`]'s shape so the HTTP layer (routing, auth,
/// JSON) is testable against a stub. Mutating methods take the
/// authenticated operator name as `actor` and must record the attempt in
/// the admin audit log — success or refusal — before returning.
/// One safety check, as the daemon re-ran it server-side.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GlcRefundCheckView {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

/// What one `POST /refunds/glc/{id}/execute` invocation actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlcRefundAction {
    /// Built, signed and broadcast for the first time.
    Broadcast,
    /// A signed transaction already existed; the SAME bytes were sent
    /// again. No second transaction was constructed.
    Rebroadcast,
    /// Already broadcast by an earlier invocation; nothing to do but wait.
    AlreadyBroadcast,
    /// Already terminal.
    AlreadyRefunded,
}

/// The structured result of a refund execution attempt.
///
/// Deliberately carries no bearer token, no signer identity or endpoint,
/// no signed transaction hex and no key material — only what an operator
/// needs to understand what happened to their request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GlcRefundExecuteView {
    pub request_id: i64,
    pub action: GlcRefundAction,
    /// The refund row's durable lifecycle state after this invocation.
    pub lifecycle_state: String,
    /// The bridge request's state after this invocation.
    pub request_state: String,
    pub source_txid: String,
    pub source_vout: u32,
    pub observed_amount_atomic: u64,
    pub observed_amount_glc: String,
    pub refund_destination: String,
    pub refund_principal_atomic: u64,
    pub refund_principal_glc: String,
    pub fee_atomic: u64,
    pub fee_glc: String,
    /// Present once a transaction exists on chain or in a mempool.
    pub txid: Option<String>,
    pub confirmations: i64,
    /// Which witness backed the principal: "durable chain-vs-ledger", or
    /// the reduced-assurance legacy mode. Reported so an operator never
    /// has to infer which assurance a refund was executed under.
    pub amount_witness_mode: String,
    /// True when this refund used the reduced-assurance legacy mode.
    pub amount_witness_is_legacy: bool,
    /// The scriptPubKey the daemon independently derived for this request
    /// and required the deposit to pay, byte for byte.
    pub expected_deposit_script_hex: String,
    /// Every server-side check, re-run immediately before signing.
    pub checks: Vec<GlcRefundCheckView>,
    pub note: String,
    pub actor: String,
}

pub trait AdminSource: Send + Sync + 'static {
    /// The configured per-route fees, for `GET /fee`.
    ///
    /// On the trait rather than reached out of a concrete type, because
    /// the HTTP layer here is generic over the source and the fee table
    /// is now a per-deployment value rather than a compile-time constant.
    /// Defaulted to empty so an existing implementor keeps compiling and
    /// reports "no rates configured" — which is honest, and is not a
    /// number anything could mistake for a real one.
    fn route_fees(&self) -> crate::fees::RouteFees {
        crate::fees::RouteFees::new()
    }
    fn status(&self) -> BoxFut<'_, Result<AdminStatusView, AdminError>>;
    fn reserve_health(&self) -> BoxFut<'_, Result<Vec<ReserveHealthView>, AdminError>>;
    fn onchain(&self) -> BoxFut<'_, Result<OnchainView, AdminError>>;
    fn manual_review(&self) -> BoxFut<'_, Result<ManualReviewView, AdminError>>;
    /// Read-only: refund candidates and refund lifecycle rows. Pure
    /// ledger projection — no RPC, no signer, no keypair, no mutation.
    fn refunds(&self) -> BoxFut<'_, Result<RefundsView, AdminError>>;
    /// Read-only: the strict PR-#50 refund dry run for one request,
    /// delegating to [`crate::solana::refund::dry_run_refund`] verbatim.
    /// Reads the ledger and the chain; writes nothing, contacts no
    /// signer, loads no keypair, broadcasts nothing.
    fn refund_dry_run(&self, request_id: i64) -> BoxFut<'_, Result<RefundDryRunView, AdminError>>;
    /// THE ONE FUND-MOVING OPERATION on this API.
    ///
    /// Executes (or safely resumes) the Goldcoin-side refund of a
    /// `GlcToSol` request parked in `ManualReview`. Re-runs every Goldcoin
    /// and Solana safety check against fresh state immediately before
    /// signing — nothing validated earlier by `glc-admin` is trusted — and
    /// signs through the daemon's existing 2-of-3 vault signers.
    ///
    /// Takes only a request id and a mandatory audit note. There is no
    /// parameter for a destination, an amount, a fee, a transaction, a
    /// signer or an override, so no caller can influence where the money
    /// goes: every such value is derived server-side from chain evidence.
    fn execute_glc_refund(
        &self,
        request_id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<GlcRefundExecuteView, AdminError>>;
    fn set_local_pause(
        &self,
        direction: ReserveDirection,
        paused: bool,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn set_admission(
        &self,
        direction: ReserveDirection,
        closed: bool,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn resume_manual_review(
        &self,
        request_id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalances(&self) -> BoxFut<'_, Result<RebalancesView, AdminError>>;
    fn rebalance(&self, id: i64) -> BoxFut<'_, Result<RebalanceView, AdminError>>;
    /// Every Robinhood treasury-withdrawal operation, newest first.
    /// READ-ONLY: execution stays a `glc-admin robinhood-treasury-withdraw`
    /// command line holding the submitter key, which this API never does
    /// — the same split the Solana refund keeps.
    fn robinhood_treasury_withdrawals(
        &self,
    ) -> BoxFut<'_, Result<Vec<crate::robinhood::admin::TreasuryWithdrawalView>, AdminError>>;
    fn robinhood_treasury_withdrawal(
        &self,
        operation_id: i64,
    ) -> BoxFut<'_, Result<crate::robinhood::admin::TreasuryWithdrawalView, AdminError>>;
    fn rebalance_propose(
        &self,
        input: RebalanceProposeInput,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalance_approve(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalance_reject(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalance_cancel(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalance_record_executed(
        &self,
        id: i64,
        input: RebalanceRecordExecutedInput,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalance_confirm(
        &self,
        id: i64,
        input: RebalanceConfirmInput,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalance_fail(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn audit_log(&self, filter: AdminAuditFilter) -> BoxFut<'_, Result<AuditLogView, AdminError>>;
    fn cli_command(
        &self,
        input: cli_command::CliCommandInput,
    ) -> BoxFut<'_, Result<cli_command::CliCommandView, AdminError>>;
}

// ------------------------------------------------------- real impl --

/// The real [`AdminSource`]: a fresh `Ledger` connection per call (the
/// same `BEGIN IMMEDIATE`-based concurrency model `api::BridgeApi` and
/// `ops::OpsCollector` use) plus a live Solana RPC for the read-only
/// on-chain views. Holds no admin keypair and no attestation signer, by
/// construction. It holds vault signer handles ONLY when the daemon
/// injected a refund executor via [`AdminApi::with_refund_executor`] —
/// the single, capability-gated fund-moving route documented in the
/// module docs. Every other construction holds none.
/// What the admin API needs to report the Robinhood leg.
///
/// Read-only by construction: a route gate (which only ever answers
/// questions) and the startup facts. There is no signer, no submitter key
/// and no RPC client here — the admin API remains incapable of
/// broadcasting a Robinhood transaction, and the operator CLI stays the
/// only place a Robinhood refund can be executed.
pub struct RobinhoodAdminContext {
    pub route_gate: std::sync::Arc<crate::routes::RouteGate>,
    pub readiness: crate::robinhood::admin::RobinhoodReadiness,
    /// The operator-approved `[robinhood.policy]` for this deployment,
    /// when one is configured.
    ///
    /// A value, not a capability: `ChainPolicy`'s fields are private and
    /// its only constructor validates, so holding one here is evidence
    /// the configured fee and ceilings passed every check in
    /// `chain_policy::ChainPolicy::new` — and nothing more. It grants no
    /// ability to change a limit, on chain or off.
    ///
    /// `None` when the deployment configured a Robinhood indexer but no
    /// `[robinhood.policy]` section. Reported as `null` rather than
    /// substituted, for the reason the whole module repeats: an operator
    /// reading a limit needs to know whether anybody approved it.
    pub policy: Option<crate::chain_policy::ChainPolicy>,
}

pub struct AdminApi<SR: SolanaRpc> {
    db_path: PathBuf,
    rpc: SR,
    /// The ONE fund-moving capability, present only when the deployment
    /// wired it. `None` — the default, and every pre-existing
    /// construction — keeps this API strictly incapable of moving funds:
    /// the endpoint answers "not enabled" rather than failing somewhere
    /// deeper. Fail-closed by absence, not by flag.
    refund_executor: Option<std::sync::Arc<dyn glc_refund_exec::GlcRefundExecutor>>,
    /// `None` on every deployment that has not configured Robinhood.
    robinhood: Option<RobinhoodAdminContext>,
    /// The configured per-route fees, for `GET /fee`. Read-only here —
    /// this API reports the table and has no endpoint that changes it.
    route_fees: crate::fees::RouteFees,
}

impl<SR: SolanaRpc> AdminApi<SR> {
    pub fn new(db_path: PathBuf, rpc: SR) -> Self {
        AdminApi {
            db_path,
            rpc,
            refund_executor: None,
            robinhood: None,
            // Empty until `with_route_fees`: `GET /fee` then reports an
            // empty table, which is the honest answer for an API that was
            // never told the rates, and is not a rate anything could
            // mistake for a real one.
            route_fees: crate::fees::RouteFees::new(),
        }
    }

    /// Supplies the configured per-route fees for `GET /fee`.
    ///
    /// A BUILDER rather than a `new` parameter, matching the other two
    /// optional contexts here: this API prices nothing and moves nothing,
    /// so an absent table degrades one read-only endpoint rather than
    /// risking a wrong number anywhere.
    pub fn with_route_fees(mut self, route_fees: crate::fees::RouteFees) -> Self {
        self.route_fees = route_fees;
        self
    }

    /// Adds the Robinhood route/readiness context.
    ///
    /// A BUILDER, like [`AdminApi::with_refund_executor`], so every
    /// existing construction is untouched and a deployment that never
    /// calls it serves exactly the responses it always did — the
    /// Robinhood fields are omitted from the JSON entirely rather than
    /// serialized as empty, which is a different statement.
    ///
    /// It grants no capability. Everything it enables is a READ.
    pub fn with_robinhood(mut self, robinhood: RobinhoodAdminContext) -> Self {
        self.robinhood = Some(robinhood);
        self
    }

    /// Grants this API the Goldcoin refund execution capability. Called
    /// only by the daemon, which owns the vault signers; nothing else in
    /// the tree calls it, so no other process can gain it by accident.
    pub fn with_refund_executor(
        mut self,
        executor: std::sync::Arc<dyn glc_refund_exec::GlcRefundExecutor>,
    ) -> Self {
        self.refund_executor = Some(executor);
        self
    }

    fn open_ledger(&self) -> Result<Ledger, AdminError> {
        Ledger::open(&self.db_path)
            .map_err(|_| AdminError::Ledger("could not open ledger".to_string()))
    }

    fn now() -> i64 {
        now_unix()
    }

    async fn fetch_onchain(&self) -> Result<OnchainView, AdminError> {
        let config_account = self
            .rpc
            .get_account(&accounts::bridge_config_pda())
            .await
            .map_err(|e| AdminError::Upstream(format!("bridge config read failed: {e}")))?
            .ok_or_else(|| AdminError::Upstream("bridge config account not found".to_string()))?;
        let config = accounts::decode_bridge_config(&config_account.data)
            .map_err(|e| AdminError::Upstream(format!("bridge config decode failed: {e}")))?;

        let now = Self::now();
        let mut rolling_windows = Vec::with_capacity(2);
        for (byte, name) in [(0u8, "glc-to-sol"), (1u8, "sol-to-glc")] {
            let account = self
                .rpc
                .get_account(&accounts::rolling_volume_window_pda(byte))
                .await
                .map_err(|e| AdminError::Upstream(format!("rolling window read failed: {e}")))?
                .ok_or_else(|| {
                    AdminError::Upstream("rolling window account not found".to_string())
                })?;
            let window = accounts::decode_rolling_volume_window(&account.data)
                .map_err(|e| AdminError::Upstream(format!("rolling window decode failed: {e}")))?;
            let remaining = accounts::rolling_volume_remaining(
                config.rolling_volume_limit,
                config.rolling_window_seconds,
                window,
                now,
            );
            rolling_windows.push(RollingWindowView {
                window: name.to_string(),
                window_start: window.window_start,
                window_total: window.window_total,
                remaining,
            });
        }

        // The mint's LIVE decimals — never a compile-time assumption
        // (`amount_conversion` module docs) — read here so every consumer
        // of this view, notably `cli_command`'s GLC→atomic conversion,
        // converts against what the chain actually says. `None` only in
        // the pre-`initialize_reserve_vault` state, where no mint is
        // configured yet.
        let reserve_mint_decimals = if config.reserve_token_mint == Pubkey::default() {
            None
        } else {
            Some(
                accounts::fetch_reserve_mint_decimals(&self.rpc, &config.reserve_token_mint)
                    .await
                    .map_err(|e| {
                        AdminError::Upstream(format!("reserve mint decimals read failed: {e}"))
                    })?,
            )
        };

        Ok(OnchainView {
            paused: config.paused,
            release_paused: config.release_paused,
            deposit_paused: config.deposit_paused,
            min_transfer_amount: config.min_transfer_amount,
            per_transfer_limit: config.per_transfer_limit,
            protected_minimum: config.protected_minimum,
            rolling_volume_limit: config.rolling_volume_limit,
            rolling_window_seconds: config.rolling_window_seconds,
            obligation_count: config.obligation_count,
            reserve_mint_decimals,
            rolling_windows,
        })
    }
}

/// Builds [`AdminStatusView::route_admission`] — the route-scoped
/// admission axis beside the reserve-wide one, for every route that has
/// a route-scoped gate.
///
/// Returns an EMPTY vector on a pre-v25 ledger, where `route_admission`
/// does not exist. That is absence, not "both routes are open", and the
/// two are reported differently for the same reason
/// `Ledger::route_ledger_rows` distinguishes them: the remedies differ
/// (run the migration versus write the flag).
///
/// Read-only in the strongest sense: `route_admission_blocker` evaluates
/// the confirmed-liquidity gate's PERSISTED state and never the
/// hysteresis rule, so rendering this page can never move a gate.
fn route_admission_status(ledger: &Ledger) -> Result<Vec<RouteAdmissionStatusView>, AdminError> {
    let Some(state) = ledger.route_admission_rows()? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(state.rows.len());
    for row in &state.rows {
        // Every route in this table is admission-settable and therefore
        // has a `Direction` — `Route::is_admission_settable` implies
        // `as_direction().is_some()`, pinned by
        // `routes::tests::admission_settable_routes_all_have_a_direction`.
        // A row that somehow lacked one is skipped rather than rendered
        // against a guessed reserve.
        let Some(direction) = row.route.as_direction() else {
            continue;
        };
        let reserve = direction.destination_reserve();
        // A destination reserve this deployment has no row for — the
        // Robinhood reserve on a deployment without `[reserve.robinhood]`,
        // reachable since the two Solana<->Robinhood routes gained
        // admission rows — admits nothing, and must render as exactly
        // that rather than fail the whole status page.
        let (blocker, reserve_paused, reserve_admission_closed) =
            match ledger.route_admission_blocker(direction) {
                Ok(blocker) => (
                    blocker.map(|b| b.as_str().to_string()),
                    ledger.is_paused(reserve)?,
                    ledger.is_admission_closed(reserve)?,
                ),
                Err(LedgerError::ReserveNotInitialized(_)) => {
                    (Some("reserve_not_configured".to_string()), false, false)
                }
                Err(e) => return Err(e.into()),
            };
        out.push(RouteAdmissionStatusView {
            route: row.route.as_str().to_string(),
            destination_reserve: direction_name(reserve).to_string(),
            route_admission_closed: row.admission_closed,
            route_admission_closed_reason: row.admission_closed_reason.clone(),
            route_admission_updated_at: row.updated_at,
            reserve_paused,
            reserve_admission_closed,
            admits_now: blocker.is_none(),
            blocker,
        });
    }
    Ok(out)
}

fn direction_name(direction: ReserveDirection) -> &'static str {
    match direction {
        ReserveDirection::GoldcoinReserve => "goldcoin",
        ReserveDirection::SolanaReserve => "solana",
        ReserveDirection::RobinhoodReserve => "robinhood",
    }
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn storage_error() -> AdminError {
    AdminError::Ledger("ledger storage error".to_string())
}

/// Descriptor for one audited admin action — who did what to what, with
/// the mandatory note and the new-value snapshot the audit row records.
/// The OLD value is deliberately not a field: it is read inside the
/// audited transaction (see [`audited_mutation`]) so concurrent
/// mutations can never record stale, impossible old→new histories.
pub struct AuditedAction<'a> {
    pub actor: &'a str,
    pub action: &'a str,
    pub target: String,
    pub note: &'a str,
    pub new_value: Option<String>,
}

/// Runs one admin mutation with the audit discipline, ATOMICALLY: the
/// old-value read, the mutation, and the audit row all share one
/// `BEGIN IMMEDIATE` scope ([`Ledger::begin_admin_action`]), so either
/// the mutation persists together with its audit row or neither does —
/// an audit-append failure rolls the already-applied mutation back
/// instead of leaving it committed and unaudited (where a retry would
/// duplicate a non-idempotent action), and the old-value snapshot is
/// taken under the same write lock, so two racing operators can never
/// both record the same impossible old→new transition. A validated
/// refusal from the mutation rolls back only the mutation's own writes
/// (its inner savepoint) and the scope then commits just the failure
/// audit row, so refusals stay audited. `on_success` may adjust the
/// entry once the mutation's result is known — the target for propose
/// (the id it just created), the new-value for resume (whose no-op
/// outcome must never be recorded as a transition that happened).
///
/// Shared by the admin API's HTTP handlers and `glc-admin`'s local
/// mutation commands — one implementation, so the two surfaces cannot
/// drift on what gets audited.
pub fn audited_mutation<T>(
    ledger: &mut Ledger,
    mut params: AuditedAction<'_>,
    old_value: impl FnOnce(&mut Ledger) -> Result<Option<String>, AdminError>,
    mutation: impl FnOnce(&mut Ledger) -> Result<T, AdminError>,
    on_success: impl FnOnce(&T, &mut AuditedAction<'_>),
) -> Result<(T, MutationReceipt), AdminError> {
    ledger.begin_admin_action().map_err(|_| storage_error())?;
    // Inside the scope: this read holds the same write lock the mutation
    // will use, so the snapshot cannot go stale under a concurrent
    // mutation. A failing pre-read aborts before anything mutated —
    // nothing to audit.
    let old_value = match old_value(ledger) {
        Ok(v) => v,
        Err(e) => {
            let _ = ledger.rollback_admin_action();
            return Err(e);
        }
    };
    let result = mutation(ledger);
    if let Ok(value) = &result {
        on_success(value, &mut params);
    }
    let outcome = match &result {
        Ok(_) => AdminAuditOutcome::Success,
        Err(e) => AdminAuditOutcome::Error(e.to_string()),
    };
    let entry = AdminAuditEntry {
        at: now_unix(),
        actor: params.actor.to_string(),
        action: params.action.to_string(),
        target: Some(params.target.clone()),
        old_value: old_value.clone(),
        new_value: params.new_value.clone(),
        note: params.note.to_string(),
        outcome,
    };
    match ledger.append_admin_audit(&entry) {
        Ok(audit_id) => {
            if ledger.commit_admin_action().is_err() {
                let _ = ledger.rollback_admin_action();
                return Err(storage_error());
            }
            let value = result?;
            Ok((
                value,
                MutationReceipt {
                    audit_id,
                    action: params.action.to_string(),
                    target: params.target,
                    old_value,
                    new_value: params.new_value,
                },
            ))
        }
        Err(_) => {
            let _ = ledger.rollback_admin_action();
            Err(AdminError::Ledger(
                "ledger storage error: the audit row could not be written, so the action was \
                 rolled back"
                    .to_string(),
            ))
        }
    }
}

/// The audit wiring shared by every local `reserve_ledger.paused`
/// mutation, so the surfaces below cannot drift on what an audit row for
/// a pause looks like: one action name (`pause`/`unpause`), one target
/// (the reserve's operator-facing name), one `paused=<bool>` old/new
/// value pair, all inside [`audited_mutation`]'s single atomic scope.
///
/// `apply` is the only thing that varies — which is the point. Pausing
/// is an unconditional emergency stop everywhere; UNpausing is guarded
/// on the reserves where a guard exists, and the guard lives in
/// [`guard`], never inline here.
fn audited_local_pause_with(
    ledger: &mut Ledger,
    direction: ReserveDirection,
    paused: bool,
    note: &str,
    actor: &str,
    apply: impl FnOnce(&mut Ledger) -> Result<(), AdminError>,
) -> Result<MutationReceipt, AdminError> {
    // One note shape regardless of surface: the CLI validates but does
    // not trim, the HTTP layer trims — normalize here so the shared
    // audit log never records the same note padded from one surface and
    // bare from the other.
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: if paused { "pause" } else { "unpause" },
            target: direction_name(direction).to_string(),
            note,
            new_value: Some(format!("paused={paused}")),
        },
        |l| Ok(Some(format!("paused={}", l.is_paused(direction)?))),
        apply,
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
}

/// Local reserve-direction pause/unpause, audited — the one
/// implementation behind both `POST /pause`//`unpause` and `glc-admin
/// pause`/`unpause`.
///
/// Reaches `GoldcoinReserve` and `SolanaReserve` only, because those are
/// the directions both of those surfaces parse. The third reserve's
/// local gate has its own command and its own unpause guard; see
/// [`audited_set_robinhood_local_pause`].
pub fn audited_set_local_pause(
    ledger: &mut Ledger,
    direction: ReserveDirection,
    paused: bool,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    let trimmed = note.trim();
    audited_local_pause_with(ledger, direction, paused, note, actor, |l| {
        l.set_paused(direction, paused, Some(trimmed))
            .map_err(AdminError::from)
    })
}

/// The LOCAL `RobinhoodReserve.paused` gate, audited — the one
/// implementation behind `glc-admin robinhood-local-pause`.
///
/// # Why this is its own entry point rather than a third `--direction`
///
/// Not because the flag is different — it is the same
/// `reserve_ledger.paused` column, written through the same
/// [`Ledger::set_paused`], recorded with the same audit shape as
/// [`audited_set_local_pause`] (which is why both go through
/// [`audited_local_pause_with`] rather than each spelling the wiring
/// out). It is separate because UNpausing it is guarded and unpausing
/// the other two is not: `GoldcoinReserve`'s pause is the vault-sweep
/// and refund emergency stop whose documented recovery step is an
/// unconditional `glc-admin unpause`, and `SolanaReserve`'s is what
/// [`crate::quota`] engages automatically on rolling-volume exhaustion.
/// Adding a refusal to either would change a documented production
/// recovery path; adding one here does not, because nothing could reach
/// this flag before.
///
/// # Scope
///
/// Writes exactly one column of exactly one row: `paused` on
/// `reserve_ledger`'s `RobinhoodReserve` row (plus `pause_reason`, the
/// last-write-wins display note [`Ledger::set_paused`] has always
/// recorded alongside it). It is the `GlcToRhn` LOCAL RESERVE GATE and
/// nothing else — see [`guard::unpause_robinhood_reserve_guarded`] for
/// the full list of flags it is not, and does not touch.
pub fn audited_set_robinhood_local_pause(
    ledger: &mut Ledger,
    paused: bool,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    let trimmed = note.trim();
    audited_local_pause_with(
        ledger,
        ReserveDirection::RobinhoodReserve,
        paused,
        note,
        actor,
        |l| {
            if paused {
                // An emergency stop is never refused.
                l.set_paused(ReserveDirection::RobinhoodReserve, true, Some(trimmed))
                    .map_err(AdminError::from)
            } else {
                // Refusals land INSIDE the audited scope, exactly as
                // `audited_set_admission`'s direction restriction does,
                // so a refused unpause still leaves an audit row.
                guard::unpause_robinhood_reserve_guarded(l, trimmed).map_err(|e| match e {
                    guard::OpenAdmissionError::Refused(message) => AdminError::Conflict(message),
                    guard::OpenAdmissionError::Ledger(ledger_error) => {
                        AdminError::from(ledger_error)
                    }
                })
            }
        },
    )
}

/// Admission close/open, audited — the one implementation behind both
/// `POST /admission/...` and `glc-admin close-admission`/
/// `open-admission`. The goldcoin-direction-only rule is enforced INSIDE
/// the audited scope, so even that refusal leaves an audit row.
pub fn audited_set_admission(
    ledger: &mut Ledger,
    direction: ReserveDirection,
    closed: bool,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    // One note shape regardless of surface: the CLI validates but does
    // not trim, the HTTP layer trims — normalize here so the shared
    // audit log never records the same note padded from one surface and
    // bare from the other.
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: if closed {
                "admission_close"
            } else {
                "admission_open"
            },
            target: direction_name(direction).to_string(),
            note,
            new_value: Some(format!("admission_closed={closed}")),
        },
        |l| {
            if direction == ReserveDirection::GoldcoinReserve {
                Ok(Some(format!(
                    "admission_closed={}",
                    l.is_admission_closed(direction)?
                )))
            } else {
                Ok(None)
            }
        },
        |l| {
            // Same restriction as always: only the Goldcoin direction
            // implements admission control in this version.
            if direction != ReserveDirection::GoldcoinReserve {
                return Err(AdminError::BadRequest(
                    "admission control is only implemented for direction goldcoin in this version"
                        .to_string(),
                ));
            }
            if closed {
                l.set_admission(direction, true, Some(note))
                    .map_err(AdminError::from)
            } else {
                guard::open_admission_guarded(l, direction, note).map_err(|e| match e {
                    guard::OpenAdmissionError::Refused(message) => AdminError::Conflict(message),
                    guard::OpenAdmissionError::Ledger(ledger_error) => {
                        AdminError::from(ledger_error)
                    }
                })
            }
        },
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
}

/// Per-route ledger enablement, audited — the one implementation behind
/// `glc-admin robinhood-route-enable`/`robinhood-route-disable`.
///
/// Sets ONE of [`crate::routes::RouteGate`]'s three gates: the
/// `bridge_routes` row. Enabling here is necessary and nowhere near
/// sufficient — the config gate, the adapter-capability gate, the
/// contract's own `routeEnabled`/pause flags, preflight, the signer
/// quorum and reserve availability all still decide every transfer
/// independently, and none of them is touched by this call.
///
/// Restricted to [`crate::routes::Route::is_operator_settable`] routes
/// (`GlcToRhn`/`RhnToGlc`) by [`crate::ledger::Ledger::
/// set_route_enabled`] itself, INSIDE the audited scope, so a refused
/// attempt still leaves an audit row — the same discipline
/// [`audited_set_admission`] uses for its direction restriction.
pub fn audited_set_route_enabled(
    ledger: &mut Ledger,
    route: crate::routes::Route,
    enabled: bool,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    // One note shape regardless of surface, as everywhere else here.
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: if enabled {
                "route_enable"
            } else {
                "route_disable"
            },
            target: route.as_str().to_string(),
            note,
            new_value: Some(format!("enabled={enabled}")),
        },
        |l| {
            Ok(Some(format!(
                "enabled={}",
                l.route_enabled(route.as_str(), route.default_enabled())?
            )))
        },
        |l| {
            l.set_route_enabled(route, enabled, (!enabled).then_some(note))
                .map_err(AdminError::from)
        },
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
}

/// Per-route ADMISSION, audited — the one implementation behind
/// `glc-admin route-admission-close`/`route-admission-open`.
///
/// # A different axis from [`audited_set_route_enabled`]
///
/// That function writes `bridge_routes.enabled`: one of
/// [`crate::routes::RouteGate`]'s three ENABLEMENT gates, settable for
/// `GlcToRhn`/`RhnToGlc`. This writes `route_admission.admission_closed`:
/// a route-scoped ADMISSION gate, settable for `SolToGlc`/`RhnToGlc`,
/// evaluated by [`crate::ledger::InboundAdmissionGates`] alongside the
/// destination reserve's own `paused`/`admission_closed`. The two sets
/// overlap in exactly one route and neither substitutes for the other;
/// see [`crate::routes::Route::is_admission_settable`] for the table.
///
/// # What it can and cannot do
///
/// Closing a route parks NEWLY observed deposits on that route alone
/// into `ManualReview` with `route_admission_closed_at_fold`, leaving
/// the other inbound route running out of the same reserve. It never
/// affects an already-accepted obligation — payout processing has never
/// been gated by any admission flag and still is not.
///
/// Opening a route grants nothing on its own: the reserve-wide `paused`
/// and `admission_closed`, the confirmed-liquidity gate, the mature-UTXO
/// floor, capacity, the route ENABLEMENT gate and (for `RhnToGlc`) the
/// custody contract's own flags all still stand in front of every
/// deposit. It runs behind [`guard::open_route_admission_guarded`],
/// which applies the SAME three safety checks `open-admission` does, so
/// it cannot be used to route around them.
///
/// Which routes it accepts is enforced by
/// [`crate::ledger::Ledger::set_route_admission`] INSIDE the audited
/// scope, so a refused attempt still leaves an audit row — the same
/// discipline [`audited_set_admission`] and [`audited_set_route_enabled`]
/// use for their own restrictions.
pub fn audited_set_route_admission(
    ledger: &mut Ledger,
    route: crate::routes::Route,
    closed: bool,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    // One note shape regardless of surface, as everywhere else here.
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: if closed {
                "route_admission_close"
            } else {
                "route_admission_open"
            },
            target: route.as_str().to_string(),
            note,
            new_value: Some(format!("admission_closed={closed}")),
        },
        |l| {
            Ok(Some(format!(
                "admission_closed={}",
                l.route_admission_closed(route)?
            )))
        },
        |l| {
            if closed {
                l.set_route_admission(route, true, Some(note))
                    .map_err(AdminError::from)
            } else {
                guard::open_route_admission_guarded(l, route, note).map_err(|e| match e {
                    guard::OpenAdmissionError::Refused(message) => AdminError::Conflict(message),
                    guard::OpenAdmissionError::Ledger(ledger_error) => {
                        AdminError::from(ledger_error)
                    }
                })
            }
        },
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
}

/// Places an operator auto-resume hold on one `ManualReview` request
/// (schema v29), through the shared audit path — see
/// [`Ledger::set_manual_review_hold`] for every refusal, none of which is
/// re-implemented here.
pub fn audited_manual_review_hold(
    ledger: &mut Ledger,
    request_id: i64,
    hold_until: i64,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: "manual_review_hold",
            target: request_id.to_string(),
            note,
            new_value: Some(format!("auto_resume_hold_until={hold_until}")),
        },
        |l| {
            Ok(l.get_request(request_id)?.map(|r| {
                format!(
                    "state={} hold={}",
                    r.state.as_str(),
                    r.auto_resume_hold_until
                        .map(|u| u.to_string())
                        .unwrap_or_else(|| "none".to_string())
                )
            }))
        },
        |l| {
            l.set_manual_review_hold(request_id, hold_until, note, actor, now_unix())
                .map_err(AdminError::from)
        },
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
}

/// Clears a hold placed by [`audited_manual_review_hold`]. Returns whether
/// a hold was actually removed (`false` = already unheld, no-op).
pub fn audited_manual_review_hold_release(
    ledger: &mut Ledger,
    request_id: i64,
    note: &str,
    actor: &str,
) -> Result<(bool, MutationReceipt), AdminError> {
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: "manual_review_hold_release",
            target: request_id.to_string(),
            note,
            new_value: None,
        },
        |l| {
            Ok(l.get_request(request_id)?.map(|r| {
                format!(
                    "hold={}",
                    r.auto_resume_hold_until
                        .map(|u| u.to_string())
                        .unwrap_or_else(|| "none".to_string())
                )
            }))
        },
        |l| {
            l.clear_manual_review_hold(request_id, actor, now_unix())
                .map_err(AdminError::from)
        },
        |released, params| {
            params.new_value = Some(if *released {
                "hold=none".to_string()
            } else {
                "no-op: not held".to_string()
            });
        },
    )
}

/// ManualReview resume, audited — the one implementation behind both
/// `POST /manual-review/{id}/resume` and `glc-admin
/// resume-manual-review`. The authenticated `actor` is recorded on BOTH
/// trails: the admin audit row and `bridge_request_state_log`'s
/// transition row (never a hardcoded placeholder), so per-operator
/// attribution survives into the request's authoritative history.
pub fn audited_resume_manual_review(
    ledger: &mut Ledger,
    request_id: i64,
    note: &str,
    actor: &str,
) -> Result<(crate::ledger::ResumeManualReviewOutcome, MutationReceipt), AdminError> {
    // One note shape regardless of surface: the CLI validates but does
    // not trim, the HTTP layer trims — normalize here so the shared
    // audit log never records the same note padded from one surface and
    // bare from the other.
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: "resume_manual_review",
            target: request_id.to_string(),
            note,
            // Overwritten by `on_success` below once the real outcome is
            // known — an AlreadyResumed no-op must never be recorded as
            // a transition that happened.
            new_value: None,
        },
        |l| {
            Ok(l.get_request(request_id)?
                .map(|r| r.state.as_str().to_string()))
        },
        |l| {
            // Called as-is: every safety check (direction, state, reason
            // whitelist, no-payout, refund lifecycle, capacity, and the
            // unconditional source-wallet/recipient rate-limit re-checks)
            // lives INSIDE the Ledger method and is never re-implemented
            // or pre-filtered here.
            //
            // The only thing decided out here is WHICH of the two thin
            // wrappers to call, from the request's own recorded
            // direction, so an operator resuming an `RhnToGlc` park does
            // not have to know that it is a different entry point. An
            // unreadable or non-inbound direction falls through to the
            // Solana wrapper, which then refuses it by direction with
            // `NotASolToGlcRequest` — the pre-existing behaviour for a
            // wrong-direction request id, unchanged.
            let direction = l.get_request(request_id)?.map(|r| r.direction);
            let outcome = match direction {
                Some(crate::ledger::Direction::RhnToGlc) => {
                    l.resume_manual_review_rhn_to_glc(request_id, note, actor, now_unix())
                }
                Some(
                    d @ (crate::ledger::Direction::SolToRhn | crate::ledger::Direction::RhnToSol),
                ) => l.resume_manual_review_cross_route(d, request_id, note, actor, now_unix()),
                _ => l.resume_manual_review_sol_to_glc(request_id, note, actor, now_unix()),
            };
            outcome.map_err(AdminError::from)
        },
        |outcome, params| {
            params.new_value = Some(match outcome {
                crate::ledger::ResumeManualReviewOutcome::Resumed => "SourceFinalized".to_string(),
                crate::ledger::ResumeManualReviewOutcome::AlreadyResumed { state } => {
                    format!("no-op: already resumed (state={})", state.as_str())
                }
            });
        },
    )
}

/// Refund lifecycle begin, audited — the `ManualReview -> RefundPending`
/// transition plus the `solana_refunds` row, atomic with its audit row.
/// EXECUTION is a CLI-only surface (`glc-admin refund-manual-review
/// --execute`, via `solana::refund::execute_refund`): the HTTP admin API
/// deliberately has no refund EXECUTION route, matching its
/// no-keypair/no-transaction posture — refund execution needs the admin
/// keypair and the signer stack, which never belong on that surface. The
/// API does serve the read-only halves (`GET /refunds`, `GET
/// /refunds/{id}/dry-run`) and generates the `glc-admin` command line for
/// a human to run, exactly as it does for `set_paused`/`set_limit`.
pub fn audited_begin_solana_refund(
    ledger: &mut Ledger,
    request_id: i64,
    verified: &crate::ledger::VerifiedRefundInputs,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    let note = note.trim();
    let verified = *verified;
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: "refund_begin",
            target: request_id.to_string(),
            note,
            new_value: Some(format!(
                "RefundPending (nonce {:#x}, amount {} native, destination ATA of original \
                 requester)",
                Ledger::solana_refund_nonce(request_id).map_err(AdminError::from)?,
                verified.amount_solana_atomic
            )),
        },
        |l| {
            Ok(l.get_request(request_id)?
                .map(|r| r.state.as_str().to_string()))
        },
        |l| {
            l.begin_solana_refund(request_id, &verified, note, actor, now_unix())
                .map_err(AdminError::from)
        },
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
}

/// Refund broadcast record, audited — persists the transaction signature,
/// blockhash, epoch, and the simulation summary BEFORE the send, atomic
/// with its audit row.
#[allow(clippy::too_many_arguments)]
pub fn audited_record_solana_refund_broadcast(
    ledger: &mut Ledger,
    request_id: i64,
    refund_signature: &str,
    recent_blockhash: &str,
    attestation_epoch: u64,
    simulation_summary: &str,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: "refund_broadcast",
            target: request_id.to_string(),
            note,
            new_value: Some(format!(
                "RefundBroadcast tx {refund_signature} ({simulation_summary})"
            )),
        },
        |l| {
            Ok(l.get_request(request_id)?
                .map(|r| r.state.as_str().to_string()))
        },
        |l| {
            l.record_solana_refund_broadcast(
                request_id,
                refund_signature,
                recent_blockhash,
                attestation_epoch,
                now_unix(),
            )
            .map_err(AdminError::from)
        },
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
}

/// Refund confirmation, audited — the terminal `Refunded` transition plus
/// the SolanaReserve book debit, atomic with its audit row.
pub fn audited_mark_solana_refund_confirmed(
    ledger: &mut Ledger,
    request_id: i64,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: "refund_confirm",
            target: request_id.to_string(),
            note,
            new_value: Some("Refunded".to_string()),
        },
        |l| {
            Ok(l.get_request(request_id)?
                .map(|r| r.state.as_str().to_string()))
        },
        |l| {
            l.mark_solana_refund_confirmed(request_id, now_unix())
                .map_err(AdminError::from)
        },
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
}

impl<SR: SolanaRpc + Send + Sync + 'static> AdminSource for AdminApi<SR> {
    fn route_fees(&self) -> crate::fees::RouteFees {
        self.route_fees.clone()
    }

    fn status(&self) -> BoxFut<'_, Result<AdminStatusView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let mut views = Vec::new();
            for (reserve, transfer_direction) in [
                (ReserveDirection::SolanaReserve, Direction::GlcToSol),
                (ReserveDirection::GoldcoinReserve, Direction::SolToGlc),
            ] {
                let snapshot = reserve_health::check(&ledger, reserve, unix_now())?;
                let manual_review = ledger
                    .requests_by_state(transfer_direction, RequestState::ManualReview)?
                    .len();
                views.push(DirectionStatusView {
                    direction: direction_name(reserve).to_string(),
                    paused: snapshot.paused,
                    pause_reason: ledger.pause_reason(reserve)?,
                    admission_closed: snapshot.admission_closed,
                    admission_reason: ledger.admission_reason(reserve)?,
                    liquidity_admission_closed: snapshot.liquidity_admission_closed,
                    manual_review_count: manual_review,
                });
            }
            let sol_to_glc = views.pop().expect("two views were pushed");
            let glc_to_sol = views.pop().expect("two views were pushed");
            // Empty unless Robinhood is configured. The contract's own
            // route flag is left `None` here: reading it needs an
            // `eth_call`, and this API deliberately holds no Robinhood
            // RPC client. `glc-admin robinhood-preflight` is where the
            // on-chain flag is read.
            let robinhood_routes = match &self.robinhood {
                None => Vec::new(),
                Some(context) => crate::robinhood::admin::route_status(
                    &ledger,
                    &context.route_gate,
                    &context.readiness,
                    |_| None,
                )
                .into_iter()
                .map(|status| RobinhoodRouteView {
                    route: status.route.to_string(),
                    source_chain: status.source_chain.to_string(),
                    destination_chain: status.destination_chain.to_string(),
                    implemented: status.implemented,
                    service_enabled: status.service_enabled,
                    contract_route_enabled: status.contract_route_enabled,
                    effective_available: status.effective_available,
                    disabled_reason: status.disabled_reason,
                    health_reason: status.health_reason,
                    per_transfer_limit_atomic: context
                        .policy
                        .map(|policy| policy.per_transfer_limit().0),
                    // `status.route` is `Route::as_str()`'s own output, so
                    // it round-trips; the fallback is the same policy
                    // constant `source_minimum` would return anyway, so a
                    // spelling this build did not expect still reports the
                    // floor rather than a hole.
                    min_transfer_atomic: status
                        .route
                        .parse::<crate::routes::Route>()
                        .map(crate::min_transfer::source_minimum)
                        .unwrap_or(crate::min_transfer::SOURCE_MINIMUM_CANONICAL)
                        .0,
                })
                .collect(),
            };
            Ok(AdminStatusView {
                glc_to_sol,
                sol_to_glc,
                post_finality_reorg_events: ledger.post_finality_reorg_event_count()?,
                robinhood_routes,
                route_admission: route_admission_status(&ledger)?,
            })
        })
    }

    fn reserve_health(&self) -> BoxFut<'_, Result<Vec<ReserveHealthView>, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let mut out = Vec::with_capacity(2);
            for direction in [
                ReserveDirection::GoldcoinReserve,
                ReserveDirection::SolanaReserve,
            ] {
                let s = reserve_health::check(&ledger, direction, unix_now())?;
                out.push(ReserveHealthView {
                    direction: direction_name(direction).to_string(),
                    total_reserve_balance: s.total_reserve_balance,
                    protected_minimum: s.protected_minimum,
                    reserved_liquidity: s.reserved_liquidity,
                    pending_obligations: s.pending_obligations,
                    accrued_fees: s.accrued_fees,
                    immature_vault_utxo_total: s.immature_vault_utxo_total,
                    mature_available_atomic: s.utxo_pool.mature_available_atomic,
                    available_utxo_count: s.utxo_pool.available_utxo_count,
                    utxo_pool_warning: s.utxo_pool_warning,
                    paused: s.paused,
                    admission_closed: s.admission_closed,
                    liquidity_admission_closed: s.liquidity_admission_closed,
                    confirmed_admission_headroom: s.confirmed_admission_headroom,
                    admission_buffer_atomic: s.admission_buffer_atomic,
                    admission_reopen_atomic: s.admission_reopen_atomic,
                    invariant_holds: s.invariant_holds,
                });
            }
            // A THIRD independent reserve, appended rather than merged:
            // never netted against either of the two above, and absent
            // entirely when `[reserve.robinhood]` was never configured —
            // an unconfigured reserve has no row, and reporting zeroes
            // would read as "configured and empty".
            if let Ok(s) =
                reserve_health::check(&ledger, ReserveDirection::RobinhoodReserve, unix_now())
            {
                out.push(ReserveHealthView {
                    direction: direction_name(ReserveDirection::RobinhoodReserve).to_string(),
                    total_reserve_balance: s.total_reserve_balance,
                    protected_minimum: s.protected_minimum,
                    reserved_liquidity: s.reserved_liquidity,
                    pending_obligations: s.pending_obligations,
                    accrued_fees: s.accrued_fees,
                    // Robinhood's reserve is a contract balance; it has no
                    // UTXO-pool concept, so these are structurally zero
                    // rather than unmeasured.
                    immature_vault_utxo_total: s.immature_vault_utxo_total,
                    mature_available_atomic: s.utxo_pool.mature_available_atomic,
                    available_utxo_count: s.utxo_pool.available_utxo_count,
                    utxo_pool_warning: s.utxo_pool_warning,
                    paused: s.paused,
                    admission_closed: s.admission_closed,
                    liquidity_admission_closed: s.liquidity_admission_closed,
                    confirmed_admission_headroom: s.confirmed_admission_headroom,
                    admission_buffer_atomic: s.admission_buffer_atomic,
                    admission_reopen_atomic: s.admission_reopen_atomic,
                    invariant_holds: s.invariant_holds,
                });
            }
            Ok(out)
        })
    }

    fn onchain(&self) -> BoxFut<'_, Result<OnchainView, AdminError>> {
        Box::pin(self.fetch_onchain())
    }

    fn manual_review(&self) -> BoxFut<'_, Result<ManualReviewView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let now = Self::now();
            let mut requests = Vec::new();
            for direction in Direction::ALL {
                for req in ledger.requests_by_state(direction, RequestState::ManualReview)? {
                    // Wallet-window context via the SAME route-generic
                    // Ledger read the public eligibility endpoint uses
                    // (`Ledger::route_wallet_eligibility`) — never a
                    // second implementation of the window arithmetic.
                    // Every route has both windows now: the destination
                    // keyed on `recipient`, the source on `source_wallet`
                    // (schema v28; NULL only on a Goldcoin-sourced row
                    // whose deposit has not been traced yet, which then
                    // reads as no source window).
                    let windows = ledger.route_wallet_eligibility(
                        direction,
                        req.source_wallet.as_deref(),
                        Some(&req.recipient),
                        now,
                    )?;
                    let (recipient_until, wallet_until) =
                        (windows.destination_retry_after, windows.source_retry_after);
                    requests.push(ManualReviewItemView {
                        request_id: req.id,
                        // `Direction::as_str` — the exact spelling the
                        // rest of the system parses ("GlcToSol"/
                        // "SolToGlc"), explicit rather than `{:?}` so a
                        // Rust rename can't change the wire format.
                        direction: direction.as_str().to_string(),
                        reason: req.manual_review_note.clone(),
                        gross_amount_atomic: req.gross_amount_atomic,
                        net_amount_atomic: req.net_amount_atomic,
                        created_at: req.created_at,
                        recipient_rate_limited_until: recipient_until,
                        source_wallet_rate_limited_until: wallet_until,
                    });
                }
            }
            Ok(ManualReviewView { requests })
        })
    }

    fn refunds(&self) -> BoxFut<'_, Result<RefundsView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let mut refunds: Vec<RefundItemView> = Vec::new();
            let mut with_row: std::collections::HashSet<i64> = std::collections::HashSet::new();

            // Existing refund lifecycle rows, at any stage.
            for row in ledger.list_solana_refunds(false)? {
                with_row.insert(row.request_id);
                let request = ledger.get_request(row.request_id)?;
                let request_state = request
                    .as_ref()
                    .map(|r| r.state.as_str().to_string())
                    .unwrap_or_else(|| "Unknown".to_string());
                let terminal = row.state == SolanaRefundState::Confirmed;
                let gross = request.as_ref().map(|r| r.gross_amount_atomic).unwrap_or(0);
                refunds.push(RefundItemView {
                    request_id: row.request_id,
                    request_state,
                    direction: Direction::SolToGlc.as_str().to_string(),
                    manual_review_reason: Some(row.manual_review_reason.clone()),
                    gross_amount_atomic: gross,
                    gross_amount_display_glc: cli_command::format_atomic_as_decimal_string(
                        gross,
                        CANONICAL_DISPLAY_DECIMALS,
                    ),
                    source_obligation_index: Some(row.obligation_index),
                    requester: Some(base58(&row.requester)),
                    destination_token_account: Some(base58(&row.destination_token_account)),
                    refund_state: Some(row.state.as_str().to_string()),
                    refund_signature: row.refund_signature.clone(),
                    refund_nonce: Some(row.nonce),
                    refund_amount_solana_atomic: Some(row.amount_solana_atomic),
                    refund_note: Some(row.note.clone()),
                    refund_created_by: Some(row.created_by.clone()),
                    refund_created_at: Some(row.created_at),
                    refund_broadcast_at: row.broadcast_at,
                    refund_confirmed_at: row.confirmed_at,
                    terminal,
                    // A terminal refund is done: the console must never
                    // offer it any further action.
                    dry_run_available: !terminal,
                });
            }

            // Refund CANDIDATES: still parked in ManualReview, with a
            // reason on the same whitelist the refund path itself
            // enforces — read from `Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS`
            // so this listing can never drift from what is actually
            // refundable.
            for req in ledger.requests_by_state(Direction::SolToGlc, RequestState::ManualReview)? {
                if with_row.contains(&req.id) {
                    continue;
                }
                let whitelisted = req
                    .manual_review_note
                    .as_deref()
                    .is_some_and(|r| Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS.contains(&r));
                if !whitelisted {
                    continue;
                }
                refunds.push(RefundItemView {
                    request_id: req.id,
                    request_state: req.state.as_str().to_string(),
                    direction: req.direction.as_str().to_string(),
                    manual_review_reason: req.manual_review_note.clone(),
                    gross_amount_atomic: req.gross_amount_atomic,
                    gross_amount_display_glc: cli_command::format_atomic_as_decimal_string(
                        req.gross_amount_atomic,
                        CANONICAL_DISPLAY_DECIMALS,
                    ),
                    source_obligation_index: req.source_obligation_index,
                    requester: req.requester.as_ref().map(base58),
                    // Derived only at dry-run time, from the verified
                    // on-chain obligation — never stored ahead of it and
                    // never caller-supplied.
                    destination_token_account: None,
                    refund_state: None,
                    refund_signature: None,
                    refund_nonce: None,
                    refund_amount_solana_atomic: None,
                    refund_note: None,
                    refund_created_by: None,
                    refund_created_at: None,
                    refund_broadcast_at: None,
                    refund_confirmed_at: None,
                    terminal: false,
                    dry_run_available: true,
                });
            }

            refunds.sort_by_key(|r| r.request_id);
            Ok(RefundsView { refunds })
        })
    }

    fn refund_dry_run(&self, request_id: i64) -> BoxFut<'_, Result<RefundDryRunView, AdminError>> {
        Box::pin(async move {
            use crate::solana::refund;
            // Phased exactly as `refund::dry_run_refund` phases itself,
            // for one reason: `Ledger` is not `Sync`, so a `&Ledger` held
            // across the chain-read `.await` would make this handler
            // future non-`Send`. Each ledger borrow is therefore opened
            // and dropped inside its own scope. No check is
            // reimplemented here — phases 1 and 3 are the refund
            // module's own functions, and the verdict comes from its
            // `assemble_refund_dry_run`.
            let inputs = {
                let ledger = self.open_ledger()?;
                // A clean 404 for a request that simply does not exist,
                // rather than surfacing it as a failed dry run.
                if ledger.get_request(request_id)?.is_none() {
                    return Err(AdminError::NotFound(format!(
                        "bridge request {request_id} not found"
                    )));
                }
                refund::refund_dry_run_ledger_inputs(&ledger, request_id)
                    .map_err(AdminError::Upstream)?
            };
            let plan = refund::build_refund_plan(&self.rpc, &inputs.request).await;
            let capacity = match &plan {
                Ok(p) => {
                    let ledger = self.open_ledger()?;
                    Some(ledger.solana_refund_capacity(request_id, p.amount_solana_atomic)?)
                }
                Err(_) => None,
            };
            Ok(refund_dry_run_view(refund::assemble_refund_dry_run(
                inputs, plan, capacity,
            )))
        })
    }

    fn execute_glc_refund(
        &self,
        request_id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<GlcRefundExecuteView, AdminError>> {
        Box::pin(async move {
            let Some(executor) = self.refund_executor.as_ref() else {
                return Err(AdminError::Upstream(
                    "refund execution is not wired on this deployment: the admin API was built \
                     without a vault signer executor, so it cannot move funds"
                        .to_string(),
                ));
            };
            executor.execute(request_id, note, actor).await
        })
    }

    fn set_local_pause(
        &self,
        direction: ReserveDirection,
        paused: bool,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            audited_set_local_pause(&mut ledger, direction, paused, &note, &actor)
        })
    }

    fn set_admission(
        &self,
        direction: ReserveDirection,
        closed: bool,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            audited_set_admission(&mut ledger, direction, closed, &note, &actor)
        })
    }

    fn resume_manual_review(
        &self,
        request_id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            audited_resume_manual_review(&mut ledger, request_id, &note, &actor)
                .map(|(_outcome, receipt)| receipt)
        })
    }

    fn robinhood_treasury_withdrawals(
        &self,
    ) -> BoxFut<'_, Result<Vec<crate::robinhood::admin::TreasuryWithdrawalView>, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            Ok(crate::robinhood::admin::treasury_withdrawal_views(
                &ledger, None, None,
            )?)
        })
    }

    fn robinhood_treasury_withdrawal(
        &self,
        operation_id: i64,
    ) -> BoxFut<'_, Result<crate::robinhood::admin::TreasuryWithdrawalView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            crate::robinhood::admin::treasury_withdrawal_views(&ledger, None, Some(operation_id))?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    AdminError::NotFound(format!(
                        "treasury withdrawal operation {operation_id} not found"
                    ))
                })
        })
    }

    fn rebalances(&self) -> BoxFut<'_, Result<RebalancesView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let mut assessments = Vec::with_capacity(3);
            for direction in [
                ReserveDirection::GoldcoinReserve,
                ReserveDirection::SolanaReserve,
                ReserveDirection::RobinhoodReserve,
            ] {
                // The Robinhood reserve exists only when
                // `[reserve.robinhood]` is configured; absent is not an
                // error for this listing, it is simply not listed.
                let a = match crate::rebalance::assess(&ledger, direction) {
                    Ok(a) => a,
                    Err(LedgerError::ReserveNotInitialized(ReserveDirection::RobinhoodReserve)) => {
                        continue
                    }
                    Err(e) => return Err(e.into()),
                };
                assessments.push(RebalanceStatusView {
                    direction: direction_name(direction).to_string(),
                    severity: match a.severity {
                        crate::rebalance::ImbalanceSeverity::Normal => "Normal",
                        crate::rebalance::ImbalanceSeverity::Warning => "Warning",
                        crate::rebalance::ImbalanceSeverity::Critical => "Critical",
                    }
                    .to_string(),
                    total_reserve_balance: a.total_reserve_balance,
                    protected_minimum: a.protected_minimum,
                    target_reserve: a.target_reserve,
                    warning_reserve: a.warning_reserve,
                    critical_reserve: a.critical_reserve,
                    suggested_deposit_atomic: a.suggested_deposit_atomic,
                });
            }
            let requests = ledger
                .list_rebalances(None, false)?
                .into_iter()
                .map(RebalanceView::from_request)
                .collect();
            Ok(RebalancesView {
                assessments,
                requests,
            })
        })
    }

    fn rebalance(&self, id: i64) -> BoxFut<'_, Result<RebalanceView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            ledger
                .get_rebalance(id)?
                .map(RebalanceView::from_request)
                .ok_or_else(|| AdminError::NotFound(format!("rebalance request {id} not found")))
        })
    }

    fn rebalance_propose(
        &self,
        input: RebalanceProposeInput,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let direction = parse_rebalance_direction(&input.direction)?;
            let kind = match input.kind.as_str() {
                "deposit" => RebalanceKind::Deposit,
                "withdraw" => RebalanceKind::Withdraw,
                other => {
                    return Err(AdminError::BadRequest(format!(
                        "unknown kind {other:?} (expected deposit|withdraw)"
                    )))
                }
            };
            let mut ledger = self.open_ledger()?;
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_propose",
                    target: "new".to_string(),
                    note: &input.note,
                    new_value: Some(format!(
                        "{} {} amount={}",
                        direction_name(direction),
                        input.kind,
                        input.amount_atomic
                    )),
                },
                |_| Ok(None),
                |l| {
                    l.propose_rebalance(
                        direction,
                        kind,
                        input.amount_atomic,
                        &input.note,
                        &actor,
                        input.required_approvals,
                        now_unix(),
                    )
                    .map_err(AdminError::from)
                },
                |id, params| params.target = id.to_string(),
            )
            .map(|(_id, receipt)| receipt)
        })
    }

    fn rebalance_approve(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_approve",
                    target: id.to_string(),
                    note: &note,
                    new_value: None,
                },
                |_| Ok(None),
                |l| {
                    l.approve_rebalance(id, &actor, now_unix())
                        .map(|_| ())
                        .map_err(AdminError::from)
                },
                |_, _| {},
            )
            .map(|((), receipt)| receipt)
        })
    }

    fn rebalance_reject(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_reject",
                    target: id.to_string(),
                    note: &note,
                    new_value: None,
                },
                |_| Ok(None),
                |l| {
                    l.reject_rebalance(id, &note, &actor, now_unix())
                        .map_err(AdminError::from)
                },
                |_, _| {},
            )
            .map(|((), receipt)| receipt)
        })
    }

    fn rebalance_cancel(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_cancel",
                    target: id.to_string(),
                    note: &note,
                    new_value: None,
                },
                |_| Ok(None),
                |l| {
                    l.cancel_rebalance(id, &note, &actor, now_unix())
                        .map_err(AdminError::from)
                },
                |_, _| {},
            )
            .map(|((), receipt)| receipt)
        })
    }

    fn rebalance_record_executed(
        &self,
        id: i64,
        input: RebalanceRecordExecutedInput,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            // Records evidence of a transaction the operator already
            // executed through real custody tooling outside this system —
            // this never constructs, signs, or broadcasts anything.
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_record_executed",
                    target: id.to_string(),
                    note: &input.note,
                    new_value: Some(format!("tx_reference={}", input.tx_reference)),
                },
                |_| Ok(None),
                |l| {
                    l.record_rebalance_executed(id, &input.tx_reference, &actor, now_unix())
                        .map_err(AdminError::from)
                },
                |_, _| {},
            )
            .map(|((), receipt)| receipt)
        })
    }

    fn rebalance_confirm(
        &self,
        id: i64,
        input: RebalanceConfirmInput,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_confirm",
                    target: id.to_string(),
                    note: &input.note,
                    new_value: Some(format!("observed_amount={}", input.observed_amount_atomic)),
                },
                |_| Ok(None),
                |l| {
                    l.confirm_rebalance(id, input.observed_amount_atomic, &actor, now_unix())
                        .map_err(AdminError::from)
                },
                |_, _| {},
            )
            .map(|((), receipt)| receipt)
        })
    }

    fn rebalance_fail(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_fail",
                    target: id.to_string(),
                    note: &note,
                    new_value: None,
                },
                |_| Ok(None),
                |l| {
                    l.fail_rebalance(id, &note, &actor, now_unix())
                        .map_err(AdminError::from)
                },
                |_, _| {},
            )
            .map(|((), receipt)| receipt)
        })
    }

    fn audit_log(&self, filter: AdminAuditFilter) -> BoxFut<'_, Result<AuditLogView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let rows = ledger
                .list_admin_audit(&filter)?
                .into_iter()
                .map(AuditRowView::from_row)
                .collect();
            Ok(AuditLogView { rows })
        })
    }

    fn cli_command(
        &self,
        input: cli_command::CliCommandInput,
    ) -> BoxFut<'_, Result<cli_command::CliCommandView, AdminError>> {
        Box::pin(async move {
            let onchain = self.fetch_onchain().await?;
            cli_command::generate(&input, &onchain).map_err(AdminError::BadRequest)
        })
    }
}

// ------------------------------------------------------------- router --

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(bytes)))
        .expect("well-formed response")
}

fn error_response(err: AdminError) -> Response<Full<Bytes>> {
    json_response(
        err.status(),
        &ErrorBody {
            error: err.to_string(),
        },
    )
}

fn unauthorized() -> Response<Full<Bytes>> {
    json_response(
        StatusCode::UNAUTHORIZED,
        &ErrorBody {
            error: "missing or invalid bearer token".to_string(),
        },
    )
}

/// Every admin mutation body is a small JSON object; anything beyond
/// this is a mistake or abuse, and buffering it unbounded on the daemon
/// that also runs settlements would be an OOM lever for anyone holding a
/// leaked token.
const MAX_BODY_BYTES: u64 = 64 * 1024;

async fn read_json<T: serde::de::DeserializeOwned>(
    req: Request<hyper::body::Incoming>,
) -> Result<T, Box<Response<Full<Bytes>>>> {
    let limited = http_body_util::Limited::new(req.into_body(), MAX_BODY_BYTES as usize);
    let body = match limited.collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            return Err(Box::new(json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &ErrorBody {
                    error: format!("request body unreadable or larger than {MAX_BODY_BYTES} bytes"),
                },
            )))
        }
    };
    serde_json::from_slice::<T>(&body).map_err(|e| {
        Box::new(json_response(
            StatusCode::BAD_REQUEST,
            &ErrorBody {
                error: format!("malformed request body: {e}"),
            },
        ))
    })
}

/// Minimal application/x-www-form-urlencoded value decoding for the
/// audit filters: `+` is a space and `%XX` is a byte — an operator name
/// like "ops team" arrives as `ops+team` (URLSearchParams) or
/// `ops%20team` and must filter the same rows either way. A malformed
/// escape is a caller error, not something to pass through silently.
fn percent_decode(value: &str) -> Result<String, AdminError> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .and_then(|pair| std::str::from_utf8(pair).ok())
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                    .ok_or_else(|| {
                        AdminError::BadRequest(format!("malformed percent-escape in {value:?}"))
                    })?;
                out.push(hex);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out)
        .map_err(|_| AdminError::BadRequest(format!("query value {value:?} is not valid UTF-8")))
}

fn parse_audit_query(query: Option<&str>) -> Result<AdminAuditFilter, AdminError> {
    let mut filter = AdminAuditFilter::default();
    let Some(q) = query else {
        return Ok(filter);
    };
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");
        // Strict on an audit surface: an empty filter value (`?actor=`)
        // or an unknown key (`?acton=...`) silently matching EVERY row
        // would misattribute what a reviewer reads as filtered results —
        // fail loudly instead.
        if value.is_empty() {
            return Err(AdminError::BadRequest(format!(
                "query parameter {key:?} has an empty value"
            )));
        }
        match key {
            "before_id" => {
                filter.before_id = Some(value.parse::<i64>().map_err(|_| {
                    AdminError::BadRequest("before_id must be an integer".to_string())
                })?);
            }
            "limit" => {
                let n = value.parse::<u32>().map_err(|_| {
                    AdminError::BadRequest("limit must be a positive integer".to_string())
                })?;
                // Zero would be a permanently empty page that looks like
                // "no audit rows" — reject it like the public API's
                // pagination does, never serve it.
                if n == 0 {
                    return Err(AdminError::BadRequest("limit must be >= 1".to_string()));
                }
                filter.limit = Some(n);
            }
            "action" => filter.action = Some(percent_decode(value)?),
            "actor" => filter.actor = Some(percent_decode(value)?),
            other => {
                return Err(AdminError::BadRequest(format!(
                    "unknown query parameter {other:?} (expected before_id|limit|action|actor)"
                )))
            }
        }
    }
    Ok(filter)
}

/// `/rebalances/{id}` and `/rebalances/{id}/{verb}` path parsing.
fn parse_rebalance_path(path: &str) -> Option<(i64, Option<&str>)> {
    let rest = path.strip_prefix("/rebalances/")?;
    let mut parts = rest.splitn(2, '/');
    let id = parts.next()?.parse::<i64>().ok()?;
    Some((id, parts.next()))
}

/// `/refunds/glc/{id}/execute` path parsing — the ONE fund-moving route.
/// Deliberately a distinct prefix from the Solana-side `/refunds/{id}/…`
/// family so the two can never be confused by a proxy rule or a reader:
/// the Solana refund's execution still stays a `glc-admin` command line
/// requiring the admin keypair, which this API never holds.
fn parse_glc_refund_execute_path(path: &str) -> Option<i64> {
    let rest = path.strip_prefix("/refunds/glc/")?;
    let id = rest.strip_suffix("/execute")?;
    id.parse::<i64>().ok()
}

/// `/refunds/{id}/dry-run` path parsing. There is deliberately no
/// `/refunds/{id}/execute` counterpart: SOLANA refund execution needs the
/// admin keypair and the attestation signer stack, which this API never
/// holds (see the module docs). The console renders a `glc-admin` command
/// line for CLI approval instead.
fn parse_refund_dry_run_path(path: &str) -> Option<i64> {
    let rest = path.strip_prefix("/refunds/")?;
    let id = rest.strip_suffix("/dry-run")?;
    id.parse::<i64>().ok()
}

/// `/manual-review/{id}/resume` path parsing.
fn parse_manual_review_resume_path(path: &str) -> Option<i64> {
    let rest = path.strip_prefix("/manual-review/")?;
    let id = rest.strip_suffix("/resume")?;
    id.parse::<i64>().ok()
}

async fn handle<S: AdminSource>(
    req: Request<hyper::body::Incoming>,
    source: Arc<S>,
    registry: Arc<OperatorRegistry>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Structural anti-CSRF: this API is for non-browser callers only. A
    // request carrying browser-ambient credentials (`Cookie`) or a
    // browser-stamped `Origin` header is refused before authentication is
    // even considered — see the module docs.
    if req.headers().contains_key(hyper::header::COOKIE)
        || req.headers().contains_key(hyper::header::ORIGIN)
    {
        return Ok(json_response(
            StatusCode::FORBIDDEN,
            &ErrorBody {
                error: "browser-originated requests are not accepted by this API".to_string(),
            },
        ));
    }

    // Every endpoint — reads included — requires a valid operator token.
    // Capabilities are captured here alongside the identity, so the one
    // fund-moving route can gate on more than "is this a valid admin
    // token" without re-parsing the header.
    let (actor, may_execute_glc_refunds) = match req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| registry.verify_bearer_with_capabilities(v))
    {
        Some(op) => (op.name.to_string(), op.may_execute_glc_refunds),
        None => return Ok(unauthorized()),
    };

    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let response = match (&method, path.as_str()) {
        (&Method::GET, "/whoami") => json_response(StatusCode::OK, &WhoamiView { operator: actor }),
        (&Method::GET, "/status") => match source.status().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/reserve-health") => match source.reserve_health().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/onchain") => match source.onchain().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/fee") => json_response(StatusCode::OK, &fee_view(&source.route_fees())),
        (&Method::GET, "/manual-review") => match source.manual_review().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/refunds") => match source.refunds().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/rebalances") => match source.rebalances().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/robinhood/treasury-withdrawals") => {
            match source.robinhood_treasury_withdrawals().await {
                Ok(v) => json_response(StatusCode::OK, &v),
                Err(e) => error_response(e),
            }
        }
        (&Method::GET, "/audit-log") => match parse_audit_query(req.uri().query()) {
            Ok(filter) => match source.audit_log(filter).await {
                Ok(v) => json_response(StatusCode::OK, &v),
                Err(e) => error_response(e),
            },
            Err(e) => error_response(e),
        },
        (&Method::POST, "/pause") | (&Method::POST, "/unpause") => {
            let pausing = path == "/pause";
            match read_json::<DirectionNoteInput>(req).await {
                Ok(input) => {
                    match (
                        parse_reserve_direction(&input.direction),
                        require_note(&input.note),
                    ) {
                        (Ok(direction), Ok(note)) => {
                            match source
                                .set_local_pause(direction, pausing, note.to_string(), actor)
                                .await
                            {
                                Ok(v) => json_response(StatusCode::OK, &v),
                                Err(e) => error_response(e),
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => error_response(e),
                    }
                }
                Err(resp) => *resp,
            }
        }
        (&Method::POST, "/admission/close") | (&Method::POST, "/admission/open") => {
            let closing = path == "/admission/close";
            match read_json::<DirectionNoteInput>(req).await {
                Ok(input) => {
                    match (
                        parse_reserve_direction(&input.direction),
                        require_note(&input.note),
                    ) {
                        (Ok(direction), Ok(note)) => {
                            match source
                                .set_admission(direction, closing, note.to_string(), actor)
                                .await
                            {
                                Ok(v) => json_response(StatusCode::OK, &v),
                                Err(e) => error_response(e),
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => error_response(e),
                    }
                }
                Err(resp) => *resp,
            }
        }
        (&Method::POST, "/rebalances") => match read_json::<RebalanceProposeInput>(req).await {
            Ok(input) => match require_note(&input.note) {
                Ok(_) => match source.rebalance_propose(input, actor).await {
                    Ok(v) => json_response(StatusCode::CREATED, &v),
                    Err(e) => error_response(e),
                },
                Err(e) => error_response(e),
            },
            Err(resp) => *resp,
        },
        (&Method::POST, "/cli-command") => {
            match read_json::<cli_command::CliCommandInput>(req).await {
                Ok(input) => match source.cli_command(input).await {
                    Ok(v) => json_response(StatusCode::OK, &v),
                    Err(e) => error_response(e),
                },
                Err(resp) => *resp,
            }
        }
        (&Method::POST, other_path) => {
            if let Some(request_id) = parse_glc_refund_execute_path(other_path) {
                // The ONE fund-moving route. Three independent gates
                // before the handler is even reached, each fail-closed:
                //
                // 1. A valid operator token (already checked above).
                // 2. The deployment has granted the capability to SOMEONE.
                //    An absent or empty allow-list refuses outright, so a
                //    deployment that never opted in cannot move funds
                //    through this API at all.
                // 3. THIS operator holds it. An ordinary admin token is
                //    deliberately not enough.
                //
                // The request body carries a request id and a note and
                // nothing else — no destination, amount, fee, transaction,
                // signer or override input exists to be supplied. Every
                // value is derived server-side from chain and ledger.
                if !registry.any_refund_executor() {
                    json_response(
                        StatusCode::FORBIDDEN,
                        &ErrorBody {
                            error: "refund execution is not enabled on this deployment: no \
                                    operator has may_execute_glc_refunds = true"
                                .to_string(),
                        },
                    )
                } else if !may_execute_glc_refunds {
                    json_response(
                        StatusCode::FORBIDDEN,
                        &ErrorBody {
                            error: "this operator is not on the refund-execution allow-list; \
                                    ordinary admin access does not permit moving vault funds"
                                .to_string(),
                        },
                    )
                } else {
                    match read_json::<NoteInput>(req).await {
                        Ok(input) => match require_note(&input.note) {
                            Ok(note) => {
                                match source
                                    .execute_glc_refund(request_id, note.to_string(), actor)
                                    .await
                                {
                                    Ok(v) => json_response(StatusCode::OK, &v),
                                    Err(e) => error_response(e),
                                }
                            }
                            Err(e) => error_response(e),
                        },
                        Err(resp) => *resp,
                    }
                }
            } else if let Some(request_id) = parse_manual_review_resume_path(other_path) {
                match read_json::<NoteInput>(req).await {
                    Ok(input) => match require_note(&input.note) {
                        Ok(note) => {
                            match source
                                .resume_manual_review(request_id, note.to_string(), actor)
                                .await
                            {
                                Ok(v) => json_response(StatusCode::OK, &v),
                                Err(e) => error_response(e),
                            }
                        }
                        Err(e) => error_response(e),
                    },
                    Err(resp) => *resp,
                }
            } else if let Some((id, Some(verb))) = parse_rebalance_path(other_path) {
                match verb {
                    "approve" | "reject" | "cancel" | "fail" => {
                        match read_json::<NoteInput>(req).await {
                            Ok(input) => match require_note(&input.note) {
                                Ok(note) => {
                                    let note = note.to_string();
                                    let result = match verb {
                                        "approve" => {
                                            source.rebalance_approve(id, note, actor).await
                                        }
                                        "reject" => source.rebalance_reject(id, note, actor).await,
                                        "cancel" => source.rebalance_cancel(id, note, actor).await,
                                        _ => source.rebalance_fail(id, note, actor).await,
                                    };
                                    match result {
                                        Ok(v) => json_response(StatusCode::OK, &v),
                                        Err(e) => error_response(e),
                                    }
                                }
                                Err(e) => error_response(e),
                            },
                            Err(resp) => *resp,
                        }
                    }
                    "record-executed" => {
                        match read_json::<RebalanceRecordExecutedInput>(req).await {
                            Ok(input) => match require_note(&input.note) {
                                Ok(_) => {
                                    match source.rebalance_record_executed(id, input, actor).await {
                                        Ok(v) => json_response(StatusCode::OK, &v),
                                        Err(e) => error_response(e),
                                    }
                                }
                                Err(e) => error_response(e),
                            },
                            Err(resp) => *resp,
                        }
                    }
                    "confirm" => match read_json::<RebalanceConfirmInput>(req).await {
                        Ok(input) => match require_note(&input.note) {
                            Ok(_) => match source.rebalance_confirm(id, input, actor).await {
                                Ok(v) => json_response(StatusCode::OK, &v),
                                Err(e) => error_response(e),
                            },
                            Err(e) => error_response(e),
                        },
                        Err(resp) => *resp,
                    },
                    _ => json_response(
                        StatusCode::NOT_FOUND,
                        &ErrorBody {
                            error: "not found".to_string(),
                        },
                    ),
                }
            } else {
                json_response(
                    StatusCode::NOT_FOUND,
                    &ErrorBody {
                        error: "not found".to_string(),
                    },
                )
            }
        }
        (&Method::GET, other_path) => {
            if let Some(request_id) = parse_refund_dry_run_path(other_path) {
                match source.refund_dry_run(request_id).await {
                    Ok(v) => json_response(StatusCode::OK, &v),
                    Err(e) => error_response(e),
                }
            } else if let Some((id, None)) = parse_rebalance_path(other_path) {
                match source.rebalance(id).await {
                    Ok(v) => json_response(StatusCode::OK, &v),
                    Err(e) => error_response(e),
                }
            } else if let Some(id) = other_path
                .strip_prefix("/robinhood/treasury-withdrawals/")
                .and_then(|rest| rest.parse::<i64>().ok())
            {
                match source.robinhood_treasury_withdrawal(id).await {
                    Ok(v) => json_response(StatusCode::OK, &v),
                    Err(e) => error_response(e),
                }
            } else {
                json_response(
                    StatusCode::NOT_FOUND,
                    &ErrorBody {
                        error: "not found".to_string(),
                    },
                )
            }
        }
        _ => json_response(
            StatusCode::NOT_FOUND,
            &ErrorBody {
                error: "not found".to_string(),
            },
        ),
    };

    Ok(response)
}

/// Serves the admin API until `shutdown` flips. Bind `addr` privately —
/// see the module docs; there is no TLS termination here (put the
/// operators' reverse proxy in front for that), but unlike the public
/// API every request is authenticated.
pub async fn serve<S: AdminSource>(
    addr: SocketAddr,
    source: Arc<S>,
    registry: Arc<OperatorRegistry>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "admin API listening");
    serve_on(listener, source, registry, shutdown).await
}

/// [`serve`] over a listener the CALLER already bound.
///
/// Behaviourally identical — [`serve`] is this function plus the bind —
/// and exists so a caller that must know the port BEFORE the server
/// starts can bind it itself and never let go. Binding to port 0, reading
/// the assigned port, dropping the listener and re-binding that port
/// later is a race: between the drop and the re-bind the port is owned by
/// nobody, and anything else on the host may take it. The test harnesses
/// need exactly that "tell me the port first" ordering, so they hand the
/// live listener over instead.
pub async fn serve_on<S: AdminSource>(
    listener: tokio::net::TcpListener,
    source: Arc<S>,
    registry: Arc<OperatorRegistry>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("admin API: shutdown signal received, exiting");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "admin API: accept failed");
                        continue;
                    }
                };
                let source = Arc::clone(&source);
                let registry = Arc::clone(&registry);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        handle(req, Arc::clone(&source), Arc::clone(&registry))
                    });
                    if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                        tracing::debug!(%peer, error = %e, "admin API connection ended");
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
