//! ManualReview refund execution (Solana side): plans, verifies,
//! constructs, simulates, broadcasts, and confirms the on-chain
//! `rebalance_withdraw` transaction that returns a fold-parked SolToGlc
//! deposit to its ORIGINAL depositor — driven by `glc-admin
//! refund-manual-review` (docs/09-runbook.md "ManualReview refunds
//! (Solana->Goldcoin)").
//!
//! # Security posture (nothing new, nothing weakened)
//!
//! Fund movement uses the dedicated refund instruction
//! (`programs/glc-reserve-bridge/src/instructions/refund_withdraw.rs`)
//! and its full authorization stack: admin signature AND a 2-of-3
//! threshold attestation over the canonical claim, the bridge already
//! globally paused (on-chain enforced), the live protected-minimum check,
//! Token-2022 `transfer_checked` via the reserve-authority PDA, and the
//! per-nonce `rebalance_withdrawal` PDA replay guard — every one of them
//! preserved unchanged from the retired shared instruction, plus three
//! new on-chain bindings: the destination must be the requester's derived
//! ATA, the amount must equal the obligation exactly, and the obligation
//! must still be `Pending`. Attestation
//! signatures come through the same `signing` signer stack the daemon
//! uses (remote endpoints in production — no attestation key ever exists
//! on this host); the admin keypair is CLI-supplied, exactly like every
//! other `glc-admin` on-chain command.
//!
//! # What the signed claim binds
//!
//! Since the 2026-09-02 reserve-withdrawal hardening this path uses its
//! own claim family, `shared::claim::refund_withdraw_claim_message`
//! (action byte `0x06`, 210 bytes), instead of the retired
//! `rebalance_withdraw_claim_message` (`0x03`) it shared with operator
//! withdrawals. The bytes bind: protocol version, program id, attestation
//! epoch, action, **nonce**, **amount**, **destination token account**,
//! **reserve mint**, **source reserve token account**, **obligation
//! index**, and the **obligation's requester**.
//!
//! Two consequences worth stating plainly:
//!
//! 1. A refund approval can no longer be presented to the operator
//!    withdrawal instruction, or vice versa — the action byte and the
//!    length both differ, and the program compares the full message for
//!    byte equality. Before the split, one signature format served both
//!    classes, so an attestation signer could not tell which it was
//!    approving.
//! 2. The obligation index is now IN the signed bytes and is also the seed
//!    of the obligation account the program loads, so an approval for one
//!    deposit cannot be redirected at another.
//!
//! The remaining bindings are still carried by construction:
//!
//! - request id + refund domain — the nonce IS `refund domain bit |
//!   request_id` ([`crate::ledger::Ledger::solana_refund_nonce`]), and the
//!   program now ENFORCES that the domain bit is set;
//! - original requester + token program — the destination IS the
//!   canonical ATA of (requester, reserve mint, reserve token program),
//!   now enforced structurally on chain by Anchor's
//!   `associated_token::authority` constraint rather than only by the
//!   off-chain builder;
//! - `solana_refunds` (UNIQUE `obligation_index`, PRIMARY KEY
//!   `request_id`) and `ux_bridge_requests_sol_source` remain the
//!   off-chain once-only guard.
//!
//! # Idempotency / crash recovery
//!
//! At most one refund transaction can ever land per request, on-chain,
//! forever: the deterministic nonce's PDA `init`. Recovery never resolves
//! uncertainty by building another transfer — a rebuild (same nonce) is
//! only permitted after POSITIVELY observing that the recorded
//! transaction can no longer land (`is_blockhash_valid` false on its
//! recorded blockhash) AND its nonce PDA does not exist.

use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction;

use crate::admin_api::{
    audited_begin_solana_refund, audited_mark_solana_refund_confirmed,
    audited_record_solana_refund_broadcast,
};
use crate::amount_conversion::CanonicalAtomic;
use crate::ledger::{
    BridgeRequest, Ledger, SolanaRefund, SolanaRefundCapacityCheck, SolanaRefundDbChecks,
    SolanaRefundState, VerifiedRefundInputs,
};
use crate::signing::signers::AttestationSigner;
use crate::solana::accounts::{self, PROGRAM_ID};
use crate::solana::confirm::{confirm_transaction, ConfirmFailure, ConfirmPolicy};
use crate::solana::ed25519;
use crate::solana::instructions;
use crate::solana::rpc::SolanaRpc;

/// Same value `glc-treasury-withdraw` uses — must match the live
/// `BridgeConfig.protocol_version` or the on-chain claim comparison
/// fails closed.
pub const REFUND_PROTOCOL_VERSION: u8 = 1;

/// `WithdrawalStatus::Pending`'s wire value (`programs/.../state.rs`:
/// `Pending` = 0, `Broadcast` = 1, `Completed` = 2). Anything other than
/// `Pending` is on-chain settlement evidence and refuses a refund.
const WITHDRAWAL_STATUS_PENDING: u8 = 0;

/// Everything a refund needs, assembled from FRESH `finalized`-commitment
/// chain reads plus the stored request row — with every cross-check
/// between the two already enforced (any failure is an error from
/// [`build_refund_plan`], never a value in here).
#[derive(Debug, Clone)]
pub struct RefundPlan {
    pub request_id: i64,
    pub obligation_index: u64,
    pub obligation_pda: Pubkey,
    /// The depositor wallet, from the on-chain obligation (verified equal
    /// to the stored `bridge_requests.requester`).
    pub requester: Pubkey,
    /// ATA(requester, reserve mint, reserve token program) — derived,
    /// never supplied.
    pub destination_token_account: Pubkey,
    /// Whether the destination ATA currently exists. When it does not,
    /// the execute transaction prepends the idempotent ATA-create
    /// instruction (submitter-paid) — the exact pattern
    /// `Orchestrator::submit_release` already uses for release
    /// recipients.
    pub destination_exists: bool,
    pub reserve_mint: Pubkey,
    pub token_program: Pubkey,
    pub mint_decimals: u8,
    /// Exact gross deposited amount (on-chain `WithdrawalObligation
    /// .amount`, Solana-native units) — verified equal to the stored
    /// canonical gross narrowed to `mint_decimals`. No fee is deducted:
    /// the SolToGlc bridge fee only accrues at settlement, which this
    /// request never reached.
    pub amount_solana_atomic: u64,
    pub gross_canonical_atomic: u64,
    pub nonce: u64,
    pub nonce_pda: Pubkey,
    pub nonce_pda_exists: bool,
    pub attestation_epoch: u64,
    pub attestation_threshold: u8,
    pub attestation_keys: Vec<Pubkey>,
    pub bridge_paused: bool,
    pub protected_minimum: u64,
    pub reserve_token_account: Pubkey,
    pub reserve_balance: u64,
    /// The canonical 138-byte claim the attestation signers sign and the
    /// on-chain program re-derives and verifies.
    pub claim_message: Vec<u8>,
}

/// Decoded on-chain `RebalanceWithdrawal` record (the nonce PDA) — read
/// back during crash recovery to verify that whatever landed under this
/// refund's nonce is EXACTLY this refund, never assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebalanceWithdrawalRecord {
    pub nonce: u64,
    pub amount: u64,
    pub destination: Pubkey,
    pub admin: Pubkey,
    pub attestation_epoch: u64,
}

/// Layout per `programs/glc-reserve-bridge/src/state.rs::RebalanceWithdrawal`
/// (after the 8-byte Anchor discriminator): nonce u64, amount u64,
/// destination Pubkey, admin Pubkey, attestation_epoch u64, ...
pub fn decode_rebalance_withdrawal(data: &[u8]) -> Result<RebalanceWithdrawalRecord, String> {
    let body = data
        .get(8..)
        .ok_or("rebalance_withdrawal account shorter than discriminator")?;
    let field = |range: std::ops::Range<usize>| -> Result<&[u8], String> {
        body.get(range)
            .ok_or_else(|| "truncated rebalance_withdrawal account".to_string())
    };
    Ok(RebalanceWithdrawalRecord {
        nonce: u64::from_le_bytes(field(0..8)?.try_into().unwrap()),
        amount: u64::from_le_bytes(field(8..16)?.try_into().unwrap()),
        destination: Pubkey::try_from(field(16..48)?).unwrap(),
        admin: Pubkey::try_from(field(48..80)?).unwrap(),
        attestation_epoch: u64::from_le_bytes(field(80..88)?.try_into().unwrap()),
    })
}

/// Builds the full refund plan from fresh finalized chain reads,
/// enforcing every chain-side cross-check fail-closed. Read-only: no
/// signer contact, no keypair, no broadcast, no database write.
pub async fn build_refund_plan<R: SolanaRpc>(
    rpc: &R,
    request: &BridgeRequest,
) -> Result<RefundPlan, String> {
    // Both Solana-SOURCED directions: the deposit being returned is the
    // same `WithdrawalObligation` whichever chain it was bound for.
    if !request.direction.source_is_solana() {
        return Err(format!(
            "request {} is {:?}, not SolToGlc/SolToRhn",
            request.id, request.direction
        ));
    }
    let obligation_index = request
        .source_obligation_index
        .ok_or_else(|| format!("request {} has no source_obligation_index", request.id))?;
    let stored_requester = request
        .requester
        .ok_or_else(|| format!("request {} has no requester recorded", request.id))?;

    let config_account = rpc
        .get_account(&accounts::bridge_config_pda())
        .await
        .map_err(|e| e.to_string())?
        .ok_or("bridge_config does not exist on this cluster")?;
    let config = accounts::decode_bridge_config(&config_account.data).map_err(|e| e.to_string())?;
    if config.reserve_token_mint == Pubkey::default()
        || config.reserve_token_program == Pubkey::default()
    {
        return Err("reserve vault is not configured on this deployment".to_string());
    }

    let key_set_account = rpc
        .get_account(&accounts::attestation_key_set_pda())
        .await
        .map_err(|e| e.to_string())?
        .ok_or("attestation_key_set does not exist")?;
    let key_set =
        accounts::decode_attestation_key_set(&key_set_account.data).map_err(|e| e.to_string())?;

    // The original, finalized deposit — the on-chain obligation is the
    // ground truth every stored value must agree with.
    let obligation_pda = accounts::withdrawal_obligation_pda(obligation_index);
    let obligation_account = rpc
        .get_account(&obligation_pda)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "withdrawal obligation #{obligation_index} does not exist at {obligation_pda} — \
                 refusing (the finalized deposit record is the refund's ground truth)"
            )
        })?;
    let obligation = accounts::decode_withdrawal_obligation(&obligation_account.data)
        .map_err(|e| e.to_string())?;
    if obligation.index != obligation_index {
        return Err(format!(
            "obligation PDA {obligation_pda} decodes to index {}, expected {obligation_index}",
            obligation.index
        ));
    }
    if obligation.requester.to_bytes() != stored_requester {
        return Err(format!(
            "REFUSING — stored requester does not match the on-chain obligation's requester \
             ({}); the database and chain disagree about the original sender",
            obligation.requester
        ));
    }
    if obligation.status != WITHDRAWAL_STATUS_PENDING {
        return Err(format!(
            "REFUSING — on-chain obligation #{obligation_index} status is {} (not Pending): \
             settlement evidence exists on-chain",
            obligation.status
        ));
    }

    let mint_decimals = accounts::fetch_reserve_mint_decimals(rpc, &config.reserve_token_mint)
        .await
        .map_err(|e| e.to_string())?;
    let expected_solana_gross = CanonicalAtomic(request.gross_amount_atomic)
        .to_solana(mint_decimals)
        .map_err(|e| format!("stored canonical gross does not narrow exactly: {e}"))?;
    if expected_solana_gross.0 != obligation.amount {
        return Err(format!(
            "REFUSING — stored gross ({} canonical -> {} native) does not equal the on-chain \
             deposited amount ({} native)",
            request.gross_amount_atomic, expected_solana_gross.0, obligation.amount
        ));
    }

    let requester = obligation.requester;
    let destination_token_account = accounts::associated_token_address(
        &requester,
        &config.reserve_token_mint,
        &config.reserve_token_program,
    );
    let destination_exists = match rpc
        .get_account(&destination_token_account)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(account) => {
            let dest_mint =
                accounts::decode_token_account_mint(&account.data).map_err(|e| e.to_string())?;
            if dest_mint != config.reserve_token_mint {
                return Err(format!(
                    "REFUSING — destination token account {destination_token_account} has mint \
                     {dest_mint}, expected the reserve mint {}",
                    config.reserve_token_mint
                ));
            }
            if account.owner != config.reserve_token_program {
                return Err(format!(
                    "REFUSING — destination token account {destination_token_account} is owned \
                     by program {}, expected the reserve token program {}",
                    account.owner, config.reserve_token_program
                ));
            }
            true
        }
        None => false,
    };

    let reserve_authority = accounts::reserve_authority_pda();
    let reserve_token_account = accounts::associated_token_address(
        &reserve_authority,
        &config.reserve_token_mint,
        &config.reserve_token_program,
    );
    let reserve_balance_account = rpc
        .get_account(&reserve_token_account)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("reserve token account {reserve_token_account} does not exist"))?;
    let reserve_balance = accounts::decode_token_account_amount(&reserve_balance_account.data)
        .map_err(|e| e.to_string())?;

    let nonce = Ledger::solana_refund_nonce(request.id).map_err(|e| e.to_string())?;
    let nonce_pda = accounts::rebalance_withdrawal_pda(nonce);
    let nonce_pda_exists = rpc
        .get_account(&nonce_pda)
        .await
        .map_err(|e| e.to_string())?
        .is_some();

    let claim_message = glc_reserve_bridge_shared::claim::refund_withdraw_claim_message(
        REFUND_PROTOCOL_VERSION,
        &PROGRAM_ID.to_bytes(),
        key_set.epoch,
        nonce,
        obligation.amount,
        &destination_token_account.to_bytes(),
        &config.reserve_token_mint.to_bytes(),
        &reserve_token_account.to_bytes(),
        obligation_index,
        &requester.to_bytes(),
    )
    .to_vec();

    Ok(RefundPlan {
        request_id: request.id,
        obligation_index,
        obligation_pda,
        requester,
        destination_token_account,
        destination_exists,
        reserve_mint: config.reserve_token_mint,
        token_program: config.reserve_token_program,
        mint_decimals,
        amount_solana_atomic: obligation.amount,
        gross_canonical_atomic: request.gross_amount_atomic,
        nonce,
        nonce_pda,
        nonce_pda_exists,
        attestation_epoch: key_set.epoch,
        attestation_threshold: key_set.threshold,
        attestation_keys: key_set.keys,
        bridge_paused: config.paused,
        protected_minimum: config.protected_minimum,
        reserve_token_account,
        reserve_balance,
        claim_message,
    })
}

/// The chain-verified inputs the ledger records at refund-begin, taken
/// straight from an already-cross-checked plan.
pub fn verified_inputs(plan: &RefundPlan) -> VerifiedRefundInputs {
    VerifiedRefundInputs {
        obligation_index: plan.obligation_index,
        amount_solana_atomic: plan.amount_solana_atomic,
        gross_canonical_atomic: plan.gross_canonical_atomic,
        requester: plan.requester.to_bytes(),
        destination_token_account: plan.destination_token_account.to_bytes(),
        reserve_mint: plan.reserve_mint.to_bytes(),
        token_program: plan.token_program.to_bytes(),
    }
}

/// Verifies an existing `solana_refunds` row against a freshly built
/// plan — the two derive from the same immutable on-chain facts, so ANY
/// disagreement means tampering/corruption and is a hard refusal.
pub fn verify_refund_row_matches_plan(
    refund: &SolanaRefund,
    plan: &RefundPlan,
) -> Result<(), String> {
    let mismatch = |what: &str| {
        Err(format!(
            "REFUSING — stored refund row for request {} does not match the freshly re-derived \
             plan ({what} differs); investigate before proceeding",
            refund.request_id
        ))
    };
    if refund.nonce != plan.nonce {
        return mismatch("nonce");
    }
    if refund.obligation_index != plan.obligation_index {
        return mismatch("obligation index");
    }
    if refund.amount_solana_atomic != plan.amount_solana_atomic {
        return mismatch("amount");
    }
    if refund.requester != plan.requester.to_bytes() {
        return mismatch("requester");
    }
    if refund.destination_token_account != plan.destination_token_account.to_bytes() {
        return mismatch("destination token account");
    }
    if refund.reserve_mint != plan.reserve_mint.to_bytes() {
        return mismatch("reserve mint");
    }
    if refund.token_program != plan.token_program.to_bytes() {
        return mismatch("token program");
    }
    Ok(())
}

/// The execute-time chain preconditions — mirrors what the on-chain
/// instruction itself enforces so a violation is caught clearly, before
/// any signer or the network is contacted. `expect_nonce_unused` is true
/// for a fresh/rebuild broadcast (a used nonce there means the refund
/// already happened — or, with no refund row, that the DATABASE is
/// behind the chain, e.g. restored from an old backup: refuse and point
/// at the runbook, never transfer again).
pub fn verify_execute_preconditions(plan: &RefundPlan) -> Result<(), String> {
    if plan.amount_solana_atomic == 0 {
        return Err("refund amount is zero".to_string());
    }
    if !plan.bridge_paused {
        return Err(
            "REFUSING — live on-chain BridgeConfig.paused is false. Refund execution requires \
             the global pause (on-chain enforced). Pause first: glc-admin onchain-pause \
             --scope global ... — and unpause explicitly afterwards; this command never \
             pauses or unpauses on its own"
                .to_string(),
        );
    }
    let required_floor = plan
        .amount_solana_atomic
        .saturating_add(plan.protected_minimum);
    if plan.reserve_balance < required_floor {
        return Err(format!(
            "REFUSING — refunding {} would breach protected_minimum; only {} is available \
             above the floor",
            plan.amount_solana_atomic,
            plan.reserve_balance.saturating_sub(plan.protected_minimum)
        ));
    }
    Ok(())
}

/// Collects EXACTLY `threshold` valid attestation signatures over the
/// claim from the configured signer stack, verifying each returned
/// signature locally and only counting current on-chain attestation keys
/// — the identical discipline the on-chain verifier applies, run
/// client-side first.
///
/// Exactly the threshold, not every signer that answers: the same
/// `take(attestation_threshold)` the orchestrator's release path applies.
/// Every signature past the threshold is dead weight the program never
/// reads, and it is not free — each one adds 110 bytes to the ed25519
/// proof instruction, and with three signers the assembled
/// `[create_ata, proof, refund_withdraw]` transaction (210-byte refund
/// claim since 2026-09-02) exceeded Solana's 1232-byte packet limit and
/// was refused by the node before it was ever executed. Found by the
/// real-node acceptance rehearsal; `tests::collect_attestations_stops_at_
/// the_threshold_and_the_refund_fits_a_packet` pins the bound.
pub async fn collect_attestations(
    signers: &[Box<dyn AttestationSigner>],
    message: &[u8],
    current_keys: &[Pubkey],
    threshold: u8,
) -> Result<Vec<(Pubkey, Signature)>, String> {
    let mut valid: Vec<(Pubkey, Signature)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for signer in signers {
        let pubkey = signer.pubkey();
        if !current_keys.contains(&pubkey) {
            eprintln!("warning: skipping signer {pubkey} — not a current attestation key");
            continue;
        }
        if !seen.insert(pubkey) {
            continue;
        }
        match signer.sign_message(message).await {
            Ok(signature) => {
                if !signature.verify(pubkey.as_ref(), message) {
                    eprintln!(
                        "warning: signer {pubkey} returned a signature that does not verify — \
                         skipping"
                    );
                    continue;
                }
                valid.push((pubkey, signature));
                if valid.len() == usize::from(threshold) {
                    break;
                }
            }
            Err(e) => eprintln!("warning: signer {pubkey} refused/failed: {e}"),
        }
    }
    if valid.len() < threshold as usize {
        return Err(format!(
            "only {} valid attestation signature(s) collected, but the threshold is {threshold}",
            valid.len()
        ));
    }
    Ok(valid)
}

/// Builds the refund transaction's instruction list:
/// `[ed25519 proof, refund_withdraw]` — the proof's mandatory relative -1
/// adjacency to `refund_withdraw`, and nothing else. The depositor's ATA
/// is created, when missing, by its own transaction
/// ([`build_destination_ata_instruction`]) so the refund itself stays
/// within one packet.
pub fn build_refund_instructions(
    plan: &RefundPlan,
    attestations: &[(Pubkey, Signature)],
    admin: &Pubkey,
    _submitter: &Pubkey,
) -> Vec<solana_sdk::instruction::Instruction> {
    let proof = ed25519::build_attestation_proof(attestations, &plan.claim_message);
    let withdraw = instructions::refund_withdraw(
        admin,
        &plan.reserve_mint,
        &plan.token_program,
        &plan.requester,
        &plan.destination_token_account,
        plan.nonce,
        plan.amount_solana_atomic,
        plan.attestation_epoch,
        plan.obligation_index,
    );
    vec![proof, withdraw]
}

/// The depositor's own ATA for the reserve mint, created idempotently in
/// ITS OWN transaction when the plan found it missing — never bundled
/// with the refund. Bundling was the shape until the real-node rehearsal
/// measured it: `[create_ata, 2-of-3 proof, refund_withdraw]` with the
/// 210-byte refund claim is 1244 bytes at worst, over Solana's 1232-byte
/// packet limit, so the node refused the whole refund unexecuted. The
/// creation moves no reserve funds, needs only the submitter's signature,
/// and is idempotent, so it needs no ledger record and a crash between
/// the two transactions costs nothing. `None` when the ATA already
/// exists — the common case, since the deposit itself was paid out of it.
pub fn build_destination_ata_instruction(
    plan: &RefundPlan,
    submitter: &Pubkey,
) -> Option<solana_sdk::instruction::Instruction> {
    if plan.destination_exists {
        return None;
    }
    Some(instructions::create_recipient_ata_idempotent(
        submitter,
        &plan.requester,
        &plan.reserve_mint,
        &plan.token_program,
    ))
}

/// The one check that is an operator PRECONDITION rather than a property
/// of the request — named once so the report can identify it without a
/// duplicated string literal.
pub const PAUSE_CHECK_NAME: &str =
    "bridge globally paused (operator precondition for --execute; not required for this dry run)";

/// One named safety check for the dry-run report.
#[derive(Debug, Clone)]
pub struct RefundCheck {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    /// True for a check that is an OPERATOR PRECONDITION for executing
    /// rather than a property of the request itself — currently only the
    /// global pause. The runbook's procedure is deliberately "dry-run
    /// first, then pause, then execute", so a not-yet-engaged pause must
    /// not be reported as the request being ineligible; it is reported
    /// separately as work still to do. Execution still enforces it (and
    /// re-checks it immediately before simulating) with no override.
    pub is_execute_precondition: bool,
}

/// The full dry-run view: the stored rows, the per-check results, and
/// (when the chain-side plan could be built at all) the plan + capacity
/// figures. Assembled read-only.
#[derive(Debug)]
pub struct RefundDryRunReport {
    pub request: BridgeRequest,
    pub refund: Option<SolanaRefund>,
    pub db_checks: SolanaRefundDbChecks,
    /// `Err` carries the fail-closed reason the chain-side plan could not
    /// be built (itself a failed check).
    pub plan: Result<RefundPlan, String>,
    pub capacity: Option<SolanaRefundCapacityCheck>,
    pub checks: Vec<RefundCheck>,
    /// Every REQUEST-level check passes: the request itself is refundable
    /// on the merits. Says nothing about whether the operator has engaged
    /// the global pause yet — see [`Self::pause_engaged`].
    pub eligible_ignoring_pause: bool,
    /// The on-chain global pause is currently engaged (an execute
    /// precondition, enforced on-chain and re-checked immediately before
    /// simulation).
    pub pause_engaged: bool,
    /// Executing right now would proceed: eligible on the merits AND the
    /// pause is already engaged. Also true when the refund has already
    /// confirmed, where `--execute` is a safe no-op; read it together
    /// with [`Self::already_refunded`].
    pub would_execute: bool,
    /// The refund already confirmed — `--execute` would report the
    /// existing transaction and change nothing.
    pub already_refunded: bool,
}

/// The ledger-side inputs of a dry run, gathered in ONE synchronous pass.
///
/// Split out so a caller can finish its ledger work and drop the `Ledger`
/// borrow before awaiting any chain read. `Ledger` wraps a rusqlite
/// `Connection`, which is `Send` but not `Sync`, so a `&Ledger` held
/// across an `.await` makes the whole future non-`Send` — which the admin
/// API's `Send` handler futures cannot accept. Phasing the work this way
/// keeps every eligibility rule in one place (see
/// [`assemble_refund_dry_run`]) instead of forcing a second, duplicated
/// implementation for the HTTP surface.
#[derive(Debug)]
pub struct RefundDryRunLedgerInputs {
    pub request: BridgeRequest,
    pub refund: Option<SolanaRefund>,
    pub db_checks: SolanaRefundDbChecks,
}

/// Phase 1 of a dry run: every ledger read, no chain access.
pub fn refund_dry_run_ledger_inputs(
    ledger: &Ledger,
    request_id: i64,
) -> Result<RefundDryRunLedgerInputs, String> {
    let request = ledger
        .get_request(request_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("bridge request {request_id} not found"))?;
    let refund = ledger
        .get_solana_refund(request_id)
        .map_err(|e| e.to_string())?;
    let db_checks = ledger
        .solana_refund_db_checks(request_id)
        .map_err(|e| e.to_string())?;
    Ok(RefundDryRunLedgerInputs {
        request,
        refund,
        db_checks,
    })
}

/// The full dry run: ledger inputs, then the chain-side plan, then the
/// capacity check, then [`assemble_refund_dry_run`]. Read-only
/// everywhere: no signer contact, no keypair loads, no broadcast, no
/// database write.
pub async fn dry_run_refund<R: SolanaRpc>(
    rpc: &R,
    ledger: &Ledger,
    request_id: i64,
) -> Result<RefundDryRunReport, String> {
    let inputs = refund_dry_run_ledger_inputs(ledger, request_id)?;
    let plan = build_refund_plan(rpc, &inputs.request).await;
    let capacity = match &plan {
        Ok(p) => Some(
            ledger
                .solana_refund_capacity(request_id, p.amount_solana_atomic)
                .map_err(|e| e.to_string())?,
        ),
        Err(_) => None,
    };
    Ok(assemble_refund_dry_run(inputs, plan, capacity))
}

/// Phase 3: assembles the report from already-gathered inputs. PURE — no
/// ledger, no chain, no I/O of any kind. Every eligibility, precondition
/// and verdict rule lives here and nowhere else, so the CLI dry run and
/// the admin API's dry run are the same logic by construction.
pub fn assemble_refund_dry_run(
    inputs: RefundDryRunLedgerInputs,
    plan: Result<RefundPlan, String>,
    capacity: Option<SolanaRefundCapacityCheck>,
) -> RefundDryRunReport {
    let RefundDryRunLedgerInputs {
        request,
        refund,
        db_checks,
    } = inputs;

    let mut checks: Vec<RefundCheck> = Vec::new();
    let already_terminal = matches!(
        refund.as_ref().map(|r| r.state),
        Some(SolanaRefundState::Confirmed)
    );
    let mut push = |name: &'static str, ok: bool, detail: String| {
        checks.push(RefundCheck {
            name,
            ok,
            detail,
            is_execute_precondition: false,
        });
    };

    push(
        "direction is Solana-sourced (SolToGlc/SolToRhn)",
        db_checks.direction_ok,
        format!("{:?}", db_checks.direction),
    );
    push(
        "state allows a refund lifecycle",
        db_checks.state_is_manual_review || refund.is_some(),
        format!("{:?}", db_checks.state),
    );
    push(
        "manual-review reason is whitelisted",
        db_checks.reason_whitelisted || refund.is_some(),
        format!("{:?}", db_checks.manual_review_reason),
    );
    push(
        "source deposit finalized",
        db_checks.source_finalized,
        String::new(),
    );
    push(
        "no Goldcoin payout row",
        db_checks.no_goldcoin_payout,
        String::new(),
    );
    push(
        "no destination transaction",
        db_checks.no_destination_txid,
        String::new(),
    );
    push("not settled", db_checks.not_settled, String::new());
    push(
        "never advanced past ManualReview (no reservation ever applied)",
        db_checks.never_advanced_past_manual_review,
        String::new(),
    );
    push(
        "not held, or held with the `refund` operator decision recorded",
        db_checks.hold_blocker.is_none(),
        db_checks.hold_blocker.clone().unwrap_or_default(),
    );
    match &plan {
        Ok(p) => {
            push(
                "on-chain obligation matches stored request (index, requester, amount, Pending)",
                true,
                format!(
                    "obligation #{} at {}, amount {} native",
                    p.obligation_index, p.obligation_pda, p.amount_solana_atomic
                ),
            );
            push(
                "destination is the depositor's canonical Token-2022 ATA",
                true,
                format!(
                    "{} ({})",
                    p.destination_token_account,
                    if p.destination_exists {
                        "exists"
                    } else {
                        "will be created idempotently at execute, submitter-paid"
                    }
                ),
            );
            // Passes when no on-chain refund record exists yet, or when
            // this database's own refund lifecycle accounts for it. An
            // existing PDA with NO refund row means the database is
            // behind the chain (e.g. restored from an older backup) —
            // execute refuses that outright.
            push(
                "refund nonce consistent with this database",
                refund.is_some() || !p.nonce_pda_exists,
                format!(
                    "nonce {:#x}, PDA {} ({})",
                    p.nonce,
                    p.nonce_pda,
                    if p.nonce_pda_exists {
                        "exists on-chain"
                    } else {
                        "unused"
                    }
                ),
            );
            push(
                "protected minimum preserved (on-chain floor)",
                p.reserve_balance >= p.amount_solana_atomic.saturating_add(p.protected_minimum),
                format!(
                    "balance {} - refund {} >= protected_minimum {}",
                    p.reserve_balance, p.amount_solana_atomic, p.protected_minimum
                ),
            );
            push(
                PAUSE_CHECK_NAME,
                p.bridge_paused,
                format!("paused = {}", p.bridge_paused),
            );
        }
        Err(e) => push("chain-side verification", false, e.clone()),
    }
    if let Some(c) = &capacity {
        push(
            "SolanaReserve capacity (stricter than the on-chain floor: reserved GlcToSol \
             liquidity and other open refunds excluded first)",
            c.ok,
            format!(
                "balance {} - protected {} - reserved {} - other open refunds {} >= amount {}",
                c.total_reserve_balance,
                c.protected_minimum,
                c.reserved_liquidity,
                c.other_open_refunds_atomic,
                c.amount_solana_atomic
            ),
        );
    }

    // The closure's borrow of `checks` has ended; mark the single
    // execute-precondition entry so the verdict can exclude it from
    // request-level eligibility.
    let mut precondition_count = 0;
    for c in checks.iter_mut() {
        if c.name == PAUSE_CHECK_NAME {
            c.is_execute_precondition = true;
            precondition_count += 1;
        }
    }
    debug_assert!(
        precondition_count <= 1,
        "exactly one pause precondition check is expected"
    );

    let eligible_ignoring_pause = already_terminal
        || checks
            .iter()
            .filter(|c| !c.is_execute_precondition)
            .all(|c| c.ok);
    let pause_engaged = plan.as_ref().map(|p| p.bridge_paused).unwrap_or(false);
    let would_execute = already_terminal || (eligible_ignoring_pause && pause_engaged);
    RefundDryRunReport {
        request,
        refund,
        db_checks,
        plan,
        capacity,
        checks,
        eligible_ignoring_pause,
        pause_engaged,
        would_execute,
        already_refunded: already_terminal,
    }
}

/// Outcome of a guarded execute run.
#[derive(Debug)]
pub enum RefundExecuteOutcome {
    /// The refund had already confirmed (this run, or a previous one) —
    /// reported, nothing new broadcast.
    AlreadyRefunded { signature: Option<String> },
    /// The refund transaction confirmed at `finalized` commitment during
    /// this run and the request is now `Refunded`.
    Confirmed { signature: String },
}

/// The guarded execute pipeline. Re-runs every check against fresh
/// state, is safe to re-run at any point, and NEVER builds a second
/// transfer while a recorded one could still land. See module docs.
#[allow(clippy::too_many_arguments)]
pub async fn execute_refund<R: SolanaRpc>(
    rpc: &R,
    ledger: &mut Ledger,
    attestation_signers: &[Box<dyn AttestationSigner>],
    admin: &Keypair,
    submitter: &Keypair,
    request_id: i64,
    note: &str,
    actor: &str,
    policy: ConfirmPolicy,
) -> Result<RefundExecuteOutcome, String> {
    let request = ledger
        .get_request(request_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("bridge request {request_id} not found"))?;
    let existing = ledger
        .get_solana_refund(request_id)
        .map_err(|e| e.to_string())?;

    if let Some(refund) = &existing {
        if refund.state == SolanaRefundState::Confirmed {
            return Ok(RefundExecuteOutcome::AlreadyRefunded {
                signature: refund.refund_signature.clone(),
            });
        }
    }

    let plan = build_refund_plan(rpc, &request).await?;
    if let Some(refund) = &existing {
        verify_refund_row_matches_plan(refund, &plan)?;
    }

    // ---- crash/rerun recovery for an already-recorded broadcast -------
    if let Some(refund) = &existing {
        if refund.state == SolanaRefundState::Broadcast {
            return recover_broadcast(
                rpc,
                ledger,
                attestation_signers,
                admin,
                submitter,
                &plan,
                refund,
                note,
                actor,
                policy,
            )
            .await;
        }
    }

    // ---- fresh begin (or resume of a Pending row) ---------------------
    verify_execute_preconditions(&plan)?;
    if plan.nonce_pda_exists {
        return Err(format!(
            "REFUSING — the on-chain refund record for nonce {:#x} already exists at {} but \
             this database has no matching broadcast record. The refund very likely already \
             happened against a database state this one does not reflect (e.g. restored from \
             an older backup). Reconcile the database against the on-chain record before doing \
             anything else — a second transfer will never be constructed for this request",
            plan.nonce, plan.nonce_pda
        ));
    }
    if existing.is_none() {
        audited_begin_solana_refund(ledger, request_id, &verified_inputs(&plan), note, actor)
            .map_err(|e| e.to_string())?;
    }

    broadcast_and_confirm(
        rpc,
        ledger,
        attestation_signers,
        admin,
        submitter,
        &plan,
        note,
        actor,
        policy,
    )
    .await
}

/// Recovery for a refund whose broadcast was recorded but whose fate is
/// unknown (crash, timeout, or operator rerun): read the on-chain
/// postconditions back, and only ever rebuild once the recorded
/// transaction demonstrably can never land.
#[allow(clippy::too_many_arguments)]
async fn recover_broadcast<R: SolanaRpc>(
    rpc: &R,
    ledger: &mut Ledger,
    attestation_signers: &[Box<dyn AttestationSigner>],
    admin: &Keypair,
    submitter: &Keypair,
    plan: &RefundPlan,
    refund: &SolanaRefund,
    note: &str,
    actor: &str,
    policy: ConfirmPolicy,
) -> Result<RefundExecuteOutcome, String> {
    let signature_str = refund
        .refund_signature
        .as_deref()
        .expect("Broadcast state implies a recorded signature (schema CHECK)");
    let signature: Signature = signature_str
        .parse()
        .map_err(|e| format!("recorded refund signature is invalid: {e}"))?;

    // Did anything land under this refund's nonce? The PDA is readable at
    // finalized commitment, so its presence alone proves a finalized
    // transfer — verify it is EXACTLY ours before concluding anything.
    let nonce_pda_account = rpc
        .get_account(&plan.nonce_pda)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(account) = nonce_pda_account {
        let record = decode_rebalance_withdrawal(&account.data)?;
        if record.nonce != plan.nonce
            || record.amount != plan.amount_solana_atomic
            || record.destination != plan.destination_token_account
        {
            return Err(format!(
                "CRITICAL — the on-chain record at {} does not match this refund (recorded \
                 amount {}, destination {}); investigate before proceeding",
                plan.nonce_pda, record.amount, record.destination
            ));
        }
        audited_mark_solana_refund_confirmed(ledger, refund.request_id, note, actor)
            .map_err(|e| e.to_string())?;
        return Ok(RefundExecuteOutcome::Confirmed {
            signature: signature_str.to_string(),
        });
    }

    // No PDA: the transfer has NOT happened (at finalized commitment).
    match rpc
        .get_signature_status(&signature)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(Ok(())) => {
            // Finalized success but no PDA — impossible for this
            // transaction shape; fail closed rather than guess.
            Err(format!(
                "CRITICAL — signature {signature} reports finalized success but the refund \
                 record PDA does not exist; refusing to proceed, investigate manually"
            ))
        }
        Some(Err(reason)) => {
            // Landed and FAILED: the transfer did not happen; the nonce
            // is not consumed. Rebuild under the same nonce.
            eprintln!(
                "recorded refund tx {signature} landed but FAILED on-chain ({reason}); \
                 rebuilding under the same nonce"
            );
            verify_execute_preconditions(plan)?;
            broadcast_and_confirm(
                rpc,
                ledger,
                attestation_signers,
                admin,
                submitter,
                plan,
                note,
                actor,
                policy,
            )
            .await
        }
        None => {
            let recorded_blockhash: Hash = refund
                .recent_blockhash
                .as_deref()
                .expect("Broadcast state implies a recorded blockhash (schema CHECK)")
                .parse()
                .map_err(|e| format!("recorded blockhash is invalid: {e}"))?;
            let still_landable = rpc
                .is_blockhash_valid(&recorded_blockhash)
                .await
                .map_err(|e| e.to_string())?;
            if still_landable {
                // The recorded transaction could still land. Wait for a
                // definite outcome — NEVER a concurrent second transfer.
                match confirm_transaction(rpc, &signature, &recorded_blockhash, policy).await {
                    Ok(()) => {
                        audited_mark_solana_refund_confirmed(
                            ledger,
                            refund.request_id,
                            note,
                            actor,
                        )
                        .map_err(|e| e.to_string())?;
                        Ok(RefundExecuteOutcome::Confirmed {
                            signature: signature_str.to_string(),
                        })
                    }
                    Err(ConfirmFailure::Expired { .. }) => {
                        // Now positively dead — rebuild below.
                        verify_execute_preconditions(plan)?;
                        broadcast_and_confirm(
                            rpc,
                            ledger,
                            attestation_signers,
                            admin,
                            submitter,
                            plan,
                            note,
                            actor,
                            policy,
                        )
                        .await
                    }
                    Err(e) => Err(format!(
                        "recorded refund tx's outcome is still undetermined ({e}); nothing was \
                         rebuilt or re-broadcast — rerun this command to continue recovery"
                    )),
                }
            } else {
                // Positively dead: blockhash can no longer land AND the
                // nonce PDA does not exist. Rebuilding under the SAME
                // nonce is safe forever.
                verify_execute_preconditions(plan)?;
                broadcast_and_confirm(
                    rpc,
                    ledger,
                    attestation_signers,
                    admin,
                    submitter,
                    plan,
                    note,
                    actor,
                    policy,
                )
                .await
            }
        }
    }
}

/// Fresh broadcast: attest -> re-check pause/floor immediately before
/// simulation -> simulate -> record -> send -> confirm.
#[allow(clippy::too_many_arguments)]
async fn broadcast_and_confirm<R: SolanaRpc>(
    rpc: &R,
    ledger: &mut Ledger,
    attestation_signers: &[Box<dyn AttestationSigner>],
    admin: &Keypair,
    submitter: &Keypair,
    plan: &RefundPlan,
    note: &str,
    actor: &str,
    policy: ConfirmPolicy,
) -> Result<RefundExecuteOutcome, String> {
    let attestations = collect_attestations(
        attestation_signers,
        &plan.claim_message,
        &plan.attestation_keys,
        plan.attestation_threshold,
    )
    .await?;

    // The mandated LAST-INSTANT re-check: global pause + protected
    // minimum + nonce still unused, against fresh finalized reads,
    // immediately before simulation/broadcast — state may have changed
    // since the plan was built.
    let fresh_config_account = rpc
        .get_account(&accounts::bridge_config_pda())
        .await
        .map_err(|e| e.to_string())?
        .ok_or("bridge_config disappeared")?;
    let fresh_config =
        accounts::decode_bridge_config(&fresh_config_account.data).map_err(|e| e.to_string())?;
    let fresh_balance_account = rpc
        .get_account(&plan.reserve_token_account)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("reserve token account disappeared")?;
    let fresh_balance = accounts::decode_token_account_amount(&fresh_balance_account.data)
        .map_err(|e| e.to_string())?;
    let fresh_nonce_pda_exists = rpc
        .get_account(&plan.nonce_pda)
        .await
        .map_err(|e| e.to_string())?
        .is_some();
    let mut fresh_plan = plan.clone();
    fresh_plan.bridge_paused = fresh_config.paused;
    fresh_plan.protected_minimum = fresh_config.protected_minimum;
    fresh_plan.reserve_balance = fresh_balance;
    verify_execute_preconditions(&fresh_plan)?;
    if fresh_nonce_pda_exists {
        return Err(
            "the refund nonce PDA appeared between planning and broadcast — a concurrent \
             execution won; rerun to verify and finalize it"
                .to_string(),
        );
    }

    // The destination ATA first, on its own, when it is missing — see
    // `build_destination_ata_instruction` for why it is never bundled.
    if let Some(create_ata) = build_destination_ata_instruction(plan, &submitter.pubkey()) {
        let blockhash = rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let ata_tx = Transaction::new_signed_with_payer(
            &[create_ata],
            Some(&submitter.pubkey()),
            &[submitter],
            blockhash,
        );
        let ata_signature = ata_tx.signatures[0];
        rpc.send_transaction(&ata_tx)
            .await
            .map_err(|e| format!("creating the depositor's token account: {e}"))?;
        confirm_transaction(rpc, &ata_signature, &blockhash, policy)
            .await
            .map_err(|e| {
                format!(
                    "the depositor's token account creation ({ata_signature}) did not confirm: \
                     {e}. Nothing else was sent; rerun this command"
                )
            })?;
    }

    let ix = build_refund_instructions(plan, &attestations, &admin.pubkey(), &submitter.pubkey());
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|e| e.to_string())?;
    let tx = Transaction::new_signed_with_payer(
        &ix,
        Some(&submitter.pubkey()),
        &[submitter, admin],
        blockhash,
    );
    let signature = tx.signatures[0];

    let simulation = rpc
        .simulate_transaction(&tx)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(err) = &simulation.err {
        let mut detail = format!("simulation FAILED: {err}");
        for log in &simulation.logs {
            detail.push_str("\n    ");
            detail.push_str(log);
        }
        return Err(format!(
            "{detail}\nrefusing to broadcast — nothing was sent; fix the cause and rerun"
        ));
    }
    let sim_summary = format!(
        "simulation ok (units consumed: {:?})",
        simulation.units_consumed
    );

    // Record BEFORE sending: real fund movement is never un-evidenced,
    // and a crash between the two is recovered from the nonce PDA /
    // recorded signature+blockhash, never by a blind second transfer.
    audited_record_solana_refund_broadcast(
        ledger,
        plan.request_id,
        &signature.to_string(),
        &blockhash.to_string(),
        plan.attestation_epoch,
        &sim_summary,
        note,
        actor,
    )
    .map_err(|e| e.to_string())?;

    rpc.send_transaction(&tx).await.map_err(|e| {
        format!(
            "broadcast failed after the intent was recorded ({e}); rerun this command — it \
             will read the on-chain state back and either finalize or safely rebuild under \
             the same nonce"
        )
    })?;

    match confirm_transaction(rpc, &signature, &blockhash, policy).await {
        Ok(()) => {
            audited_mark_solana_refund_confirmed(ledger, plan.request_id, note, actor)
                .map_err(|e| e.to_string())?;
            Ok(RefundExecuteOutcome::Confirmed {
                signature: signature.to_string(),
            })
        }
        Err(ConfirmFailure::Rejected { reason, .. }) => Err(format!(
            "refund tx {signature} was rejected on-chain: {reason}. No funds moved (the nonce \
             record was not created). Rerun to re-verify and rebuild under the same nonce"
        )),
        Err(e) => Err(format!(
            "refund tx {signature} is not yet demonstrably final ({e}); its intent and \
             signature are durably recorded — rerun this command to continue confirmation or \
             recovery. No second transfer will ever be constructed for this request"
        )),
    }
}

#[cfg(test)]
mod tests;
